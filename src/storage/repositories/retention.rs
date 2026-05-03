use sqlx::SqlitePool;

use crate::error::AppResult;

/// One row of `retention_policy`. Read by the retention task to know how
/// many seconds of data to keep for each (resource, resolution) bucket.
#[derive(Debug, Clone)]
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
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT resource, resolution, keep_seconds FROM retention_policy",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| RetentionPolicy {
                resource: r.0,
                resolution: r.1,
                keep_seconds: r.2,
            })
            .collect())
    }

    pub async fn upsert(&self, p: &RetentionPolicy) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO retention_policy (resource, resolution, keep_seconds)
            VALUES (?, ?, ?)
            ON CONFLICT(resource, resolution) DO UPDATE SET
                keep_seconds = excluded.keep_seconds
            "#,
        )
        .bind(&p.resource)
        .bind(&p.resolution)
        .bind(p.keep_seconds)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
