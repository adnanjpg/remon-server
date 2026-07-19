//! Incident-snapshot repository — flight-recorder rows.
//!
//! Bundles are opaque JSON assembled by `services::incidents`; this module
//! only stores, amends (`after_bundle`), and reads them. Rows age out via the
//! `('incident_snapshots', 'raw', …)` retention seed.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Listing row — everything except the (potentially large) bundles.
#[derive(Debug, Clone)]
pub struct IncidentSummaryRow {
    pub id: i64,
    pub created_at: i64,
    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub metric_value: Option<f64>,
    pub reason: Option<String>,
    pub has_after: bool,
}

/// Full row, bundles included.
#[derive(Debug, Clone)]
pub struct IncidentRow {
    pub id: i64,
    pub created_at: i64,
    pub trigger_kind: String,
    pub category: String,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub metric_value: Option<f64>,
    pub reason: Option<String>,
    pub bundle: String,
    pub after_bundle: Option<String>,
}

/// Arguments for a new capture. Alert-driven rows carry rule fields;
/// manual ones carry `reason`.
#[derive(Debug, Clone, Default)]
pub struct NewIncident {
    pub trigger_kind: &'static str,
    pub category: String,
    pub rule_id: Option<i64>,
    pub rule_name: Option<String>,
    pub label_set: Option<String>,
    pub metric_value: Option<f64>,
    pub reason: Option<String>,
    pub bundle: String,
}

pub struct IncidentRepository {
    pool: SqlitePool,
}

impl IncidentRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, n: &NewIncident) -> AppResult<i64> {
        let r = sqlx::query!(
            "INSERT INTO incident_snapshots
               (trigger_kind, category, rule_id, rule_name, label_set,
                metric_value, reason, bundle)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            n.trigger_kind,
            n.category,
            n.rule_id,
            n.rule_name,
            n.label_set,
            n.metric_value,
            n.reason,
            n.bundle,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// Attach the delayed follow-up bundle. A missing row (aged out or
    /// deleted meanwhile) is a silent no-op.
    pub async fn set_after(&self, id: i64, after_bundle: &str) -> AppResult<()> {
        sqlx::query!(
            "UPDATE incident_snapshots SET after_bundle = ? WHERE id = ?",
            after_bundle,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Newest capture timestamp for a (rule, label_set) — the flap-cooldown
    /// check: a fresh transition within the window skips re-capturing.
    pub async fn latest_alert_capture(
        &self,
        rule_id: i64,
        label_set: &str,
    ) -> AppResult<Option<i64>> {
        let row = sqlx::query!(
            r#"SELECT MAX(created_at) as "ts: i64" FROM incident_snapshots
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
            r#"SELECT id as "id!", created_at as "created_at!",
                      trigger_kind as "trigger_kind!", category as "category!",
                      rule_name, label_set, metric_value, reason,
                      (after_bundle IS NOT NULL) as "has_after!: bool"
               FROM incident_snapshots
              ORDER BY created_at DESC, id DESC
              LIMIT ?"#,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| IncidentSummaryRow {
                id: r.id,
                created_at: r.created_at,
                trigger_kind: r.trigger_kind,
                category: r.category,
                rule_name: r.rule_name,
                label_set: r.label_set,
                metric_value: r.metric_value,
                reason: r.reason,
                has_after: r.has_after,
            })
            .collect())
    }

    /// Range slice for the `GET /events` union timeline — summary rows only,
    /// newest first; the bundle stays behind the incident detail endpoint.
    pub async fn list_range(
        &self,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<IncidentSummaryRow>> {
        let rows = sqlx::query!(
            r#"SELECT id as "id!", created_at as "created_at!",
                      trigger_kind as "trigger_kind!", category as "category!",
                      rule_name, label_set, metric_value, reason,
                      (after_bundle IS NOT NULL) as "has_after!: bool"
               FROM incident_snapshots
              WHERE created_at >= ? AND created_at <= ?
              ORDER BY created_at DESC, id DESC
              LIMIT ?"#,
            start,
            end,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| IncidentSummaryRow {
                id: r.id,
                created_at: r.created_at,
                trigger_kind: r.trigger_kind,
                category: r.category,
                rule_name: r.rule_name,
                label_set: r.label_set,
                metric_value: r.metric_value,
                reason: r.reason,
                has_after: r.has_after,
            })
            .collect())
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<IncidentRow>> {
        let row = sqlx::query!(
            r#"SELECT id as "id!", created_at as "created_at!",
                      trigger_kind as "trigger_kind!", category as "category!",
                      rule_name, label_set, metric_value, reason,
                      bundle as "bundle!", after_bundle
               FROM incident_snapshots WHERE id = ?"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| IncidentRow {
            id: r.id,
            created_at: r.created_at,
            trigger_kind: r.trigger_kind,
            category: r.category,
            rule_name: r.rule_name,
            label_set: r.label_set,
            metric_value: r.metric_value,
            reason: r.reason,
            bundle: r.bundle,
            after_bundle: r.after_bundle,
        }))
    }
}
