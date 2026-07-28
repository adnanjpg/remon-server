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
        if rows.is_empty() {
            return Ok(());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "INSERT INTO metrics_process \
             (resolution, timestamp, name, pid_count, \
              cpu_percent, memory_bytes, disk_read_bps, disk_write_bps) ",
        );
        qb.push_values(rows.iter(), |mut b, r| {
            b.push_bind("raw")
                .push_bind(timestamp)
                .push_bind(&r.name)
                .push_bind(r.pid_count)
                .push_bind(r.cpu_percent)
                .push_bind(r.memory_bytes)
                .push_bind(r.disk_read_bps)
                .push_bind(r.disk_write_bps);
        });
        qb.push(" ON CONFLICT(resolution, timestamp, name) DO NOTHING");
        qb.build().execute(&self.pool).await?;
        Ok(())
    }
}
