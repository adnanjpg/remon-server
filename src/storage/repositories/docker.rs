//! Container-metrics repository — `metrics_docker` writes.
//!
//! One row per (running container, tick). `container_id` holds the container
//! *name* (leading `/` stripped): a name persists across recreation (a new
//! image pull yields a new short-id but the same compose name), so alert
//! labels and time-series continuity survive restarts. Rows are rolled up
//! raw→1m→5m→1h and aged out per the `('docker', …)` retention seeds — this
//! module only appends raw samples.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// One container's resource snapshot for a single tick.
#[derive(Debug, Clone, PartialEq)]
pub struct DockerStatsRow {
    /// Container name (stable across recreation); stored in `container_id`.
    pub container_id: String,
    pub cpu_percent: f64,
    pub memory_used_bytes: i64,
    pub memory_limit_bytes: i64,
    pub network_rx_bytes: i64,
    pub network_tx_bytes: i64,
    pub block_read_bytes: i64,
    pub block_write_bytes: i64,
    pub pids: i64,
}

pub struct DockerMetricsRepository {
    pool: SqlitePool,
}

impl DockerMetricsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Append one collector tick. Same-(timestamp, container) collisions are
    /// dropped, mirroring the other metrics tables.
    pub async fn insert_tick(&self, timestamp: i64, rows: &[DockerStatsRow]) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        for r in rows {
            sqlx::query!(
                "INSERT INTO metrics_docker
                   (resolution, timestamp, container_id, cpu_percent,
                    memory_used_bytes, memory_limit_bytes,
                    network_rx_bytes, network_tx_bytes,
                    block_read_bytes, block_write_bytes, pids)
                 VALUES ('raw', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(resolution, timestamp, container_id) DO NOTHING",
                timestamp,
                r.container_id,
                r.cpu_percent,
                r.memory_used_bytes,
                r.memory_limit_bytes,
                r.network_rx_bytes,
                r.network_tx_bytes,
                r.block_read_bytes,
                r.block_write_bytes,
                r.pids,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
