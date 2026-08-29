//! Alert-action repository — bindings (`alert_actions`) + run ledger
//! (`action_runs`).
//!
//! Two responsibilities worth keeping apart:
//! - **Bindings** are CRUD-shaped, edited by an operator through REST.
//! - **Runs** are append-only history *and* the live queue: a `pending` row
//!   is an unconfirmed proposal, a `running` row is the single-flight latch.
//!
//! Concurrency is handled the same way `heartbeats` handles racing pings —
//! every state transition carries its precondition in the `WHERE` clause and
//! reports `rows_affected`, so two callers racing to confirm the same
//! proposal produce one run and one "already handled", never two executions.

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::action::{
    ActionBinding, ActionKind, ActionMode, ActionRun, ActionTrigger, ActionVerb, OnEvent,
    RunOrigin, RunStatus,
};

pub struct ActionRepository {
    pool: SqlitePool,
}

/// Body shape for creating / replacing a binding. REST DTOs map onto this.
#[derive(Debug, Clone)]
pub struct UpsertActionBinding {
    pub rule_id: i64,
    pub kind: ActionKind,
    pub target: String,
    pub verb: Option<ActionVerb>,
    pub on_event: OnEvent,
    pub mode: ActionMode,
    pub enabled: bool,
    pub cooldown_secs: i64,
    pub max_runs_per_hour: i64,
    pub failure_limit: i64,
}

/// Everything needed to open a run row. Kept as one struct because the
/// denormalized columns travel together and a positional call with eight
/// strings would be a bug waiting to happen.
#[derive(Debug, Clone)]
pub struct NewActionRun {
    pub action_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_name: String,
    pub label_set: String,
    pub kind: ActionKind,
    pub target: String,
    pub verb: Option<ActionVerb>,
    pub trigger_event: ActionTrigger,
    pub origin: RunOrigin,
    pub requested_by: Option<String>,
    pub status: RunStatus,
    pub expires_at: Option<i64>,
    pub message: Option<String>,
}

/// Filter for `GET /actions/runs`.
#[derive(Debug, Clone, Default)]
pub struct RunQuery {
    pub status: Option<RunStatus>,
    pub action_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub limit: u32,
    pub offset: u32,
}

impl ActionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ===== bindings =====

    pub async fn list_bindings(&self) -> AppResult<Vec<ActionBinding>> {
        let rows = sqlx::query_as!(
            BindingRow,
            r#"SELECT id, rule_id, action_kind, action_target, action_verb, on_event, mode,
                      enabled as "enabled: bool", cooldown_secs, max_runs_per_hour,
                      failure_limit, consecutive_failures, disabled_reason,
                      created_at, updated_at
                 FROM alert_actions
                ORDER BY rule_id ASC, id ASC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(BindingRow::decode).collect())
    }

    pub async fn list_for_rule(&self, rule_id: i64) -> AppResult<Vec<ActionBinding>> {
        let rows = sqlx::query_as!(
            BindingRow,
            r#"SELECT id, rule_id, action_kind, action_target, action_verb, on_event, mode,
                      enabled as "enabled: bool", cooldown_secs, max_runs_per_hour,
                      failure_limit, consecutive_failures, disabled_reason,
                      created_at, updated_at
                 FROM alert_actions
                WHERE rule_id = ?
                ORDER BY id ASC"#,
            rule_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(BindingRow::decode).collect())
    }

    /// The evaluator's hot path: only bindings that could possibly act on
    /// this transition. Filtering `enabled` here keeps a disarmed binding
    /// from costing anything at all on every fire.
    pub async fn list_armed_for_rule(&self, rule_id: i64) -> AppResult<Vec<ActionBinding>> {
        let rows = sqlx::query_as!(
            BindingRow,
            r#"SELECT id, rule_id, action_kind, action_target, action_verb, on_event, mode,
                      enabled as "enabled: bool", cooldown_secs, max_runs_per_hour,
                      failure_limit, consecutive_failures, disabled_reason,
                      created_at, updated_at
                 FROM alert_actions
                WHERE rule_id = ? AND enabled = 1
                ORDER BY id ASC"#,
            rule_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(BindingRow::decode).collect())
    }

    pub async fn get_binding(&self, id: i64) -> AppResult<Option<ActionBinding>> {
        let row = sqlx::query_as!(
            BindingRow,
            r#"SELECT id, rule_id, action_kind, action_target, action_verb, on_event, mode,
                      enabled as "enabled: bool", cooldown_secs, max_runs_per_hour,
                      failure_limit, consecutive_failures, disabled_reason,
                      created_at, updated_at
                 FROM alert_actions
                WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(BindingRow::decode))
    }

    pub async fn insert_binding(&self, b: &UpsertActionBinding) -> AppResult<i64> {
        let now = chrono::Utc::now().timestamp();
        let kind = b.kind.as_str();
        let verb = b.verb.map(|v| v.as_str());
        let on_event = b.on_event.as_str();
        let mode = b.mode.as_str();
        let r = sqlx::query!(
            "INSERT INTO alert_actions
                (rule_id, action_kind, action_target, action_verb, on_event, mode, enabled,
                 cooldown_secs, max_runs_per_hour, failure_limit, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            b.rule_id,
            kind,
            b.target,
            verb,
            on_event,
            mode,
            b.enabled,
            b.cooldown_secs,
            b.max_runs_per_hour,
            b.failure_limit,
            now,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Full replace of the editable columns. Re-enabling clears the breaker:
    /// an operator turning a binding back on has, by that act, decided the
    /// consecutive failures behind it are no longer the current situation.
    pub async fn update_binding(&self, id: i64, b: &UpsertActionBinding) -> AppResult<bool> {
        let now = chrono::Utc::now().timestamp();
        let kind = b.kind.as_str();
        let verb = b.verb.map(|v| v.as_str());
        let on_event = b.on_event.as_str();
        let mode = b.mode.as_str();
        let r = sqlx::query!(
            "UPDATE alert_actions
                SET action_kind = ?, action_target = ?, action_verb = ?, on_event = ?,
                    mode = ?, enabled = ?, cooldown_secs = ?, max_runs_per_hour = ?,
                    failure_limit = ?,
                    consecutive_failures = CASE WHEN ? = 1 THEN 0 ELSE consecutive_failures END,
                    disabled_reason = CASE WHEN ? = 1 THEN NULL ELSE disabled_reason END,
                    updated_at = ?
              WHERE id = ?",
            kind,
            b.target,
            verb,
            on_event,
            mode,
            b.enabled,
            b.cooldown_secs,
            b.max_runs_per_hour,
            b.failure_limit,
            b.enabled,
            b.enabled,
            now,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete_binding(&self, id: i64) -> AppResult<bool> {
        let r = sqlx::query!("DELETE FROM alert_actions WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    // ===== circuit breaker =====

    /// Reset the failure streak after a successful run.
    pub async fn clear_failures(&self, id: i64) -> AppResult<()> {
        sqlx::query!(
            "UPDATE alert_actions SET consecutive_failures = 0 WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count one failure and, at the limit, disarm the binding in the same
    /// statement. Returns the new streak and whether this call tripped the
    /// breaker — done in SQL so two concurrent failures cannot both read the
    /// pre-increment value and both decide they are not the last straw.
    pub async fn record_failure(&self, id: i64, reason: &str) -> AppResult<(i64, bool)> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query!(
            "UPDATE alert_actions
                SET consecutive_failures = consecutive_failures + 1,
                    enabled = CASE WHEN consecutive_failures + 1 >= failure_limit
                                   THEN 0 ELSE enabled END,
                    disabled_reason = CASE WHEN consecutive_failures + 1 >= failure_limit
                                           THEN ? ELSE disabled_reason END,
                    updated_at = ?
              WHERE id = ?",
            reason,
            now,
            id
        )
        .execute(&self.pool)
        .await?;

        let row = sqlx::query!(
            r#"SELECT consecutive_failures, enabled as "enabled: bool" FROM alert_actions WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(r) => (r.consecutive_failures, !r.enabled),
            None => (0, false),
        })
    }

    // ===== guardrail queries =====

    /// Newest terminal run for this (binding, label_set) — the cooldown
    /// reference point. Skipped and dismissed rows do not count: nothing
    /// executed, so nothing needs cooling off.
    pub async fn last_effective_run_at(
        &self,
        action_id: i64,
        label_set: &str,
    ) -> AppResult<Option<i64>> {
        let row = sqlx::query!(
            "SELECT MAX(COALESCE(finished_at, started_at, created_at)) AS ts
               FROM action_runs
              WHERE action_id = ? AND label_set = ?
                AND status IN ('succeeded','failed','running')",
            action_id,
            label_set
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.ts))
    }

    /// Executions started in the trailing hour, across every label_set. The
    /// ceiling is deliberately per-binding rather than per-target: a rule
    /// firing on twenty containers at once is exactly the case the limit
    /// exists to contain.
    pub async fn runs_since(&self, action_id: i64, since: i64) -> AppResult<i64> {
        let row = sqlx::query!(
            "SELECT COUNT(*) AS n
               FROM action_runs
              WHERE action_id = ? AND created_at >= ?
                AND status IN ('succeeded','failed','running','pending')",
            action_id,
            since
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.n as i64)
    }

    /// True when a proposal or execution for this (binding, label_set) is
    /// still outstanding. The single-flight guard: a rule that keeps firing
    /// must not stack five restarts of the same unit.
    pub async fn has_in_flight(&self, action_id: i64, label_set: &str) -> AppResult<bool> {
        let row = sqlx::query!(
            "SELECT COUNT(*) AS n
               FROM action_runs
              WHERE action_id = ? AND label_set = ? AND status IN ('pending','running')",
            action_id,
            label_set
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.n > 0)
    }

    // ===== runs =====

    pub async fn insert_run(&self, run: &NewActionRun) -> AppResult<i64> {
        let now = chrono::Utc::now().timestamp();
        let kind = run.kind.as_str();
        let verb = run.verb.map(|v| v.as_str());
        let trigger = run.trigger_event.as_str();
        let origin = run.origin.as_str();
        let status = run.status.as_str();
        // A row that opens already running has its clock started here; a
        // pending proposal has not started anything yet.
        let started_at = (run.status == RunStatus::Running).then_some(now);
        let r = sqlx::query!(
            "INSERT INTO action_runs
                (action_id, rule_id, rule_name, label_set, action_kind, action_target,
                 action_verb, trigger_event, origin, requested_by, status, expires_at,
                 started_at, message, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            run.action_id,
            run.rule_id,
            run.rule_name,
            run.label_set,
            kind,
            run.target,
            verb,
            trigger,
            origin,
            run.requested_by,
            status,
            run.expires_at,
            started_at,
            run.message,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn get_run(&self, id: i64) -> AppResult<Option<ActionRun>> {
        let row = sqlx::query_as!(
            RunRow,
            r#"SELECT id, action_id, rule_id, rule_name, label_set, action_kind, action_target,
                      action_verb, trigger_event, origin, requested_by, status, expires_at,
                      started_at, finished_at, duration_ms, exit_code as "exit_code: i32",
                      message, output_tail, created_at
                 FROM action_runs
                WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(RunRow::decode))
    }

    pub async fn list_runs(&self, q: &RunQuery) -> AppResult<Vec<ActionRun>> {
        // NULL means "no filter" for each optional predicate — one prepared
        // statement instead of string-built SQL, so this stays a checked
        // query and no caller value ever reaches the SQL text.
        let status = q.status.map(|s| s.as_str());
        let rows = sqlx::query_as!(
            RunRow,
            r#"SELECT id, action_id, rule_id, rule_name, label_set, action_kind, action_target,
                      action_verb, trigger_event, origin, requested_by, status, expires_at,
                      started_at, finished_at, duration_ms, exit_code as "exit_code: i32",
                      message, output_tail, created_at
                 FROM action_runs
                WHERE (?1 IS NULL OR status = ?1)
                  AND (?2 IS NULL OR action_id = ?2)
                  AND (?3 IS NULL OR rule_id = ?3)
                ORDER BY created_at DESC, id DESC
                LIMIT ?4 OFFSET ?5"#,
            status,
            q.action_id,
            q.rule_id,
            q.limit,
            q.offset
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(RunRow::decode).collect())
    }

    /// Claim a pending proposal for execution. The `status = 'pending'`
    /// precondition is the whole point: whoever's UPDATE lands first owns the
    /// run, and a second confirm affects zero rows and is told so.
    pub async fn claim_pending(&self, id: i64, device_id: &str) -> AppResult<bool> {
        let now = chrono::Utc::now().timestamp();
        let r = sqlx::query!(
            "UPDATE action_runs
                SET status = 'running', origin = 'confirmed', requested_by = ?,
                    started_at = ?, expires_at = NULL
              WHERE id = ? AND status = 'pending'",
            device_id,
            now,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Operator declined a proposal. Same precondition discipline.
    pub async fn dismiss_pending(&self, id: i64, device_id: &str) -> AppResult<bool> {
        let now = chrono::Utc::now().timestamp();
        let r = sqlx::query!(
            "UPDATE action_runs
                SET status = 'dismissed', requested_by = ?, finished_at = ?, expires_at = NULL,
                    message = COALESCE(message, 'dismissed by operator')
              WHERE id = ? AND status = 'pending'",
            device_id,
            now,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Write a terminal outcome onto a `running` row.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_run(
        &self,
        id: i64,
        status: RunStatus,
        exit_code: Option<i32>,
        duration_ms: i64,
        message: &str,
        output_tail: Option<&str>,
    ) -> AppResult<()> {
        let now = chrono::Utc::now().timestamp();
        let status = status.as_str();
        sqlx::query!(
            "UPDATE action_runs
                SET status = ?, exit_code = ?, duration_ms = ?, message = ?, output_tail = ?,
                    finished_at = ?
              WHERE id = ?",
            status,
            exit_code,
            duration_ms,
            message,
            output_tail,
            now,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Retire proposals nobody answered. Returns the rows that expired so the
    /// caller can put them on the timeline — a proposal that quietly vanished
    /// is indistinguishable from one that was never made.
    pub async fn expire_stale_proposals(&self, now: i64) -> AppResult<Vec<ActionRun>> {
        let due = sqlx::query_as!(
            RunRow,
            r#"SELECT id, action_id, rule_id, rule_name, label_set, action_kind, action_target,
                      action_verb, trigger_event, origin, requested_by, status, expires_at,
                      started_at, finished_at, duration_ms, exit_code as "exit_code: i32",
                      message, output_tail, created_at
                 FROM action_runs
                WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?"#,
            now
        )
        .fetch_all(&self.pool)
        .await?;
        if due.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query!(
            "UPDATE action_runs
                SET status = 'expired', finished_at = ?,
                    message = COALESCE(message, 'no confirmation before the proposal expired')
              WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?",
            now,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(due.into_iter().filter_map(RunRow::decode).collect())
    }

    /// Startup sweep: a `running` row can only be a run this process was
    /// executing when it died, since nothing else writes that state. Left
    /// alone it would hold the single-flight latch shut forever.
    pub async fn fail_orphaned_runs(&self) -> AppResult<u64> {
        let now = chrono::Utc::now().timestamp();
        let r = sqlx::query!(
            "UPDATE action_runs
                SET status = 'failed', finished_at = ?,
                    message = 'server restarted while this action was running; outcome unknown'
              WHERE status = 'running'",
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    id: i64,
    rule_id: i64,
    action_kind: String,
    action_target: String,
    action_verb: Option<String>,
    on_event: String,
    mode: String,
    enabled: bool,
    cooldown_secs: i64,
    max_runs_per_hour: i64,
    failure_limit: i64,
    consecutive_failures: i64,
    disabled_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl BindingRow {
    /// A row whose enum columns don't decode is dropped rather than
    /// surfaced — same policy as `AlertRuleRow`. The CHECK constraints make
    /// it unreachable short of hand-editing the database.
    fn decode(self) -> Option<ActionBinding> {
        Some(ActionBinding {
            id: self.id,
            rule_id: self.rule_id,
            kind: ActionKind::parse(&self.action_kind)?,
            target: self.action_target,
            verb: match self.action_verb {
                Some(v) => Some(ActionVerb::parse(&v)?),
                None => None,
            },
            on_event: OnEvent::parse(&self.on_event)?,
            mode: ActionMode::parse(&self.mode)?,
            enabled: self.enabled,
            cooldown_secs: self.cooldown_secs,
            max_runs_per_hour: self.max_runs_per_hour,
            failure_limit: self.failure_limit,
            consecutive_failures: self.consecutive_failures,
            disabled_reason: self.disabled_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: i64,
    action_id: Option<i64>,
    rule_id: Option<i64>,
    rule_name: String,
    label_set: String,
    action_kind: String,
    action_target: String,
    action_verb: Option<String>,
    trigger_event: String,
    origin: String,
    requested_by: Option<String>,
    status: String,
    expires_at: Option<i64>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    duration_ms: Option<i64>,
    exit_code: Option<i32>,
    message: Option<String>,
    output_tail: Option<String>,
    created_at: i64,
}

impl RunRow {
    fn decode(self) -> Option<ActionRun> {
        Some(ActionRun {
            id: self.id,
            action_id: self.action_id,
            rule_id: self.rule_id,
            rule_name: self.rule_name,
            label_set: self.label_set,
            kind: ActionKind::parse(&self.action_kind)?,
            target: self.action_target,
            verb: match self.action_verb {
                Some(v) => Some(ActionVerb::parse(&v)?),
                None => None,
            },
            trigger_event: ActionTrigger::parse(&self.trigger_event)?,
            origin: RunOrigin::parse(&self.origin)?,
            requested_by: self.requested_by,
            status: RunStatus::parse(&self.status)?,
            expires_at: self.expires_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            duration_ms: self.duration_ms,
            exit_code: self.exit_code,
            message: self.message,
            output_tail: self.output_tail,
            created_at: self.created_at,
        })
    }
}
