use log::warn;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::stats::{
    ComponentsSnapshot, CpuStats, DiskStats, MemoryStats, NetworkStats, PressureSnapshot,
};

pub struct MetricsRepository {
    pool: SqlitePool,
}

/// Surface a collision when an `ON CONFLICT DO NOTHING` insert produced
/// fewer rows than the batch carried. With the 1s sampling floor (admin.rs)
/// and second-resolution timestamps this should be unreachable in normal
/// flow — a warn-level log makes any future regression visible instead of
/// being silently swallowed by `INSERT OR REPLACE`.
fn note_collision(table: &str, expected: u64, affected: u64, ts: i64, resolution: &str) {
    if affected < expected {
        warn!(
            "metric collision on {}: ts={} resolution={} expected={} affected={} \
             (a same-timestamp row already exists; new row dropped)",
            table, ts, resolution, expected, affected,
        );
    }
}

impl MetricsRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persist one collector tick (CPU host-level + per-core, memory, disks,
    /// networks) inside a single transaction. All goes into `resolution = 'raw'`.
    /// Failure rolls everything back so partial frames don't leak into the
    /// rollup pipeline.
    pub async fn insert_raw_tick(
        &self,
        cpu: &CpuStats,
        memory: &MemoryStats,
        disks: &[DiskStats],
        networks: &[NetworkStats],
        pressure: Option<&PressureSnapshot>,
        components: Option<&ComponentsSnapshot>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;

        let ctx_switches = cpu.context_switches_per_sec.map(|v| v as i64);
        let proc_forks = cpu.process_forks_per_sec.map(|v| v as i64);
        let r = sqlx::query!(
            "INSERT INTO metrics_cpu
               (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m,
                steal_percent, iowait_percent, guest_percent,
                user_percent, system_percent,
                context_switches_per_sec, process_forks_per_sec)
             VALUES ('raw', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(resolution, timestamp) DO NOTHING",
            cpu.timestamp,
            cpu.usage_percent,
            cpu.load_avg.one,
            cpu.load_avg.five,
            cpu.load_avg.fifteen,
            cpu.steal_percent,
            cpu.iowait_percent,
            cpu.guest_percent,
            cpu.user_percent,
            cpu.system_percent,
            ctx_switches,
            proc_forks,
        )
        .execute(&mut *tx)
        .await?;
        note_collision("metrics_cpu", 1, r.rows_affected(), cpu.timestamp, "raw");

        // `metrics_cpu_cores` deliberately has no `resolution` column —
        // rolling up per-core would multiply row count by core_count × N
        // rollup intervals. UI shows per-core only on the live tail; rolled
        // history is served from the host-level `metrics_cpu` table.
        if !cpu.per_core.is_empty() {
            let mut qb = sqlx::QueryBuilder::new(
                "INSERT INTO metrics_cpu_cores (timestamp, core_index, usage_percent, freq_mhz) ",
            );
            qb.push_values(cpu.per_core.iter(), |mut b, core| {
                b.push_bind(cpu.timestamp)
                    .push_bind(core.core_index as i64)
                    .push_bind(core.usage_percent)
                    .push_bind(core.freq_mhz as i64);
            });
            qb.push(" ON CONFLICT(timestamp, core_index) DO NOTHING");
            let r = qb.build().execute(&mut *tx).await?;
            note_collision(
                "metrics_cpu_cores",
                cpu.per_core.len() as u64,
                r.rows_affected(),
                cpu.timestamp,
                "raw",
            );
        }

        let pf_minor = memory.page_faults_minor_per_sec.map(|v| v as i64);
        let pf_major = memory.page_faults_major_per_sec.map(|v| v as i64);
        let swap_in = memory.swap_in_pages_per_sec.map(|v| v as i64);
        let swap_out = memory.swap_out_pages_per_sec.map(|v| v as i64);
        let r = sqlx::query!(
            "INSERT INTO metrics_memory
               (resolution, timestamp, total_bytes, used_bytes, available_bytes,
                cached_bytes, swap_used_bytes,
                page_faults_minor_per_sec, page_faults_major_per_sec,
                swap_in_pages_per_sec, swap_out_pages_per_sec)
             VALUES ('raw', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(resolution, timestamp) DO NOTHING",
            memory.timestamp,
            memory.total_bytes as i64,
            memory.used_bytes as i64,
            memory.available_bytes as i64,
            memory.cached_bytes as i64,
            memory.swap_used_bytes as i64,
            pf_minor,
            pf_major,
            swap_in,
            swap_out,
        )
        .execute(&mut *tx)
        .await?;
        note_collision(
            "metrics_memory",
            1,
            r.rows_affected(),
            memory.timestamp,
            "raw",
        );

        if !disks.is_empty() {
            let mut qb = sqlx::QueryBuilder::new(
                "INSERT INTO metrics_disk \
                 (resolution, timestamp, mount_point, \
                  total_bytes, used_bytes, available_bytes, read_bytes_per_sec, write_bytes_per_sec, \
                  inode_used_percent, read_iops, write_iops, io_util_percent) ",
            );
            qb.push_values(disks.iter(), |mut b, d| {
                b.push_bind("raw")
                    .push_bind(d.timestamp)
                    .push_bind(&d.mount_point)
                    .push_bind(d.total_bytes as i64)
                    .push_bind(d.used_bytes as i64)
                    .push_bind(d.available_bytes as i64)
                    .push_bind(d.read_bytes_per_sec as i64)
                    .push_bind(d.write_bytes_per_sec as i64)
                    .push_bind(d.inode_used_percent)
                    .push_bind(d.read_iops.map(|v| v as i64))
                    .push_bind(d.write_iops.map(|v| v as i64))
                    .push_bind(d.io_util_percent);
            });
            qb.push(" ON CONFLICT(resolution, timestamp, mount_point) DO NOTHING");
            let r = qb.build().execute(&mut *tx).await?;
            let ts = disks.last().map(|d| d.timestamp).unwrap_or(0);
            note_collision(
                "metrics_disk",
                disks.len() as u64,
                r.rows_affected(),
                ts,
                "raw",
            );
        }

        if !networks.is_empty() {
            let mut qb = sqlx::QueryBuilder::new(
                "INSERT INTO metrics_network \
                 (resolution, timestamp, interface_name, \
                  rx_bytes_per_sec, tx_bytes_per_sec, \
                  rx_packets_per_sec, tx_packets_per_sec, \
                  errors_in_per_sec, errors_out_per_sec) ",
            );
            qb.push_values(networks.iter(), |mut b, n| {
                b.push_bind("raw")
                    .push_bind(n.timestamp)
                    .push_bind(&n.interface)
                    .push_bind(n.rx_bytes_per_sec as i64)
                    .push_bind(n.tx_bytes_per_sec as i64)
                    .push_bind(n.rx_packets_per_sec as i64)
                    .push_bind(n.tx_packets_per_sec as i64)
                    .push_bind(n.errors_in_per_sec as i64)
                    .push_bind(n.errors_out_per_sec as i64);
            });
            qb.push(" ON CONFLICT(resolution, timestamp, interface_name) DO NOTHING");
            let r = qb.build().execute(&mut *tx).await?;
            let ts = networks.last().map(|n| n.timestamp).unwrap_or(0);
            note_collision(
                "metrics_network",
                networks.len() as u64,
                r.rows_affected(),
                ts,
                "raw",
            );
        }

        if let Some(c) = components
            && !c.components.is_empty()
        {
            let mut qb = sqlx::QueryBuilder::new(
                "INSERT INTO metrics_components \
                 (resolution, timestamp, label, temperature_c, max_c, critical_c) ",
            );
            qb.push_values(c.components.iter(), |mut b, comp| {
                b.push_bind("raw")
                    .push_bind(c.timestamp)
                    .push_bind(&comp.label)
                    .push_bind(comp.temperature_c)
                    .push_bind(comp.max_c)
                    .push_bind(comp.critical_c);
            });
            qb.push(" ON CONFLICT(resolution, timestamp, label) DO NOTHING");
            let r = qb.build().execute(&mut *tx).await?;
            note_collision(
                "metrics_components",
                c.components.len() as u64,
                r.rows_affected(),
                c.timestamp,
                "raw",
            );
        }

        if let Some(p) = pressure {
            let resources: [(&str, Option<&_>); 3] = [
                ("cpu", p.cpu.as_ref()),
                ("memory", p.memory.as_ref()),
                ("io", p.io.as_ref()),
            ];
            let present: Vec<(&str, &_)> = resources
                .iter()
                .filter_map(|(name, ps)| ps.map(|ps| (*name, ps)))
                .collect();
            if !present.is_empty() {
                let mut qb = sqlx::QueryBuilder::new(
                    "INSERT INTO metrics_pressure \
                     (resolution, timestamp, resource, \
                      some_avg10, some_avg60, some_avg300, \
                      full_avg10, full_avg60, full_avg300) ",
                );
                qb.push_values(present.iter(), |mut b, (resource, ps)| {
                    b.push_bind("raw")
                        .push_bind(p.timestamp)
                        .push_bind(*resource)
                        .push_bind(ps.some_avg10)
                        .push_bind(ps.some_avg60)
                        .push_bind(ps.some_avg300)
                        .push_bind(ps.full_avg10)
                        .push_bind(ps.full_avg60)
                        .push_bind(ps.full_avg300);
                });
                qb.push(" ON CONFLICT(resolution, timestamp, resource) DO NOTHING");
                let r = qb.build().execute(&mut *tx).await?;
                note_collision(
                    "metrics_pressure",
                    present.len() as u64,
                    r.rows_affected(),
                    p.timestamp,
                    "raw",
                );
            }
        }

        tx.commit().await?;
        Ok(())
    }

    // ===== Reads (history endpoints) =====

    pub async fn read_cpu(
        &self,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<
        Vec<(
            i64,
            f64,
            f64,
            f64,
            f64,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<i64>,
            Option<i64>,
        )>,
    > {
        // ORDER BY timestamp DESC + LIMIT N gives the most-recent N rows;
        // we reverse client-side so the response is still in ascending
        // order. ORDER BY ASC + LIMIT would silently drop the live tail
        // when the window contains more samples than the limit allows.
        let mut rows = sqlx::query_as::<
            _,
            (
                i64,
                f64,
                f64,
                f64,
                f64,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                Option<f64>,
                Option<i64>,
                Option<i64>,
            ),
        >(
            r#"
            SELECT timestamp, usage_percent, load_1m, load_5m, load_15m,
                   steal_percent, iowait_percent, guest_percent,
                   user_percent, system_percent,
                   context_switches_per_sec, process_forks_per_sec
              FROM metrics_cpu
             WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
             ORDER BY timestamp DESC
             LIMIT ?
            "#,
        )
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    pub async fn read_cpu_cores(
        &self,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<(i64, i64, f64, i64)>> {
        // Multi-row-per-timestamp (one per core) — pick the most recent
        // `limit` distinct timestamps in a subquery so we don't truncate
        // the live tail when N × cores exceeds the row limit.
        let rows = sqlx::query_as::<_, (i64, i64, f64, i64)>(
            r#"
            SELECT timestamp, core_index, usage_percent, freq_mhz
              FROM metrics_cpu_cores
             WHERE timestamp >= ? AND timestamp <= ?
               AND timestamp IN (
                   SELECT DISTINCT timestamp FROM metrics_cpu_cores
                    WHERE timestamp >= ? AND timestamp <= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
               )
             ORDER BY timestamp ASC, core_index ASC
            "#,
        )
        .bind(start)
        .bind(end)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn read_memory(
        &self,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<
        Vec<(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        )>,
    > {
        // ORDER BY DESC + reverse keeps the live tail when the limit is
        // smaller than the window's sample count (see read_cpu).
        let mut rows = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
            ),
        >(
            r#"
            SELECT timestamp, used_bytes, available_bytes, cached_bytes, swap_used_bytes,
                   total_bytes,
                   page_faults_minor_per_sec, page_faults_major_per_sec,
                   swap_in_pages_per_sec, swap_out_pages_per_sec
              FROM metrics_memory
             WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
             ORDER BY timestamp DESC
             LIMIT ?
            "#,
        )
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    pub async fn read_disk(
        &self,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<
        Vec<(
            i64,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<f64>,
            Option<i64>,
            Option<i64>,
            Option<f64>,
        )>,
    > {
        // Disk has N rows per timestamp (one per mount). A flat
        // `LIMIT N` would chop off the most recent timestamps once
        // N × mount_count exceeds the limit, leaving the client with
        // an incomplete tail and a visible gap in the sparkline.
        // Pick the most recent `limit` distinct timestamps first, then
        // join all mount rows for them.
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                String,
                i64,
                i64,
                i64,
                i64,
                i64,
                Option<f64>,
                Option<i64>,
                Option<i64>,
                Option<f64>,
            ),
        >(
            r#"
            SELECT timestamp, mount_point,
                   total_bytes,
                   used_bytes, available_bytes,
                   read_bytes_per_sec, write_bytes_per_sec, inode_used_percent,
                   read_iops, write_iops, io_util_percent
              FROM metrics_disk
             WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
               AND timestamp IN (
                   SELECT DISTINCT timestamp FROM metrics_disk
                    WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
               )
             ORDER BY timestamp ASC, mount_point ASC
            "#,
        )
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn read_pressure(
        &self,
        resource: &str,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<(i64, f64, f64, f64, f64, f64, f64)>> {
        // ORDER BY DESC + reverse keeps the live tail (see read_cpu).
        let mut rows = sqlx::query_as::<_, (i64, f64, f64, f64, f64, f64, f64)>(
            r#"
            SELECT timestamp,
                   some_avg10, some_avg60, some_avg300,
                   full_avg10, full_avg60, full_avg300
              FROM metrics_pressure
             WHERE resource = ? AND resolution = ?
               AND timestamp >= ? AND timestamp <= ?
             ORDER BY timestamp DESC
             LIMIT ?
            "#,
        )
        .bind(resource)
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    pub async fn read_network(
        &self,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<(i64, String, i64, i64, i64, i64, i64, i64)>> {
        // Network has N rows per timestamp (one per interface). On a host
        // with docker / k8s plumbing N can easily exceed 20, and a flat
        // `LIMIT 1000` then truncates the response to roughly the oldest 40
        // timestamps — the live tail goes missing and the sparkline gets a
        // visible gap between the prefetched history and the first SSE
        // sample. Pick the most recent `limit` distinct timestamps in a
        // subquery and join all interface rows for them.
        let rows = sqlx::query_as::<_, (i64, String, i64, i64, i64, i64, i64, i64)>(
            r#"
            SELECT timestamp, interface_name, rx_bytes_per_sec, tx_bytes_per_sec,
                   rx_packets_per_sec, tx_packets_per_sec,
                   errors_in_per_sec, errors_out_per_sec
              FROM metrics_network
             WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
               AND timestamp IN (
                   SELECT DISTINCT timestamp FROM metrics_network
                    WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
               )
             ORDER BY timestamp ASC, interface_name ASC
            "#,
        )
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn read_components(
        &self,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<(i64, String, Option<f64>, Option<f64>, Option<f64>)>> {
        // Multi-row-per-timestamp (one per component) — same subquery
        // pattern as cpu_cores / network / disk to preserve the live tail.
        let rows = sqlx::query_as::<_, (i64, String, Option<f64>, Option<f64>, Option<f64>)>(
            r#"
            SELECT timestamp, label, temperature_c, max_c, critical_c
              FROM metrics_components
             WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
               AND timestamp IN (
                   SELECT DISTINCT timestamp FROM metrics_components
                    WHERE resolution = ? AND timestamp >= ? AND timestamp <= ?
                    ORDER BY timestamp DESC
                    LIMIT ?
               )
             ORDER BY timestamp ASC, label ASC
            "#,
        )
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(resolution)
        .bind(start)
        .bind(end)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn delete_older_than(
        &self,
        resource: &str,
        resolution: &str,
        cutoff_ts: i64,
    ) -> AppResult<u64> {
        let result = match resource {
            "cpu_cores" => {
                if resolution != "raw" {
                    return Ok(0);
                }
                sqlx::query!(
                    "DELETE FROM metrics_cpu_cores WHERE timestamp < ?",
                    cutoff_ts
                )
                .execute(&self.pool)
                .await?
            }
            "logs" => {
                sqlx::query!("DELETE FROM logs WHERE timestamp < ?", cutoff_ts)
                    .execute(&self.pool)
                    .await?
            }
            "probe_runs" => {
                if resolution != "raw" {
                    return Ok(0);
                }
                sqlx::query!("DELETE FROM probe_runs WHERE timestamp < ?", cutoff_ts)
                    .execute(&self.pool)
                    .await?
            }
            "heartbeat_pings" => {
                if resolution != "raw" {
                    return Ok(0);
                }
                sqlx::query!(
                    "DELETE FROM heartbeat_pings WHERE received_at < ?",
                    cutoff_ts
                )
                .execute(&self.pool)
                .await?
            }
            "alert_events" => {
                if resolution != "raw" {
                    return Ok(0);
                }
                sqlx::query!("DELETE FROM alert_events WHERE occurred_at < ?", cutoff_ts)
                    .execute(&self.pool)
                    .await?
            }
            "cpu" | "memory" | "disk" | "network" | "docker" | "pressure" | "components"
            | "probe" | "smart" => {
                let table = format!("metrics_{}", resource);
                let sql = format!(
                    "DELETE FROM {} WHERE resolution = ? AND timestamp < ?",
                    table
                );
                sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(resolution)
                    .bind(cutoff_ts)
                    .execute(&self.pool)
                    .await?
            }
            other => {
                // Likely a typo in retention_policy or a new resource added
                // without a matching DELETE branch above. Silent no-op
                // would let stale data accumulate forever.
                warn!(
                    "retention DELETE skipped: unknown resource '{}' (resolution={}, cutoff={})",
                    other, resolution, cutoff_ts
                );
                return Ok(0);
            }
        };

        Ok(result.rows_affected())
    }
}
