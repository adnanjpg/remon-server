//! Alert engine v2 REST DTOs.

use serde::{Deserialize, Serialize};

use crate::models::alert::{
    AlertEvent, AlertEventType, AlertLifecycle, AlertRule, AlertSeverity, AlertStateRow,
};

// ===== Rule CRUD =====

#[derive(Debug, Serialize)]
pub struct AlertRuleDto {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub expression: String,
    pub severity: AlertSeverity,
    pub for_duration_secs: i64,
    pub eval_interval_secs: i64,
    pub cooldown_secs: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<AlertRule> for AlertRuleDto {
    fn from(r: AlertRule) -> Self {
        Self {
            id: r.id,
            name: r.name,
            description: r.description,
            enabled: r.enabled,
            expression: r.expression,
            severity: r.severity,
            for_duration_secs: r.for_duration_secs,
            eval_interval_secs: r.eval_interval_secs,
            cooldown_secs: r.cooldown_secs,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListAlertRulesResponse {
    pub rules: Vec<AlertRuleDto>,
}

#[derive(Debug, Deserialize)]
pub struct CreateAlertRuleRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub expression: String,
    pub severity: AlertSeverity,
    #[serde(default = "default_for")]
    pub for_duration_secs: i64,
    #[serde(default = "default_eval")]
    pub eval_interval_secs: i64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: i64,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAlertRuleRequest {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub expression: Option<String>,
    pub severity: Option<AlertSeverity>,
    pub for_duration_secs: Option<i64>,
    pub eval_interval_secs: Option<i64>,
    pub cooldown_secs: Option<i64>,
}

fn default_true() -> bool {
    true
}
fn default_for() -> i64 {
    30
}
fn default_eval() -> i64 {
    10
}
fn default_cooldown() -> i64 {
    900
}

// ===== Active state ("what's currently firing?") =====

#[derive(Debug, Serialize)]
pub struct AlertStateDto {
    pub rule_id: i64,
    pub rule_name: String,
    pub severity: AlertSeverity,
    /// Canonical labels JSON. UI parses for display.
    pub label_set: String,
    pub state: AlertLifecycle,
    pub state_since: i64,
    pub last_value: Option<f64>,
    pub last_eval_at: i64,
    pub last_notified_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ListAlertStateResponse {
    pub states: Vec<AlertStateDto>,
}

// ===== Event log =====

#[derive(Debug, Serialize)]
pub struct AlertEventDto {
    pub id: i64,
    pub rule_id: i64,
    pub label_set: String,
    pub event_type: AlertEventType,
    pub severity: AlertSeverity,
    pub occurred_at: i64,
    pub metric_value: Option<f64>,
    pub notified: bool,
}

impl From<AlertEvent> for AlertEventDto {
    fn from(e: AlertEvent) -> Self {
        Self {
            id: e.id,
            rule_id: e.rule_id,
            label_set: e.label_set,
            event_type: e.event_type,
            severity: e.severity,
            occurred_at: e.occurred_at,
            metric_value: e.metric_value,
            notified: e.notified,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListAlertEventsResponse {
    pub events: Vec<AlertEventDto>,
}

// ===== Helpers (used by REST handlers to build state DTOs joined with rule) =====

pub fn state_dto_from(state: AlertStateRow, rule: &AlertRule) -> AlertStateDto {
    AlertStateDto {
        rule_id: state.rule_id,
        rule_name: rule.name.clone(),
        severity: rule.severity,
        label_set: state.label_set,
        state: state.state,
        state_since: state.state_since,
        last_value: state.last_value,
        last_eval_at: state.last_eval_at,
        last_notified_at: state.last_notified_at,
    }
}
