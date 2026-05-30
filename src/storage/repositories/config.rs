use sqlx::SqlitePool;

use crate::error::AppResult;

/// Server-side runtime overrides loaded from the `server_config` table.
/// Layered on top of the TOML defaults at boot; DB always wins.
#[derive(Debug, Clone)]
pub struct RuntimeOverrides {
    pub server_name: String,
    pub collector_stats_interval_ms: u64,
    pub processes_cache_ttl_ms: u64,
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
}

pub struct ConfigRepository {
    pool: SqlitePool,
}

impl ConfigRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // NOTE: the `collector_docker_interval_ms` column still exists in
    // `server_config` (reserved for a future container-stats collector) but is
    // intentionally not read or written here — nothing consumes it today, so
    // exposing it via /config would be a silent no-op for API clients.
    pub async fn load(&self) -> AppResult<RuntimeOverrides> {
        let row = sqlx::query_as::<_, (String, i64, i64, i64, i64)>(
            r#"
            SELECT server_name,
                   collector_stats_interval_ms,
                   collector_processes_interval_ms,
                   rollup_tick_interval_ms,
                   retention_tick_interval_ms
              FROM server_config WHERE id = 1
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(RuntimeOverrides {
            server_name: row.0,
            collector_stats_interval_ms: row.1 as u64,
            processes_cache_ttl_ms: row.2 as u64,
            rollup_tick_interval_ms: row.3 as u64,
            retention_tick_interval_ms: row.4 as u64,
        })
    }

    pub async fn update(&self, c: &RuntimeOverrides) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE server_config SET
                server_name                     = ?,
                collector_stats_interval_ms     = ?,
                collector_processes_interval_ms = ?,
                rollup_tick_interval_ms         = ?,
                retention_tick_interval_ms      = ?,
                updated_at                      = unixepoch()
            WHERE id = 1
            "#,
        )
        .bind(&c.server_name)
        .bind(c.collector_stats_interval_ms as i64)
        .bind(c.processes_cache_ttl_ms as i64)
        .bind(c.rollup_tick_interval_ms as i64)
        .bind(c.retention_tick_interval_ms as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
