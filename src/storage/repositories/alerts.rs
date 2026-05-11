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
}

impl AlertRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ===== alert_rules =====

    pub async fn list(&self) -> AppResult<Vec<AlertRule>> {
        let rows: Vec<(
            i64,
            String,
            Option<String>,
            i64,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
        )> = sqlx::query_as(
            r#"
            SELECT id, name, description, enabled, expression, severity,
                   for_duration_secs, eval_interval_secs, cooldown_secs,
                   created_at, updated_at
              FROM alert_rules
             ORDER BY id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(decode_rule).collect())
    }

    pub async fn list_enabled(&self) -> AppResult<Vec<AlertRule>> {
        let rows: Vec<(
            i64,
            String,
            Option<String>,
            i64,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
        )> = sqlx::query_as(
            r#"
            SELECT id, name, description, enabled, expression, severity,
                   for_duration_secs, eval_interval_secs, cooldown_secs,
                   created_at, updated_at
              FROM alert_rules
             WHERE enabled = 1
             ORDER BY id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(decode_rule).collect())
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<AlertRule>> {
        let row: Option<(
            i64,
            String,
            Option<String>,
            i64,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
        )> = sqlx::query_as(
            r#"
            SELECT id, name, description, enabled, expression, severity,
                   for_duration_secs, eval_interval_secs, cooldown_secs,
                   created_at, updated_at
              FROM alert_rules
             WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(decode_rule))
    }

    pub async fn insert(&self, rule: &UpsertAlertRule) -> AppResult<i64> {
        let r = sqlx::query(
            r#"
            INSERT INTO alert_rules
                (name, description, enabled, expression, severity,
                 for_duration_secs, eval_interval_secs, cooldown_secs,
                 created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, unixepoch(), unixepoch())
            "#,
        )
        .bind(&rule.name)
        .bind(rule.description.as_deref())
        .bind(rule.enabled as i64)
        .bind(&rule.expression)
        .bind(rule.severity.as_str())
        .bind(rule.for_duration_secs)
        .bind(rule.eval_interval_secs)
        .bind(rule.cooldown_secs)
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn update(&self, id: i64, rule: &UpsertAlertRule) -> AppResult<bool> {
        let r = sqlx::query(
            r#"
            UPDATE alert_rules SET
                name               = ?,
                description        = ?,
                enabled            = ?,
                expression         = ?,
                severity           = ?,
                for_duration_secs  = ?,
                eval_interval_secs = ?,
                cooldown_secs      = ?,
                updated_at         = unixepoch()
              WHERE id = ?
            "#,
        )
        .bind(&rule.name)
        .bind(rule.description.as_deref())
        .bind(rule.enabled as i64)
        .bind(&rule.expression)
        .bind(rule.severity.as_str())
        .bind(rule.for_duration_secs)
        .bind(rule.eval_interval_secs)
        .bind(rule.cooldown_secs)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete(&self, id: i64) -> AppResult<bool> {
        let r = sqlx::query("DELETE FROM alert_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    // ===== alert_state =====

    /// Upsert the current lifecycle for one (rule, label_set). The
    /// evaluator calls this on every tick. Idempotent — re-applies the
    /// same state with refreshed last_value / last_eval_at fine.
    pub async fn upsert_state(&self, st: &AlertStateRow) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO alert_state
                (rule_id, label_set, state, state_since,
                 last_value, last_eval_at, last_notified_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(rule_id, label_set) DO UPDATE SET
                state            = excluded.state,
                state_since      = excluded.state_since,
                last_value       = excluded.last_value,
                last_eval_at     = excluded.last_eval_at,
                last_notified_at = COALESCE(excluded.last_notified_at, alert_state.last_notified_at)
            "#,
        )
        .bind(st.rule_id)
        .bind(&st.label_set)
        .bind(st.state.as_str())
        .bind(st.state_since)
        .bind(st.last_value)
        .bind(st.last_eval_at)
        .bind(st.last_notified_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch all state rows for one rule. The evaluator uses this to
    /// reconcile against the metric resolver's current label_set output —
    /// rows present in DB but absent from current evaluation are taken
    /// to mean "the label_set disappeared" (e.g. unmounted disk) and
    /// the state row is left alone but eventually purged via
    /// `prune_state_for_rule` once the operator confirms.
    pub async fn list_state_for_rule(&self, rule_id: i64) -> AppResult<Vec<AlertStateRow>> {
        let rows: Vec<(i64, String, String, i64, Option<f64>, i64, Option<i64>)> = sqlx::query_as(
            r#"
            SELECT rule_id, label_set, state, state_since,
                   last_value, last_eval_at, last_notified_at
              FROM alert_state
             WHERE rule_id = ?
            "#,
        )
        .bind(rule_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(decode_state).collect())
    }

    /// All currently-firing or pending state rows, joined with their
    /// rule's name + severity. Drives `GET /alerts/state` for the
    /// dashboard's "active alerts" view.
    ///
    /// Returns the rule meta inline so callers don't need a second
    /// `list()` round-trip + in-memory hash join. With many rules but
    /// few active states, that previous pattern was an N+1-ish read
    /// that scaled with rule count instead of active count.
    pub async fn list_active_state(
        &self,
    ) -> AppResult<Vec<(AlertStateRow, String, AlertSeverity)>> {
        let rows: Vec<(
            i64,
            String,
            String,
            i64,
            Option<f64>,
            i64,
            Option<i64>,
            String,
            String,
        )> = sqlx::query_as(
            r#"
            SELECT s.rule_id, s.label_set, s.state, s.state_since,
                   s.last_value, s.last_eval_at, s.last_notified_at,
                   r.name, r.severity
              FROM alert_state s
              INNER JOIN alert_rules r ON r.id = s.rule_id
             WHERE s.state IN ('pending','firing')
             ORDER BY s.state_since ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let (
                    rule_id,
                    label_set,
                    state,
                    state_since,
                    last_value,
                    last_eval_at,
                    last_notified_at,
                    name,
                    severity_str,
                ) = row;
                let lifecycle = AlertLifecycle::parse(&state)?;
                let severity = AlertSeverity::parse(&severity_str)?;
                Some((
                    AlertStateRow {
                        rule_id,
                        label_set,
                        state: lifecycle,
                        state_since,
                        last_value,
                        last_eval_at,
                        last_notified_at,
                    },
                    name,
                    severity,
                ))
            })
            .collect())
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
        let r = sqlx::query(
            r#"
            INSERT INTO alert_events
                (rule_id, label_set, event_type, severity,
                 occurred_at, metric_value, notified)
            VALUES (?, ?, ?, ?, unixepoch(), ?, ?)
            "#,
        )
        .bind(rule_id)
        .bind(label_set)
        .bind(event_type.as_str())
        .bind(severity.as_str())
        .bind(metric_value)
        .bind(notified as i64)
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
        let rows: Vec<(i64, i64, String, String, String, i64, Option<f64>, i64)> = sqlx::query_as(
            r#"
            SELECT id, rule_id, label_set, event_type, severity,
                   occurred_at, metric_value, notified
              FROM alert_events
             WHERE rule_id = ?
             ORDER BY occurred_at DESC
             LIMIT ? OFFSET ?
            "#,
        )
        .bind(rule_id)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(decode_event).collect())
    }

    /// Cross-rule recent events, newest first. See `events_for_rule` for the
    /// offset semantics.
    pub async fn recent_events(&self, limit: u32, offset: u32) -> AppResult<Vec<AlertEvent>> {
        let rows: Vec<(i64, i64, String, String, String, i64, Option<f64>, i64)> = sqlx::query_as(
            r#"
            SELECT id, rule_id, label_set, event_type, severity,
                   occurred_at, metric_value, notified
              FROM alert_events
             ORDER BY occurred_at DESC
             LIMIT ? OFFSET ?
            "#,
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(decode_event).collect())
    }
}

// ===== row decoders =====

#[allow(clippy::type_complexity)]
fn decode_rule(
    row: (
        i64,
        String,
        Option<String>,
        i64,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
    ),
) -> Option<AlertRule> {
    let (
        id,
        name,
        description,
        enabled,
        expression,
        severity,
        for_duration_secs,
        eval_interval_secs,
        cooldown_secs,
        created_at,
        updated_at,
    ) = row;
    Some(AlertRule {
        id,
        name,
        description,
        enabled: enabled != 0,
        expression,
        severity: AlertSeverity::parse(&severity)?,
        for_duration_secs,
        eval_interval_secs,
        cooldown_secs,
        created_at,
        updated_at,
    })
}

fn decode_state(
    row: (i64, String, String, i64, Option<f64>, i64, Option<i64>),
) -> Option<AlertStateRow> {
    let (rule_id, label_set, state, state_since, last_value, last_eval_at, last_notified_at) = row;
    Some(AlertStateRow {
        rule_id,
        label_set,
        state: AlertLifecycle::parse(&state)?,
        state_since,
        last_value,
        last_eval_at,
        last_notified_at,
    })
}

fn decode_event(
    row: (i64, i64, String, String, String, i64, Option<f64>, i64),
) -> Option<AlertEvent> {
    let (id, rule_id, label_set, event_type, severity, occurred_at, metric_value, notified) = row;
    Some(AlertEvent {
        id,
        rule_id,
        label_set,
        event_type: AlertEventType::parse(&event_type)?,
        severity: AlertSeverity::parse(&severity)?,
        occurred_at,
        metric_value,
        notified: notified != 0,
    })
}
