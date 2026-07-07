//! Per-probe scheduler. One tokio task per enabled probe; each task
//! sleeps until the next fire (cron or interval), runs the probe, and
//! persists the run-meta plus any emitted metrics.
//!
//! Severity / notification is deliberately NOT this module's job —
//! `services/alerts.rs` evaluates `alert_rules` of `metric_type='probe'`
//! against the values we land in `metrics_probe`, with cooldown and
//! FCM fan-out. One alert path for everything.
//!
//! `load_and_spawn` (called once at boot and again on hot-reload) is the
//! only public entry point. It diffs the on-disk manifest set against
//! the current registry, aborts tasks for removed/changed probes, and
//! starts new tasks for added/changed ones.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::Utc;
use log::{debug, info, warn};
use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::models::probe::{ProbeMetric, ProbeRun};
use crate::storage::repositories::{ProbeDefinitionRow, ProbeRepository};

use super::manifest::{Manifest, ManifestError, ProbeMode, Schedule};
use super::registry::{ProbeEntry, ProbeRegistry};
use super::runner;

/// Bound concurrent oneshot probe executions. Each probe runs as its own
/// child process, so an unbounded fan-out (e.g. ten probes all firing at
/// the same cron minute on a small VPS) can spike CPU and memory hard.
/// 5 is a comfortable default — enough to avoid serialising healthy
/// probes back-to-back, low enough to never exhaust the host. Stream
/// probes are deliberately exempt: they're long-running and would just
/// hold permits forever.
const PROBE_PERMITS: usize = 5;
static PROBE_GATE: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(PROBE_PERMITS)));

/// Serializes load passes. `load_and_spawn` is read-diff-write over the
/// registry with no lock held across the pass; two concurrent reloads
/// interleave so both see a probe as "already running", both spawn, and
/// the second `insert` drops the first task's JoinHandle detached — a
/// zombie loop that keeps firing child processes forever.
static LOAD_PASS: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Grace window for an aborted probe task to drop its DB connection and
/// file handles before the replacement spawns. Windows file locks are
/// mandatory — parallel runs can hit "process cannot access file".
const ABORTED_TASK_GRACE: Duration = Duration::from_secs(2);

/// Compute the next fire time for a schedule from "now". Always returns
/// `Some` for `Interval` (mathematically determinate) and may return
/// `None` for a `Cron` whose next match doesn't exist within ~10y from
/// now (degenerate, but possible — `0 0 30 2 *` for example).
fn next_fire(schedule: &Schedule, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    match schedule {
        Schedule::Interval(d) => Some(now + chrono::Duration::from_std(*d).ok()?),
        Schedule::Cron(c) => c.upcoming(Utc).next(),
    }
}

/// Persist run-meta + metrics + mirror into registry. Shared
/// post-execution path for both oneshot and stream modes — keeps the
/// "what to do with one result" rule in a single place.
async fn persist_and_mirror(
    repo: &ProbeRepository,
    registry: &ProbeRegistry,
    probe_name: &str,
    run: ProbeRun,
    metrics: Vec<ProbeMetric>,
) {
    if let Err(e) = repo.insert_run(&run).await {
        warn!("probe '{}' run-meta insert failed: {:?}", probe_name, e);
    }
    if !metrics.is_empty()
        && let Err(e) = repo
            .insert_metrics(probe_name, run.timestamp, &metrics)
            .await
    {
        warn!("probe '{}' metrics insert failed: {:?}", probe_name, e);
    }
    let mut reg = registry.write().await;
    if let Some(entry) = reg.probes.get_mut(probe_name) {
        entry.last_run = Some(run);
        entry.last_metrics = metrics;
    }
}

/// Sleep until the next scheduled fire. Returns `false` if the schedule
/// has no upcoming fire time (degenerate cron expression — caller
/// should exit the loop).
async fn sleep_until_next_fire(probe_name: &str, schedule: &Schedule) -> bool {
    let now = Utc::now();
    let next = match next_fire(schedule, now) {
        Some(t) => t,
        None => {
            warn!(
                "probe '{}': schedule has no upcoming fire; loop exiting",
                probe_name
            );
            return false;
        }
    };
    let sleep_for = (next - now).to_std().unwrap_or(Duration::from_secs(0));
    debug!(
        "probe '{}' next fire at {} (in {:?})",
        probe_name, next, sleep_for
    );
    tokio::time::sleep(sleep_for).await;
    true
}

/// Drive a oneshot probe forever. Sleep → execute → persist → repeat.
async fn run_oneshot_loop(manifest: Manifest, registry: ProbeRegistry, db: SqlitePool) {
    let repo = ProbeRepository::new(db);
    let probe_name = manifest.name.clone();

    info!(
        "probe '{}' (oneshot) scheduler started (schedule={}, timeout={}ms)",
        probe_name,
        manifest.schedule.as_db_string(),
        manifest.timeout.as_millis()
    );

    loop {
        if !sleep_until_next_fire(&probe_name, &manifest.schedule).await {
            return;
        }
        // Acquire a permit before spawning the child. If all permits are
        // held this loop just waits — the probe will fire late rather
        // than piling up new instances behind a stuck one. acquire_owned
        // returns Err only if the semaphore is closed (we never close).
        let _permit = match PROBE_GATE.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let (run, metrics) = runner::execute(&manifest).await;
        persist_and_mirror(&repo, &registry, &probe_name, run, metrics).await;
    }
}

/// Drive a stream-mode probe forever. Spawn → consume lines per
/// `runner::execute_stream` until child exits or runner gives up →
/// wait next-fire as restart cadence → respawn.
///
/// One key difference from oneshot: each line yields its own persist
/// step, so a steadily-emitting daemon can produce hundreds of
/// `probe_runs` rows per minute. Operators should pick a sensible
/// emission rate inside the script — there's no scheduler-side throttle.
async fn run_stream_loop(manifest: Manifest, registry: ProbeRegistry, db: SqlitePool) {
    let repo = ProbeRepository::new(db);
    let probe_name = manifest.name.clone();

    info!(
        "probe '{}' (stream) scheduler started (idle_timeout={}ms, restart cadence={})",
        probe_name,
        manifest.timeout.as_millis(),
        manifest.schedule.as_db_string()
    );

    loop {
        // First-fire respect. On a fresh task we want the schedule's
        // next match too — guards "every probe restarts at boot".
        if !sleep_until_next_fire(&probe_name, &manifest.schedule).await {
            return;
        }

        // Modest channel — bursty scripts can hit 10s of lines/sec; a
        // 64-deep buffer absorbs that without backpressure pinning the
        // child. Larger backpressure indicates persist is slow, in which
        // case we *want* to slow the script's stdout writes.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(ProbeRun, Vec<ProbeMetric>)>(64);

        let manifest_for_runner = manifest.clone();
        let runner_handle = tokio::spawn(async move {
            runner::execute_stream(&manifest_for_runner, tx).await;
        });

        // Drain results until the channel closes (runner returned).
        while let Some((run, metrics)) = rx.recv().await {
            persist_and_mirror(&repo, &registry, &probe_name, run, metrics).await;
        }
        // Ensure the runner task is fully done before looping back into
        // the schedule wait.
        let _ = runner_handle.await;
    }
}

/// Dispatch on probe mode. Single entry point so the registry's task
/// handle joins on whichever loop the manifest selected.
async fn run_probe_loop(manifest: Manifest, registry: ProbeRegistry, db: SqlitePool) {
    match manifest.mode {
        ProbeMode::Oneshot => run_oneshot_loop(manifest, registry, db).await,
        ProbeMode::Stream => run_stream_loop(manifest, registry, db).await,
    }
}

/// Result of one load pass — useful in REST responses for the reload
/// endpoint so operators see what actually picked up.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub loaded: Vec<String>,
    pub skipped_disabled: Vec<String>,
    pub skipped_platform: Vec<String>,
    pub failed: Vec<(PathBuf, String)>,
}

/// Load every YAML in `dir`, diff against current registry, sync.
///
/// Behaviour:
/// - Manifests that fail to parse / validate are recorded in `failed`
///   and the rest of the pass continues.
/// - A loaded manifest with `enabled: false` is upserted to the DB
///   (so the row exists for history) but no scheduler task is spawned.
/// - A manifest whose `platforms` list excludes the current host is
///   skipped entirely (no DB row, no task) — so a fail2ban probe doesn't
///   waste a slot on a Windows server.
/// - For probes already running, if the manifest hash matches the DB
///   row we leave the task alone. Otherwise we abort the old task and
///   spawn a fresh one with the new manifest.
pub async fn load_and_spawn(
    dir: &std::path::Path,
    registry: ProbeRegistry,
    db: SqlitePool,
) -> LoadReport {
    // One pass at a time — see LOAD_PASS.
    let _pass = LOAD_PASS.lock().await;
    let mut report = LoadReport::default();
    let repo = ProbeRepository::new(db.clone());

    let raw_results = match Manifest::load_dir(dir).await {
        Ok(rs) => rs,
        Err(e) => {
            warn!("probe loader could not read directory {:?}: {:?}", dir, e);
            return report;
        }
    };

    let mut loaded_manifests: Vec<Manifest> = Vec::new();
    for r in raw_results {
        match r {
            Ok(m) => loaded_manifests.push(m),
            Err((path, ManifestError::Validation(msg)))
            | Err((path, ManifestError::Parse(msg))) => {
                warn!("probe manifest {:?} rejected: {}", path, msg);
                report.failed.push((path, msg));
            }
            Err((path, ManifestError::Io(e))) => {
                warn!("probe manifest {:?} read failed: {}", path, e);
                report.failed.push((path, e.to_string()));
            }
        }
    }

    // Disable probes whose manifest disappeared from disk (or never matched
    // the platform). DB row stays so history queries keep working.
    let kept_names: Vec<String> = loaded_manifests
        .iter()
        .filter(|m| m.matches_current_platform())
        .map(|m| m.name.clone())
        .collect();
    if let Err(e) = repo.disable_missing(&kept_names).await {
        warn!("probe loader: disable_missing failed: {:?}", e);
    }

    // Snapshot existing DB rows to compare manifest hashes.
    let existing_rows: std::collections::HashMap<String, ProbeDefinitionRow> = repo
        .list_all()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();

    for manifest in loaded_manifests {
        if !manifest.matches_current_platform() {
            report.skipped_platform.push(manifest.name);
            continue;
        }

        // Upsert the DB shadow.
        let def = ProbeDefinitionRow {
            name: manifest.name.clone(),
            enabled: manifest.enabled,
            schedule: manifest.schedule.as_db_string(),
            timeout_ms: manifest.timeout.as_millis() as i64,
            manifest_hash: manifest.manifest_hash.clone(),
        };
        if let Err(e) = repo.upsert(&def).await {
            warn!("probe '{}' DB upsert failed: {:?}", manifest.name, e);
            report
                .failed
                .push((manifest.source_path.clone(), e.to_string()));
            continue;
        }

        if !manifest.enabled {
            report.skipped_disabled.push(manifest.name);
            continue;
        }

        // Diff against running registry — keep tasks whose manifest hash
        // is unchanged, restart the rest.
        let prior_hash = existing_rows
            .get(&manifest.name)
            .map(|r| r.manifest_hash.as_str());
        let already_running = {
            let reg = registry.read().await;
            reg.probes
                .get(&manifest.name)
                .and_then(|e| e.task.as_ref())
                .map(|h| !h.is_finished())
                .unwrap_or(false)
        };
        let unchanged = prior_hash == Some(manifest.manifest_hash.as_str());
        if already_running && unchanged {
            // Just refresh the manifest fields (description etc may be
            // identical-by-hash but we copy anyway for cheapness).
            let mut reg = registry.write().await;
            if let Some(entry) = reg.probes.get_mut(&manifest.name) {
                entry.manifest = manifest.clone();
            }
            report.loaded.push(manifest.name);
            continue;
        }

        // Otherwise: abort old task (if any) and spawn fresh. The wait is
        // done outside the registry lock so readers aren't blocked for up
        // to ABORTED_TASK_GRACE.
        let aborted_task = {
            let mut reg = registry.write().await;
            reg.probes.remove(&manifest.name).and_then(|e| e.task)
        };
        if let Some(task) = aborted_task {
            task.abort();
            let _ = tokio::time::timeout(ABORTED_TASK_GRACE, task).await;
        }

        let manifest_for_task = manifest.clone();
        let registry_for_task = Arc::clone(&registry);
        let db_for_task = db.clone();
        let task = tokio::spawn(async move {
            run_probe_loop(manifest_for_task, registry_for_task, db_for_task).await;
        });

        let mut reg = registry.write().await;
        reg.probes.insert(
            manifest.name.clone(),
            ProbeEntry {
                manifest: manifest.clone(),
                last_run: None,
                last_metrics: Vec::new(),
                task: Some(task),
            },
        );
        report.loaded.push(manifest.name);
    }

    // Reconcile: stop tasks for probes that no longer have a live definition
    // this pass — removed from disk, newly disabled, now failing to parse, or
    // platform-excluded. `report.loaded` is exactly the set that should keep a
    // running task; anything else still in the registry is a stale task
    // executing an old manifest (and spawning child processes on schedule)
    // that the loop above never reached.
    {
        let live: std::collections::HashSet<&str> =
            report.loaded.iter().map(String::as_str).collect();
        let stale: Vec<(String, Option<tokio::task::JoinHandle<()>>)> = {
            let mut reg = registry.write().await;
            let names: Vec<String> = reg
                .probes
                .keys()
                .filter(|n| !live.contains(n.as_str()))
                .cloned()
                .collect();
            names
                .into_iter()
                .map(|n| {
                    let task = reg.probes.remove(&n).and_then(|e| e.task);
                    (n, task)
                })
                .collect()
        };
        for (name, task) in stale {
            if let Some(task) = task {
                info!("probe loader: '{}' no longer active, stopping task", name);
                task.abort();
                let _ = tokio::time::timeout(ABORTED_TASK_GRACE, task).await;
            }
        }
    }

    info!(
        "probe loader: {} loaded, {} disabled, {} skipped(platform), {} failed",
        report.loaded.len(),
        report.skipped_disabled.len(),
        report.skipped_platform.len(),
        report.failed.len()
    );
    report
}
