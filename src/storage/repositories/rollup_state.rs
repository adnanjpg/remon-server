use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Bookkeeping cursors: how far the rollup has progressed per
/// (resource, resolution), so a restart resumes rather than rebuilding history.
#[derive(Clone, Copy, Debug)]
pub struct RollupProgress {
    pub processed_from: Option<i64>,
    pub last_bucket_ts: i64,
}

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
    pub async fn load_all(&self) -> AppResult<HashMap<(String, String), RollupProgress>> {
        let rows: Vec<(String, String, Option<i64>, i64)> = sqlx::query_as(
            "SELECT resource, resolution, processed_from, last_bucket_ts FROM rollup_state",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(resource, resolution, processed_from, last_bucket_ts)| {
                (
                    (resource, resolution),
                    RollupProgress {
                        processed_from,
                        last_bucket_ts,
                    },
                )
            })
            .collect())
    }

    pub async fn set(
        &self,
        resource: &str,
        resolution: &str,
        processed_from: i64,
        last_bucket_ts: i64,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO rollup_state (resource, resolution, processed_from, last_bucket_ts, last_run_at)
            VALUES (?, ?, ?, ?, unixepoch())
            ON CONFLICT(resource, resolution) DO UPDATE SET
                processed_from = excluded.processed_from,
                last_bucket_ts = excluded.last_bucket_ts,
                last_run_at    = unixepoch()
            "#,
        )
        .bind(resource)
        .bind(resolution)
        .bind(processed_from)
        .bind(last_bucket_ts)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
