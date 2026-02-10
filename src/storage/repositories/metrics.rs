use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::stats::{CpuStats, MemoryStats, DiskStats, NetworkStats, LoadAverage};

pub struct MetricsRepository {
    pool: SqlitePool,
}

impl MetricsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ==================== CPU ====================

    pub async fn insert_cpu(&self, stats: &CpuStats) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO metrics_cpu (timestamp, usage_percent, load_1m, load_5m, load_15m)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(stats.timestamp)
        .bind(stats.usage_percent)
        .bind(stats.load_avg.one)
        .bind(stats.load_avg.five)
        .bind(stats.load_avg.fifteen)
        .execute(&self.pool)
        .await?;

        // Insert per-core stats
        for core in &stats.per_core {
            sqlx::query(
                r#"
                INSERT INTO metrics_cpu_cores (timestamp, core_index, usage_percent, freq_mhz)
                VALUES (?, ?, ?, ?)
                "#,
            )
            .bind(stats.timestamp)
            .bind(core.core_index as i32)
            .bind(core.usage_percent)
            .bind(core.freq_mhz as i64)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    pub async fn get_cpu_history(&self, since: i64, limit: u32) -> AppResult<Vec<CpuStats>> {
        let rows = sqlx::query_as::<_, (i64, f64, f64, f64, f64)>(
            r#"
            SELECT timestamp, usage_percent, load_1m, load_5m, load_15m
            FROM metrics_cpu
            WHERE timestamp >= ?
            ORDER BY timestamp DESC
            LIMIT ?
            "#,
        )
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            results.push(CpuStats {
                usage_percent: row.1,
                per_core: vec![], // Could load separately if needed
                load_avg: LoadAverage {
                    one: row.2,
                    five: row.3,
                    fifteen: row.4,
                },
                timestamp: row.0,
            });
        }

        Ok(results)
    }

    // ==================== Memory ====================

    pub async fn insert_memory(&self, stats: &MemoryStats) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO metrics_memory (timestamp, used_bytes, available_bytes, cached_bytes, swap_used_bytes)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(stats.timestamp)
        .bind(stats.used_bytes as i64)
        .bind(stats.available_bytes as i64)
        .bind(stats.cached_bytes as i64)
        .bind(stats.swap_used_bytes as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_memory_history(&self, since: i64, limit: u32) -> AppResult<Vec<MemoryStats>> {
        let rows = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            r#"
            SELECT timestamp, used_bytes, available_bytes, cached_bytes, swap_used_bytes
            FROM metrics_memory
            WHERE timestamp >= ?
            ORDER BY timestamp DESC
            LIMIT ?
            "#,
        )
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| MemoryStats {
                total_bytes: 0, // Not stored, from hardware info
                used_bytes: r.1 as u64,
                available_bytes: r.2 as u64,
                cached_bytes: r.3 as u64,
                swap_total_bytes: 0,
                swap_used_bytes: r.4 as u64,
                timestamp: r.0,
            })
            .collect())
    }

    // ==================== Disk ====================

    pub async fn insert_disk(&self, stats: &[DiskStats]) -> AppResult<()> {
        for stat in stats {
            sqlx::query(
                r#"
                INSERT INTO metrics_disk (timestamp, mount_point, used_bytes, available_bytes, read_bytes_per_sec, write_bytes_per_sec)
                VALUES (?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(stat.timestamp)
            .bind(&stat.mount_point)
            .bind(stat.used_bytes as i64)
            .bind(stat.available_bytes as i64)
            .bind(stat.read_bytes_per_sec as i64)
            .bind(stat.write_bytes_per_sec as i64)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    // ==================== Network ====================

    pub async fn insert_network(&self, stats: &[NetworkStats]) -> AppResult<()> {
        for stat in stats {
            sqlx::query(
                r#"
                INSERT INTO metrics_network (timestamp, interface_name, rx_bytes_per_sec, tx_bytes_per_sec, rx_packets_per_sec, tx_packets_per_sec)
                VALUES (?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(stat.timestamp)
            .bind(&stat.interface)
            .bind(stat.rx_bytes_per_sec as i64)
            .bind(stat.tx_bytes_per_sec as i64)
            .bind(stat.rx_packets_per_sec as i64)
            .bind(stat.tx_packets_per_sec as i64)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    // ==================== Cleanup ====================

    pub async fn cleanup_old_metrics(&self, retention_days: u32) -> AppResult<u64> {
        let cutoff = chrono::Utc::now().timestamp() - (retention_days as i64 * 24 * 3600);
        let mut total = 0u64;

        let tables = ["metrics_cpu", "metrics_cpu_cores", "metrics_memory", "metrics_disk", "metrics_network", "metrics_docker"];

        for table in tables {
            let result = sqlx::query(&format!("DELETE FROM {} WHERE timestamp < ?", table))
                .bind(cutoff)
                .execute(&self.pool)
                .await?;
            total += result.rows_affected();
        }

        Ok(total)
    }
}
