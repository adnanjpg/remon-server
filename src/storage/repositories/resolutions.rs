use sqlx::SqlitePool;

use crate::error::AppResult;

/// One row in the `resolutions` table. The rollup task reads these to learn
/// which buckets exist and how they chain together (`rollup_from`).
#[derive(Debug, Clone, sqlx::FromRow)]
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
        let rows = sqlx::query_as!(
            Resolution,
            r#"SELECT name as "name!", interval_seconds, rollup_from,
                      enabled as "enabled: bool"
               FROM resolutions
              ORDER BY sort_order"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
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

    /// Flip a resolution on/off. Returns false when the name doesn't exist.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> AppResult<bool> {
        let result = sqlx::query!(
            "UPDATE resolutions SET enabled = ? WHERE name = ?",
            enabled,
            name
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}
