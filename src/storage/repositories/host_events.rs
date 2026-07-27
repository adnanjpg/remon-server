//! Host-event repository — the discrete-happenings ledger (`host_events`).
//!
//! Writers go through `services::events` (which spawns fire-and-forget
//! inserts so no request or collector tick ever blocks on the ledger);
//! this module is plain storage. Rows age out via the
//! `('host_events', 'raw', …)` retention seed.

use sqlx::SqlitePool;

use super::wrap_csv;
use crate::error::AppResult;

/// A new ledger entry. `created_at = None` stamps the row with "now";
/// detectors that know the real occurrence time (boot, OOM parsed from the
/// journal) pass it explicitly so the timeline shows when it happened, not
/// when we noticed.
#[derive(Debug, Clone, Default)]
pub struct NewHostEvent {
    pub created_at: Option<i64>,
    pub source: &'static str,
    pub kind: &'static str,
    pub severity: &'static str,
    pub message: String,
    pub actor_device_id: Option<String>,
    pub actor_name: Option<String>,
    pub ref_type: Option<&'static str>,
    pub ref_id: Option<String>,
    pub details: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HostEventRow {
    pub created_at: i64,
    pub source: String,
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub actor_device_id: Option<String>,
    pub actor_name: Option<String>,
    pub ref_type: Option<String>,
    pub ref_id: Option<String>,
    pub details: Option<String>,
}

pub struct HostEventRepository {
    pool: SqlitePool,
}

impl HostEventRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, e: &NewHostEvent) -> AppResult<i64> {
        let created_at = e
            .created_at
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        let r = sqlx::query!(
            "INSERT INTO host_events
               (created_at, source, kind, severity, message,
                actor_device_id, actor_name, ref_type, ref_id, details)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            created_at,
            e.source,
            e.kind,
            e.severity,
            e.message,
            e.actor_device_id,
            e.actor_name,
            e.ref_type,
            e.ref_id,
            e.details,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Range listing, newest first. `kinds` / `sources` are optional
    /// allowlists; `None` means no filter. The lists arrive as
    /// `,a,b,`-wrapped CSVs so the filter stays a single static query the
    /// macro can check (`instr` against `','||col||','`).
    pub async fn list_range(
        &self,
        start: i64,
        end: i64,
        kinds: Option<&[String]>,
        sources: Option<&[String]>,
        limit: u32,
    ) -> AppResult<Vec<HostEventRow>> {
        let kinds_csv = kinds.map(wrap_csv);
        let sources_csv = sources.map(wrap_csv);
        let rows = sqlx::query!(
            r#"SELECT created_at as "created_at!",
                      source as "source!", kind as "kind!",
                      severity as "severity!", message as "message!",
                      actor_device_id, actor_name, ref_type, ref_id, details
               FROM host_events
              WHERE created_at >= ?1 AND created_at <= ?2
                AND (?3 IS NULL OR instr(?3, ',' || kind || ',') > 0)
                AND (?4 IS NULL OR instr(?4, ',' || source || ',') > 0)
              ORDER BY created_at DESC, id DESC
              LIMIT ?5"#,
            start,
            end,
            kinds_csv,
            sources_csv,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| HostEventRow {
                created_at: r.created_at,
                source: r.source,
                kind: r.kind,
                severity: r.severity,
                message: r.message,
                actor_device_id: r.actor_device_id,
                actor_name: r.actor_name,
                ref_type: r.ref_type,
                ref_id: r.ref_id,
                details: r.details,
            })
            .collect())
    }
}
