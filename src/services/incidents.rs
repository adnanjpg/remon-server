//! Incident flight recorder — an episode with a duration, not a photograph.
//!
//! An incident opens when an alert first crosses its threshold (or an operator
//! asks), collects a frame at each moment that carries information, and closes
//! when the rule resolves. `GET /incidents/{id}` hands back the whole reel.
//!
//! Why it is shaped this way. The previous design took one bundle at the
//! crossing and one 60 s later, and that could only describe an instantaneous
//! spike — not even reliably. With the default `for_duration_secs = 30`, the
//! `pending→firing` capture always landed inside the 900 s flap cooldown and
//! was dropped, so the only surviving frame was the one taken at `ok→pending`:
//! the moment nobody yet knows whether there is an incident at all. Nothing
//! was recorded when the rule resolved. A fifteen-minute event was therefore
//! described entirely by its first sixty seconds.
//!
//! Design rules, inherited and extended:
//! - **Trigger-agnostic core.** Alert transitions and `capture_manual` share
//!   one frame builder; who pulled the handle is just a column.
//! - **No added load during the incident.** Every slice comes from data that
//!   already exists (stats cache, process ring, metrics rows, alert state).
//!   The two shell-outs (journal, failed units) are reserved for the `onset`
//!   and `resolution` frames — the ones an operator actually reads first —
//!   so a long, spiky episode cannot turn into a shell-out storm.
//! - **Bounded output.** Fixed top-N, line counts, per-string clamps, and a
//!   hard ceiling on frames per episode. A reel is tens of KB, never a dump.
//! - **Gauges are referenced, not copied.** A frame carries the instant it was
//!   taken and nothing more; what the gauges did *across* the episode is read
//!   back from `metrics_*` between `opened_at` and `closed_at`. An earlier
//!   draft froze a min/avg/max summary into every frame, on the argument that
//!   the finer metric tiers age out from under a 90-day episode. Two things
//!   retired it. Rollups now preserve true extrema, so a `5m` bucket's maximum
//!   *is* the worst raw sample inside it — the fidelity gap was mostly
//!   imagined. And the host-gauge tiers are now retained as long as episodes
//!   are (see the `retention_policy` seed), which is one seed row against a
//!   duplicated copy of data that already exists. Storage is the cheaper half
//!   of that trade; the expensive half was that a frame had to know when its
//!   episode began, and threading that through every capture site was where
//!   the bugs lived.

use std::sync::Arc;

use log::{debug, warn};
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::state::AppState;
use crate::storage::repositories::{
    AlertRepository, IncidentRepository, LogRepository, NewIncident,
};

/// Minimum spacing between *episodes* for the same (rule, label_set). A rule
/// flapping ok→pending→ok→pending opens at most one episode per window.
///
/// This no longer suppresses frames: an escalation or a resolution belongs to
/// the episode that is already open, and dropping it was the bug that made the
/// old shape lose the second half of every incident.
pub const ALERT_CAPTURE_COOLDOWN_SECS: i64 = 900;

/// Manual captures have no rule to resolve them, so they get one follow-up
/// frame and then close.
const FOLLOWUP_DELAY_SECS: u64 = 60;

/// An episode open longer than this is closed as `expired`. Without a ceiling
/// a rule that never resolves would hold one row open forever, which blocks
/// retention and makes every later crossing read as a continuation.
const MAX_EPISODE_SECS: i64 = 6 * 3600;

/// A peak frame costs a process snapshot and a row, so it has to earn its
/// place: the value must beat the running peak by this margin *and* be this
/// far from the last peak frame.
const PEAK_MARGIN: f64 = 0.05;
const PEAK_FRAME_MIN_GAP_SECS: i64 = 120;

/// Hard ceiling on frames in one episode. Reached only by something pathological
/// — a six-hour episode climbing in steps every two minutes — and past it the
/// peak *value* keeps rising even though no further frames are written.
const MAX_FRAMES: u32 = 12;

/// Processes kept per ranking (cpu, memory) in a frame.
const TOP_N: usize = 8;

/// Live state for one open alert episode. Held in `AppState` so peak tracking
/// costs a read lock per tick instead of a database round-trip.
#[derive(Debug, Clone)]
pub struct Episode {
    pub incident_id: i64,
    pub opened_at: i64,
    pub peak_value: f64,
    pub frames: u32,
    pub last_peak_frame_at: i64,
}

/// How much of the bundle a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Everything, shell-outs included.
    Full,
    /// Everything available from memory and the database, no shell-outs.
    Light,
}

/// Where in its life the evaluator found this rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertPhase {
    /// ok→pending: the threshold was just crossed.
    Onset,
    /// pending→firing: it held long enough to count.
    Escalation,
    /// Still firing. Cheap: only a new worst value writes anything.
    Sustained,
    /// Back to ok.
    Resolved,
}

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

/// Close any episode left open by a previous process. Called once at boot:
/// whatever would have closed them is gone.
pub async fn close_orphaned_episodes(state: &Arc<AppState>) {
    let now = chrono::Utc::now().timestamp();
    match IncidentRepository::new(state.db.clone())
        .close_all_open(now, "daemon_restart")
        .await
    {
        Ok(n) if n > 0 => debug!("closed {n} incident episode(s) orphaned by a restart"),
        Ok(_) => {}
        Err(e) => warn!("orphaned-episode sweep failed: {e}"),
    }
}

/// The evaluator's one-line hook. Everything expensive is spawned; the
/// `Sustained` path stays on the caller's task because it is a map lookup that
/// almost always decides to do nothing.
pub async fn on_alert_transition(
    state: &Arc<AppState>,
    phase: AlertPhase,
    rule_id: i64,
    rule_name: &str,
    label_set: &str,
    namespace: &str,
    value: f64,
) {
    let key = (rule_id, label_set.to_string());

    match phase {
        AlertPhase::Sustained => {
            // Hot path: one read lock, and a write only when this tick is a
            // materially new worst.
            let now = chrono::Utc::now().timestamp();
            let due = {
                let map = state.incident_episodes.read().await;
                match map.get(&key) {
                    Some(ep) => {
                        let worse = value > ep.peak_value * (1.0 + PEAK_MARGIN);
                        let spaced = now - ep.last_peak_frame_at >= PEAK_FRAME_MIN_GAP_SECS;
                        let expired = now - ep.opened_at >= MAX_EPISODE_SECS;
                        if expired {
                            Some((ep.incident_id, true, false))
                        } else if worse && spaced && ep.frames < MAX_FRAMES {
                            Some((ep.incident_id, false, true))
                        } else if value > ep.peak_value {
                            // Worth remembering, not worth a frame.
                            Some((ep.incident_id, false, false))
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            };
            let Some((incident_id, expired, write_frame)) = due else {
                return;
            };
            if expired {
                finish_episode(state, key, incident_id, "expired", None).await;
                return;
            }
            {
                let mut map = state.incident_episodes.write().await;
                if let Some(ep) = map.get_mut(&key) {
                    ep.peak_value = ep.peak_value.max(value);
                    if write_frame {
                        ep.frames += 1;
                        ep.last_peak_frame_at = now;
                    }
                }
            }
            let state = Arc::clone(state);
            tokio::spawn(async move {
                let repo = IncidentRepository::new(state.db.clone());
                if let Err(e) = repo.raise_peak(incident_id, value).await {
                    warn!("incident peak update failed for id={incident_id}: {e}");
                }
                if write_frame {
                    append_frame(&state, incident_id, "peak", Depth::Light).await;
                }
            });
        }

        AlertPhase::Onset => {
            let (state, rule_name, namespace) = (
                Arc::clone(state),
                rule_name.to_string(),
                namespace.to_string(),
            );
            tokio::spawn(async move {
                open_episode(&state, key, rule_id, rule_name, namespace, value).await;
            });
        }

        AlertPhase::Escalation => {
            let state = Arc::clone(state);
            tokio::spawn(async move {
                let Some((incident_id, _)) = live_episode(&state, &key).await else {
                    // No open episode: the cooldown declined to open one, or the
                    // daemon restarted mid-incident. Either way there is nothing
                    // to append to, and inventing an episode here would report a
                    // start time that never happened.
                    return;
                };
                bump_frames(&state, &key).await;
                append_frame(&state, incident_id, "escalation", Depth::Light).await;
            });
        }

        AlertPhase::Resolved => {
            let state = Arc::clone(state);
            tokio::spawn(async move {
                let Some((incident_id, _)) = live_episode(&state, &key).await else {
                    return;
                };
                finish_episode(&state, key, incident_id, "resolved", Some(value)).await;
            });
        }
    }
}

/// Open an episode and take its `onset` frame, unless the flap cooldown says
/// this key already had one recently.
async fn open_episode(
    state: &Arc<AppState>,
    key: (i64, String),
    rule_id: i64,
    rule_name: String,
    namespace: String,
    value: f64,
) {
    let repo = IncidentRepository::new(state.db.clone());
    let now = chrono::Utc::now().timestamp();

    // An episode already open for this key means the evaluator saw ok→pending
    // without an intervening resolve (a restored state, say). Keep the older
    // one: its `opened_at` is the truthful start of the trouble.
    match repo.open_episode_for(rule_id, &key.1).await {
        Ok(Some(_)) => return,
        Err(e) => {
            warn!("incident open-episode check failed for rule='{rule_name}': {e}");
            return;
        }
        Ok(None) => {}
    }

    match repo.latest_alert_capture(rule_id, &key.1).await {
        Ok(Some(ts)) if now - ts < ALERT_CAPTURE_COOLDOWN_SECS => {
            debug!(
                "incident episode skipped (cooldown): rule='{rule_name}' label={}",
                key.1
            );
            return;
        }
        Err(e) => {
            warn!("incident cooldown check failed for rule='{rule_name}': {e}");
            return;
        }
        _ => {}
    }

    let new = NewIncident {
        trigger_kind: "alert",
        category: category_for_namespace(&namespace).to_string(),
        rule_id: Some(rule_id),
        rule_name: Some(rule_name.clone()),
        label_set: Some(key.1.clone()),
        trigger_value: Some(value),
        reason: None,
    };
    let incident_id = match repo.open(&new).await {
        Ok(id) => id,
        Err(e) => {
            warn!("incident open failed for rule='{rule_name}': {e}");
            return;
        }
    };

    state.incident_episodes.write().await.insert(
        key,
        Episode {
            incident_id,
            opened_at: now,
            peak_value: value,
            frames: 1,
            last_peak_frame_at: 0,
        },
    );
    debug!("incident episode opened: id={incident_id} rule='{rule_name}'");
    append_frame(state, incident_id, "onset", Depth::Full).await;
}

/// Take the closing frame, write the close, and forget the live state.
async fn finish_episode(
    state: &Arc<AppState>,
    key: (i64, String),
    incident_id: i64,
    reason: &str,
    final_value: Option<f64>,
) {
    let now = chrono::Utc::now().timestamp();
    // Forget the live state first: whatever happens below, this key is no
    // longer recording, and a later tick must not append to a closed episode.
    state.incident_episodes.write().await.remove(&key);

    if let Some(v) = final_value {
        let repo = IncidentRepository::new(state.db.clone());
        if let Err(e) = repo.raise_peak(incident_id, v).await {
            warn!("incident peak update failed for id={incident_id}: {e}");
        }
    }
    // The closing frame is the one the old shape never took, so it gets the
    // full depth: whether a unit died or the kernel complained during the
    // episode is answerable here and nowhere else.
    append_frame(state, incident_id, "resolution", Depth::Full).await;

    if let Err(e) = IncidentRepository::new(state.db.clone())
        .close(incident_id, now, reason)
        .await
    {
        warn!("incident close failed for id={incident_id}: {e}");
    }
}

/// The open episode's `(id, opened_at)`, from memory when this process opened
/// it and from the row when it did not.
async fn live_episode(state: &Arc<AppState>, key: &(i64, String)) -> Option<(i64, i64)> {
    if let Some(ep) = state.incident_episodes.read().await.get(key) {
        return Some((ep.incident_id, ep.opened_at));
    }
    // The map is process-local; the row is the truth.
    IncidentRepository::new(state.db.clone())
        .open_episode_for(key.0, &key.1)
        .await
        .ok()
        .flatten()
}

async fn bump_frames(state: &Arc<AppState>, key: &(i64, String)) {
    if let Some(ep) = state.incident_episodes.write().await.get_mut(key) {
        ep.frames += 1;
    }
}

/// Operator/external capture ("record the box, now"). One episode, an `onset`
/// frame, a follow-up a minute later, then closed — there is no rule whose
/// resolution could close it.
pub async fn capture_manual(state: &Arc<AppState>, reason: &str, category: &str) -> AppResult<i64> {
    let repo = IncidentRepository::new(state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: category.to_string(),
            rule_id: None,
            rule_name: None,
            label_set: None,
            trigger_value: None,
            reason: Some(reason.chars().take(500).collect()),
        })
        .await?;
    append_frame(state, id, "onset", Depth::Full).await;

    let state = Arc::clone(state);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(FOLLOWUP_DELAY_SECS)).await;
        append_frame(&state, id, "followup", Depth::Light).await;
        let now = chrono::Utc::now().timestamp();
        if let Err(e) = IncidentRepository::new(state.db.clone())
            .close(id, now, "resolved")
            .await
        {
            warn!("manual incident close failed for id={id}: {e}");
        }
    });
    Ok(id)
}

/// Build one frame and store it. Every slice is best-effort: a failed slice
/// becomes `null` (or `{"error": …}`) rather than sinking the frame.
///
/// Shell-out slices (journal, init-system state) are skipped in the test
/// profile: hermetic tests must not depend on the host's journald/systemd
/// state, and their multi-second best-effort timeouts would turn every
/// spawned-capture assertion into a timing lottery (bit CI on Linux).
async fn append_frame(state: &Arc<AppState>, incident_id: i64, kind: &str, depth: Depth) {
    let now = chrono::Utc::now().timestamp();
    let payload = build_frame(state, kind, depth, now).await;
    if let Err(e) = IncidentRepository::new(state.db.clone())
        .append_frame(incident_id, kind, now, &payload.to_string())
        .await
    {
        warn!("incident frame write failed for id={incident_id} kind={kind}: {e}");
    }
}

async fn build_frame(state: &Arc<AppState>, kind: &str, depth: Depth, now: i64) -> Value {
    let shell_out_slices = !cfg!(test) && depth == Depth::Full;
    // The instant, and only the instant. What the gauges did between frames is
    // in `metrics_*`, which outlives the episode — see the module header.
    let mut frame = json!({
        "kind": kind,
        "captured_at": now,
        "vitals": vitals_slice(state).await,
        "top_processes": processes_slice(state).await,
        "co_active_alerts": co_active_alerts_slice(state).await,
    });

    if depth == Depth::Full {
        frame["recent_daemon_errors"] = daemon_errors_slice(state).await;
        frame["failed_services"] = if shell_out_slices {
            failed_services_slice(state).await
        } else {
            Value::Null
        };
        // System-level error events (OOM kills, segfaults, disk errors) are the
        // slice that answers "did the kernel do something" — Linux journal only;
        // other platforms simply omit it.
        if cfg!(target_os = "linux") && shell_out_slices {
            frame["system_errors"] = match system_events("err", 20, Some(15)).await {
                Ok(v) => v,
                Err(e) => json!({ "error": e }),
            };
        }
    }
    frame
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
