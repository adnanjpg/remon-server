//! Process-metrics repository — `metrics_process` writes.
//!
//! One row per (process-name group, write tick). The collector aggregates the
//! live snapshot by name and stores only the top-K groups by cpu and by
//! memory (union), so row volume is bounded by config, never by the host's
//! process table. Rows are rolled up raw→1m→5m→1h and aged out per the
//! `('process', …)` retention seeds — this module only appends raw samples.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// One name-group's aggregated resource snapshot for a single write tick.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessGroupRow {
    /// Process name (the group key; pids come and go underneath it).
    pub name: String,
    /// Live pids aggregated into this row at sample time.
    pub pid_count: i64,
    /// Sum of per-pid cpu percent across the group.
    pub cpu_percent: f64,
    /// Sum of per-pid RSS across the group.
    pub memory_bytes: i64,
    pub disk_read_bps: i64,
    pub disk_write_bps: i64,
}

pub struct ProcessMetricsRepository {
    pool: SqlitePool,
}

impl ProcessMetricsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Append one write tick. Same-(timestamp, name) collisions are dropped,
    /// mirroring the other metrics tables.
    pub async fn insert_tick(&self, timestamp: i64, rows: &[ProcessGroupRow]) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        for r in rows {
            sqlx::query!(
                "INSERT INTO metrics_process
                   (resolution, timestamp, name, pid_count,
                    cpu_percent, memory_bytes, disk_read_bps, disk_write_bps)
                 VALUES ('raw', ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(resolution, timestamp, name) DO NOTHING",
                timestamp,
                r.name,
                r.pid_count,
                r.cpu_percent,
                r.memory_bytes,
                r.disk_read_bps,
                r.disk_write_bps,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
