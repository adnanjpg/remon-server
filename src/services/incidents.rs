//! Bounded, observation-driven incident episodes.
//! Lifecycle writes are serialized; shell enrichment never delays a transition.
//! A close records why observation ended, and only a healthy sample means recovery.
use crate::error::AppResult;
use crate::services::alerting::expression::{Comparator, Expression};
use crate::state::AppState;
use crate::storage::repositories::{
    AlertRepository, IncidentRepository, LogRepository, NewIncident,
};
use log::warn;
use serde_json::{Value, json};
use std::sync::Arc;

const FOLLOWUP_DELAY_SECS: u64 = 60;
const MAX_EPISODE_SECS: i64 = 6 * 3600;
const PEAK_MARGIN: f64 = 0.05;
const PEAK_FRAME_MIN_GAP_SECS: i64 = 120;
const CHECKPOINT_AT: [i64; 4] = [600, 1800, 7200, 14400];
const MAX_FRAMES: u32 = 12;
const TOP_N: usize = 8;

#[derive(Debug, Clone)]
pub struct Episode {
    pub incident_id: i64,
    pub opened_at: i64,
    pub worst_value: f64,
    pub frames: u32,
    pub last_peak_frame_at: i64,
    pub last_frame_value: f64,
    pub last_seen_at: i64,
    pub comparator: Comparator,
    pub threshold: f64,
    pub expression: String,
    pub eval_interval: i64,
    pub for_duration: i64,
    pub confirmed: bool,
    pub checkpoints: usize,
    pub peaks: u32,
    pub recovery_started_at: Option<i64>,
    pub violation_count: i64,
    pub confirmation_count: i64,
    pub cycle_confirmed: bool,
    pub recovery_frame_taken: bool,
    pub relapse_frame_taken: bool,
    pub escalation_frame_taken: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    Full,
    Light,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertPhase {
    Onset,
    Pending,
    Escalation,
    Sustained,
    Resolved,
}

pub fn category_for_namespace(namespace: &str) -> &'static str {
    match namespace {
        "service" | "heartbeat" => "availability",
        "probe" => "custom",
        _ => "resource",
    }
}
/// State comparisons have no numeric severity. Inequalities have a direction.
fn severity(comparator: Comparator, threshold: f64, value: f64) -> Option<f64> {
    match comparator {
        Comparator::Gt | Comparator::Ge => Some(value - threshold),
        Comparator::Lt | Comparator::Le => Some(threshold - value),
        Comparator::Eq | Comparator::Ne => None,
    }
}

pub async fn close_orphaned_episodes(state: &Arc<AppState>) {
    if let Err(e) = IncidentRepository::new(state.db.clone())
        .close_all_open(chrono::Utc::now().timestamp(), "daemon_restart")
        .await
    {
        warn!("orphaned-episode sweep failed: {e}");
    }
}

/// Independent of samples: a disappeared source cannot keep an episode open forever.
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown = state.shutdown.subscribe();
        loop {
            tokio::select! { _ = shutdown.changed() => break, _ = timer.tick() => {} }
            maintain(&state, chrono::Utc::now().timestamp()).await;
        }
    });
}

pub async fn maintain(state: &Arc<AppState>, now: i64) {
    let _guard = state.incident_gate.lock().await;
    if let Err(e) = IncidentRepository::new(state.db.clone())
        .close_stale_manual(now)
        .await
    {
        warn!("manual incident sweep failed: {e}");
    }
    let episodes = state.incident_episodes.read().await.clone();
    let repo = AlertRepository::new(state.db.clone());
    for (key, ep) in episodes {
        let reason = match repo.get(key.0).await {
            Ok(None) => Some("rule_removed"),
            Ok(Some(r)) if !r.enabled => Some("rule_disabled"),
            Ok(Some(r))
                if r.expression != ep.expression
                    || r.for_duration_secs != ep.for_duration
                    || r.eval_interval_secs != ep.eval_interval =>
            {
                Some("rule_changed")
            }
            Err(e) => {
                warn!("incident rule lookup failed: {e}");
                continue;
            }
            _ if now - ep.last_seen_at >= (ep.eval_interval * 3).max(120) => Some("data_gap"),
            _ if now - ep.opened_at >= MAX_EPISODE_SECS => Some("expired"),
            _ => None,
        };
        if let Some(reason) = reason
            && let Err(e) = finish_episode(state, &key, &ep, reason, None, now).await
        {
            warn!("incident maintenance failed: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn on_alert_transition(
    state: &Arc<AppState>,
    phase: AlertPhase,
    rule_id: i64,
    _rule_name: &str,
    label_set: &str,
    _namespace: &str,
    value: f64,
) {
    if let Ok(Some(rule)) = AlertRepository::new(state.db.clone()).get(rule_id).await
        && let Ok(expr) = crate::services::alerting::expression::parse(&rule.expression)
    {
        on_rule_observation(state, phase, &rule, &expr, label_set, value).await;
    }
}
/// The definition comes from the same compiled rule that evaluated this sample.
pub async fn on_rule_observation(
    state: &Arc<AppState>,
    phase: AlertPhase,
    rule: &crate::models::alert::AlertRule,
    expr: &Expression,
    label_set: &str,
    value: f64,
) {
    if !value.is_finite() {
        return;
    }
    let _guard = state.incident_gate.lock().await;
    if let Err(e) = observe(
        state,
        phase,
        (rule.id, label_set.to_string()),
        rule,
        expr,
        value,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        warn!("incident observation failed for rule={}: {e}", rule.id);
    }
}
/// Tests drive the real observer with explicit wall time, without sleeps.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn observe_at(
    state: &Arc<AppState>,
    phase: AlertPhase,
    rule: &crate::models::alert::AlertRule,
    expr: &Expression,
    label: &str,
    value: f64,
    now: i64,
) -> AppResult<()> {
    let _guard = state.incident_gate.lock().await;
    observe(
        state,
        phase,
        (rule.id, label.into()),
        rule,
        expr,
        value,
        now,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn observe(
    state: &Arc<AppState>,
    phase: AlertPhase,
    key: (i64, String),
    rule: &crate::models::alert::AlertRule,
    expr: &Expression,
    value: f64,
    now: i64,
) -> AppResult<()> {
    let repo = IncidentRepository::new(state.db.clone());
    let mut existing = state.incident_episodes.read().await.get(&key).cloned();
    if let Some(ep) = &existing
        && (ep.expression != rule.expression
            || ep.for_duration != rule.for_duration_secs
            || ep.eval_interval != rule.eval_interval_secs)
    {
        finish_episode(state, &key, ep, "rule_changed", None, now).await?;
        existing = None;
    }
    // A late sample cannot bridge a missing-data interval merely because the sweep has not run.
    if let Some(ep) = &existing {
        let reason = if now - ep.last_seen_at >= (ep.eval_interval * 3).max(120) {
            Some("data_gap")
        } else if now - ep.opened_at >= MAX_EPISODE_SECS {
            Some("expired")
        } else {
            None
        };
        if let Some(reason) = reason {
            finish_episode(state, &key, ep, reason, None, now).await?;
            existing = None;
        }
    }
    if phase == AlertPhase::Resolved {
        let Some(mut ep) = existing else {
            return Ok(());
        };
        ep.last_seen_at = now;
        if ep.recovery_started_at.is_none() {
            ep.recovery_started_at = Some(now);
            repo.update_observation(
                ep.incident_id,
                ep.recovery_started_at,
                ep.violation_count,
                ep.confirmation_count,
            )
            .await?;
        }
        state
            .incident_episodes
            .write()
            .await
            .insert(key.clone(), ep.clone());
        if !ep.recovery_frame_taken {
            record_frame_at(
                state,
                ep.incident_id,
                "recovery",
                Depth::Light,
                Some(value),
                None,
                now,
            )
            .await?;
            ep.recovery_frame_taken = true;
            ep.frames += 1;
            state
                .incident_episodes
                .write()
                .await
                .insert(key.clone(), ep.clone());
        }
        // Only healthy evaluations close a recovery window; a maintenance timer never does.
        if now - ep.recovery_started_at.unwrap() >= (ep.eval_interval * 2).max(60) {
            finish_episode(state, &key, &ep, "resolved", Some(value), now).await?;
        }
        return Ok(());
    }
    let mut ep = if let Some(ep) = existing {
        ep
    } else {
        let continuation = phase != AlertPhase::Onset;
        let previous = if continuation {
            repo.previous_episode(key.0, &key.1).await?
        } else {
            None
        };
        let context = json!({ "previous_incident_id":previous, "expression":rule.expression, "comparator":expr.comparator.as_str(),
            "threshold":expr.threshold, "namespace":expr.metric.namespace, "field":expr.metric.field,
            "for_duration_secs":rule.for_duration_secs, "eval_interval_secs":rule.eval_interval_secs,
            "severity":rule.severity, "capture_policy": {"recovery_hold_secs":(rule.eval_interval_secs*2).max(60),"max_frames":MAX_FRAMES,"max_episode_secs":MAX_EPISODE_SECS,"checkpoint_offsets_secs":CHECKPOINT_AT,"peak_relative_change":PEAK_MARGIN,"min_peak_interval_secs":PEAK_FRAME_MIN_GAP_SECS}, "start_kind": if continuation { "continuation" } else { "crossing" } });
        let id = repo
            .open(&NewIncident {
                trigger_kind: "alert",
                category: category_for_namespace(&expr.metric.namespace).into(),
                rule_id: Some(key.0),
                rule_name: Some(rule.name.clone()),
                label_set: Some(key.1.clone()),
                trigger_value: Some(value),
                // A directionless comparison has no worst: `severity` returns
                // `None` for it, and nothing below would ever raise this.
                initial_worst: severity(expr.comparator, expr.threshold, value).map(|_| value),
                reason: None,
                trigger_context: Some(context.to_string()),
            })
            .await?;
        let ep = Episode {
            incident_id: id,
            opened_at: now,
            worst_value: value,
            frames: 0,
            last_peak_frame_at: now,
            last_frame_value: value,
            last_seen_at: now,
            comparator: expr.comparator,
            threshold: expr.threshold,
            expression: rule.expression.clone(),
            eval_interval: rule.eval_interval_secs,
            for_duration: rule.for_duration_secs,
            confirmed: false,
            checkpoints: 0,
            peaks: 0,
            recovery_started_at: None,
            violation_count: 1,
            confirmation_count: 0,
            cycle_confirmed: false,
            recovery_frame_taken: false,
            relapse_frame_taken: false,
            escalation_frame_taken: false,
        };
        // Publish before capture so a failed write is retried by the next observation.
        state
            .incident_episodes
            .write()
            .await
            .insert(key.clone(), ep.clone());
        ep
    };
    let relapsed = ep.recovery_started_at.is_some();
    if relapsed {
        ep.recovery_started_at = None;
        ep.violation_count += 1;
        ep.cycle_confirmed = false;
    }
    let confirming =
        !ep.cycle_confirmed && matches!(phase, AlertPhase::Escalation | AlertPhase::Sustained);
    if confirming {
        ep.confirmation_count += 1;
        ep.cycle_confirmed = true;
        ep.confirmed = true;
    }
    if relapsed || confirming {
        repo.update_observation(
            ep.incident_id,
            ep.recovery_started_at,
            ep.violation_count,
            ep.confirmation_count,
        )
        .await?;
    }
    ep.last_seen_at = now;
    state
        .incident_episodes
        .write()
        .await
        .insert(key.clone(), ep.clone());
    if severity(ep.comparator, ep.threshold, value)
        > severity(ep.comparator, ep.threshold, ep.worst_value)
    {
        repo.set_worst(ep.incident_id, value).await?;
        ep.worst_value = value;
    }
    state
        .incident_episodes
        .write()
        .await
        .insert(key.clone(), ep.clone());
    let kind = if ep.frames == 0 {
        Some(if phase == AlertPhase::Onset {
            "onset"
        } else {
            "continuation"
        })
    } else if ep.confirmed && !ep.escalation_frame_taken {
        Some("escalation")
    } else if ep.violation_count > 1 && !ep.relapse_frame_taken {
        Some("relapse")
    } else if ep.frames
        < MAX_FRAMES
            - 1
            - u32::from(!ep.escalation_frame_taken)
            - u32::from(!ep.recovery_frame_taken)
            - u32::from(!ep.relapse_frame_taken)
        && now - ep.last_peak_frame_at >= PEAK_FRAME_MIN_GAP_SECS
    {
        let delta = severity(ep.comparator, ep.threshold, value)
            .zip(severity(ep.comparator, ep.threshold, ep.last_frame_value))
            .map(|(a, b)| a - b)
            .unwrap_or(0.0);
        let margin = ep.last_frame_value.abs().max(ep.threshold.abs()).max(1.0) * PEAK_MARGIN;
        if delta >= margin && ep.peaks < 3 && value == ep.worst_value {
            Some("peak")
        } else if CHECKPOINT_AT
            .get(ep.checkpoints)
            .is_some_and(|at| now - ep.opened_at >= *at)
        {
            Some("checkpoint")
        } else {
            None
        }
    } else {
        None
    };
    if let Some(kind) = kind {
        let depth = if ep.frames == 0 {
            Depth::Full
        } else {
            Depth::Light
        };
        record_frame_at(state, ep.incident_id, kind, depth, Some(value), None, now).await?;
        ep.frames += 1;
        if kind == "escalation" {
            ep.escalation_frame_taken = true;
        }
        if kind == "relapse" {
            ep.relapse_frame_taken = true;
        }
        if kind == "peak" {
            ep.peaks += 1;
        }
        if kind == "checkpoint" {
            ep.checkpoints += 1;
        }
        ep.last_peak_frame_at = now;
        ep.last_frame_value = value;
        if matches!(phase, AlertPhase::Escalation | AlertPhase::Sustained) {
            ep.confirmed = true;
        }
    }
    state.incident_episodes.write().await.insert(key, ep);
    Ok(())
}

async fn finish_episode(
    state: &Arc<AppState>,
    key: &(i64, String),
    ep: &Episode,
    reason: &str,
    value: Option<f64>,
    now: i64,
) -> AppResult<()> {
    let kind = if reason == "resolved" {
        if ep.confirmed {
            "resolution"
        } else {
            "cleared"
        }
    } else {
        "interrupted"
    };
    let wrote = record_frame_at(
        state,
        ep.incident_id,
        kind,
        Depth::Full,
        value,
        Some(reason),
        now,
    )
    .await;

    // A transient database error is worth retrying, so the live entry stays and
    // the next observation or sweep tries again. A row that is already closed
    // is not: the terminal insert is guarded on `closed_at IS NULL`, so it
    // returns nothing and there is no state left to write. Keeping the entry
    // for that case would retry forever, once every maintenance tick.
    // `NotFound` here can only be the guarded insert returning no row, since
    // that is the one lookup this path makes.
    let gone = matches!(&wrote, Err(crate::error::AppError::NotFound(_)));
    if wrote.is_ok() || gone {
        state.incident_episodes.write().await.remove(key);
    }
    if gone { Ok(()) } else { wrote }
}

pub async fn capture_manual(state: &Arc<AppState>, reason: &str, category: &str) -> AppResult<i64> {
    let repo = IncidentRepository::new(state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: category.into(),
            reason: Some(reason.chars().take(500).collect()),
            ..Default::default()
        })
        .await?;
    if let Err(e) = record_frame(state, id, "onset", Depth::Full, None, None).await {
        repo.close(id, chrono::Utc::now().timestamp(), "data_gap")
            .await?;
        return Err(e);
    }
    let state = Arc::clone(state);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(FOLLOWUP_DELAY_SECS)).await;
        if let Err(e) = record_frame(
            &state,
            id,
            "followup",
            Depth::Light,
            None,
            Some("completed"),
        )
        .await
        {
            warn!("manual followup failed: {e}");
        }
    });
    Ok(id)
}

/// Freeze cheap context and persist its order now. Bounded enrichment edits only this frame.
async fn record_frame(
    state: &Arc<AppState>,
    id: i64,
    kind: &str,
    depth: Depth,
    value: Option<f64>,
    close_reason: Option<&str>,
) -> AppResult<()> {
    record_frame_at(
        state,
        id,
        kind,
        depth,
        value,
        close_reason,
        chrono::Utc::now().timestamp(),
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn record_frame_at(
    state: &Arc<AppState>,
    id: i64,
    kind: &str,
    depth: Depth,
    value: Option<f64>,
    close_reason: Option<&str>,
    now: i64,
) -> AppResult<()> {
    let mut payload = json!({ "kind":kind, "captured_at":now, "trigger_value":value,
        "vitals":vitals_slice(state).await, "top_processes":processes_slice(state).await,
        "co_active_alerts":co_active_alerts_slice(state).await });
    if depth == Depth::Full {
        payload["recent_daemon_errors"] = daemon_errors_slice(state).await;
    }
    let permit = if depth == Depth::Full && !cfg!(test) {
        state.incident_enrichment.clone().try_acquire_owned().ok()
    } else {
        None
    };
    payload["enrichment"] = json!(if permit.is_some() {
        "pending"
    } else if depth == Depth::Full && !cfg!(test) {
        "skipped_busy"
    } else {
        "not_requested"
    });
    let repo = IncidentRepository::new(state.db.clone());
    let seq = if close_reason.is_some() {
        repo.write_frame(id, kind, now, &payload.to_string(), close_reason)
            .await?
    } else {
        repo.append_frame(id, kind, now, &payload.to_string())
            .await?
    };
    if let Some(permit) = permit {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let _permit = permit;
            payload["failed_services"] = failed_services_slice(&state).await;
            if cfg!(target_os = "linux") {
                payload["system_errors"] = match system_events("err", 20, Some(15)).await {
                    Ok(v) => v,
                    Err(e) => json!({"error":e}),
                };
            }
            payload["enrichment"] = json!("complete");
            payload["enriched_at"] = json!(chrono::Utc::now().timestamp());
            if let Err(e) = IncidentRepository::new(state.db.clone())
                .enrich_frame(id, seq, &payload.to_string())
                .await
            {
                warn!("incident enrichment failed: {e}");
            }
        });
    }
    Ok(())
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
