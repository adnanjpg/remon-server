//! Host-event ledger — writers and detectors for `host_events`.
//!
//! Three producers feed the ledger:
//! - **Operator audit** (`record_operator`): mutating REST handlers report
//!   what an authenticated device just did. The insert is spawned so the
//!   request path never blocks on it, and the actor's display name is
//!   resolved inside the task.
//! - **Boot detection** (`detect_boot_on_startup` + `mark_clean_shutdown`):
//!   compares the host's boot time against the value persisted in
//!   `runtime_state`. Every start records `server_started`; a changed boot
//!   time additionally records `boot` (a reboot), and a clean-shutdown
//!   marker that was never written distinguishes powercycle/crash from an
//!   orderly restart.
//! - **System-event sweep** (`spawn_system_event_sweep`): a low-frequency log
//!   scan turning OOM kills, process crashes (`app_crash`), and disk /
//!   filesystem errors (`disk_error`) into structured events — the only real
//!   answer to "why did my process vanish". Linux reads the kernel journal;
//!   Windows reads the System + Application event logs.
//!
//! SMART health transitions are detected by `collectors::smart` and land
//! here via [`record`]. Alert fire/resolve and incident captures keep their
//! own tables; `GET /events` unions all of them.
//!
//! **Notifications.** A curated set of system-source kinds (unclean `boot`,
//! `oom_kill`, `smart_health` failure, `disk_error` — see [`notify_severity`])
//! pages the configured channels the moment it lands, no alert rule required.
//! Routine lifecycle, operator/agent audit, and noisy `app_crash` stay quiet.
//! This is the "set and forget" half: the ledger records everything; only the
//! alarming kinds push.

use std::sync::Arc;

use log::{debug, info, warn};
use serde_json::json;

use crate::notify::{Notification, NotificationEvent, Severity};
use crate::state::AppState;
use crate::storage::repositories::{
    DeviceRepository, HostEventRepository, NewHostEvent, RuntimeStateRepository,
};

const KEY_BOOT_TS: &str = "host_boot_ts";
const KEY_CLEAN_SHUTDOWN: &str = "clean_shutdown";
#[cfg(any(target_os = "linux", windows))]
const KEY_SYSEVENT_CURSOR: &str = "sysevent_sweep_cursor";

/// Uptime-derived boot timestamps wobble by a few seconds between reads;
/// only a difference beyond this is a real reboot.
const BOOT_JITTER_SECS: i64 = 120;

/// Fire-and-forget ledger write. Failures are logged, never surfaced —
/// an audit row must not fail the action it records. Notification-worthy
/// system events (see [`notify_severity`]) also page the configured channels.
pub fn record(state: &Arc<AppState>, event: NewHostEvent) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        insert_and_maybe_notify(&state, &event).await;
    });
}

/// Insert a host event, then page an operator if it's notification-worthy.
/// The one place the notify policy lives, so every producer (boot detection,
/// the system-event sweep, SMART, operator audit) shares it. Insert failure is
/// logged and skips the notify.
async fn insert_and_maybe_notify(state: &Arc<AppState>, event: &NewHostEvent) {
    if let Err(e) = HostEventRepository::new(state.db.clone())
        .insert(event)
        .await
    {
        warn!("host event insert failed (kind={}): {e}", event.kind);
        return;
    }
    let Some(sev) = notify_severity(event.kind, event.severity) else {
        return;
    };
    let server_name = state.effective_config.read().await.server_name.clone();
    let n = Notification {
        title: format!("[{}] {}", server_name, host_event_subject(event.kind)),
        body: event.message.clone(),
        severity: sev,
        event: NotificationEvent::HostEvent,
    };
    // Queued, not awaited: this runs inside the system-event sweep loop, and a
    // relay taking its full budget would hold up both the remaining events of
    // this tick and the cursor write that follows them.
    state.notify_queue.dispatch(n, None);
}

/// Which host-event kinds page an operator by default, and at what severity.
/// A curated set — routine lifecycle (`server_started`, a clean `boot`) and
/// operator/agent audit stay quiet; only the "something's wrong on the box"
/// kinds notify. Severity distinguishes failure from recovery (a SMART
/// recovery is `info` and doesn't page; a failure is `error` and does; a
/// clean reboot is `info`, an unclean one `warn`). Channel `min_severity`
/// gives the operator the final say per channel.
fn notify_severity(kind: &str, severity: &str) -> Option<Severity> {
    // `app_crash` is intentionally absent — process crashes can be routine and
    // noisy; they land in the ledger but don't page.
    if !matches!(kind, "boot" | "oom_kill" | "smart_health" | "disk_error") {
        return None;
    }
    match severity {
        "error" => Some(Severity::Crit),
        "warn" => Some(Severity::Warn),
        _ => None,
    }
}

/// Short notification subject per kind — the headline; the event's own message
/// carries the specifics in the body.
fn host_event_subject(kind: &str) -> &'static str {
    match kind {
        "boot" => "Host restarted uncleanly",
        "oom_kill" => "Out-of-memory kill",
        "smart_health" => "Disk SMART health failed",
        "disk_error" => "Disk / filesystem error",
        _ => "Host event",
    }
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
        // Operator kinds are never notification-worthy, but routing through
        // the shared path keeps the insert/notify logic in one place.
        insert_and_maybe_notify(&state, &event).await;
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

    for event in classify_startup(prev_boot, boot_ts, clean_shutdown) {
        info!("startup event: {}", event.message);
        insert_and_maybe_notify(state, &event).await;
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
/// the previous run exited cleanly → the lifecycle events to record.
///
/// Always yields a `server_started` event (the daemon is up). A changed
/// boot ts additionally yields a `boot` event (the host rebooted), `warn`
/// when the prior shutdown was unclean. `server_started` is itself `warn`
/// only for a genuine daemon crash-restart (same boot, no clean marker).
fn classify_startup(
    prev_boot: Option<i64>,
    boot_ts: i64,
    clean_shutdown: Option<bool>,
) -> Vec<NewHostEvent> {
    let clean = clean_shutdown == Some(true);
    let host_rebooted = prev_boot.is_some_and(|prev| (boot_ts - prev).abs() > BOOT_JITTER_SECS);
    let mut events = Vec::new();

    // The host came up (or came back). Stamped with the actual boot moment
    // so chart annotations line up with the gap in the metric series, not
    // with daemon start.
    if host_rebooted {
        let prev = prev_boot.expect("host_rebooted implies a previous boot");
        let (severity, message) = if clean {
            ("info", "Host booted".to_string())
        } else {
            (
                "warn",
                "Host booted after unclean shutdown (power loss, crash, or hard reset)".to_string(),
            )
        };
        events.push(NewHostEvent {
            created_at: Some(boot_ts),
            source: "system",
            kind: "boot",
            severity,
            message,
            details: Some(json!({ "previous_boot_ts": prev, "clean_shutdown": clean }).to_string()),
            ..Default::default()
        });
    }

    // The daemon itself started — recorded every run, so a metric-series gap
    // from a plain restart (deploy, manual bounce) is explained too. `warn`
    // only for a genuine crash-restart: same host boot as last run but the
    // previous run never wrote its clean-shutdown marker. A missing marker
    // right after a host reboot is expected (the box went down under the
    // daemon) and already carried by the `boot` event, so it stays `info`.
    let crash_restart = prev_boot.is_some() && !host_rebooted && !clean;
    let (severity, message) = if crash_restart {
        (
            "warn",
            "remon-server started after an unclean exit (crash or kill)".to_string(),
        )
    } else {
        ("info", "remon-server started".to_string())
    };
    events.push(NewHostEvent {
        created_at: None,
        source: "system",
        kind: "server_started",
        severity,
        message,
        details: Some(
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "clean_previous_exit": clean,
                "host_rebooted": host_rebooted,
                "first_run": prev_boot.is_none(),
            })
            .to_string(),
        ),
        ..Default::default()
    });

    events
}

// ===== System-event sweep (Linux journald + Windows Event Log) =====

/// Spawn the periodic host system-event sweep: OOM kills, process crashes,
/// and disk / filesystem errors, turned into host events. Linux reads the
/// kernel journal (`journalctl -k`); Windows reads the System + Application
/// event logs (`Get-WinEvent`). No-op elsewhere — macOS `log show` parsing is
/// a future addition.
pub fn spawn_system_event_sweep(state: Arc<AppState>) {
    #[cfg(any(target_os = "linux", windows))]
    tokio::spawn(system_event_sweep_loop(state));
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = state;
}

#[cfg(any(target_os = "linux", windows))]
const SWEEP_INTERVAL_SECS: u64 = 300;

/// First-run lookback: long enough to catch the crash/OOM that likely
/// preceded an unclean restart, short enough not to replay ancient history.
/// That crash is by definition in the *previous* boot, which is why the Linux
/// scan must not restrict itself to the current one.
#[cfg(any(target_os = "linux", windows))]
const SWEEP_FIRST_LOOKBACK_SECS: i64 = 900;

/// Furthest back any single scan will reach, however stale the cursor is.
/// Without it, a host that was off for a month would ask the journal for a
/// month of history on its first tick and time out doing it.
#[cfg(any(target_os = "linux", windows))]
const SWEEP_MAX_LOOKBACK_SECS: i64 = 86_400;

/// How far behind `now` the cursor is left when a tick records nothing. The
/// scan and the log writer race: an entry stamped a moment before the scan ran
/// may not have been flushed in time to appear in it, and a cursor parked at
/// `now` would step straight over it.
#[cfg(any(target_os = "linux", windows))]
const SWEEP_FLUSH_GRACE_SECS: i64 = 60;

/// Most events recorded in one tick. A safety valve against a pathological
/// journal, not a budget — whatever is left stays ahead of the cursor and is
/// picked up on the next tick rather than dropped.
#[cfg(any(target_os = "linux", windows))]
const SWEEP_MAX_EVENTS_PER_TICK: usize = 500;

/// Why a scan produced nothing usable.
///
/// The distinction is the whole difference between "this host will never do
/// this" and "not this time": only the former is worth switching the sweep
/// off for, and conflating them is what silently disabled it before.
#[cfg(any(target_os = "linux", windows))]
enum ScanError {
    /// The tool this platform needs is not installed. Retrying cannot help.
    Unsupported(String),
    /// This attempt failed — a timeout, a busy journal, a transient read
    /// error. The next tick may well succeed.
    Transient(String),
}

/// One detected system event, platform-agnostic. The scan functions parse
/// their native log format into this; the loop turns it into a `NewHostEvent`.
#[allow(dead_code)] // fields unused on platforms without a scan (macOS)
struct SysEvent {
    ts: i64,
    kind: &'static str,
    severity: &'static str,
    message: String,
    ref_type: Option<&'static str>,
    ref_id: Option<String>,
}

#[cfg(any(target_os = "linux", windows))]
async fn system_event_sweep_loop(state: Arc<AppState>) {
    use std::time::Duration;

    let rs = RuntimeStateRepository::new(state.db.clone());
    let mut cursor = match rs.get(KEY_SYSEVENT_CURSOR).await {
        Ok(v) => v
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| chrono::Utc::now().timestamp() - SWEEP_FIRST_LOOKBACK_SECS),
        Err(e) => {
            warn!("system-event sweep: cursor read failed, disabling: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let now = chrono::Utc::now().timestamp();
        // However far behind the cursor is, never ask for more than a day.
        let since = cursor.max(now - SWEEP_MAX_LOOKBACK_SECS);

        let events = match scan_system_events(since).await {
            Ok(e) => e,
            // Only a missing tool ends the sweep. Anything else gets another
            // tick: the first one lands during boot, with the log daemon still
            // replaying, which is exactly when a transient failure is likely
            // and exactly when giving up costs the most.
            Err(ScanError::Unsupported(e)) => {
                info!("system-event sweep: unavailable on this host, disabling ({e})");
                return;
            }
            Err(ScanError::Transient(e)) => {
                warn!("system-event sweep failed, retrying next tick: {e}");
                continue;
            }
        };

        let plan = plan_sweep(events, cursor, now);
        if plan.truncated {
            warn!(
                "system-event sweep: more than {} events in one window; \
                 recording the oldest and resuming next tick",
                SWEEP_MAX_EVENTS_PER_TICK
            );
        }
        for ev in plan.record {
            let event = NewHostEvent {
                created_at: Some(ev.ts),
                source: "system",
                kind: ev.kind,
                severity: ev.severity,
                message: ev.message,
                ref_type: ev.ref_type,
                ref_id: ev.ref_id,
                ..Default::default()
            };
            insert_and_maybe_notify(&state, &event).await;
        }
        cursor = plan.next_cursor;
        let _ = rs.set(KEY_SYSEVENT_CURSOR, &cursor.to_string()).await;
    }
}

/// What a tick should record, and where the cursor lands afterwards.
#[cfg(any(target_os = "linux", windows))]
struct SweepPlan {
    record: Vec<SysEvent>,
    next_cursor: i64,
    /// More matched than one tick will take; the rest is still ahead of the
    /// cursor and will be picked up next time.
    truncated: bool,
}

/// Decide what to record and where the cursor lands.
///
/// Split out because both halves of this used to be wrong in ways nothing
/// surfaced: events past the per-tick cap were dropped, and the cursor jumped
/// to wall-clock `now` regardless of what was actually read — so anything the
/// log daemon flushed while the scan was running was stepped over. The cursor
/// is a high-water mark of what was *handled*, never a clock reading.
#[cfg(any(target_os = "linux", windows))]
fn plan_sweep(events: Vec<SysEvent>, cursor: i64, now: i64) -> SweepPlan {
    let mut record: Vec<SysEvent> = events.into_iter().filter(|e| e.ts > cursor).collect();
    record.sort_by_key(|e| e.ts);

    let truncated = record.len() > SWEEP_MAX_EVENTS_PER_TICK;
    record.truncate(SWEEP_MAX_EVENTS_PER_TICK);

    let next_cursor = match record.last() {
        Some(last) => last.ts,
        // Nothing recorded, so there is no high-water mark to move to. Advance
        // anyway or a quiet host rescans an ever-growing window, but stay a
        // grace period behind `now` so a late-flushed entry is not skipped.
        None => (now - SWEEP_FLUSH_GRACE_SECS).max(cursor),
    };

    SweepPlan {
        record,
        next_cursor,
        truncated,
    }
}

// ----- Linux: kernel journal -----

/// One bounded kernel-journal read since the cursor, classified into events.
#[cfg(target_os = "linux")]
async fn scan_system_events(since_ts: i64) -> Result<Vec<SysEvent>, ScanError> {
    // PCRE union of the message families we record. journald's -g uses PCRE2
    // (standard on modern systemd), so one read covers every detector.
    const PATTERN: &str = "Killed process|segfault|general protection|I/O error|EXT4-fs error|Buffer I/O error|critical (medium|target) error";
    let since = format!("@{}", since_ts);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("journalctl")
            .args([
                "--no-pager",
                "-o",
                "short-unix",
                "-g",
                PATTERN,
                "--since",
                &since,
                // The kernel filter written out rather than `-k`, which is
                // documented to imply `-b` — restricting every read to the
                // current boot. The startup lookback exists to find the OOM or
                // panic behind an unclean restart, and that record is in the
                // boot before this one, so `-k` made it unreachable by
                // construction.
                "_TRANSPORT=kernel",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Nothing owns this child once the timeout above fires.
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ScanError::Transient("journalctl timed out after 10s".to_string()))?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            ScanError::Unsupported("journalctl is not installed".to_string())
        }
        _ => ScanError::Transient(format!("journalctl spawn failed: {e}")),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // `-g` with no matches exits 1 but still prints a "-- No entries --"
    // banner to stdout, so the exit status alone says nothing. stderr is a
    // better signal but not a decisive one either: journald reports a damaged
    // or rotated file there ("journal file ... is truncated, ignoring file")
    // while still serving every intact entry on stdout. Treating that as fatal
    // blinded the sweep on exactly the hosts most likely to have something
    // worth reporting. So stderr only fails the scan when nothing came back
    // with it.
    if !stderr.trim().is_empty() {
        if stdout.trim().is_empty() {
            return Err(ScanError::Transient(format!(
                "journalctl exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        debug!(
            "journalctl warned while still returning entries: {}",
            stderr.trim()
        );
    }

    Ok(stdout
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("--"))
        .filter_map(classify_kernel_line)
        .collect())
}

/// Classify one `short-unix` kernel line into a host event. The grep upstream
/// guarantees every line matches one of the families below, so the trailing
/// arm (disk / filesystem errors) is a safe catch-all.
#[allow(dead_code)] // used only by the Linux scan
fn classify_kernel_line(line: &str) -> Option<SysEvent> {
    let ts = line.split_whitespace().next()?.parse::<f64>().ok()? as i64;

    if line.contains("Killed process") {
        let (_, pid, name) = parse_oom_line(line)?;
        return Some(SysEvent {
            ts,
            kind: "oom_kill",
            severity: "error",
            message: format!("Kernel OOM killer terminated '{name}' (pid {pid})"),
            ref_type: Some("process"),
            ref_id: Some(name),
        });
    }
    if line.contains("segfault") || line.contains("general protection") {
        let fault = if line.contains("segfault") {
            "segfaulted"
        } else {
            "hit a general-protection fault"
        };
        return Some(match parse_crash_line(line) {
            Some((name, pid)) => SysEvent {
                ts,
                kind: "app_crash",
                severity: "warn",
                message: format!("Process '{name}' (pid {pid}) {fault}"),
                ref_type: Some("process"),
                ref_id: Some(name),
            },
            None => SysEvent {
                ts,
                kind: "app_crash",
                severity: "warn",
                message: format!("A process {fault}: {}", kernel_message(line)),
                ref_type: None,
                ref_id: None,
            },
        });
    }
    // Everything else the grep let through is a storage error.
    Some(SysEvent {
        ts,
        kind: "disk_error",
        severity: "error",
        message: format!("Kernel storage error: {}", kernel_message(line)),
        ref_type: None,
        ref_id: None,
    })
}

/// Parse a `short-unix` OOM line like
/// `1721375123.456789 host kernel: Out of memory: Killed process 1234 (chrome) total-vm:…`
/// → (unix ts, pid, process name).
#[allow(dead_code)] // used only by the Linux scan
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

/// Parse the `<name>[<pid>]` token from a crash line like
/// `… kernel: chrome[1234]: segfault at 0 ip …` → ("chrome", 1234).
#[allow(dead_code)] // used only by the Linux scan
fn parse_crash_line(line: &str) -> Option<(String, u32)> {
    let open = line.find('[')?;
    let close = line[open..].find(']')? + open;
    let pid: u32 = line[open + 1..close].parse().ok()?;
    let name = line[..open]
        .rsplit(|c: char| c.is_whitespace() || c == ':')
        .find(|s| !s.is_empty())?
        .to_string();
    if name.is_empty() {
        return None;
    }
    Some((name, pid))
}

/// Strip the `<epoch> <host> kernel: ` prefix, keeping the message, clamped.
#[allow(dead_code)] // used only by the Linux scan
fn kernel_message(line: &str) -> String {
    line.split_once("kernel: ")
        .map_or(line, |(_, msg)| msg)
        .chars()
        .take(200)
        .collect()
}

// ----- Windows: System + Application event logs -----

/// Query the Windows event logs for storage errors and application crashes
/// since the cursor. Each match is emitted by PowerShell as `<unixts>|<kind>|
/// <message>` and parsed back by `classify_win_line`.
///
/// NOTE: compile-verified only — there is no Windows host in the deploy path
/// to runtime-test this against. The provider list is deliberately narrow so a
/// misfit can't page (`disk_error` is the only Windows kind that notifies).
#[cfg(windows)]
async fn scan_system_events(since_ts: i64) -> Result<Vec<SysEvent>, ScanError> {
    // Braces are unescaped because we substitute the cursor by string replace
    // rather than format!, so the PCRE-free script stays readable.
    const SCRIPT: &str = r#"
function Emit($k,$ev){foreach($e in $ev){$ts=[int64]([System.DateTimeOffset]$e.TimeCreated).ToUnixTimeSeconds();$m=($e.Message -split "`r?`n")[0];"$ts|$k|$m"}}
$since=[System.DateTimeOffset]::FromUnixTimeSeconds(__CURSOR__).LocalDateTime
Emit 'disk_error' (Get-WinEvent -FilterHashtable @{LogName='System';Level=1,2;StartTime=$since;ProviderName='disk','Disk','Ntfs','Microsoft-Windows-Ntfs','volmgr','Microsoft-Windows-DiskDiagnosticResolver','storahci','stornvme'} -ErrorAction SilentlyContinue)
Emit 'app_crash' (Get-WinEvent -FilterHashtable @{LogName='Application';StartTime=$since;ProviderName='Application Error'} -ErrorAction SilentlyContinue)
"#;
    let script = SCRIPT.replace("__CURSOR__", &since_ts.to_string());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Nothing owns this child once the timeout above fires.
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ScanError::Transient("Get-WinEvent timed out after 15s".to_string()))?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            ScanError::Unsupported("powershell.exe is not available".to_string())
        }
        _ => ScanError::Transient(format!("powershell spawn failed: {e}")),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // `-ErrorAction SilentlyContinue` swallows "no events matched" at the
    // Get-WinEvent level, so stderr here means a real script or host problem.
    // Still only fatal when it came back alone: one inaccessible provider must
    // not discard the events the others returned.
    if !stderr.trim().is_empty() {
        if stdout.trim().is_empty() {
            return Err(ScanError::Transient(format!(
                "powershell exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        debug!(
            "Get-WinEvent warned while still returning entries: {}",
            stderr.trim()
        );
    }

    Ok(stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(classify_win_line)
        .collect())
}

/// Parse one `<unixts>|<kind>|<message>` line emitted by the PowerShell probe.
#[allow(dead_code)] // used only by the Windows scan
fn classify_win_line(line: &str) -> Option<SysEvent> {
    let mut parts = line.splitn(3, '|');
    let ts: i64 = parts.next()?.trim().parse().ok()?;
    let raw_kind = parts.next()?.trim();
    let msg = parts.next().unwrap_or("").trim();
    let (kind, severity, subject) = match raw_kind {
        "disk_error" => ("disk_error", "error", "Disk / filesystem error"),
        "app_crash" => ("app_crash", "warn", "Application crashed"),
        _ => return None,
    };
    Some(SysEvent {
        ts,
        kind,
        severity,
        message: if msg.is_empty() {
            subject.to_string()
        } else {
            msg.chars().take(200).collect()
        },
        ref_type: None,
        ref_id: None,
    })
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

    /// Helper: the single event of a given kind, or panic.
    fn one<'a>(events: &'a [NewHostEvent], kind: &str) -> &'a NewHostEvent {
        events.iter().find(|e| e.kind == kind).unwrap_or_else(|| {
            panic!(
                "no {kind} event in {:?}",
                events.iter().map(|e| e.kind).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn first_run_records_only_server_started_info() {
        for marker in [None, Some(true)] {
            let evs = classify_startup(None, 1_000_000, marker);
            assert_eq!(evs.len(), 1, "no boot event on first run");
            let s = one(&evs, "server_started");
            assert_eq!(s.severity, "info");
        }
    }

    #[test]
    fn clean_reboot_emits_boot_info_and_server_started() {
        let evs = classify_startup(Some(1_000_000), 1_005_000, Some(true));
        let boot = one(&evs, "boot");
        assert_eq!(boot.severity, "info");
        assert_eq!(boot.created_at, Some(1_005_000));
        // A reboot start is not the daemon's fault → info, not warn.
        assert_eq!(one(&evs, "server_started").severity, "info");
    }

    #[test]
    fn unclean_reboot_is_warn_boot_but_info_server_started() {
        for marker in [Some(false), None] {
            let evs = classify_startup(Some(1_000_000), 1_005_000, marker);
            assert_eq!(one(&evs, "boot").severity, "warn");
            // Unclean-ness belongs to the reboot, already flagged on `boot`.
            assert_eq!(one(&evs, "server_started").severity, "info");
        }
    }

    #[test]
    fn boot_jitter_is_not_a_reboot() {
        for delta in [60, -60] {
            let evs = classify_startup(Some(1_000_000), 1_000_000 + delta, Some(true));
            assert_eq!(evs.len(), 1, "jitter must not record a boot");
            assert_eq!(evs[0].kind, "server_started");
        }
    }

    #[test]
    fn crash_restart_without_reboot_is_server_started_warn() {
        let evs = classify_startup(Some(1_000_000), 1_000_010, Some(false));
        assert_eq!(evs.len(), 1, "no boot event without a reboot");
        assert_eq!(one(&evs, "server_started").severity, "warn");
    }

    #[test]
    fn clean_restart_without_reboot_is_server_started_info() {
        let evs = classify_startup(Some(1_000_000), 1_000_010, Some(true));
        assert_eq!(evs.len(), 1);
        assert_eq!(one(&evs, "server_started").severity, "info");
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

    #[test]
    fn notify_policy_pages_only_the_alarming_kinds() {
        // Failures / unclean events page, at a severity-mapped level.
        assert_eq!(notify_severity("oom_kill", "error"), Some(Severity::Crit));
        assert_eq!(
            notify_severity("smart_health", "error"),
            Some(Severity::Crit)
        );
        assert_eq!(notify_severity("disk_error", "error"), Some(Severity::Crit));
        assert_eq!(notify_severity("boot", "warn"), Some(Severity::Warn));

        // Recovery / routine lifecycle / clean reboot stay quiet.
        assert_eq!(notify_severity("smart_health", "info"), None);
        assert_eq!(notify_severity("boot", "info"), None);
        assert_eq!(notify_severity("server_started", "warn"), None);

        // Process crashes are recorded but too noisy to page.
        assert_eq!(notify_severity("app_crash", "warn"), None);
        assert_eq!(notify_severity("app_crash", "error"), None);

        // Operator audit is never notification-worthy.
        assert_eq!(notify_severity("service_action", "info"), None);
        assert_eq!(notify_severity("config_changed", "info"), None);
        assert_eq!(notify_severity("process_killed", "info"), None);
    }

    #[cfg(any(target_os = "linux", windows))]
    fn sys_event(ts: i64) -> SysEvent {
        SysEvent {
            ts,
            kind: "oom_kill",
            severity: "error",
            message: format!("event at {ts}"),
            ref_type: None,
            ref_id: None,
        }
    }

    /// The cursor is a high-water mark of what was recorded, not a clock
    /// reading. Parked at `now`, it stepped over anything the log daemon
    /// flushed while the scan was running — the entry existed, was never read,
    /// and was then behind the cursor forever.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn cursor_follows_what_was_recorded_not_the_clock() {
        let now = 10_000;
        let plan = plan_sweep(vec![sys_event(100), sys_event(250)], 50, now);

        assert_eq!(plan.record.len(), 2);
        assert_eq!(
            plan.next_cursor, 250,
            "must land on the last recorded event, not on now"
        );
    }

    /// Nothing to record still has to advance, or a quiet host rescans an
    /// ever-widening window — but only to a grace period behind `now`.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn an_empty_tick_advances_but_stays_behind_now() {
        let now = 10_000;
        let plan = plan_sweep(vec![], 50, now);

        assert!(plan.record.is_empty());
        assert_eq!(plan.next_cursor, now - SWEEP_FLUSH_GRACE_SECS);
    }

    /// And never backwards: a cursor already inside the grace window stays put
    /// rather than being dragged back into ground it has covered.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn an_empty_tick_never_moves_the_cursor_back() {
        let now = 10_000;
        let plan = plan_sweep(vec![], now - 5, now);

        assert_eq!(plan.next_cursor, now - 5);
    }

    /// Past the per-tick cap the remainder must stay *ahead* of the cursor.
    /// Truncating the read and then jumping the cursor to `now` is how these
    /// events used to disappear without a trace.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn overflow_is_deferred_to_the_next_tick_not_dropped() {
        let now = 10_000;
        let events: Vec<SysEvent> = (1..=(SWEEP_MAX_EVENTS_PER_TICK as i64 + 20))
            .map(sys_event)
            .collect();

        let plan = plan_sweep(events, 0, now);

        assert!(plan.truncated);
        assert_eq!(plan.record.len(), SWEEP_MAX_EVENTS_PER_TICK);
        assert_eq!(
            plan.next_cursor, SWEEP_MAX_EVENTS_PER_TICK as i64,
            "the cursor stops at the last one recorded, leaving the rest ahead of it"
        );
    }

    /// Anything at or before the cursor was handled on an earlier tick.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn already_seen_events_are_filtered_out() {
        let plan = plan_sweep(
            vec![sys_event(10), sys_event(20), sys_event(30)],
            20,
            10_000,
        );

        assert_eq!(plan.record.len(), 1);
        assert_eq!(plan.record[0].ts, 30);
    }

    #[test]
    fn classifies_oom_kill_line() {
        let line = "1721375123.456789 myhost kernel: Out of memory: Killed process 1234 \
                    (chrome) total-vm:1000kB";
        let ev = classify_kernel_line(line).expect("classify");
        assert_eq!(ev.kind, "oom_kill");
        assert_eq!(ev.severity, "error");
        assert_eq!(ev.ts, 1721375123);
        assert_eq!(ev.ref_id.as_deref(), Some("chrome"));
    }

    #[test]
    fn classifies_segfault_as_app_crash() {
        let line = "1721375200.000000 myhost kernel: nginx[4321]: segfault at 0 ip \
                    00007f error 4 in libc.so.6";
        let ev = classify_kernel_line(line).expect("classify");
        assert_eq!(ev.kind, "app_crash");
        assert_eq!(ev.severity, "warn");
        assert_eq!(ev.ref_id.as_deref(), Some("nginx"));
        assert!(ev.message.contains("pid 4321"));
        assert!(ev.message.contains("segfaulted"));
    }

    #[test]
    fn classifies_general_protection_fault() {
        let line = "1721375201.5 myhost kernel: traps: app[99] general protection ip:400 sp:7ff";
        let ev = classify_kernel_line(line).expect("classify");
        assert_eq!(ev.kind, "app_crash");
        assert_eq!(ev.ref_id.as_deref(), Some("app"));
        assert!(ev.message.contains("general-protection"));
    }

    #[test]
    fn classifies_disk_error() {
        for line in [
            "1721375300.0 myhost kernel: EXT4-fs error (device sda1): ext4_find_entry:1234",
            "1721375301.0 myhost kernel: blk_update_request: I/O error, dev sda, sector 12345",
            "1721375302.0 myhost kernel: critical medium error, dev sdb, sector 999",
        ] {
            let ev = classify_kernel_line(line).expect("classify");
            assert_eq!(ev.kind, "disk_error", "line: {line}");
            assert_eq!(ev.severity, "error");
        }
    }

    #[test]
    fn parses_crash_process_name() {
        assert_eq!(
            parse_crash_line("… kernel: chrome[1234]: segfault at 0"),
            Some(("chrome".to_string(), 1234))
        );
        // The traps: form has no colon after the bracket.
        assert_eq!(
            parse_crash_line("… kernel: traps: worker[57] general protection"),
            Some(("worker".to_string(), 57))
        );
        assert!(parse_crash_line("no brackets here").is_none());
    }

    #[test]
    fn win_line_parses_and_maps_severity() {
        let disk =
            classify_win_line("1721375400|disk_error|The device is not ready").expect("disk");
        assert_eq!(disk.kind, "disk_error");
        assert_eq!(disk.severity, "error");
        assert_eq!(disk.ts, 1721375400);
        assert_eq!(disk.message, "The device is not ready");

        let app =
            classify_win_line("1721375401|app_crash|Faulting application foo.exe").expect("app");
        assert_eq!(app.kind, "app_crash");
        assert_eq!(app.severity, "warn");

        // Empty message falls back to the kind subject; unknown kinds drop.
        assert_eq!(
            classify_win_line("1721375402|disk_error|").unwrap().message,
            "Disk / filesystem error"
        );
        assert!(classify_win_line("1721375403|something_else|x").is_none());
        assert!(classify_win_line("not a valid line").is_none());
    }
}
