//! Host-event ledger — writers and detectors for `host_events`.
//!
//! Three producers feed the ledger:
//! - **Operator audit** (`record_operator`): mutating REST handlers report
//!   what an authenticated device just did. The insert is spawned so the
//!   request path never blocks on it, and the actor's display name is
//!   resolved inside the task.
//! - **Boot detection** (`detect_boot_on_startup` + `mark_clean_shutdown`):
//!   compares the host's boot time against the value persisted in
//!   `runtime_state`. A changed boot time is a reboot; a clean-shutdown
//!   marker that was never written distinguishes powercycle/crash from an
//!   orderly restart.
//! - **OOM sweep** (`spawn_oom_sweep`, Linux): a low-frequency journal scan
//!   turning kernel "Killed process" lines into structured events — the
//!   only real answer to "why did my process vanish".
//!
//! SMART health transitions are detected by `collectors::smart` and land
//! here via [`record`]. Alert fire/resolve and incident captures keep their
//! own tables; `GET /events` unions all of them.

use std::sync::Arc;

use log::{info, warn};
use serde_json::json;

use crate::state::AppState;
use crate::storage::repositories::{
    DeviceRepository, HostEventRepository, NewHostEvent, RuntimeStateRepository,
};

const KEY_BOOT_TS: &str = "host_boot_ts";
const KEY_CLEAN_SHUTDOWN: &str = "clean_shutdown";
#[cfg(target_os = "linux")]
const KEY_OOM_CURSOR: &str = "oom_sweep_cursor";

/// Uptime-derived boot timestamps wobble by a few seconds between reads;
/// only a difference beyond this is a real reboot.
const BOOT_JITTER_SECS: i64 = 120;

/// Fire-and-forget ledger write. Failures are logged, never surfaced —
/// an audit row must not fail the action it records.
pub fn record(state: &Arc<AppState>, event: NewHostEvent) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        if let Err(e) = HostEventRepository::new(state.db.clone())
            .insert(&event)
            .await
        {
            warn!("host event insert failed (kind={}): {e}", event.kind);
        }
    });
}

/// Audit entry for an operator action, attributed to the calling device.
/// Name resolution happens inside the spawned task — one indexed lookup,
/// off the request path.
pub fn record_operator(
    state: &Arc<AppState>,
    device_id: &str,
    kind: &'static str,
    message: String,
    ref_type: Option<&'static str>,
    ref_id: Option<String>,
    details: Option<serde_json::Value>,
) {
    let state = Arc::clone(state);
    let device_id = device_id.to_string();
    tokio::spawn(async move {
        let actor_name = DeviceRepository::new(state.db.clone())
            .get_by_id(&device_id)
            .await
            .ok()
            .flatten()
            .map(|d| d.name);
        let event = NewHostEvent {
            created_at: None,
            source: "operator",
            kind,
            severity: "info",
            message,
            actor_device_id: Some(device_id),
            actor_name,
            ref_type,
            ref_id,
            details: details.map(|d| d.to_string()),
        };
        if let Err(e) = HostEventRepository::new(state.db.clone())
            .insert(&event)
            .await
        {
            warn!("host event insert failed (kind={}): {e}", event.kind);
        }
    });
}

// ===== Boot / reboot detection =====

/// Startup hook: detect a host reboot (or an unclean daemon exit) since the
/// previous run, record it, then re-arm the markers for this run.
pub async fn detect_boot_on_startup(state: &Arc<AppState>) {
    let now = chrono::Utc::now().timestamp();
    let boot_ts = now - sysinfo::System::uptime() as i64;

    let rs = RuntimeStateRepository::new(state.db.clone());
    let prev_boot = match rs.get(KEY_BOOT_TS).await {
        Ok(v) => v.and_then(|s| s.parse::<i64>().ok()),
        Err(e) => {
            warn!("boot detection: runtime_state read failed: {e}");
            return;
        }
    };
    let clean_shutdown = rs
        .get(KEY_CLEAN_SHUTDOWN)
        .await
        .ok()
        .flatten()
        .map(|v| v == "1");

    if let Some(event) = classify_startup(prev_boot, boot_ts, clean_shutdown) {
        info!("startup event: {}", event.message);
        if let Err(e) = HostEventRepository::new(state.db.clone())
            .insert(&event)
            .await
        {
            warn!("boot event insert failed: {e}");
        }
    }

    let _ = rs.set(KEY_BOOT_TS, &boot_ts.to_string()).await;
    // Re-armed to "unclean" for this run; `mark_clean_shutdown` flips it
    // on graceful exit. Absent flip + same boot ts next start = crash.
    let _ = rs.set(KEY_CLEAN_SHUTDOWN, "0").await;
}

/// Graceful-exit hook — call after the server has drained.
pub async fn mark_clean_shutdown(state: &Arc<AppState>) {
    if let Err(e) = RuntimeStateRepository::new(state.db.clone())
        .set(KEY_CLEAN_SHUTDOWN, "1")
        .await
    {
        warn!("clean-shutdown marker write failed: {e}");
    }
}

/// Pure startup classification: previous boot ts + this boot ts + whether
/// the previous run exited cleanly → the event to record, if any.
///
/// - First run ever: nothing to compare, no event.
/// - Boot ts moved: the host rebooted. Clean prior shutdown → `info` (an
///   orderly reboot); otherwise `warn` — power loss, crash, or hard reset.
/// - Boot ts unchanged but the clean marker is missing: the daemon itself
///   died uncleanly (OOM-killed, `kill -9`, panic) and is back.
fn classify_startup(
    prev_boot: Option<i64>,
    boot_ts: i64,
    clean_shutdown: Option<bool>,
) -> Option<NewHostEvent> {
    let prev = prev_boot?;
    let clean = clean_shutdown == Some(true);

    if (boot_ts - prev).abs() > BOOT_JITTER_SECS {
        let (severity, message) = if clean {
            ("info", "Host booted".to_string())
        } else {
            (
                "warn",
                "Host booted after unclean shutdown (power loss, crash, or hard reset)".to_string(),
            )
        };
        return Some(NewHostEvent {
            // Stamped with the actual boot moment so chart annotations line
            // up with the gap in the metric series, not with daemon start.
            created_at: Some(boot_ts),
            source: "system",
            kind: "boot",
            severity,
            message,
            details: Some(json!({ "previous_boot_ts": prev, "clean_shutdown": clean }).to_string()),
            ..Default::default()
        });
    }

    if !clean {
        return Some(NewHostEvent {
            created_at: None,
            source: "system",
            kind: "agent_restart",
            severity: "warn",
            message: "remon-server restarted after unclean exit (crash or kill)".to_string(),
            ..Default::default()
        });
    }
    None
}

// ===== OOM sweep (Linux) =====

/// Spawn the periodic kernel-journal sweep for OOM kills. No-op off Linux —
/// the OOM killer is a Linux concept and the journal is the only reliable
/// witness. Windows has no equivalent event; macOS jetsam is out of scope.
pub fn spawn_oom_sweep(state: Arc<AppState>) {
    #[cfg(target_os = "linux")]
    tokio::spawn(oom_sweep_loop(state));
    #[cfg(not(target_os = "linux"))]
    let _ = state;
}

#[cfg(target_os = "linux")]
const OOM_SWEEP_INTERVAL_SECS: u64 = 300;

/// First-run lookback: long enough to catch the OOM that likely preceded
/// an unclean restart, short enough to not replay ancient history.
#[cfg(target_os = "linux")]
const OOM_FIRST_LOOKBACK_SECS: i64 = 900;

#[cfg(target_os = "linux")]
async fn oom_sweep_loop(state: Arc<AppState>) {
    use std::time::Duration;

    let rs = RuntimeStateRepository::new(state.db.clone());
    let mut cursor = match rs.get(KEY_OOM_CURSOR).await {
        Ok(v) => v
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| chrono::Utc::now().timestamp() - OOM_FIRST_LOOKBACK_SECS),
        Err(e) => {
            warn!("OOM sweep: cursor read failed, disabling: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(OOM_SWEEP_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // interval fires immediately; that first sweep doubles as the
    // journalctl availability probe.
    let mut first = true;

    loop {
        ticker.tick().await;
        let now = chrono::Utc::now().timestamp();
        let lines = match read_kernel_kill_lines(cursor).await {
            Ok(l) => l,
            Err(e) => {
                if first {
                    info!("OOM sweep: journalctl unavailable, disabling ({e})");
                    return;
                }
                warn!("OOM sweep failed: {e}");
                continue;
            }
        };
        first = false;

        let repo = HostEventRepository::new(state.db.clone());
        for line in &lines {
            let Some((ts, pid, name)) = parse_oom_line(line) else {
                continue;
            };
            if ts <= cursor {
                continue;
            }
            let event = NewHostEvent {
                created_at: Some(ts),
                source: "system",
                kind: "oom_kill",
                severity: "error",
                message: format!("Kernel OOM killer terminated '{name}' (pid {pid})"),
                ref_type: Some("process"),
                ref_id: Some(name),
                details: Some(
                    json!({ "pid": pid, "line": line.chars().take(300).collect::<String>() })
                        .to_string(),
                ),
                ..Default::default()
            };
            if let Err(e) = repo.insert(&event).await {
                warn!("OOM event insert failed: {e}");
            }
        }
        cursor = now;
        let _ = rs.set(KEY_OOM_CURSOR, &cursor.to_string()).await;
    }
}

/// One bounded journal read: kernel messages containing "Killed process"
/// since the cursor — matches both global ("Out of memory: Killed process")
/// and cgroup ("Memory cgroup out of memory: Killed process") kills.
#[cfg(target_os = "linux")]
async fn read_kernel_kill_lines(since_ts: i64) -> Result<Vec<String>, String> {
    let since = format!("@{}", since_ts);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("journalctl")
            .args([
                "-k",
                "--no-pager",
                "-o",
                "short-unix",
                "-g",
                "Killed process",
                "--since",
                &since,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| "journalctl timed out after 10s".to_string())?
    .map_err(|e| format!("journalctl spawn failed: {e}"))?;

    // -g with no matches exits 1 with empty output — that's "nothing new",
    // not an error.
    if !output.status.success() && !output.stdout.is_empty() {
        return Err(format!("journalctl exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("--"))
        .take(50)
        .map(str::to_string)
        .collect())
}

/// Parse a `short-unix` journal line like
/// `1721375123.456789 host kernel: Out of memory: Killed process 1234 (chrome) total-vm:…`
/// → (unix ts, pid, process name).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_oom_line(line: &str) -> Option<(i64, u32, String)> {
    let ts = line.split_whitespace().next()?.parse::<f64>().ok()? as i64;
    let rest = &line[line.find("Killed process ")? + "Killed process ".len()..];
    let pid_len = rest.find(|c: char| !c.is_ascii_digit())?;
    let pid: u32 = rest[..pid_len].parse().ok()?;
    let after_pid = rest[pid_len..].trim_start().strip_prefix('(')?;
    let name = &after_pid[..after_pid.find(')')?];
    if name.is_empty() {
        return None;
    }
    Some((ts, pid, name.to_string()))
}

// ===== SMART transition classification =====

/// Verdict-pair → event shape for a disk whose SMART health changed.
/// `None` = nothing worth recording. Used by `collectors::smart`.
pub fn smart_transition(
    prev: Option<bool>,
    new: Option<bool>,
) -> Option<(&'static str, &'static str)> {
    match (prev, new) {
        // Healthy (or never seen) → failing: the event that matters.
        (Some(true) | None, Some(false)) => Some(("error", "SMART health check failed")),
        // Failing → healthy again (attribute cleared, disk replaced under
        // the same device node, …).
        (Some(false), Some(true)) => Some(("info", "SMART health recovered")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_records_nothing() {
        assert!(classify_startup(None, 1_000_000, None).is_none());
        assert!(classify_startup(None, 1_000_000, Some(true)).is_none());
    }

    #[test]
    fn clean_reboot_is_info_at_boot_time() {
        let ev = classify_startup(Some(1_000_000), 1_005_000, Some(true)).expect("event");
        assert_eq!(ev.kind, "boot");
        assert_eq!(ev.severity, "info");
        assert_eq!(ev.created_at, Some(1_005_000));
    }

    #[test]
    fn unclean_reboot_is_warn() {
        for marker in [Some(false), None] {
            let ev = classify_startup(Some(1_000_000), 1_005_000, marker).expect("event");
            assert_eq!(ev.kind, "boot");
            assert_eq!(ev.severity, "warn");
        }
    }

    #[test]
    fn boot_jitter_is_not_a_reboot() {
        assert!(classify_startup(Some(1_000_000), 1_000_000 + 60, Some(true)).is_none());
        assert!(classify_startup(Some(1_000_000), 1_000_000 - 60, Some(true)).is_none());
    }

    #[test]
    fn unclean_daemon_exit_without_reboot_is_agent_restart() {
        let ev = classify_startup(Some(1_000_000), 1_000_010, Some(false)).expect("event");
        assert_eq!(ev.kind, "agent_restart");
        assert_eq!(ev.severity, "warn");
    }

    #[test]
    fn parses_global_oom_line() {
        let line = "1721375123.456789 myhost kernel: Out of memory: Killed process 1234 \
                    (chrome) total-vm:1000kB, anon-rss:100kB";
        let (ts, pid, name) = parse_oom_line(line).expect("parse");
        assert_eq!(ts, 1721375123);
        assert_eq!(pid, 1234);
        assert_eq!(name, "chrome");
    }

    #[test]
    fn parses_cgroup_oom_line() {
        let line = "1721375123.000000 myhost kernel: Memory cgroup out of memory: \
                    Killed process 567 (postgres) total-vm:2048kB";
        let (_, pid, name) = parse_oom_line(line).expect("parse");
        assert_eq!(pid, 567);
        assert_eq!(name, "postgres");
    }

    #[test]
    fn rejects_non_oom_lines() {
        assert!(parse_oom_line("garbage").is_none());
        assert!(parse_oom_line("1721375123.0 host kernel: something else entirely").is_none());
    }

    #[test]
    fn smart_transitions_cover_the_matrix() {
        assert_eq!(
            smart_transition(Some(true), Some(false)),
            Some(("error", "SMART health check failed"))
        );
        assert_eq!(
            smart_transition(None, Some(false)),
            Some(("error", "SMART health check failed"))
        );
        assert_eq!(
            smart_transition(Some(false), Some(true)),
            Some(("info", "SMART health recovered"))
        );
        assert_eq!(smart_transition(Some(true), Some(true)), None);
        assert_eq!(smart_transition(Some(false), Some(false)), None);
        assert_eq!(smart_transition(None, None), None);
        assert_eq!(smart_transition(Some(true), None), None);
    }
}
