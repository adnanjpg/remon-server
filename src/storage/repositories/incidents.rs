//! Persistence for bounded incident episodes. Frame order is reserved before enrichment.
use crate::error::AppResult;
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct IncidentSummaryRow {
    pub id: i64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub recovery_started_at: Option<i64>,
    pub violation_count: i64,
    pub confirmation_count: i64,

    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    pub worst_value: Option<f64>,
    pub reason: Option<String>,
    pub trigger_context: Option<String>,
    pub frame_count: i64,
}
#[derive(Debug, Clone, FromRow)]
pub struct IncidentFrameRow {
    pub seq: i64,
    pub kind: String,
    pub captured_at: i64,
    pub payload: String,
}
#[derive(Debug, Clone)]
pub struct IncidentRow {
    pub id: i64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub recovery_started_at: Option<i64>,
    pub violation_count: i64,
    pub confirmation_count: i64,

    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    pub worst_value: Option<f64>,
    pub reason: Option<String>,
    pub trigger_context: Option<String>,
    pub frames: Vec<IncidentFrameRow>,
}
#[derive(Debug, Clone, Default)]
pub struct NewIncident {
    pub trigger_kind: &'static str,
    pub category: String,
    pub rule_id: Option<i64>,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    /// Where `worst_value` starts. `None` for a comparison with no direction
    /// (`==` / `!=`), where "worse" is not a thing that can be measured — the
    /// caller decides that, because only it knows the comparator.
    pub initial_worst: Option<f64>,
    pub reason: Option<String>,
    pub trigger_context: Option<String>,
}
pub struct IncidentRepository {
    pool: SqlitePool,
}
impl IncidentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn open(&self, n: &NewIncident) -> AppResult<i64> {
        let r = sqlx::query("INSERT INTO incidents (trigger_kind, category, rule_id, rule_name, label_set, trigger_value, worst_value, reason, trigger_context, violation_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(n.trigger_kind).bind(&n.category).bind(n.rule_id).bind(&n.rule_name)
            .bind(&n.label_set).bind(n.trigger_value).bind(n.initial_worst).bind(&n.reason)
            .bind(&n.trigger_context).bind(if n.trigger_kind == "alert" {1} else {0}).execute(&self.pool).await?;
        Ok(r.last_insert_rowid())
    }
    pub async fn update_observation(
        &self,
        id: i64,
        recovery: Option<i64>,
        violations: i64,
        confirmations: i64,
    ) -> AppResult<()> {
        sqlx::query("UPDATE incidents SET recovery_started_at=?2,violation_count=?3,confirmation_count=?4 WHERE id=?1 AND closed_at IS NULL")
            .bind(id).bind(recovery).bind(violations).bind(confirmations).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn close_stale_manual(&self, now: i64) -> AppResult<()> {
        sqlx::query("UPDATE incidents SET closed_at=?1,close_reason='data_gap' WHERE trigger_kind='manual' AND closed_at IS NULL AND opened_at<?1-120").bind(now).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn previous_episode(&self, rule_id: i64, label: &str) -> AppResult<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT id FROM incidents WHERE rule_id=? AND label_set=? ORDER BY opened_at DESC,id DESC LIMIT 1")
            .bind(rule_id).bind(label).fetch_optional(&self.pool).await?)
    }
    /// Only open episodes accept frames; a slot is reserved synchronously.
    pub async fn append_frame(
        &self,
        id: i64,
        kind: &str,
        at: i64,
        payload: &str,
    ) -> AppResult<i64> {
        self.write_frame(id, kind, at, payload, None).await
    }
    /// The terminal frame and close are one commit: neither can be left half-written.
    pub async fn write_frame(
        &self,
        id: i64,
        kind: &str,
        at: i64,
        payload: &str,
        close_reason: Option<&str>,
    ) -> AppResult<i64> {
        let mut tx = self.pool.begin().await?;
        let seq=sqlx::query_scalar::<_,i64>("INSERT INTO incident_frames (incident_id,seq,kind,captured_at,payload) SELECT ?1,COALESCE((SELECT MAX(seq) FROM incident_frames WHERE incident_id=?1),-1)+1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM incidents WHERE id=?1 AND closed_at IS NULL) RETURNING seq")
            .bind(id).bind(kind).bind(at).bind(payload).fetch_one(&mut *tx).await?;
        if let Some(reason) = close_reason {
            sqlx::query("UPDATE incidents SET closed_at=?2,close_reason=?3 WHERE id=?1 AND closed_at IS NULL")
                .bind(id).bind(at).bind(reason).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(seq)
    }
    /// Enrichment never changes the frozen observations or the logical order.
    pub async fn enrich_frame(&self, id: i64, seq: i64, payload: &str) -> AppResult<()> {
        sqlx::query("UPDATE incident_frames SET payload=?3 WHERE incident_id=?1 AND seq=?2")
            .bind(id)
            .bind(seq)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    /// Caller serializes observations and applies the snapshotted comparator.
    pub async fn set_worst(&self, id: i64, value: f64) -> AppResult<()> {
        sqlx::query("UPDATE incidents SET worst_value=?2 WHERE id=?1 AND closed_at IS NULL")
            .bind(id)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn close(&self, id: i64, at: i64, reason: &str) -> AppResult<()> {
        sqlx::query(
            "UPDATE incidents SET closed_at=?2, close_reason=?3 WHERE id=?1 AND closed_at IS NULL",
        )
        .bind(id)
        .bind(at)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn close_all_open(&self, at: i64, reason: &str) -> AppResult<u64> {
        sqlx::query("UPDATE incident_frames SET payload=json_set(payload,'$.enrichment','interrupted') WHERE json_valid(payload) AND json_extract(payload,'$.enrichment')='pending'").execute(&self.pool).await?;
        Ok(sqlx::query(
            "UPDATE incidents SET closed_at=?1, close_reason=?2 WHERE closed_at IS NULL",
        )
        .bind(at)
        .bind(reason)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
    pub async fn list(&self, limit: u32) -> AppResult<Vec<IncidentSummaryRow>> {
        Ok(sqlx::query_as("SELECT i.*, (SELECT COUNT(*) FROM incident_frames f WHERE f.incident_id = i.id) AS frame_count FROM incidents i ORDER BY i.opened_at DESC, i.id DESC LIMIT ?")
            .bind(limit).fetch_all(&self.pool).await?)
    }
    pub async fn list_range(
        &self,
        start: i64,
        end: i64,
        alert_triggered: Option<bool>,
        limit: u32,
        cursor: Option<(i64, i64)>,
    ) -> AppResult<Vec<IncidentSummaryRow>> {
        let (ts, id) = cursor.map_or((None, None), |(t, i)| (Some(t), Some(i)));
        Ok(sqlx::query_as("SELECT i.*, (SELECT COUNT(*) FROM incident_frames f WHERE f.incident_id = i.id) AS frame_count FROM incidents i WHERE i.opened_at>=?1 AND i.opened_at<=?2 AND (?3 IS NULL OR (?3=1 AND trigger_kind='alert') OR (?3=0 AND trigger_kind<>'alert')) AND (?4 IS NULL OR i.opened_at<?4 OR (i.opened_at=?4 AND i.id<?5)) ORDER BY i.opened_at DESC, i.id DESC LIMIT ?6")
            .bind(start).bind(end).bind(alert_triggered).bind(ts).bind(id).bind(limit).fetch_all(&self.pool).await?)
    }
    pub async fn get(&self, id: i64) -> AppResult<Option<IncidentRow>> {
        let row: Option<IncidentSummaryRow> = sqlx::query_as("SELECT i.*, (SELECT COUNT(*) FROM incident_frames f WHERE f.incident_id = i.id) AS frame_count FROM incidents i WHERE i.id=?").bind(id).fetch_optional(&self.pool).await?;
        let Some(r) = row else { return Ok(None) };
        let frames = sqlx::query_as("SELECT seq, kind, captured_at, payload FROM incident_frames WHERE incident_id=? ORDER BY seq")
            .bind(id).fetch_all(&self.pool).await?;
        Ok(Some(IncidentRow {
            id: r.id,
            opened_at: r.opened_at,
            closed_at: r.closed_at,
            close_reason: r.close_reason,
            recovery_started_at: r.recovery_started_at,
            violation_count: r.violation_count,
            confirmation_count: r.confirmation_count,
            trigger_kind: r.trigger_kind,
            category: r.category,
            rule_name: r.rule_name,
            label_set: r.label_set,
            trigger_value: r.trigger_value,
            worst_value: r.worst_value,
            reason: r.reason,
            trigger_context: r.trigger_context,
            frames,
        }))
    }
}
