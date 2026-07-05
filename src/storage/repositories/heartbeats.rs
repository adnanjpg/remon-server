//! Heartbeat repository — check registry (`heartbeat_checks`) and ping
//! log (`heartbeat_pings`).
//!
//! Writes are shaped so concurrent pings stay benign: `last_ping_at` /
//! `last_fail_at` only move forward via `MAX()`, and pause transitions
//! carry their precondition in the `WHERE` clause (returning
//! rows_affected) so an operator pause can't be raced by a service one.
//! Policy — who may pause whom, clamps, what counts as auto-resume —
//! lives in the route handlers; this layer only guarantees atomicity.

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::heartbeat::{HeartbeatCheck, HeartbeatPing, PauseOrigin, PingKind};

pub struct HeartbeatRepository {
    pool: SqlitePool,
}

/// Column set for `INSERT`/`UPDATE` from the REST layer. `id`,
/// slug/pause/ping columns are managed by their dedicated methods.
#[derive(Debug, Clone)]
pub struct UpsertHeartbeatCheck {
    pub name: String,
    pub description: Option<String>,
    pub period_secs: i64,
    pub grace_secs: i64,
    pub enabled: bool,
}

impl HeartbeatRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ===== check CRUD =====

    /// `born_paused` writes the indefinite operator pause in the same
    /// statement — two statements would let a crash in between leave a
    /// live, unpaused check whose one-shot slug the caller never saw.
    pub async fn insert(
        &self,
        check: &UpsertHeartbeatCheck,
        slug_hash: &str,
        born_paused: bool,
    ) -> AppResult<i64> {
        // created_at anchors the never-pinged deadline, so it must come
        // from the same clock as state derivation (Utc::now), not the DB
        // default.
        let now = chrono::Utc::now().timestamp();
        let paused_at = born_paused.then_some(now);
        let pause_origin = born_paused.then_some("operator");
        let pause_reason = born_paused.then_some("created paused");
        let r = sqlx::query!(
            "INSERT INTO heartbeat_checks
                (name, description, slug_hash, period_secs, grace_secs, enabled,
                 paused_at, pause_origin, pause_reason, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            check.name,
            check.description,
            slug_hash,
            check.period_secs,
            check.grace_secs,
            check.enabled,
            paused_at,
            pause_origin,
            pause_reason,
            now,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Fresh-window grant for the disabled→enabled edge — the one "quiet
    /// window ends" path the anchor formula can't see on its own. Bumps
    /// the anchor via paused_until (no pause is created: paused_at is
    /// untouched) and clears the stale fail latch, so a re-enabled check
    /// gets one full period+grace instead of paging instantly. Skips
    /// checks under an indefinite operator pause: giving those a concrete
    /// end would silently lift the pause.
    pub async fn re_enable_grant(&self, id: i64, now: i64) -> AppResult<()> {
        sqlx::query!(
            "UPDATE heartbeat_checks SET
                failed       = 0,
                paused_until = MAX(COALESCE(paused_until, 0), ?)
              WHERE id = ?
                AND NOT (paused_at IS NOT NULL AND paused_until IS NULL)",
            now,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, id: i64, check: &UpsertHeartbeatCheck) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET
                name        = ?,
                description = ?,
                period_secs = ?,
                grace_secs  = ?,
                enabled     = ?,
                updated_at  = unixepoch()
              WHERE id = ?",
            check.name,
            check.description,
            check.period_secs,
            check.grace_secs,
            check.enabled,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn delete(&self, id: i64) -> AppResult<bool> {
        let r = sqlx::query!("DELETE FROM heartbeat_checks WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn list_all(&self) -> AppResult<Vec<HeartbeatCheck>> {
        let rows = sqlx::query_as!(
            HeartbeatCheckRow,
            r#"SELECT id, name, description, period_secs, grace_secs,
                    enabled as "enabled: bool", last_ping_at,
                    failed as "failed: bool", last_fail_at,
                    paused_at, paused_until, pause_origin, pause_reason,
                    created_at, updated_at
               FROM heartbeat_checks
              ORDER BY name ASC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(HeartbeatCheckRow::decode)
            .collect())
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<HeartbeatCheck>> {
        let row = sqlx::query_as!(
            HeartbeatCheckRow,
            r#"SELECT id, name, description, period_secs, grace_secs,
                    enabled as "enabled: bool", last_ping_at,
                    failed as "failed: bool", last_fail_at,
                    paused_at, paused_until, pause_origin, pause_reason,
                    created_at, updated_at
               FROM heartbeat_checks
              WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(HeartbeatCheckRow::decode))
    }

    /// Ping hot path: one indexed lookup on the UNIQUE slug_hash column.
    pub async fn get_by_slug_hash(&self, slug_hash: &str) -> AppResult<Option<HeartbeatCheck>> {
        let row = sqlx::query_as!(
            HeartbeatCheckRow,
            r#"SELECT id, name, description, period_secs, grace_secs,
                    enabled as "enabled: bool", last_ping_at,
                    failed as "failed: bool", last_fail_at,
                    paused_at, paused_until, pause_origin, pause_reason,
                    created_at, updated_at
               FROM heartbeat_checks
              WHERE slug_hash = ?"#,
            slug_hash
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(HeartbeatCheckRow::decode))
    }

    pub async fn rotate_slug(&self, id: i64, slug_hash: &str) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET slug_hash = ?, updated_at = unixepoch() WHERE id = ?",
            slug_hash,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    // ===== ping-path writes =====

    /// Record a success ping. `last_ping_at` only moves forward, so two
    /// racing pings converge. A bare service pause ("quiet until I ping
    /// again") is the one pause a success resumes: its window is clamped
    /// to `now`, which the state anchor then treats as the pause end.
    pub async fn record_success(&self, id: i64, now: i64) -> AppResult<()> {
        sqlx::query!(
            "UPDATE heartbeat_checks SET
                last_ping_at = MAX(COALESCE(last_ping_at, 0), ?),
                failed = 0,
                paused_until = CASE
                    WHEN pause_until_ping = 1 AND paused_until > ? THEN ?
                    ELSE paused_until END,
                pause_until_ping = 0
              WHERE id = ?",
            now,
            now,
            now,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record an explicit failure. Does not touch pause state — a fail
    /// during maintenance stays muted until the window ends.
    pub async fn record_fail(&self, id: i64, now: i64) -> AppResult<()> {
        sqlx::query!(
            "UPDATE heartbeat_checks SET
                failed = 1,
                last_fail_at = MAX(COALESCE(last_fail_at, 0), ?)
              WHERE id = ?",
            now,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Place a service-announced pause. Refuses to overwrite an active
    /// operator pause (the precondition rides in the WHERE clause);
    /// returns false so the handler can 409.
    pub async fn service_pause(
        &self,
        id: i64,
        now: i64,
        until: i64,
        until_ping: bool,
        reason: Option<&str>,
    ) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET
                paused_at        = ?,
                paused_until     = ?,
                pause_origin     = 'service',
                pause_reason     = ?,
                pause_until_ping = ?,
                updated_at       = unixepoch()
              WHERE id = ?
                AND NOT (paused_at IS NOT NULL
                         AND pause_origin = 'operator'
                         AND (paused_until IS NULL OR paused_until > ?))",
            now,
            until,
            reason,
            until_ping,
            id,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// End an active service-origin pause by clamping its window to now.
    /// No-ops (false) when the active pause is operator-owned or when
    /// nothing is paused — handler distinguishes via the fetched row.
    pub async fn service_resume(&self, id: i64, now: i64) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET
                paused_until     = ?,
                pause_until_ping = 0,
                updated_at       = unixepoch()
              WHERE id = ?
                AND paused_at IS NOT NULL
                AND pause_origin = 'service'
                AND paused_until > ?",
            now,
            id,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Operator pause — overwrites any service pause unconditionally.
    /// `until = NULL` is the indefinite form.
    pub async fn operator_pause(
        &self,
        id: i64,
        now: i64,
        until: Option<i64>,
        reason: Option<&str>,
    ) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET
                paused_at        = ?,
                paused_until     = ?,
                pause_origin     = 'operator',
                pause_reason     = ?,
                pause_until_ping = 0,
                updated_at       = unixepoch()
              WHERE id = ?",
            now,
            until,
            reason,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Operator resume — ends any active pause (either origin). Clamping
    /// `paused_until` to now re-anchors the deadline, so resume grants a
    /// fresh period+grace instead of an instant down. Pause columns stay
    /// for audit.
    pub async fn operator_resume(&self, id: i64, now: i64) -> AppResult<bool> {
        let r = sqlx::query!(
            "UPDATE heartbeat_checks SET
                paused_until     = ?,
                pause_until_ping = 0,
                updated_at       = unixepoch()
              WHERE id = ?
                AND paused_at IS NOT NULL
                AND (paused_until IS NULL OR paused_until > ?)",
            now,
            id,
            now,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    // ===== ping log =====

    pub async fn insert_ping(&self, ping: &HeartbeatPing) -> AppResult<()> {
        let kind = ping.kind.as_str();
        sqlx::query!(
            "INSERT INTO heartbeat_pings
                (check_id, received_at, kind, exit_code, source_ip, user_agent, body)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ping.check_id,
            ping.received_at,
            kind,
            ping.exit_code,
            ping.source_ip,
            ping.user_agent,
            ping.body,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_pings(
        &self,
        check_id: i64,
        limit: u32,
        offset: u32,
    ) -> AppResult<Vec<HeartbeatPing>> {
        let limit = limit as i64;
        let offset = offset as i64;
        let rows = sqlx::query_as!(
            HeartbeatPingRow,
            "SELECT id, check_id, received_at, kind, exit_code, source_ip, user_agent, body
               FROM heartbeat_pings
              WHERE check_id = ?
              ORDER BY received_at DESC, id DESC
              LIMIT ? OFFSET ?",
            check_id,
            limit,
            offset,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(HeartbeatPingRow::decode)
            .collect())
    }
}

// ===== private row types =====

#[derive(sqlx::FromRow)]
struct HeartbeatCheckRow {
    id: i64,
    name: String,
    description: Option<String>,
    period_secs: i64,
    grace_secs: i64,
    enabled: bool,
    last_ping_at: Option<i64>,
    failed: bool,
    last_fail_at: Option<i64>,
    paused_at: Option<i64>,
    paused_until: Option<i64>,
    pause_origin: Option<String>,
    pause_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl HeartbeatCheckRow {
    fn decode(self) -> Option<HeartbeatCheck> {
        let pause_origin = match self.pause_origin {
            Some(s) => Some(PauseOrigin::parse(&s)?),
            None => None,
        };
        Some(HeartbeatCheck {
            id: self.id,
            name: self.name,
            description: self.description,
            period_secs: self.period_secs,
            grace_secs: self.grace_secs,
            enabled: self.enabled,
            last_ping_at: self.last_ping_at,
            failed: self.failed,
            last_fail_at: self.last_fail_at,
            paused_at: self.paused_at,
            paused_until: self.paused_until,
            pause_origin,
            pause_reason: self.pause_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct HeartbeatPingRow {
    id: i64,
    check_id: i64,
    received_at: i64,
    kind: String,
    exit_code: Option<i64>,
    source_ip: Option<String>,
    user_agent: Option<String>,
    body: Option<String>,
}

impl HeartbeatPingRow {
    fn decode(self) -> Option<HeartbeatPing> {
        Some(HeartbeatPing {
            id: self.id,
            check_id: self.check_id,
            received_at: self.received_at,
            kind: PingKind::parse(&self.kind)?,
            exit_code: self.exit_code.map(|v| v as i32),
            source_ip: self.source_ip,
            user_agent: self.user_agent,
            body: self.body,
        })
    }
}
