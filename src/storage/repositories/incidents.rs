//! Incident repository — flight-recorder episodes and their frames.
//!
//! An incident is an episode with a duration: opened on the first threshold
//! crossing, closed when the rule resolves, and carrying one frame per moment
//! worth freezing in between. Frame payloads are opaque JSON assembled by
//! `services::incidents`; nothing in this module or above it parses them, so
//! the bundle's shape and its storage cannot drift apart.
//!
//! Rows age out via the `('incidents', 'raw', …)` retention seed, which never
//! reaps an open episode; `incident_frames` follows by `ON DELETE CASCADE`.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Listing row — the envelope, without any frame payloads.
#[derive(Debug, Clone)]
pub struct IncidentSummaryRow {
    pub id: i64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    pub peak_value: Option<f64>,
    pub reason: Option<String>,
    /// Frames captured so far — the cheap signal for "is there more here than
    /// the opening moment", which is what the old `has_after` flag meant.
    pub frame_count: i64,
}

/// One captured moment. `payload` stays a JSON string all the way out.
#[derive(Debug, Clone)]
pub struct IncidentFrameRow {
    pub seq: i64,
    pub kind: String,
    pub captured_at: i64,
    pub payload: String,
}

/// Full episode, frames included, ordered oldest first.
#[derive(Debug, Clone)]
pub struct IncidentRow {
    pub id: i64,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    pub peak_value: Option<f64>,
    pub reason: Option<String>,
    pub frames: Vec<IncidentFrameRow>,
}

/// Arguments for opening an episode. Alert-driven rows carry rule fields;
/// manual ones carry `reason`.
#[derive(Debug, Clone, Default)]
pub struct NewIncident {
    pub trigger_kind: &'static str,
    pub category: String,
    pub rule_id: Option<i64>,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub trigger_value: Option<f64>,
    pub reason: Option<String>,
}

pub struct IncidentRepository {
    pool: SqlitePool,
}

impl IncidentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open an episode. `peak_value` starts at the onset value, so an episode
    /// that never earns a peak frame still reports the worst it ever saw.
    pub async fn open(&self, n: &NewIncident) -> AppResult<i64> {
        let r = sqlx::query!(
            "INSERT INTO incidents
               (trigger_kind, category, rule_id, rule_name, label_set,
                trigger_value, peak_value, reason)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            n.trigger_kind,
            n.category,
            n.rule_id,
            n.rule_name,
            n.label_set,
            n.trigger_value,
            n.trigger_value,
            n.reason,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Append a frame, taking the next `seq` in the same statement that writes
    /// the row.
    ///
    /// Deliberately one statement rather than a read followed by an insert.
    /// Frame builders run concurrently — an onset frame doing a journal read
    /// overlaps the escalation frame behind it — and two of them reading
    /// `MAX(seq)` before either wrote would compute the same number, so the
    /// second insert lost its frame to the primary key. Under SQLite's write
    /// lock this form cannot interleave.
    pub async fn append_frame(
        &self,
        incident_id: i64,
        kind: &str,
        captured_at: i64,
        payload: &str,
    ) -> AppResult<()> {
        sqlx::query!(
            "INSERT INTO incident_frames (incident_id, seq, kind, captured_at, payload)
             SELECT ?1, COALESCE(MAX(seq), -1) + 1, ?2, ?3, ?4
               FROM incident_frames WHERE incident_id = ?1",
            incident_id,
            kind,
            captured_at,
            payload,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Raise the recorded peak. Compared in SQL rather than in Rust so the
    /// column stays monotonic even if two writers race.
    pub async fn raise_peak(&self, incident_id: i64, value: f64) -> AppResult<()> {
        sqlx::query!(
            "UPDATE incidents SET peak_value = MAX(COALESCE(peak_value, ?2), ?2)
              WHERE id = ?1",
            incident_id,
            value,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Close an episode. An already-closed row is left alone, so a resolve
    /// racing the boot sweep cannot overwrite the earlier reason.
    pub async fn close(&self, incident_id: i64, at: i64, reason: &str) -> AppResult<()> {
        sqlx::query!(
            "UPDATE incidents SET closed_at = ?2, close_reason = ?3
              WHERE id = ?1 AND closed_at IS NULL",
            incident_id,
            at,
            reason,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The open episode for a rule key, if one is live.
    pub async fn open_episode_for(
        &self,
        rule_id: i64,
        label_set: &str,
    ) -> AppResult<Option<(i64, i64)>> {
        let row = sqlx::query!(
            r#"SELECT id as "id!", opened_at as "opened_at!"
                 FROM incidents
                WHERE rule_id = ? AND label_set = ? AND closed_at IS NULL
                ORDER BY opened_at DESC LIMIT 1"#,
            rule_id,
            label_set,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.id, r.opened_at)))
    }

    /// Close every episode still open. Run at boot: the process that would
    /// have closed them is gone, and leaving them open would both block
    /// retention and make the next crossing look like a continuation.
    pub async fn close_all_open(&self, at: i64, reason: &str) -> AppResult<u64> {
        let r = sqlx::query!(
            "UPDATE incidents SET closed_at = ?1, close_reason = ?2 WHERE closed_at IS NULL",
            at,
            reason,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Newest episode start for a (rule, label_set) — the flap cooldown asks
    /// whether a fresh crossing deserves an episode of its own.
    pub async fn latest_alert_capture(
        &self,
        rule_id: i64,
        label_set: &str,
    ) -> AppResult<Option<i64>> {
        let row = sqlx::query!(
            r#"SELECT MAX(opened_at) as "ts: i64" FROM incidents
               WHERE rule_id = ? AND label_set = ?"#,
            rule_id,
            label_set,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.ts)
    }

    pub async fn list(&self, limit: u32) -> AppResult<Vec<IncidentSummaryRow>> {
        let rows = sqlx::query!(
            r#"SELECT i.id as "id!", i.opened_at as "opened_at!", i.closed_at,
                      i.close_reason,
                      i.trigger_kind as "trigger_kind!", i.category as "category!",
                      i.rule_name, i.label_set, i.trigger_value, i.peak_value, i.reason,
                      (SELECT COUNT(*) FROM incident_frames f
                        WHERE f.incident_id = i.id) as "frame_count!: i64"
               FROM incidents i
              ORDER BY i.opened_at DESC, i.id DESC
              LIMIT ?"#,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| IncidentSummaryRow {
                id: r.id,
                opened_at: r.opened_at,
                closed_at: r.closed_at,
                close_reason: r.close_reason,
                trigger_kind: r.trigger_kind,
                category: r.category,
                rule_name: r.rule_name,
                label_set: r.label_set,
                trigger_value: r.trigger_value,
                peak_value: r.peak_value,
                reason: r.reason,
                frame_count: r.frame_count,
            })
            .collect())
    }

    /// Range slice for the `GET /events` union timeline — envelopes only,
    /// newest first; frames stay behind the incident detail endpoint.
    /// `alert_triggered` restricts to alert-driven (`Some(true)`) or everything
    /// else (`Some(false)`), which is how `/events` splits these between the
    /// `system` and `operator` sources. Expressed as `= 'alert'` / `<> 'alert'`
    /// rather than a list of the other kinds, so a trigger kind added later
    /// keeps landing on the same side of the split as the projection puts it.
    /// Applied here rather than by the caller: filtering after `LIMIT` returns
    /// nothing at all once the unwanted side fills the window on its own.
    pub async fn list_range(
        &self,
        start: i64,
        end: i64,
        alert_triggered: Option<bool>,
        limit: u32,
        cursor: Option<(i64, i64)>,
    ) -> AppResult<Vec<IncidentSummaryRow>> {
        let (cur_ts, cur_id) = match cursor {
            Some((ts, id)) => (Some(ts), Some(id)),
            None => (None, None),
        };
        let rows = sqlx::query!(
            r#"SELECT i.id as "id!", i.opened_at as "opened_at!", i.closed_at,
                      i.close_reason,
                      i.trigger_kind as "trigger_kind!", i.category as "category!",
                      i.rule_name, i.label_set, i.trigger_value, i.peak_value, i.reason,
                      (SELECT COUNT(*) FROM incident_frames f
                        WHERE f.incident_id = i.id) as "frame_count!: i64"
               FROM incidents i
              WHERE i.opened_at >= ?1 AND i.opened_at <= ?2
                AND (?3 IS NULL
                     OR (?3 = 1 AND i.trigger_kind =  'alert')
                     OR (?3 = 0 AND i.trigger_kind <> 'alert'))
                AND (?5 IS NULL
                     OR i.opened_at < ?5
                     OR (i.opened_at = ?5 AND i.id < ?6))
              ORDER BY i.opened_at DESC, i.id DESC
              LIMIT ?4"#,
            start,
            end,
            alert_triggered,
            limit,
            cur_ts,
            cur_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| IncidentSummaryRow {
                id: r.id,
                opened_at: r.opened_at,
                closed_at: r.closed_at,
                close_reason: r.close_reason,
                trigger_kind: r.trigger_kind,
                category: r.category,
                rule_name: r.rule_name,
                label_set: r.label_set,
                trigger_value: r.trigger_value,
                peak_value: r.peak_value,
                reason: r.reason,
                frame_count: r.frame_count,
            })
            .collect())
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<IncidentRow>> {
        let row = sqlx::query!(
            r#"SELECT id as "id!", opened_at as "opened_at!", closed_at, close_reason,
                      trigger_kind as "trigger_kind!", category as "category!",
                      rule_name, label_set, trigger_value, peak_value, reason
               FROM incidents WHERE id = ?"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };

        // Chronological, not insertion order. `seq` is assigned when a frame is
        // *written* and the builders run concurrently — an onset frame doing a
        // journal read can land after the escalation frame that followed it.
        // `captured_at` is stamped when the moment was taken, so it is the one
        // that replays the episode correctly; `seq` only breaks ties.
        let frames = sqlx::query!(
            r#"SELECT seq as "seq!", kind as "kind!", captured_at as "captured_at!",
                      payload as "payload!"
                 FROM incident_frames WHERE incident_id = ?
                ORDER BY captured_at ASC, seq ASC"#,
            id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|f| IncidentFrameRow {
            seq: f.seq,
            kind: f.kind,
            captured_at: f.captured_at,
            payload: f.payload,
        })
        .collect();

        Ok(Some(IncidentRow {
            id: r.id,
            opened_at: r.opened_at,
            closed_at: r.closed_at,
            close_reason: r.close_reason,
            trigger_kind: r.trigger_kind,
            category: r.category,
            rule_name: r.rule_name,
            label_set: r.label_set,
            trigger_value: r.trigger_value,
            peak_value: r.peak_value,
            reason: r.reason,
            frames,
        }))
    }
}
