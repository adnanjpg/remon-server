use sqlx::SqlitePool;

use crate::error::AppResult;

/// Server-side runtime overrides loaded from the `server_config` table.
/// Layered on top of the TOML defaults at boot; DB always wins.
#[derive(Debug, Clone)]
pub struct RuntimeOverrides {
    pub server_name: String,
    pub collector_stats_interval_ms: u64,
    pub processes_cache_ttl_ms: u64,
    pub collector_docker_interval_ms: u64,
    pub collector_smart_interval_ms: u64,
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
    /// Read-only: set by SQL on every `update()`, ignored as input.
    pub updated_at: i64,
}

pub struct ConfigRepository {
    pool: SqlitePool,
}

impl ConfigRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn load(&self) -> AppResult<RuntimeOverrides> {
        let row = sqlx::query_as::<_, (String, i64, i64, i64, i64, i64, i64, i64)>(
            r#"
            SELECT server_name,
                   collector_stats_interval_ms,
                   collector_processes_interval_ms,
                   rollup_tick_interval_ms,
                   retention_tick_interval_ms,
                   collector_docker_interval_ms,
                   collector_smart_interval_ms,
                   updated_at
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
            collector_docker_interval_ms: row.5 as u64,
            collector_smart_interval_ms: row.6 as u64,
            updated_at: row.7,
        })
    }

    /// Persist the merged overrides; returns the new `updated_at`.
    pub async fn update(&self, c: &RuntimeOverrides) -> AppResult<i64> {
        let (updated_at,) = sqlx::query_as::<_, (i64,)>(
            r#"
            UPDATE server_config SET
                server_name                     = ?,
                collector_stats_interval_ms     = ?,
                collector_processes_interval_ms = ?,
                collector_docker_interval_ms    = ?,
                collector_smart_interval_ms     = ?,
                rollup_tick_interval_ms         = ?,
                retention_tick_interval_ms      = ?,
                updated_at                      = unixepoch()
            WHERE id = 1
            RETURNING updated_at
            "#,
        )
        .bind(&c.server_name)
        .bind(c.collector_stats_interval_ms as i64)
        .bind(c.processes_cache_ttl_ms as i64)
        .bind(c.collector_docker_interval_ms as i64)
        .bind(c.collector_smart_interval_ms as i64)
        .bind(c.rollup_tick_interval_ms as i64)
        .bind(c.retention_tick_interval_ms as i64)
        .fetch_one(&self.pool)
        .await?;

        Ok(updated_at)
    }
}
