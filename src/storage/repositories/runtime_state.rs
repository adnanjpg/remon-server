//! Runtime-state repository — the daemon's cross-restart breadcrumb KV
//! (`runtime_state`): last seen host boot time, clean-shutdown marker,
//! sweep cursors. Not config (never operator-written), not metrics.

use sqlx::SqlitePool;

use crate::error::AppResult;

pub struct RuntimeStateRepository {
    pool: SqlitePool,
}

impl RuntimeStateRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, key: &str) -> AppResult<Option<String>> {
        let row = sqlx::query!("SELECT value FROM runtime_state WHERE key = ?", key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.value))
    }

    pub async fn set(&self, key: &str, value: &str) -> AppResult<()> {
        sqlx::query!(
            "INSERT INTO runtime_state (key, value, updated_at)
             VALUES (?, ?, unixepoch())
             ON CONFLICT(key) DO UPDATE
                SET value = excluded.value, updated_at = excluded.updated_at",
            key,
            value,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
