//! Alert engine repository — rules + state + events.
//!
//! Three tables, three responsibilities:
//! - `alert_rules`  — operator-authored definitions. CRUD-shaped.
//! - `alert_state`  — current lifecycle per (rule, label_set). The
//!   evaluator upserts on every tick; `/alerts/state` REST reads it.
//! - `alert_events` — append-only audit log of fire / resolve
//!   transitions. `/alerts/events` paginates over it.

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::alert::{
    AlertEvent, AlertEventType, AlertLifecycle, AlertRule, AlertSeverity, AlertStateRow,
};

pub struct AlertRepository {
    pool: SqlitePool,
}

/// Body shape for `POST /alerts` and `PUT /alerts/{id}`. Domain-side
/// representation; REST DTOs map onto this.
#[derive(Debug, Clone)]
pub struct UpsertAlertRule {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub expression: String,
    pub severity: AlertSeverity,
    pub for_duration_secs: i64,
    pub eval_interval_secs: i64,
    pub cooldown_secs: i64,
    pub silenced_until: Option<i64>,
}

impl AlertRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ===== alert_rules =====

    pub async fn list(&self) -> AppResult<Vec<AlertRule>> {
        let rows = sqlx::query_as!(
            AlertRuleRow,
            r#"SELECT id, name, description, enabled as "enabled: bool", expression, severity,
                    for_duration_secs, eval_interval_secs, cooldown_secs,
                    silenced_until, created_at, updated_at
               FROM alert_rules
              ORDER BY id ASC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(AlertRuleRow::decode).collect())
    }

    pub async fn list_enabled(&self) -> AppResult<Vec<AlertRule>> {
        let rows = sqlx::query_as!(
            AlertRuleRow,
            r#"SELECT id, name, description, enabled as "enabled: bool", expression, severity,
                    for_duration_secs, eval_interval_secs, cooldown_secs,
                    silenced_until, created_at, updated_at
               FROM alert_rules
              WHERE enabled = 1
              ORDER BY id ASC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(AlertRuleRow::decode).collect())
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<AlertRule>> {
        let row = sqlx::query_as!(
            AlertRuleRow,
            r#"SELECT id, name, description, enabled as "enabled: bool", expression, severity,
                    for_duration_secs, eval_interval_secs, cooldown_secs,
                    silenced_until, created_at, updated_at
               FROM alert_rules
              WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(AlertRuleRow::decode))
    }

    pub async fn insert(&self, rule: &UpsertAlertRule) -> AppResult<i64> {
        let r = sqlx::query!(
            "INSERT INTO alert_rules
                (name, description, enabled, expression, severity,
                 for_duration_secs, eval_interval_secs, cooldown_secs,
                 silenced_until, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, unixepoch(), unixepoch())",
            rule.name,
            rule.description,
            rule.enabled,
            rule.expression,
            rule.severity.as_str(),
            rule.for_duration_secs,
            rule.eval_interval_secs,
            rule.cooldown_secs,
            rule.silenced_until,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn update(&self, id: i64, rule: &UpsertAlertRule) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE alert_rules SET
                name               = ?,
                description        = ?,
                enabled            = ?,
                expression         = ?,
                severity           = ?,
                for_duration_secs  = ?,
                eval_interval_secs = ?,
                cooldown_secs      = ?,
                silenced_until     = ?,
                updated_at         = unixepoch()
              WHERE id = ?",
            rule.name,
            rule.description,
            rule.enabled,
            rule.expression,
            rule.severity.as_str(),
            rule.for_duration_secs,
            rule.eval_interval_secs,
            rule.cooldown_secs,
            rule.silenced_until,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Targeted silence write. Used by `POST/DELETE /alerts/{id}/silence`
    /// so a silence toggle doesn't have to round-trip the full rule body
    /// and can't accidentally stomp a concurrent PATCH.
    pub async fn set_silence(&self, id: i64, until: Option<i64>) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE alert_rules
                SET silenced_until = ?,
                    updated_at     = unixepoch()
              WHERE id = ?",
            until,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete(&self, id: i64) -> AppResult<bool> {
        let r = sqlx::query!("DELETE FROM alert_rules WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    // ===== alert_state =====

    /// Remove a state row for a label_set that disappeared from resolver
    /// output — but only if it still holds `expected` state, so a row that
    /// transitioned between the caller's snapshot and this delete survives.
    /// Returns whether a row was actually deleted; the evaluator keys its
    /// synthetic-resolve side effects off that.
    pub async fn delete_state_if(
        &self,
        rule_id: i64,
        label_set: &str,
        expected: AlertLifecycle,
    ) -> AppResult<bool> {
        let expected = expected.as_str();
        let r = sqlx::query!(
            "DELETE FROM alert_state
              WHERE rule_id = ? AND label_set = ? AND state = ?",
            rule_id,
            label_set,
            expected,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Upsert the current lifecycle for one (rule, label_set). The
    /// evaluator calls this on every tick. Idempotent — re-applies the
    /// same state with refreshed last_value / last_eval_at fine.
    pub async fn upsert_state(&self, st: &AlertStateRow) -> AppResult<()> {
        sqlx::query!(
            "INSERT INTO alert_state
                (rule_id, label_set, state, state_since,
                 last_value, last_eval_at, last_notified_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(rule_id, label_set) DO UPDATE SET
                 state            = excluded.state,
                 state_since      = excluded.state_since,
                 last_value       = excluded.last_value,
                 last_eval_at     = excluded.last_eval_at,
                 last_notified_at = COALESCE(excluded.last_notified_at, alert_state.last_notified_at)",
            st.rule_id,
            st.label_set,
            st.state.as_str(),
            st.state_since,
            st.last_value,
            st.last_eval_at,
            st.last_notified_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All persisted lifecycle rows — the event-driven evaluator's startup
    /// hydration source, so firing/pending state survives a restart without
    /// re-firing from Ok.
    pub async fn list_all_state(&self) -> AppResult<Vec<AlertStateRow>> {
        let rows = sqlx::query_as!(
            AlertStateRawRow,
            "SELECT rule_id, label_set, state, state_since,
                    last_value, last_eval_at, last_notified_at
               FROM alert_state"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(AlertStateRawRow::decode)
            .collect())
    }

    /// Prior state for one rule. Used by the single-tick test driver
    /// [`crate::services::alerting::evaluator::evaluate_once`] and heartbeat
    /// tests; production hydrates once via `list_all_state` and keeps state in
    /// memory.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn list_state_for_rule(&self, rule_id: i64) -> AppResult<Vec<AlertStateRow>> {
        let rows = sqlx::query_as!(
            AlertStateRawRow,
            "SELECT rule_id, label_set, state, state_since,
                    last_value, last_eval_at, last_notified_at
               FROM alert_state
              WHERE rule_id = ?",
            rule_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(AlertStateRawRow::decode)
            .collect())
    }

    /// Active (`pending`/`firing`) state rows for *enabled* rules, oldest
    /// first. See `count_active_state` for why disabled rules are excluded.
    pub async fn list_active_state(
        &self,
    ) -> AppResult<Vec<(AlertStateRow, String, AlertSeverity)>> {
        let rows = sqlx::query_as!(
            ActiveStateJoinRow,
            r#"SELECT s.rule_id as "rule_id!", s.label_set as "label_set!",
                    s.state as "state!", s.state_since as "state_since!",
                    s.last_value, s.last_eval_at as "last_eval_at!", s.last_notified_at,
                    r.name as "name!", r.severity as "severity!"
               FROM alert_state s
               INNER JOIN alert_rules r ON r.id = s.rule_id
              WHERE s.state IN ('pending','firing')
                AND r.enabled = 1
              ORDER BY s.state_since ASC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(ActiveStateJoinRow::decode)
            .collect())
    }

    /// Count active state rows per lifecycle — `(pending, firing)`. Backs
    /// the lightweight `GET /summary` endpoint so a fleet view can show an
    /// alert badge without pulling the full active-state list.
    ///
    /// Joined and filtered on `enabled` to stay consistent with
    /// `list_active_state`: disabling a rule stops the evaluator from touching
    /// its `alert_state` row but does not clear it, so a rule disabled while
    /// firing would otherwise keep its count in the badge with nothing in the
    /// UI able to clear it.
    pub async fn count_active_state(&self) -> AppResult<(u32, u32)> {
        let row = sqlx::query!(
            r#"SELECT
                 COALESCE(SUM(s.state = 'pending'), 0) as "pending!: i64",
                 COALESCE(SUM(s.state = 'firing'), 0) as "firing!: i64"
               FROM alert_state s
               INNER JOIN alert_rules r ON r.id = s.rule_id
              WHERE r.enabled = 1"#
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((row.pending as u32, row.firing as u32))
    }

    // ===== alert_events =====

    pub async fn insert_event(
        &self,
        rule_id: i64,
        label_set: &str,
        event_type: AlertEventType,
        severity: AlertSeverity,
        metric_value: Option<f64>,
        notified: bool,
    ) -> AppResult<i64> {
        let r = sqlx::query!(
            "INSERT INTO alert_events
                (rule_id, label_set, event_type, severity,
                 occurred_at, metric_value, notified)
             VALUES (?, ?, ?, ?, unixepoch(), ?, ?)",
            rule_id,
            label_set,
            event_type.as_str(),
            severity.as_str(),
            metric_value,
            notified,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Recent events for a single rule, newest first. `offset` skips that
    /// many rows after sorting, enabling client-side pagination.
    pub async fn events_for_rule(
        &self,
        rule_id: i64,
        limit: u32,
        offset: u32,
    ) -> AppResult<Vec<AlertEvent>> {
        let limit = limit as i64;
        let offset = offset as i64;
        let rows = sqlx::query_as!(
            AlertEventRow,
            r#"SELECT id, rule_id, label_set, event_type, severity,
                    occurred_at, metric_value, notified as "notified: bool"
               FROM alert_events
              WHERE rule_id = ?
              ORDER BY occurred_at DESC, id DESC
              LIMIT ? OFFSET ?"#,
            rule_id,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(AlertEventRow::decode).collect())
    }

    /// Range slice for the `GET /events` union timeline: fire/resolve rows
    /// joined with their rule's name and newest first. Inner join is safe —
    /// `alert_events.rule_id` cascades on rule deletion, so an event's rule
    /// always exists.
    pub async fn events_in_range(
        &self,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<AlertEventWithRule>> {
        let rows = sqlx::query!(
            r#"SELECT e.occurred_at as "occurred_at!", e.event_type as "event_type!",
                      e.severity as "severity!", e.rule_id as "rule_id!",
                      r.name as "rule_name!", e.label_set as "label_set!",
                      e.metric_value
               FROM alert_events e
               JOIN alert_rules r ON r.id = e.rule_id
              WHERE e.occurred_at >= ? AND e.occurred_at <= ?
              ORDER BY e.occurred_at DESC, e.id DESC
              LIMIT ?"#,
            start,
            end,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AlertEventWithRule {
                occurred_at: r.occurred_at,
                event_type: r.event_type,
                severity: r.severity,
                rule_id: r.rule_id,
                rule_name: r.rule_name,
                label_set: r.label_set,
                metric_value: r.metric_value,
            })
            .collect())
    }

    /// Cross-rule recent events, newest first. See `events_for_rule` for the
    /// offset semantics.
    pub async fn recent_events(&self, limit: u32, offset: u32) -> AppResult<Vec<AlertEvent>> {
        let limit = limit as i64;
        let offset = offset as i64;
        let rows = sqlx::query_as!(
            AlertEventRow,
            r#"SELECT id, rule_id, label_set, event_type, severity,
                    occurred_at, metric_value, notified as "notified: bool"
               FROM alert_events
              ORDER BY occurred_at DESC, id DESC
              LIMIT ? OFFSET ?"#,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(AlertEventRow::decode).collect())
    }
}

/// Read-side shape for the `GET /events` union: one alert transition plus
/// the rule fields the timeline renders. Strings stay raw (no enum decode) —
/// the union endpoint passes them through.
#[derive(Debug, Clone)]
pub struct AlertEventWithRule {
    pub occurred_at: i64,
    pub event_type: String,
    pub severity: String,
    pub rule_id: i64,
    pub rule_name: String,
    pub label_set: String,
    pub metric_value: Option<f64>,
}

// ===== private row types =====

#[derive(sqlx::FromRow)]
struct AlertRuleRow {
    id: i64,
    name: String,
    description: Option<String>,
    enabled: bool,
    expression: String,
    severity: String,
    for_duration_secs: i64,
    eval_interval_secs: i64,
    cooldown_secs: i64,
    silenced_until: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl AlertRuleRow {
    fn decode(self) -> Option<AlertRule> {
        Some(AlertRule {
            id: self.id,
            name: self.name,
            description: self.description,
            enabled: self.enabled,
            expression: self.expression,
            severity: AlertSeverity::parse(&self.severity)?,
            for_duration_secs: self.for_duration_secs,
            eval_interval_secs: self.eval_interval_secs,
            cooldown_secs: self.cooldown_secs,
            silenced_until: self.silenced_until,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AlertStateRawRow {
    rule_id: i64,
    label_set: String,
    state: String,
    state_since: i64,
    last_value: Option<f64>,
    last_eval_at: i64,
    last_notified_at: Option<i64>,
}

impl AlertStateRawRow {
    fn decode(self) -> Option<AlertStateRow> {
        Some(AlertStateRow {
            rule_id: self.rule_id,
            label_set: self.label_set,
            state: AlertLifecycle::parse(&self.state)?,
            state_since: self.state_since,
            last_value: self.last_value,
            last_eval_at: self.last_eval_at,
            last_notified_at: self.last_notified_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ActiveStateJoinRow {
    rule_id: i64,
    label_set: String,
    state: String,
    state_since: i64,
    last_value: Option<f64>,
    last_eval_at: i64,
    last_notified_at: Option<i64>,
    name: String,
    severity: String,
}

impl ActiveStateJoinRow {
    fn decode(self) -> Option<(AlertStateRow, String, AlertSeverity)> {
        let lifecycle = AlertLifecycle::parse(&self.state)?;
        let severity = AlertSeverity::parse(&self.severity)?;
        Some((
            AlertStateRow {
                rule_id: self.rule_id,
                label_set: self.label_set,
                state: lifecycle,
                state_since: self.state_since,
                last_value: self.last_value,
                last_eval_at: self.last_eval_at,
                last_notified_at: self.last_notified_at,
            },
            self.name,
            severity,
        ))
    }
}

#[derive(sqlx::FromRow)]
struct AlertEventRow {
    id: i64,
    rule_id: i64,
    label_set: String,
    event_type: String,
    severity: String,
    occurred_at: i64,
    metric_value: Option<f64>,
    notified: bool,
}

impl AlertEventRow {
    fn decode(self) -> Option<AlertEvent> {
        Some(AlertEvent {
            id: self.id,
            rule_id: self.rule_id,
            label_set: self.label_set,
            event_type: AlertEventType::parse(&self.event_type)?,
            severity: AlertSeverity::parse(&self.severity)?,
            occurred_at: self.occurred_at,
            metric_value: self.metric_value,
            notified: self.notified,
        })
    }
}
