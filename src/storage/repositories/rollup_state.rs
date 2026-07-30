use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Bookkeeping cursors: how far the rollup has progressed per
/// (resource, resolution), so a restart resumes rather than rebuilding history.
pub struct RollupStateRepository {
    pool: SqlitePool,
}

impl RollupStateRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Every cursor, keyed `(resource, resolution)`. One read per tick instead
    /// of one per pair: the table holds a couple of dozen integers and the
    /// rollup wants all of them, its own and its parents'.
    pub async fn load_all(&self) -> AppResult<HashMap<(String, String), i64>> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT resource, resolution, last_bucket_ts FROM rollup_state")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(resource, resolution, ts)| ((resource, resolution), ts))
            .collect())
    }

    pub async fn set(
        &self,
        resource: &str,
        resolution: &str,
        last_bucket_ts: i64,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO rollup_state (resource, resolution, last_bucket_ts, last_run_at)
            VALUES (?, ?, ?, unixepoch())
            ON CONFLICT(resource, resolution) DO UPDATE SET
                last_bucket_ts = excluded.last_bucket_ts,
                last_run_at    = unixepoch()
            "#,
        )
        .bind(resource)
        .bind(resolution)
        .bind(last_bucket_ts)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
