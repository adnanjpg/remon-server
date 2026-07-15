//! Incident flight recorder — freezes a compact context bundle the moment an
//! alert first crosses its threshold (or on operator/external request), so
//! "what was happening on the box at that moment" stays answerable long after
//! the moment is gone.
//!
//! Design rules:
//! - **Trigger-agnostic core.** `spawn_capture_for_alert` and
//!   `capture_manual` share one bundle builder; who pulled the handle is just
//!   a column. External detectors reach it via `POST /incidents/capture`.
//! - **No added load during the incident.** Every slice comes from data that
//!   already exists (stats cache, process ring, logs table, alert state) or
//!   from one bounded best-effort shell-out (journal errors, failed units).
//!   Nothing rescans the process table.
//! - **Bounded output.** Fixed top-N, line counts, and per-string clamps —
//!   a bundle is a few KB, never a dump.

use std::sync::Arc;

use log::{debug, warn};
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::state::AppState;
use crate::storage::repositories::{
    AlertRepository, IncidentRepository, LogRepository, NewIncident,
};

/// Minimum spacing between captures for the same (rule, label_set). A rule
/// flapping ok→pending→ok→pending re-captures at most once per window.
pub const ALERT_CAPTURE_COOLDOWN_SECS: i64 = 900;

/// Delay before the follow-up bundle that shows how the situation evolved.
const AFTER_DELAY_SECS: u64 = 60;

/// Processes kept per ranking (cpu, memory) in a bundle.
const TOP_N: usize = 8;

/// Category inferred from the triggering rule's metric namespace.
pub fn category_for_namespace(namespace: &str) -> &'static str {
    match namespace {
        "service" | "heartbeat" => "availability",
        // Probe metrics carry operator-defined semantics (a fail2ban probe is
        // security, a latency probe is availability) — don't pretend to know.
        "probe" => "custom",
        _ => "resource",
    }
}

/// Fire-and-forget capture for an alert transition. Runs off the evaluator's
/// tick so a slow slice (journal shell-out) can never stall rule evaluation;
/// the cooldown check lives here too, keeping the evaluator hook one line.
pub fn spawn_capture_for_alert(
    state: Arc<AppState>,
    rule_id: i64,
    rule_name: String,
    label_set: String,
    namespace: String,
    metric_value: f64,
) {
    tokio::spawn(async move {
        let repo = IncidentRepository::new(state.db.clone());
        let now = chrono::Utc::now().timestamp();
        match repo.latest_alert_capture(rule_id, &label_set).await {
            Ok(Some(ts)) if now - ts < ALERT_CAPTURE_COOLDOWN_SECS => {
                debug!("incident capture skipped (cooldown): rule='{rule_name}' label={label_set}");
                return;
            }
            Err(e) => {
                warn!("incident cooldown check failed for rule='{rule_name}': {e}");
                return;
            }
            _ => {}
        }

        let bundle = build_bundle(&state).await;
        let new = NewIncident {
            trigger_kind: "alert",
            category: category_for_namespace(&namespace).to_string(),
            rule_id: Some(rule_id),
            rule_name: Some(rule_name.clone()),
            label_set: Some(label_set),
            metric_value: Some(metric_value),
            reason: None,
            bundle: bundle.to_string(),
        };
        match repo.insert(&new).await {
            Ok(id) => {
                debug!("incident captured: id={id} rule='{rule_name}'");
                spawn_after_capture(state, id);
            }
            Err(e) => warn!("incident insert failed for rule='{rule_name}': {e}"),
        }
    });
}

/// Operator/external capture ("record the box, now"). Returns the snapshot id.
pub async fn capture_manual(state: &Arc<AppState>, reason: &str, category: &str) -> AppResult<i64> {
    let bundle = build_bundle(state).await;
    let repo = IncidentRepository::new(state.db.clone());
    let id = repo
        .insert(&NewIncident {
            trigger_kind: "manual",
            category: category.to_string(),
            rule_id: None,
            rule_name: None,
            label_set: None,
            metric_value: None,
            reason: Some(reason.chars().take(500).collect()),
            bundle: bundle.to_string(),
        })
        .await?;
    spawn_after_capture(Arc::clone(state), id);
    Ok(id)
}

/// T+60s follow-up: vitals + processes again, so before/after comparison
/// shows whether the pressure moved.
fn spawn_after_capture(state: Arc<AppState>, id: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(AFTER_DELAY_SECS)).await;
        let after = json!({
            "captured_at": chrono::Utc::now().timestamp(),
            "vitals": vitals_slice(&state).await,
            "top_processes": processes_slice(&state).await,
        });
        if let Err(e) = IncidentRepository::new(state.db.clone())
            .set_after(id, &after.to_string())
            .await
        {
            warn!("incident after-bundle write failed for id={id}: {e}");
        }
    });
}

/// The full capture bundle. Every slice is best-effort: a failed slice
/// becomes `null` (or `{"error": ...}`) rather than sinking the capture.
async fn build_bundle(state: &Arc<AppState>) -> Value {
    let mut bundle = json!({
        "captured_at": chrono::Utc::now().timestamp(),
        "vitals": vitals_slice(state).await,
        "top_processes": processes_slice(state).await,
        "recent_daemon_errors": daemon_errors_slice(state).await,
        "co_active_alerts": co_active_alerts_slice(state).await,
        "failed_services": failed_services_slice(state).await,
    });
    // System-level error events (OOM kills, segfaults, disk errors) are the
    // slice that answers "did the kernel do something" — Linux journal only;
    // other platforms simply omit it.
    if cfg!(target_os = "linux") {
        bundle["system_errors"] = match system_events("err", 20, Some(15)).await {
            Ok(v) => v,
            Err(e) => json!({ "error": e }),
        };
    }
    bundle
}

/// Cross-resource host cross-section from the stats cache. The triggering
/// metric alone can mislead (cpu alert with high iowait is a disk problem);
/// this slice keeps the neighbours in view.
async fn vitals_slice(state: &Arc<AppState>) -> Value {
    let Some(s) = state.stats_latest.read().await.clone() else {
        return Value::Null;
    };
    json!({
        "cpu_percent": s.cpu.usage_percent,
        "load": [s.cpu.load_avg.one, s.cpu.load_avg.five, s.cpu.load_avg.fifteen],
        "iowait_percent": s.cpu.iowait_percent,
        "steal_percent": s.cpu.steal_percent,
        "memory_used_bytes": s.memory.used_bytes,
        "memory_total_bytes": s.memory.total_bytes,
        "swap_used_bytes": s.memory.swap_used_bytes,
        "disks": s.disks.iter().map(|d| json!({
            "mount_point": d.mount_point,
            "used_percent": if d.total_bytes > 0 {
                Some(d.used_bytes as f64 / d.total_bytes as f64 * 100.0)
            } else { None },
            "io_util_percent": d.io_util_percent,
        })).collect::<Vec<_>>(),
        "network": s.network.iter().map(|n| json!({
            "interface": n.interface,
            "rx_bytes_per_sec": n.rx_bytes_per_sec,
            "tx_bytes_per_sec": n.tx_bytes_per_sec,
        })).collect::<Vec<_>>(),
        "pressure": s.pressure.as_ref().map(|p| json!({
            "cpu_some_avg10": p.cpu.as_ref().map(|x| x.some_avg10),
            "memory_some_avg10": p.memory.as_ref().map(|x| x.some_avg10),
            "memory_full_avg10": p.memory.as_ref().map(|x| x.full_avg10),
            "io_some_avg10": p.io.as_ref().map(|x| x.some_avg10),
        })),
    })
}

/// Top consumers by cpu and by memory (union), each enriched with its recent
/// in-memory history so the reader can tell spike from steady state — the
/// "before" context comes free from the ring the collector already keeps.
async fn processes_slice(state: &Arc<AppState>) -> Value {
    let Some(list) = state.processes_latest.read().await.clone() else {
        return Value::Null;
    };
    let mut by_cpu: Vec<usize> = (0..list.processes.len()).collect();
    by_cpu.sort_by(|&a, &b| {
        list.processes[b]
            .cpu_percent
            .total_cmp(&list.processes[a].cpu_percent)
    });
    let mut by_mem: Vec<usize> = (0..list.processes.len()).collect();
    by_mem.sort_by_key(|&i| std::cmp::Reverse(list.processes[i].memory_bytes));

    let mut picked: Vec<usize> = Vec::new();
    for i in by_cpu
        .into_iter()
        .take(TOP_N)
        .chain(by_mem.into_iter().take(TOP_N))
    {
        if !picked.contains(&i) {
            picked.push(i);
        }
    }

    let ring = state.process_history.read().await;
    let rows: Vec<Value> = picked
        .into_iter()
        .map(|i| {
            let p = &list.processes[i];
            let recent = ring.get(&p.pid).map(|h| {
                let n = h.samples.len().max(1) as f64;
                let (mut cpu_sum, mut cpu_max, mut mem_sum) = (0.0f64, 0.0f64, 0.0f64);
                for s in &h.samples {
                    cpu_sum += s.cpu_percent as f64;
                    cpu_max = cpu_max.max(s.cpu_percent as f64);
                    mem_sum += s.memory_bytes as f64;
                }
                let span = h
                    .samples
                    .back()
                    .zip(h.samples.front())
                    .map(|(b, f)| b.ts - f.ts)
                    .unwrap_or(0);
                json!({
                    "window_secs": span,
                    "cpu_avg_percent": cpu_sum / n,
                    "cpu_max_percent": cpu_max,
                    "memory_avg_bytes": mem_sum / n,
                })
            });
            let cmd: String = p.cmd.join(" ").chars().take(160).collect();
            json!({
                "pid": p.pid,
                "name": p.name,
                "cpu_percent": p.cpu_percent,
                "memory_bytes": p.memory_bytes,
                "memory_percent": p.memory_percent,
                "user": p.user,
                "cmd": cmd,
                "started_at": p.started_at,
                "recent": recent,
            })
        })
        .collect();
    json!({ "snapshot_at": list.timestamp, "processes": rows })
}

/// Last hour of the daemon's own error/warn lines.
async fn daemon_errors_slice(state: &Arc<AppState>) -> Value {
    let now = chrono::Utc::now().timestamp();
    match LogRepository::new(state.db.clone())
        .list(2, now - 3600, now, 15)
        .await
    {
        Ok(rows) => json!(
            rows.into_iter()
                .map(|r| {
                    json!({
                        "timestamp": r.timestamp,
                        "level": if r.level <= 1 { "error" } else { "warn" },
                        "target": r.target,
                        "message": r.message.chars().take(240).collect::<String>(),
                    })
                })
                .collect::<Vec<_>>()
        ),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Which other rules were pending/firing at capture time — co-occurrence is
/// half the diagnosis.
async fn co_active_alerts_slice(state: &Arc<AppState>) -> Value {
    match AlertRepository::new(state.db.clone())
        .list_active_state()
        .await
    {
        Ok(rows) => json!(
            rows.into_iter()
                .take(20)
                .map(|(row, name, severity)| {
                    json!({
                        "name": name,
                        "severity": severity.as_str(),
                        "state": row.state.as_str(),
                        "label_set": row.label_set,
                        "last_value": row.last_value,
                    })
                })
                .collect::<Vec<_>>()
        ),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Failed init-system units, bounded and under a hard timeout — the service
/// backend may shell out (Windows SCM) and must not stall a capture.
async fn failed_services_slice(state: &Arc<AppState>) -> Value {
    use crate::platform::services::{ServiceFilter, ServiceState};
    let listing = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.service_manager.list(ServiceFilter {
            state: Some(ServiceState::Failed),
        }),
    )
    .await;
    match listing {
        Ok(Ok(services)) => json!(
            services
                .into_iter()
                .take(10)
                .map(|s| json!({ "name": s.name, "raw_state": s.raw_state }))
                .collect::<Vec<_>>()
        ),
        Ok(Err(_)) | Err(_) => Value::Null,
    }
}

/// System-level error/warning events, platform-aware: journald on Linux,
/// the Windows event log (System + Application) via PowerShell. One bounded
/// one-shot read with a hard timeout; also reused by the assistant's
/// `read_system_events` tool.
pub async fn system_events(
    min_level: &str,
    lines: u64,
    since_minutes: Option<u64>,
) -> Result<Value, String> {
    let lines = lines.clamp(10, 200);
    let since = since_minutes.unwrap_or(60).clamp(1, 60 * 24 * 7);
    if min_level != "err" && min_level != "warn" {
        return Err("level must be 'err' or 'warn'".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        let priority = if min_level == "err" { "err" } else { "warning" };
        let mut cmd = tokio::process::Command::new("journalctl");
        cmd.args([
            "-p",
            priority,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "--output=short-precise",
            "--since",
            &format!("-{since}min"),
        ]);
        run_events_command(cmd, "journalctl", "journald").await
    }

    #[cfg(windows)]
    {
        let levels = if min_level == "err" { "1,2" } else { "1,2,3" };
        let script = format!(
            "Get-WinEvent -FilterHashtable @{{ LogName = @('System','Application'); \
             Level = @({levels}); StartTime = (Get-Date).AddMinutes(-{since}) }} \
             -MaxEvents {lines} -ErrorAction SilentlyContinue | ForEach-Object {{ \
             '{{0:u}} [{{1}}] {{2}}: {{3}}' -f $_.TimeCreated, $_.LevelDisplayName, \
             $_.ProviderName, (($_.Message -split \"\\r?\\n\")[0]) }}"
        );
        let mut cmd = tokio::process::Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        run_events_command(cmd, "powershell", "windows event log").await
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (lines, since);
        Err("system events are only available on Linux (journald) and Windows".to_string())
    }
}

/// Shared runner for the platform event readers: hard timeout, newest-lines
/// size clamp, per-line clamp.
#[cfg(any(target_os = "linux", windows))]
async fn run_events_command(
    mut cmd: tokio::process::Command,
    program: &str,
    source: &str,
) -> Result<Value, String> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        cmd.spawn()
            .map_err(|e| format!("{program} spawn failed: {e}"))?
            .wait_with_output()
            .await
            .map_err(|e| format!("{program} failed: {e}"))
    })
    .await
    .map_err(|_| format!("{program} timed out after 10s"))??;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} exited with {}: {}",
            output.status,
            err.chars().take(300).collect::<String>()
        ));
    }

    const MAX_CHARS: usize = 12_000;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in text.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let line: String = line.chars().take(300).collect();
        total += line.chars().count() + 1;
        if total > MAX_CHARS {
            break;
        }
        kept.push(line);
    }
    kept.reverse();

    Ok(json!({ "source": source, "count": kept.len(), "events": kept }))
}
