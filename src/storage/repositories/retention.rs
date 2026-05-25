use sqlx::SqlitePool;

use crate::error::AppResult;

/// One row of `retention_policy`. Read by the retention task to know how
/// many seconds of data to keep for each (resource, resolution) bucket.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RetentionPolicy {
    pub resource: String,
    pub resolution: String,
    pub keep_seconds: i64,
}

pub struct RetentionRepository {
    pool: SqlitePool,
}

impl RetentionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_all(&self) -> AppResult<Vec<RetentionPolicy>> {
        let rows = sqlx::query_as!(
            RetentionPolicy,
            "SELECT resource, resolution, keep_seconds FROM retention_policy"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
