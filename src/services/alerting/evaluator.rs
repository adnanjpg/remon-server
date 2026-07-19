//! Alert evaluator — a single event-driven task.
//!
//! One task drives every enabled rule. It wakes on the stats collector's
//! signal (a `watch` bump after each `stats_latest` write), falling back to a
//! short timer for liveness if the collector stalls, and evaluates each rule
//! at its `eval_interval`. All lifecycle state lives in memory; the DB is
//! touched only on transitions and to keep active (pending/firing) rows'
//! displayed value fresh — a healthy host with everything Ok writes
//! `alert_state` zero times per tick. State is hydrated from `alert_state` at
//! startup so firing/pending survives a restart without re-firing from Ok.
//!
//! State machine (per `(rule_id, label_set)` pair):
//!
//! ```text
//!                  violation               !violation
//!     ok  ─────────────────────►  pending  ─────────────►  ok
//!                                    │
//!                                    │  violation,
//!                                    ▼  for_duration elapsed
//!                                  firing  ─── !violation ──►  ok
//!                                                              (resolved)
//! ```
//!
//! - `ok → pending` on first violation. Silent.
//! - `pending → ok` if the violation clears before `for_duration_secs`.
//!   Silent — that's the false-positive debounce in action.
//! - `pending → firing` once the violation has persisted past
//!   `for_duration_secs`. Emits a `fired` event; notification fans out
//!   unless we're inside `cooldown_secs` of the previous fire.
//! - `firing → ok` on first non-violating evaluation. Emits a
//!   `resolved` event; notification fans out unconditionally (recovery
//!   is more useful than spam-protection here).
//!
//! Label_sets that vanish from resolver output are pruned: Ok and
//! Pending rows silently, Firing rows with a synthetic resolved event +
//! notification ("target removed") — otherwise a deleted/renamed target
//! would strand its Firing row forever, since nothing re-evaluates a
//! label_set that stops appearing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use log::{debug, info, warn};

use crate::models::alert::{
    AlertEventType, AlertLifecycle, AlertRule, AlertSeverity, AlertStateRow,
};
use crate::notify::{Notification, NotificationEvent, Severity};
use crate::state::AppState;
use crate::storage::repositories::AlertRepository;

use super::expression::{self, Expression};
use super::resolver::{self};

/// Longest the evaluator sleeps waiting for a fresh stats tick before
/// evaluating anyway — keeps alerting live if the collector stalls.
const FALLBACK_TICK: Duration = Duration::from_secs(2);

/// How often the rule set is reloaded from the DB, so `POST/PUT/DELETE
/// /alerts` and enable/disable take effect within one cycle.
const RELOAD_SECS: i64 = 30;

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move { run(state).await });
}

/// In-memory lifecycle for one `(rule, label_set)` — the persisted
/// `alert_state` row minus the rule join. The evaluator holds these and
/// writes to the DB only when they transition or stay non-Ok.
#[derive(Clone, Debug)]
struct Live {
    state: AlertLifecycle,
    state_since: i64,
    last_value: Option<f64>,
    last_eval_at: i64,
    last_notified_at: Option<i64>,
}

impl Live {
    fn fresh(now: i64) -> Self {
        Self {
            state: AlertLifecycle::Ok,
            state_since: now,
            last_value: None,
            last_eval_at: now,
            last_notified_at: None,
        }
    }

    fn from_row(r: &AlertStateRow) -> Self {
        Self {
            state: r.state,
            state_since: r.state_since,
            last_value: r.last_value,
            last_eval_at: r.last_eval_at,
            last_notified_at: r.last_notified_at,
        }
    }

    fn to_row(&self, rule_id: i64, label_set: &str) -> AlertStateRow {
        AlertStateRow {
            rule_id,
            label_set: label_set.to_string(),
            state: self.state,
            state_since: self.state_since,
            last_value: self.last_value,
            last_eval_at: self.last_eval_at,
            last_notified_at: self.last_notified_at,
        }
    }
}

/// A loaded, enabled rule with its parsed expression and per-rule throttle
/// clock (`last_eval_at`), so a rule keeps evaluating at its own
/// `eval_interval` even though every rule shares one task.
struct Compiled {
    rule: AlertRule,
    expr: Expression,
    last_eval_at: i64,
}

/// The evaluator task. Event-driven off the stats collector, holding all
/// lifecycle in memory.
async fn run(state: Arc<AppState>) {
    // Let the rest of boot finish before the first DB touch.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let repo = AlertRepository::new(state.db.clone());

    // Hydrate lifecycle so firing/pending survives a restart without
    // re-firing from Ok. Keyed rule_id → label_set → Live.
    let mut live: HashMap<i64, HashMap<String, Live>> = HashMap::new();
    match repo.list_all_state().await {
        Ok(rows) => {
            for r in rows {
                live.entry(r.rule_id)
                    .or_default()
                    .insert(r.label_set.clone(), Live::from_row(&r));
            }
            info!(
                "alert evaluator hydrated {} lifecycle row(s)",
                live.values().map(|m| m.len()).sum::<usize>()
            );
        }
        Err(e) => warn!(
            "alert evaluator: state hydration failed, starting cold: {:?}",
            e
        ),
    }

    let mut rules: HashMap<i64, Compiled> = HashMap::new();
    let mut last_reload = 0i64;

    let mut signal = state.stats_signal.subscribe();
    let mut fallback = tokio::time::interval(FALLBACK_TICK);
    fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Wake on a fresh stats tick or the fallback timer, whichever fires
        // first.
        tokio::select! {
            _ = signal.changed() => {}
            _ = fallback.tick() => {}
        }

        let now = Utc::now().timestamp();

        if now - last_reload >= RELOAD_SECS {
            reload_rules(&repo, &mut rules).await;
            last_reload = now;
            // Drop in-memory lifecycle for rules that no longer exist.
            live.retain(|rid, _| rules.contains_key(rid));
        }

        for compiled in rules.values_mut() {
            if now - compiled.last_eval_at < compiled.rule.eval_interval_secs.max(1) {
                continue;
            }
            compiled.last_eval_at = now;
            let rule_live = live.entry(compiled.rule.id).or_default();
            if let Err(e) = evaluate_rule(
                &state,
                &repo,
                &compiled.rule,
                &compiled.expr,
                rule_live,
                false,
                now,
            )
            .await
            {
                warn!("alert rule '{}' eval failed: {}", compiled.rule.name, e);
            }
        }
    }
}

/// Reload enabled rules, (re)compiling expressions. Removed/disabled rules
/// drop out; new or changed rules (re)compile and re-evaluate immediately.
/// A rule whose expression no longer parses is dropped with a warning.
async fn reload_rules(repo: &AlertRepository, rules: &mut HashMap<i64, Compiled>) {
    let loaded = match repo.list_enabled().await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "alert evaluator: rule reload failed, keeping current set: {:?}",
                e
            );
            return;
        }
    };
    let live_ids: HashSet<i64> = loaded.iter().map(|r| r.id).collect();
    rules.retain(|id, _| live_ids.contains(id));

    for rule in loaded {
        let unchanged = rules
            .get(&rule.id)
            .map(|c| c.rule.updated_at == rule.updated_at)
            .unwrap_or(false);
        if unchanged {
            continue;
        }
        match expression::parse(&rule.expression) {
            Ok(expr) => {
                info!(
                    "alert rule '{}' (id={}, severity={}, every {}s, for {}s) loaded",
                    rule.name,
                    rule.id,
                    rule.severity.as_str(),
                    rule.eval_interval_secs,
                    rule.for_duration_secs
                );
                rules.insert(
                    rule.id,
                    Compiled {
                        rule,
                        expr,
                        // Re-evaluate immediately on load/change.
                        last_eval_at: 0,
                    },
                );
            }
            Err(e) => {
                warn!(
                    "alert rule '{}' expression invalid: {}, skipping",
                    rule.name, e
                );
                rules.remove(&rule.id);
            }
        }
    }
}

/// Evaluate one rule against its current samples, mutating `live` (this
/// rule's per-label lifecycle) and performing side effects.
///
/// Persistence: a transition or any non-Ok result writes `alert_state` (the
/// latter keeps `GET /alerts/state`'s displayed value fresh); an unchanged Ok
/// row stays in memory only. A persist failure aborts that label's side
/// effects and leaves its in-memory state untouched, so the next tick retries
/// the same transition cleanly. `persist_all` forces every row to the DB —
/// the single-tick test driver [`evaluate_once`] sets it so tests can read the
/// result straight back from `alert_state`.
#[allow(clippy::too_many_arguments)]
async fn evaluate_rule(
    state: &Arc<AppState>,
    repo: &AlertRepository,
    rule: &AlertRule,
    expr: &Expression,
    live: &mut HashMap<String, Live>,
    persist_all: bool,
    now: i64,
) -> Result<(), String> {
    let samples = resolver::resolve_with_state(state, &expr.metric)
        .await
        .map_err(|e| e.message)?;

    let mut seen: HashSet<String> = HashSet::new();

    for sample in &samples {
        seen.insert(sample.label_set.clone());

        // NaN/inf poisons every comparator; treat as "no data this tick".
        // State is left alone and it stays in `seen` so the prune below
        // doesn't mistake it for a vanished target.
        if !sample.value.is_finite() {
            warn!(
                "alert rule '{}' label={}: non-finite sample ({}) skipped",
                rule.name, sample.label_set, sample.value
            );
            continue;
        }

        let prior = live
            .get(&sample.label_set)
            .cloned()
            .unwrap_or_else(|| Live::fresh(now));

        let violating = expr.comparator.evaluate(sample.value, expr.threshold);
        let step = transition(
            prior.state,
            prior.state_since,
            violating,
            rule.for_duration_secs,
            now,
        );

        let (event_type, notify_intent) = match (prior.state, step.state) {
            (AlertLifecycle::Pending, AlertLifecycle::Firing) => {
                let silenced = rule
                    .silenced_until
                    .map(|until| now < until)
                    .unwrap_or(false);
                let cooled_in = prior
                    .last_notified_at
                    .map(|last| now - last < rule.cooldown_secs)
                    .unwrap_or(false);
                if silenced {
                    debug!(
                        "alert rule '{}' label={} fire suppressed by silence (until {})",
                        rule.name,
                        sample.label_set,
                        rule.silenced_until.unwrap_or(0)
                    );
                    (Some(AlertEventType::Fired), false)
                } else if cooled_in {
                    debug!(
                        "alert rule '{}' label={} fire suppressed by cooldown",
                        rule.name, sample.label_set
                    );
                    (Some(AlertEventType::Fired), false)
                } else {
                    (Some(AlertEventType::Fired), true)
                }
            }
            (AlertLifecycle::Firing, AlertLifecycle::Ok) => (Some(AlertEventType::Resolved), true),
            _ => (None, false),
        };

        // Stamp `last_notified_at` only when we intend to notify AND a channel
        // would actually receive it — a fire with no channels must not arm
        // cooldown, or the rule looks rate-limited the moment a channel is
        // finally added.
        let new_last_notified_at = if notify_intent
            && state
                .notify
                .has_channel_for(alert_severity(rule.severity))
                .await
        {
            Some(now)
        } else {
            prior.last_notified_at
        };

        let next = Live {
            state: step.state,
            state_since: step.state_since,
            last_value: Some(sample.value),
            last_eval_at: now,
            last_notified_at: new_last_notified_at,
        };
        let transitioned = prior.state != step.state;

        let must_persist = persist_all || transitioned || next.state != AlertLifecycle::Ok;
        if must_persist
            && let Err(e) = repo
                .upsert_state(&next.to_row(rule.id, &sample.label_set))
                .await
        {
            warn!(
                "alert_state upsert failed for rule='{}' label={}: {:?} \
                 (skipping notify/event; will retry next tick)",
                rule.name, sample.label_set, e
            );
            continue;
        }
        live.insert(sample.label_set.clone(), next);

        // Flight recorder: freeze context on the first threshold crossing
        // (ok→pending) and on pending→firing (states restored mid-incident).
        // Spawned + cooldown-deduped in the incidents service.
        if matches!(
            (prior.state, step.state),
            (AlertLifecycle::Ok, AlertLifecycle::Pending)
                | (AlertLifecycle::Pending, AlertLifecycle::Firing)
        ) {
            crate::services::incidents::spawn_capture_for_alert(
                Arc::clone(state),
                rule.id,
                rule.name.clone(),
                sample.label_set.clone(),
                expr.metric.namespace.clone(),
                sample.value,
            );
        }

        if let Some(et) = event_type {
            let notified = if notify_intent {
                match et {
                    AlertEventType::Fired => {
                        fire_notify(
                            state,
                            rule,
                            &sample.label_set,
                            sample.value,
                            sample.meta.as_deref(),
                        )
                        .await
                    }
                    AlertEventType::Resolved => {
                        resolve_notify(
                            state,
                            rule,
                            &sample.label_set,
                            sample.value,
                            sample.meta.as_deref(),
                        )
                        .await
                    }
                }
            } else {
                false
            };
            if let Err(e) = repo
                .insert_event(
                    rule.id,
                    &sample.label_set,
                    et,
                    rule.severity,
                    Some(sample.value),
                    notified,
                )
                .await
            {
                warn!("alert_events insert failed: {:?}", e);
            }
        }
    }

    // Prune label_sets that vanished from resolver output. Ok/Pending go
    // quietly; a Firing row gets a synthetic resolve first — its target is
    // gone and nothing will flip it back. The state-guarded delete wins races
    // against a concurrent transition.
    let vanished: Vec<String> = live
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    for label_set in vanished {
        let Some(prior) = live.get(&label_set).cloned() else {
            continue;
        };
        match repo.delete_state_if(rule.id, &label_set, prior.state).await {
            Ok(true) if prior.state == AlertLifecycle::Firing => {
                let value = prior.last_value.unwrap_or(0.0);
                let notified =
                    resolve_notify(state, rule, &label_set, value, Some("target removed")).await;
                if let Err(e) = repo
                    .insert_event(
                        rule.id,
                        &label_set,
                        AlertEventType::Resolved,
                        rule.severity,
                        prior.last_value,
                        notified,
                    )
                    .await
                {
                    warn!("alert_events insert failed: {:?}", e);
                }
                live.remove(&label_set);
            }
            Ok(_) => {
                live.remove(&label_set);
            }
            Err(e) => warn!(
                "alert_state prune failed for rule='{}' label={}: {:?}",
                rule.name, label_set, e
            ),
        }
    }

    Ok(())
}

/// Single-tick DB-backed evaluation: loads prior state from `alert_state`,
/// evaluates, and persists every row. `pub(crate)` so the API-test tier can
/// drive one tick and read the result straight from the DB; production uses
/// the in-memory [`run`] loop, so this is test-only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn evaluate_once(
    rule: &AlertRule,
    expr: &Expression,
    state: &Arc<AppState>,
) -> Result<(), String> {
    let repo = AlertRepository::new(state.db.clone());
    let mut live: HashMap<String, Live> = repo
        .list_state_for_rule(rule.id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| (r.label_set.clone(), Live::from_row(&r)))
        .collect();
    evaluate_rule(
        state,
        &repo,
        rule,
        expr,
        &mut live,
        true,
        Utc::now().timestamp(),
    )
    .await
}

/// Test-only: one in-memory evaluation (`persist_all = false`) from the given
/// prior states, returning the resulting `(label_set, state)` pairs. Lets
/// tests assert the persist-selective policy — an unchanged Ok row must not
/// hit `alert_state`.
#[cfg(test)]
pub(crate) async fn evaluate_in_memory_once(
    rule: &AlertRule,
    expr: &Expression,
    state: &Arc<AppState>,
    prior: &[(&str, AlertLifecycle)],
    now: i64,
) -> Vec<(String, AlertLifecycle)> {
    let repo = AlertRepository::new(state.db.clone());
    let mut live: HashMap<String, Live> = prior
        .iter()
        .map(|(l, s)| {
            (
                l.to_string(),
                Live {
                    state: *s,
                    state_since: 0,
                    last_value: None,
                    last_eval_at: 0,
                    last_notified_at: None,
                },
            )
        })
        .collect();
    evaluate_rule(state, &repo, rule, expr, &mut live, false, now)
        .await
        .expect("evaluate_rule");
    let mut out: Vec<(String, AlertLifecycle)> =
        live.into_iter().map(|(l, v)| (l, v.state)).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Pure state-machine step. Inputs:
/// - `prior` lifecycle and `prior_since` timestamp
/// - `violating` (the current eval's verdict)
/// - `for_duration_secs` (sustained-condition window)
/// - `now`
///
/// Outputs the new lifecycle plus the timestamp to record as
/// `state_since` (preserved when not transitioning, refreshed when we
/// move).
fn transition(
    prior: AlertLifecycle,
    prior_since: i64,
    violating: bool,
    for_duration_secs: i64,
    now: i64,
) -> Step {
    use AlertLifecycle::*;
    match (prior, violating) {
        (Ok, false) => Step::keep(Ok, prior_since),
        (Ok, true) => Step::go(Pending, now),
        (Pending, false) => Step::go(Ok, now),
        (Pending, true) => {
            if now - prior_since >= for_duration_secs {
                Step::go(Firing, now)
            } else {
                Step::keep(Pending, prior_since)
            }
        }
        (Firing, true) => Step::keep(Firing, prior_since),
        (Firing, false) => Step::go(Ok, now),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    state: AlertLifecycle,
    state_since: i64,
}

impl Step {
    fn go(s: AlertLifecycle, ts: i64) -> Self {
        Self {
            state: s,
            state_since: ts,
        }
    }
    fn keep(s: AlertLifecycle, ts: i64) -> Self {
        Self {
            state: s,
            state_since: ts,
        }
    }
}

// ===== Notification helpers =====

async fn fire_notify(
    state: &AppState,
    rule: &AlertRule,
    label_set: &str,
    value: f64,
    meta: Option<&str>,
) -> bool {
    let n = Notification {
        title: titled(state, &rule.name).await,
        body: format_fire_body(rule, label_set, value, meta),
        severity: alert_severity(rule.severity),
        event: NotificationEvent::Fired,
    };
    state.notify.fanout(&n).await > 0
}

async fn resolve_notify(
    state: &AppState,
    rule: &AlertRule,
    label_set: &str,
    value: f64,
    meta: Option<&str>,
) -> bool {
    let n = Notification {
        title: titled(state, &rule.name).await,
        body: format_resolve_body(rule, label_set, value, meta),
        severity: alert_severity(rule.severity),
        event: NotificationEvent::Resolved,
    };
    state.notify.fanout(&n).await > 0
}

/// `[server_name] rule name` — lets one Telegram chat / ntfy topic receiving
/// alerts from several remon instances attribute each notification.
async fn titled(state: &AppState, rule_name: &str) -> String {
    let server_name = state.effective_config.read().await.server_name.clone();
    format!("[{}] {}", server_name, rule_name)
}

fn alert_severity(s: AlertSeverity) -> Severity {
    match s {
        AlertSeverity::Warn => Severity::Warn,
        AlertSeverity::Crit => Severity::Crit,
    }
}

fn render_observed(value: f64, meta: Option<&str>) -> String {
    match meta {
        Some(s) => format!("{} ({})", s, format_value(value)),
        None => format_value(value),
    }
}

fn format_fire_body(rule: &AlertRule, label_set: &str, value: f64, meta: Option<&str>) -> String {
    let labels = if label_set == "{}" {
        String::new()
    } else {
        format!(" {}", label_set)
    };
    let sustained = if rule.for_duration_secs > 0 {
        format!(", sustained {}s", rule.for_duration_secs)
    } else {
        String::new()
    };
    format!(
        "{}{} = {}{}",
        rule.expression,
        labels,
        render_observed(value, meta),
        sustained
    )
}

fn format_resolve_body(
    rule: &AlertRule,
    label_set: &str,
    value: f64,
    meta: Option<&str>,
) -> String {
    let labels = if label_set == "{}" {
        String::new()
    } else {
        format!(" {}", label_set)
    };
    format!(
        "{}{} back to {}",
        rule.expression,
        labels,
        render_observed(value, meta)
    )
}

fn format_value(v: f64) -> String {
    // Compact: integers as 42, fractions as 0.93. Avoids the noise of
    // "42.000000" while still being precise for small values.
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{:.4}", v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_ok_to_pending() {
        let s = transition(AlertLifecycle::Ok, 100, true, 30, 200);
        assert_eq!(s.state, AlertLifecycle::Pending);
        assert_eq!(s.state_since, 200);
    }

    #[test]
    fn state_machine_pending_holds_until_for_elapses() {
        // Pending since 100, for=30, now=120 → still pending, since stays at 100.
        let s = transition(AlertLifecycle::Pending, 100, true, 30, 120);
        assert_eq!(s.state, AlertLifecycle::Pending);
        assert_eq!(s.state_since, 100);
    }

    #[test]
    fn state_machine_pending_to_firing_at_for_boundary() {
        let s = transition(AlertLifecycle::Pending, 100, true, 30, 130);
        assert_eq!(s.state, AlertLifecycle::Firing);
        assert_eq!(s.state_since, 130);
    }

    #[test]
    fn state_machine_pending_to_ok_on_clear() {
        let s = transition(AlertLifecycle::Pending, 100, false, 30, 110);
        assert_eq!(s.state, AlertLifecycle::Ok);
    }

    #[test]
    fn state_machine_firing_holds_on_continued_violation() {
        let s = transition(AlertLifecycle::Firing, 100, true, 30, 200);
        assert_eq!(s.state, AlertLifecycle::Firing);
        // state_since preserved through firing — important for "firing
        // since" UI badges.
        assert_eq!(s.state_since, 100);
    }

    #[test]
    fn state_machine_firing_to_ok_on_clear() {
        let s = transition(AlertLifecycle::Firing, 100, false, 30, 200);
        assert_eq!(s.state, AlertLifecycle::Ok);
        assert_eq!(s.state_since, 200);
    }

    #[test]
    fn state_machine_ok_stays_ok() {
        let s = transition(AlertLifecycle::Ok, 100, false, 30, 200);
        assert_eq!(s.state, AlertLifecycle::Ok);
        assert_eq!(s.state_since, 100);
    }

    #[test]
    fn for_duration_zero_fires_immediately() {
        // for=0 means "fire on first violation" — pending should
        // transition to firing on the very next eval if violation persists.
        // Since now == prior_since (same eval that flipped to pending),
        // 0 - 0 >= 0 — but the typical use is: prior_state=Pending,
        // prior_since=earlier-tick. With for=0, every Pending+violating
        // converts to Firing immediately.
        let s = transition(AlertLifecycle::Pending, 100, true, 0, 100);
        assert_eq!(s.state, AlertLifecycle::Firing);
    }

    #[test]
    fn format_value_integer_compact() {
        assert_eq!(format_value(80.0), "80");
        assert_eq!(format_value(80.5), "80.5000");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(-1.5), "-1.5000");
    }
}
