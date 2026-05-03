use sqlx::SqlitePool;

use crate::error::AppResult;

#[derive(Debug, sqlx::FromRow)]
pub struct NotificationChannelRow {
    pub id: i64,
    pub name: String,
    pub r#type: String,
    pub enabled: bool,
    pub config: String,
    pub min_severity: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NotificationChannelRepository {
    pool: SqlitePool,
}

impl NotificationChannelRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_enabled(&self) -> AppResult<Vec<NotificationChannelRow>> {
        let rows = sqlx::query_as::<_, NotificationChannelRow>(
            "SELECT id, name, type, enabled, config, min_severity, created_at, updated_at
               FROM notification_channels
              WHERE enabled = 1
              ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn list_all(&self) -> AppResult<Vec<NotificationChannelRow>> {
        let rows = sqlx::query_as::<_, NotificationChannelRow>(
            "SELECT id, name, type, enabled, config, min_severity, created_at, updated_at
               FROM notification_channels
              ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get(&self, id: i64) -> AppResult<Option<NotificationChannelRow>> {
        let row = sqlx::query_as::<_, NotificationChannelRow>(
            "SELECT id, name, type, enabled, config, min_severity, created_at, updated_at
               FROM notification_channels
              WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn insert(
        &self,
        name: &str,
        channel_type: &str,
        enabled: bool,
        config: &str,
        min_severity: Option<&str>,
    ) -> AppResult<i64> {
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO notification_channels (name, type, enabled, config, min_severity)
             VALUES (?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(name)
        .bind(channel_type)
        .bind(enabled)
        .bind(config)
        .bind(min_severity)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn update(
        &self,
        id: i64,
        name: &str,
        enabled: bool,
        config: &str,
        min_severity: Option<&str>,
    ) -> AppResult<bool> {
        let rows = sqlx::query(
            "UPDATE notification_channels
                SET name = ?, enabled = ?, config = ?, min_severity = ?,
                    updated_at = unixepoch()
              WHERE id = ?",
        )
        .bind(name)
        .bind(enabled)
        .bind(config)
        .bind(min_severity)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows > 0)
    }

    pub async fn delete(&self, id: i64) -> AppResult<bool> {
        let rows = sqlx::query("DELETE FROM notification_channels WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(rows > 0)
    }
}
