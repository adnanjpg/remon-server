use sqlx::SqlitePool;

use crate::error::AppResult;

/// One row in the `resolutions` table. The rollup task reads these to learn
/// which buckets exist and how they chain together (`rollup_from`).
#[derive(Debug, Clone)]
pub struct Resolution {
    pub name: String,
    pub interval_seconds: i64,
    pub rollup_from: Option<String>,
    pub enabled: bool,
}

pub struct ResolutionRepository {
    pool: SqlitePool,
}

impl ResolutionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_all(&self) -> AppResult<Vec<Resolution>> {
        let rows = sqlx::query_as::<_, (String, i64, Option<String>, bool)>(
            r#"
            SELECT name, interval_seconds, rollup_from, enabled
              FROM resolutions
             ORDER BY sort_order
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Resolution {
                name: r.0,
                interval_seconds: r.1,
                rollup_from: r.2,
                enabled: r.3,
            })
            .collect())
    }

    /// Resolutions that have a parent — i.e. the buckets the rollup task
    /// must populate.
    pub async fn list_rollup_targets(&self) -> AppResult<Vec<Resolution>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .filter(|r| r.enabled && r.rollup_from.is_some())
            .collect())
    }
}
