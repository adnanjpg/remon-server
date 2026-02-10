use sqlx::SqlitePool;

use crate::config::RuntimeConfig;
use crate::error::AppResult;

pub struct ConfigRepository {
    pool: SqlitePool,
}

impl ConfigRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_runtime_config(&self) -> AppResult<Option<RuntimeConfig>> {
        let row = sqlx::query_as::<_, (String, i64, i64, i64, i32, i32)>(
            r#"
            SELECT server_name, stats_interval_ms, process_interval_ms,
                   docker_interval_ms, metrics_retention_days, logs_retention_days
            FROM server_config WHERE id = 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| RuntimeConfig {
            server_name: r.0,
            stats_interval_ms: r.1 as u64,
            process_interval_ms: r.2 as u64,
            docker_interval_ms: r.3 as u64,
            metrics_retention_days: r.4 as u32,
            logs_retention_days: r.5 as u32,
        }))
    }

    pub async fn update_runtime_config(&self, config: &RuntimeConfig) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE server_config SET
                server_name = ?,
                stats_interval_ms = ?,
                process_interval_ms = ?,
                docker_interval_ms = ?,
                metrics_retention_days = ?,
                logs_retention_days = ?,
                updated_at = unixepoch()
            WHERE id = 1
            "#,
        )
        .bind(&config.server_name)
        .bind(config.stats_interval_ms as i64)
        .bind(config.process_interval_ms as i64)
        .bind(config.docker_interval_ms as i64)
        .bind(config.metrics_retention_days as i32)
        .bind(config.logs_retention_days as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
