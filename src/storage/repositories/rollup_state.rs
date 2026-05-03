use sqlx::SqlitePool;

use crate::error::AppResult;

/// Bookkeeping cursor: how far has the rollup task progressed for a given
/// (resource, resolution)? Tracked so that, after a restart, the task can
/// resume from `last_bucket_ts` rather than rebuilding the whole history,
/// and after extended downtime it can clamp how far back it tries to go.
#[derive(Debug, Clone)]
pub struct RollupCursor {
    pub resource: String,
    pub resolution: String,
    pub last_bucket_ts: i64,
}

pub struct RollupStateRepository {
    pool: SqlitePool,
}

impl RollupStateRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, resource: &str, resolution: &str) -> AppResult<RollupCursor> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT last_bucket_ts FROM rollup_state WHERE resource = ? AND resolution = ?",
        )
        .bind(resource)
        .bind(resolution)
        .fetch_optional(&self.pool)
        .await?;

        Ok(RollupCursor {
            resource: resource.to_string(),
            resolution: resolution.to_string(),
            last_bucket_ts: row.map(|r| r.0).unwrap_or(0),
        })
    }

    pub async fn set(&self, resource: &str, resolution: &str, last_bucket_ts: i64) -> AppResult<()> {
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
