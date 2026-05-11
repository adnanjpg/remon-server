//! Probe repository — manifest shadow (`probe_definitions`), run-meta
//! log (`probe_runs`), and the time-series metric store (`metrics_probe`).
//!
//! `insert_run` and `insert_metrics` are deliberately split so the runner
//! can persist the run-meta even when no metrics were emitted (e.g. a
//! script that exited 0 but printed nothing — useful for "did this run
//! succeed?" telemetry separate from the value timeseries).

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::probe::{ProbeMetric, ProbeRun};

pub struct ProbeRepository {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct ProbeDefinitionRow {
    pub name: String,
    pub enabled: bool,
    pub schedule: String,
    pub timeout_ms: i64,
    pub manifest_hash: String,
}

/// One time-series sample returned from `read_metric_history` —
/// shape matches what `/metrics/probe/{probe}/{metric}` serves.
#[derive(Debug, Clone)]
pub struct ProbeMetricSample {
    pub timestamp: i64,
    pub labels_json: String,
    pub value: f64,
}

impl ProbeRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, def: &ProbeDefinitionRow) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO probe_definitions
                (name, enabled, schedule, timeout_ms, manifest_hash, last_loaded_at)
            VALUES (?, ?, ?, ?, ?, unixepoch())
            ON CONFLICT(name) DO UPDATE SET
                enabled        = excluded.enabled,
                schedule       = excluded.schedule,
                timeout_ms     = excluded.timeout_ms,
                manifest_hash  = excluded.manifest_hash,
                last_loaded_at = excluded.last_loaded_at
            "#,
        )
        .bind(&def.name)
        .bind(def.enabled as i64)
        .bind(&def.schedule)
        .bind(def.timeout_ms)
        .bind(&def.manifest_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn disable_missing(&self, keep: &[String]) -> AppResult<u64> {
        if keep.is_empty() {
            let r = sqlx::query("UPDATE probe_definitions SET enabled = 0 WHERE enabled = 1")
                .execute(&self.pool)
                .await?;
            return Ok(r.rows_affected());
        }
        let placeholders = std::iter::repeat("?")
            .take(keep.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE probe_definitions SET enabled = 0 \
             WHERE enabled = 1 AND name NOT IN ({})",
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for n in keep {
            q = q.bind(n);
        }
        let r = q.execute(&self.pool).await?;
        Ok(r.rows_affected())
    }

    pub async fn list_all(&self) -> AppResult<Vec<ProbeDefinitionRow>> {
        let rows: Vec<(String, i64, String, i64, String)> = sqlx::query_as(
            "SELECT name, enabled, schedule, timeout_ms, manifest_hash \
             FROM probe_definitions ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(name, enabled, schedule, timeout_ms, manifest_hash)| ProbeDefinitionRow {
                    name,
                    enabled: enabled != 0,
                    schedule,
                    timeout_ms,
                    manifest_hash,
                },
            )
            .collect())
    }

    /// Persist the run-meta row. Called after every probe execution,
    /// regardless of whether any metrics were emitted.
    pub async fn insert_run(&self, run: &ProbeRun) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO probe_runs
                (probe_name, timestamp, duration_ms, exit_code, message, parse_ok)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&run.probe_name)
        .bind(run.timestamp)
        .bind(run.duration_ms)
        .bind(run.exit_code)
        .bind(run.message.as_deref())
        .bind(run.parse_ok as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist every emitted metric as a `metrics_probe` row at raw
    /// resolution. INSERT OR REPLACE covers retries — the same probe
    /// firing twice in the same second (clock skew, manual reload during
    /// a scheduled tick) won't duplicate. One round-trip via batch INSERT.
    pub async fn insert_metrics(
        &self,
        probe_name: &str,
        timestamp: i64,
        metrics: &[ProbeMetric],
    ) -> AppResult<()> {
        if metrics.is_empty() {
            return Ok(());
        }
        let mut sql = String::with_capacity(140 + 20 * metrics.len());
        sql.push_str(
            "INSERT OR REPLACE INTO metrics_probe \
             (resolution, timestamp, probe_name, metric_name, labels, value) VALUES ",
        );
        for i in 0..metrics.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str("('raw', ?, ?, ?, ?, ?)");
        }
        let mut q = sqlx::query(&sql);
        for m in metrics {
            let labels = m.labels_canonical();
            q = q
                .bind(timestamp)
                .bind(probe_name)
                .bind(&m.name)
                .bind(labels)
                .bind(m.value);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Run-meta history (timestamp / duration / exit / message / parse_ok)
    /// for `/probes/{name}/history`. `offset` enables client-side paging
    /// back through older runs once the most recent `limit` has been read.
    pub async fn run_history(
        &self,
        probe_name: &str,
        limit: u32,
        offset: u32,
    ) -> AppResult<Vec<ProbeRun>> {
        let rows: Vec<(i64, i64, Option<i64>, Option<String>, i64)> = sqlx::query_as(
            r#"
            SELECT timestamp, duration_ms, exit_code, message, parse_ok
              FROM probe_runs
             WHERE probe_name = ?
             ORDER BY timestamp DESC
             LIMIT ? OFFSET ?
            "#,
        )
        .bind(probe_name)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(ts, dur, exit, msg, parse_ok)| ProbeRun {
                probe_name: probe_name.to_string(),
                timestamp: ts,
                duration_ms: dur,
                exit_code: exit.map(|v| v as i32),
                message: msg,
                parse_ok: parse_ok != 0,
            })
            .collect())
    }

    /// Time-series for one probe metric. `labels_filter` (canonical JSON)
    /// is optional; when omitted, every label combination is returned —
    /// caller groups client-side. Newest-first / capped by `limit`.
    pub async fn read_metric_history(
        &self,
        probe_name: &str,
        metric_name: &str,
        labels_filter: Option<&str>,
        resolution: &str,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<ProbeMetricSample>> {
        let rows = if let Some(labels) = labels_filter {
            sqlx::query_as::<_, (i64, String, f64)>(
                r#"
                SELECT timestamp, labels, value
                  FROM metrics_probe
                 WHERE resolution = ?
                   AND probe_name = ? AND metric_name = ? AND labels = ?
                   AND timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC
                 LIMIT ?
                "#,
            )
            .bind(resolution)
            .bind(probe_name)
            .bind(metric_name)
            .bind(labels)
            .bind(start)
            .bind(end)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, (i64, String, f64)>(
                r#"
                SELECT timestamp, labels, value
                  FROM metrics_probe
                 WHERE resolution = ?
                   AND probe_name = ? AND metric_name = ?
                   AND timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC
                 LIMIT ?
                "#,
            )
            .bind(resolution)
            .bind(probe_name)
            .bind(metric_name)
            .bind(start)
            .bind(end)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(|(ts, labels, value)| ProbeMetricSample {
                timestamp: ts,
                labels_json: labels,
                value,
            })
            .collect())
    }
}
