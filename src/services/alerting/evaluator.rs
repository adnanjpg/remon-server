//! Per-rule evaluator. One tokio task per enabled rule; each task ticks
//! at `eval_interval_secs`, resolves the metric, runs the comparator
//! per label_set, advances the lifecycle, and emits fire/resolve events.
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
//! Pending rows silently (pending → ok is silent by design), Firing
//! rows with a synthetic resolved event + notification ("target
//! removed") — otherwise a deleted/renamed target would strand its
//! Firing row forever, since nothing re-evaluates a label_set that
//! stops appearing.

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

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move { run_supervisor(state).await });
}

/// Top-level supervisor loop. Runs every `RELOAD_INTERVAL`, loads all
/// enabled rules from the DB, and diffs against the currently-running
/// task set. Rules added or changed (detected via `updated_at`) get a
/// fresh task; rules disabled or deleted have their task aborted.
///
/// This makes `PUT /alerts/{id}` take effect within one reload cycle
/// without a server restart.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

async fn run_supervisor(state: Arc<AppState>) {
    // Brief delay so the rest of boot finishes before we slam the DB.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let repo = AlertRepository::new(state.db.clone());
    // rule_id → (task handle, updated_at snapshot used for change detection)
    let mut running: HashMap<i64, (tokio::task::JoinHandle<()>, i64)> = HashMap::new();

    let mut ticker = tokio::time::interval(RELOAD_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let rules = match repo.list_enabled().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Alert supervisor: reload failed, will retry: {:?}", e);
                continue;
            }
        };

        // Abort tasks whose rule was disabled or deleted.
        let live_ids: HashSet<i64> = rules.iter().map(|r| r.id).collect();
        let removed: Vec<i64> = running
            .keys()
            .filter(|id| !live_ids.contains(id))
            .cloned()
            .collect();
        for id in removed {
            if let Some((task, _)) = running.remove(&id) {
                info!(
                    "Alert supervisor: rule id={} removed or disabled, stopping task",
                    id
                );
                task.abort();
            }
        }

        // Spawn or restart rules that are new or have changed.
        for rule in rules {
            let needs_start = match running.get(&rule.id) {
                None => true,
                Some((task, prev_updated_at)) => {
                    task.is_finished() || *prev_updated_at != rule.updated_at
                }
            };

            if needs_start {
                if let Some((task, _)) = running.remove(&rule.id) {
                    info!(
                        "Alert supervisor: rule '{}' (id={}) changed, restarting task",
                        rule.name, rule.id
                    );
                    task.abort();
                } else {
                    info!(
                        "Alert supervisor: rule '{}' (id={}) starting task",
                        rule.name, rule.id
                    );
                }
                let rule_id = rule.id;
                let updated_at = rule.updated_at;
                let st = state.clone();
                let task = tokio::spawn(async move { run_rule_loop(rule, st).await });
                running.insert(rule_id, (task, updated_at));
            }
        }
    }
}

/// Drive one rule. Sleeps `eval_interval_secs`, evaluates, repeats.
async fn run_rule_loop(rule: AlertRule, state: Arc<AppState>) {
    // Parse once; if the expression doesn't validate, log and exit —
    // the operator needs to fix and restart. We don't poll a broken
    // rule on every tick.
    let parsed = match expression::parse(&rule.expression) {
        Ok(e) => e,
        Err(e) => {
            warn!(
                "Alert rule '{}' expression invalid: {} — task exiting",
                rule.name, e
            );
            return;
        }
    };

    info!(
        "Alert rule '{}' (id={}, severity={}, every {}s, for {}s) running",
        rule.name,
        rule.id,
        rule.severity.as_str(),
        rule.eval_interval_secs,
        rule.for_duration_secs
    );

    let period = Duration::from_secs(rule.eval_interval_secs.max(1) as u64);
    // Wall-clock-aligned ticker. First tick fires immediately, which is
    // fine — a fresh task evaluating once on tick 0 just sees current
    // metrics, no harm done.
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if let Err(e) = evaluate_once(&rule, &parsed, &state).await {
            warn!("Alert rule '{}' eval failed: {}", rule.name, e);
        }
    }
}

/// One eval pass: resolve, walk samples, transition state, emit events.
/// `pub(crate)` so the API-test tier can drive single ticks without the
/// supervisor's timing.
pub(crate) async fn evaluate_once(
    rule: &AlertRule,
    expr: &Expression,
    state: &AppState,
) -> Result<(), String> {
    let repo = AlertRepository::new(state.db.clone());

    let samples = resolver::resolve_with_state(state, &expr.metric)
        .await
        .map_err(|e| e.message)?;

    // For each currently-observed sample, run the comparator and
    // advance the lifecycle. We collect the set of label_sets we saw
    // this tick so callers/UI know which states were touched.
    let now = Utc::now().timestamp();
    let mut seen: HashSet<String> = HashSet::new();

    // Snapshot existing state rows for this rule so we can compare
    // observed-vs-prior in one DB roundtrip.
    let prior_states = repo
        .list_state_for_rule(rule.id)
        .await
        .map_err(|e| e.to_string())?;
    let prior_by_label: std::collections::HashMap<String, AlertStateRow> = prior_states
        .into_iter()
        .map(|s| (s.label_set.clone(), s))
        .collect();

    for sample in &samples {
        seen.insert(sample.label_set.clone());

        // NaN/inf poisons every comparator (ordered ones read false, `!=`
        // reads true), silently neutralizing or spuriously firing the
        // rule. Treat the sample as "no data this tick": state is left
        // alone, and it stays in `seen` so the prune below doesn't
        // mistake it for a vanished target.
        if !sample.value.is_finite() {
            warn!(
                "Alert rule '{}' label={}: non-finite sample ({}) skipped",
                rule.name, sample.label_set, sample.value
            );
            continue;
        }

        let violating = expr.comparator.evaluate(sample.value, expr.threshold);

        let prior_state = prior_by_label
            .get(&sample.label_set)
            .map(|s| s.state)
            .unwrap_or(AlertLifecycle::Ok);
        let prior_state_since = prior_by_label
            .get(&sample.label_set)
            .map(|s| s.state_since)
            .unwrap_or(now);
        let prior_last_notified = prior_by_label
            .get(&sample.label_set)
            .and_then(|s| s.last_notified_at);

        let next = transition(
            prior_state,
            prior_state_since,
            violating,
            rule.for_duration_secs,
            now,
        );

        // Determine transition intent; state row is committed before side effects
        // to prevent re-firing on a transient DB error.
        let (event_type, notify_intent) = match (prior_state, next.state) {
            (AlertLifecycle::Pending, AlertLifecycle::Firing) => {
                let silenced = rule
                    .silenced_until
                    .map(|until| now < until)
                    .unwrap_or(false);
                let cooled_in = prior_last_notified
                    .map(|last| now - last < rule.cooldown_secs)
                    .unwrap_or(false);
                if silenced {
                    // Silence-suppressed: leave `last_notified_at` untouched so the
                    // next genuine fire after the window ends isn't also gated by
                    // cooldown. Event row still gets recorded with notified=false.
                    debug!(
                        "Alert rule '{}' label={} fire suppressed by silence (until {})",
                        rule.name,
                        sample.label_set,
                        rule.silenced_until.unwrap_or(0)
                    );
                    (Some(AlertEventType::Fired), false)
                } else if cooled_in {
                    debug!(
                        "Alert rule '{}' label={} fire suppressed by cooldown",
                        rule.name, sample.label_set
                    );
                    (Some(AlertEventType::Fired), false)
                } else {
                    (Some(AlertEventType::Fired), true)
                }
            }
            (AlertLifecycle::Firing, AlertLifecycle::Ok) => {
                // Resolves always notify — recovery is more useful than
                // spam-protected here, and silence does NOT gate recovery.
                (Some(AlertEventType::Resolved), true)
            }
            _ => (None, false),
        };

        // Stamp `last_notified_at = now` when we intend to notify AND at
        // least one channel would actually receive it. If a channel exists
        // but the fanout below fails (timeout, upstream down) cooldown still
        // arms — "we tried, don't retry every tick". But a fire with no
        // channels configured must not silently arm cooldown, or the rule
        // would look rate-limited the instant a channel is finally added.
        let new_last_notified_at = if notify_intent
            && state
                .notify
                .has_channel_for(alert_severity(rule.severity))
                .await
        {
            Some(now)
        } else {
            prior_last_notified
        };

        let row = AlertStateRow {
            rule_id: rule.id,
            label_set: sample.label_set.clone(),
            state: next.state,
            state_since: next.state_since,
            last_value: Some(sample.value),
            last_eval_at: now,
            last_notified_at: new_last_notified_at,
        };
        if let Err(e) = repo.upsert_state(&row).await {
            // State didn't persist — abort all side effects so the next
            // tick can re-attempt the same transition cleanly.
            warn!(
                "alert_state upsert failed for rule='{}' label={}: {:?} \
                 (skipping notify/event; will retry next tick)",
                rule.name, sample.label_set, e
            );
            continue;
        }

        // State is durable. Now the best-effort side effects.
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

    // Prune state rows whose label_set vanished from resolver output.
    // Ok and Pending go quietly; a Firing row gets a synthetic resolve
    // first — its target is gone (check deleted, mount unmounted, probe
    // data aged out) and nothing will ever flip it back otherwise. The
    // state-guarded delete wins races: if the row transitioned between
    // snapshot and delete, we skip side effects and re-examine next tick.
    for (label_set, prior) in &prior_by_label {
        if seen.contains(label_set) {
            continue;
        }
        match repo.delete_state_if(rule.id, label_set, prior.state).await {
            Ok(true) if prior.state == AlertLifecycle::Firing => {
                let value = prior.last_value.unwrap_or(0.0);
                let notified =
                    resolve_notify(state, rule, label_set, value, Some("target removed")).await;
                if let Err(e) = repo
                    .insert_event(
                        rule.id,
                        label_set,
                        AlertEventType::Resolved,
                        rule.severity,
                        prior.last_value,
                        notified,
                    )
                    .await
                {
                    warn!("alert_events insert failed: {:?}", e);
                }
            }
            Ok(_) => {}
            Err(e) => warn!(
                "alert_state prune failed for rule='{}' label={}: {:?}",
                rule.name, label_set, e
            ),
        }
    }

    Ok(())
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
        title: rule.name.clone(),
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
        title: rule.name.clone(),
        body: format_resolve_body(rule, label_set, value, meta),
        severity: alert_severity(rule.severity),
        event: NotificationEvent::Resolved,
    };
    state.notify.fanout(&n).await > 0
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
