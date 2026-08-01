//! Alert engine domain types.
//!
//! Three concepts to keep separate:
//! - `AlertRule` — the operator-defined rule (expression, severity,
//!   timing). Lives in `alert_rules`.
//! - `AlertLifecycle` — current state of one rule + label_set
//!   combination (ok / pending / firing). Lives in `alert_state`.
//! - `AlertEvent` — append-only audit row at every transition between
//!   lifecycle states. Lives in `alert_events`.

use serde::{Deserialize, Serialize};

/// Two-tier severity. `warn` and `crit` are the only legal values; the
/// `info` tier (a label-only "FYI" rule) is intentionally not included
/// — every fire fans out to FCM, so the meaningful split is "needs
/// attention" vs "needs immediate attention". A future `info` tier
/// would require a notification opt-out flag and is best-deferred until
/// there's a concrete request for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Warn,
    Crit,
}

impl AlertSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertSeverity::Warn => "warn",
            AlertSeverity::Crit => "crit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "warn" => Some(AlertSeverity::Warn),
            "crit" => Some(AlertSeverity::Crit),
            _ => None,
        }
    }
}

/// Lifecycle state of one (rule, label_set) pair.
///
/// Transitions:
///   ok      → pending  (first violation seen)
///   pending → ok       (violation cleared before `for` elapsed)
///   pending → firing   (violation persisted past `for_duration_secs`)
///   firing  → ok       (violation cleared)
///
/// `pending` is invisible to operators — it exists to debounce one-tick
/// hiccups. A monitor that never reaches `firing` never alerts, never
/// shows up in "active alerts" UI, and never writes an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLifecycle {
    Ok,
    Pending,
    Firing,
}

impl AlertLifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertLifecycle::Ok => "ok",
            AlertLifecycle::Pending => "pending",
            AlertLifecycle::Firing => "firing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(AlertLifecycle::Ok),
            "pending" => Some(AlertLifecycle::Pending),
            "firing" => Some(AlertLifecycle::Firing),
            _ => None,
        }
    }
}

/// Row mirror of `alert_rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub expression: String,
    pub severity: AlertSeverity,
    pub for_duration_secs: i64,
    pub eval_interval_secs: i64,
    pub cooldown_secs: i64,
    /// When set, suppresses Fired notification fanouts until `now >= silenced_until`.
    /// State transitions and event history continue. Resolved notifications are
    /// never gated by silence.
    pub silenced_until: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Live state row. `label_set` is canonical JSON (sorted keys) so the
/// PRIMARY KEY (rule_id, label_set) collapses equivalent label sets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertStateRow {
    pub rule_id: i64,
    pub label_set: String,
    pub state: AlertLifecycle,
    pub state_since: i64,
    pub last_value: Option<f64>,
    pub last_eval_at: i64,
    pub last_notified_at: Option<i64>,
}

/// One row in the audit log — appended on every transition that
/// produces a public-facing event (fire on `pending → firing`, resolve
/// on `firing → ok`). `pending → ok` is silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertEvent {
    pub id: i64,
    /// `None` once the rule has been deleted; the event outlives it.
    pub rule_id: Option<i64>,
    /// Denormalized at insert, so the event still says what it was about.
    pub rule_name: String,
    pub label_set: String,
    pub event_type: AlertEventType,
    pub severity: AlertSeverity,
    pub occurred_at: i64,
    pub metric_value: Option<f64>,
    pub notified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertEventType {
    Fired,
    Resolved,
}

impl AlertEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertEventType::Fired => "fired",
            AlertEventType::Resolved => "resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fired" => Some(AlertEventType::Fired),
            "resolved" => Some(AlertEventType::Resolved),
            _ => None,
        }
    }
}
