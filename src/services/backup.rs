//! Scheduled copies of the database, off unless `[backup] enabled`.
//!
//! `VACUUM INTO` writes a consistent, compacted copy while the server keeps
//! running. A copy holds the JWT secret and device token hashes, so it is
//! written owner-only, as `.partial` first so a half-written file is never
//! mistaken for a copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use log::{info, warn};
use sqlx::SqlitePool;

use crate::config::BackupConfig;
use crate::state::AppState;
use crate::storage::repositories::{NewHostEvent, RuntimeStateRepository};

const PREFIX: &str = "remon-";
const EXT: &str = ".sqlite3";
const PARTIAL: &str = ".partial";
/// Unix seconds of the last good copy, so a restart does not reset the clock.
const LAST_KEY: &str = "backup_last_at";
/// Never right at boot: a crash loop must not copy the database every time.
const BOOT_DELAY: Duration = Duration::from_secs(10 * 60);
const RETRY_DELAY: Duration = Duration::from_secs(60 * 60);

pub struct Copy {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Write one copy of the database at `db_path` into `dir`, then drop all but
/// the newest `keep`.
pub async fn run_once(
    pool: &SqlitePool,
    db_path: &Path,
    dir: &Path,
    keep: usize,
) -> anyhow::Result<Copy> {
    create_private_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    remove_partials(dir);

    let db_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    if let Some(free) = free_space(dir)
        && free < db_bytes.saturating_mul(2)
    {
        bail!(
            "{} has {} MiB free; a copy of the {} MiB database needs twice that",
            dir.display(),
            free >> 20,
            db_bytes >> 20
        );
    }

    let name = format!(
        "{PREFIX}{}{EXT}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let path = dir.join(&name);
    let partial = dir.join(format!("{name}{PARTIAL}"));
    // VACUUM INTO accepts an existing empty file and keeps its mode.
    create_private_file(&partial).with_context(|| format!("create {}", partial.display()))?;
    let written = sqlx::query("VACUUM INTO ?")
        .bind(partial.to_string_lossy().into_owned())
        .execute(pool)
        .await;
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e).context("VACUUM INTO");
    }
    std::fs::rename(&partial, &path).with_context(|| format!("rename to {}", path.display()))?;

    prune(dir, keep);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Copy { path, bytes })
}

/// Copies in `dir`, oldest first. Names sort by time.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut copies: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(EXT))
        })
        .collect();
    copies.sort();
    copies
}

fn prune(dir: &Path, keep: usize) {
    let copies = list(dir);
    for old in &copies[..copies.len().saturating_sub(keep)] {
        if let Err(e) = std::fs::remove_file(old) {
            warn!("backup: could not remove {}: {e}", old.display());
        }
    }
}

/// Left behind by a copy cut short by a crash or shutdown.
fn remove_partials(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for p in entries.flatten().map(|e| e.path()) {
        if p.to_string_lossy().ends_with(PARTIAL) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn create_private_file(path: &Path) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path).map(drop)
}

/// Free bytes on the filesystem holding `dir`, if it can be told.
fn free_space(dir: &Path) -> Option<u64> {
    let dir = dir.canonicalize().ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Copy on the configured interval while the server runs.
pub fn spawn(state: Arc<AppState>, cfg: BackupConfig, db_path: PathBuf) {
    if !cfg.enabled {
        return;
    }
    let dir = cfg.resolved_dir();
    let interval = Duration::from_secs(cfg.interval_hours * 3600);
    info!(
        "backups on: every {}h into {}, keeping {}",
        cfg.interval_hours,
        dir.display(),
        cfg.keep
    );

    tokio::spawn(async move {
        let runtime = RuntimeStateRepository::new(state.db.clone());
        let mut shutdown = state.shutdown.subscribe();
        let mut failed = false;
        loop {
            let wait = if failed {
                RETRY_DELAY
            } else {
                due_in(&runtime, interval).await.max(BOOT_DELAY)
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => break,
            }

            match run_once(&state.db, &db_path, &dir, cfg.keep).await {
                Ok(copy) => {
                    failed = false;
                    let now = chrono::Utc::now().timestamp();
                    if let Err(e) = runtime.set(LAST_KEY, &now.to_string()).await {
                        warn!("backup: could not record the time of the copy: {e:?}");
                    }
                    info!(
                        "backup written: {} ({} MiB)",
                        copy.path.display(),
                        copy.bytes >> 20
                    );
                    record(
                        &state,
                        "backup_created",
                        "info",
                        format!("Database copied to {}", copy.path.display()),
                    );
                }
                Err(e) => {
                    failed = true;
                    warn!("backup failed: {e:#}");
                    record(
                        &state,
                        "backup_failed",
                        "warn",
                        format!("Database backup failed: {e:#}"),
                    );
                }
            }
        }
    });
}

/// Time left until the next copy is due; zero when overdue or never taken.
async fn due_in(runtime: &RuntimeStateRepository, interval: Duration) -> Duration {
    let last = runtime
        .get(LAST_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    let Some(last) = last else {
        return Duration::ZERO;
    };
    let elapsed = (chrono::Utc::now().timestamp() - last).max(0) as u64;
    interval.saturating_sub(Duration::from_secs(elapsed))
}

fn record(state: &Arc<AppState>, kind: &'static str, severity: &'static str, message: String) {
    crate::services::events::record(
        state,
        NewHostEvent {
            source: "system",
            kind,
            severity,
            message,
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remon-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn a_copy_is_a_working_database_and_old_ones_go() {
        let work = temp_dir("copy");
        std::fs::create_dir_all(&work).unwrap();
        let db_path = work.join("live.sqlite3");
        let db = crate::storage::Database::connect(&format!("sqlite:{}", db_path.display()), 1)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('kept')")
            .execute(db.pool())
            .await
            .unwrap();

        let dir = work.join("backups");
        // Two older copies and a leftover from an interrupted run.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("remon-20200101-000000.sqlite3"), b"").unwrap();
        std::fs::write(dir.join("remon-20200102-000000.sqlite3"), b"").unwrap();
        std::fs::write(dir.join("remon-20200103-000000.sqlite3.partial"), b"").unwrap();

        let copy = run_once(db.pool(), &db_path, &dir, 2).await.unwrap();

        let names: Vec<_> = list(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "remon-20200102-000000.sqlite3");
        assert_eq!(dir.join(&names[1]), copy.path);
        assert!(!dir.join("remon-20200103-000000.sqlite3.partial").exists());

        let restored =
            crate::storage::Database::connect(&format!("sqlite:{}", copy.path.display()), 1)
                .await
                .unwrap();
        let v: String = sqlx::query_scalar("SELECT v FROM t")
            .fetch_one(restored.pool())
            .await
            .unwrap();
        assert_eq!(v, "kept");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copy.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        drop(restored);
        drop(db);
        let _ = std::fs::remove_dir_all(&work);
    }
}
