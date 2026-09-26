//! Budgeted chart reads. Stored tiers and output bucket widths are separate.
use serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};

use super::metrics::{
    CpuHistoryRow, DiskHistoryRow, MemoryHistoryRow, NetworkHistoryRow, cpu_history_row,
    disk_history_row, memory_history_row, network_history_row,
};
use crate::error::{AppError, AppResult};

pub(super) const CPU_FIELDS: &[&str] = &[
    "usage_percent",
    "load_1m",
    "load_5m",
    "load_15m",
    "steal_percent",
    "iowait_percent",
    "guest_percent",
    "user_percent",
    "system_percent",
    "context_switches_per_sec",
    "process_forks_per_sec",
];
pub(super) const MEMORY_FIELDS: &[&str] = &[
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "cached_bytes",
    "swap_used_bytes",
    "page_faults_minor_per_sec",
    "page_faults_major_per_sec",
    "swap_in_pages_per_sec",
    "swap_out_pages_per_sec",
    "used_percent",
];
pub(super) const DISK_FIELDS: &[&str] = &[
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
    "inode_used_percent",
    "read_iops",
    "write_iops",
    "io_util_percent",
    "used_percent",
];
pub(super) const NETWORK_FIELDS: &[&str] = &[
    "rx_bytes_per_sec",
    "tx_bytes_per_sec",
    "rx_packets_per_sec",
    "tx_packets_per_sec",
    "errors_in_per_sec",
    "errors_out_per_sec",
];
/// Real-valued gauges; every other field is an integer column.
pub(super) const REAL_FIELDS: &[&str] = &[
    "usage_percent",
    "load_1m",
    "load_5m",
    "load_15m",
    "steal_percent",
    "iowait_percent",
    "guest_percent",
    "user_percent",
    "system_percent",
    "inode_used_percent",
    "io_util_percent",
    "used_percent",
];
/// Rows per response across all keys, as in the stored-resolution reads.
const MAX_ROWS: i64 = 50_000;

struct Spec {
    table: &'static str,
    resource: &'static str,
    key: Option<&'static str>,
    fields: &'static [&'static str],
    /// Constant columns a row mapper expects but the table lacks.
    pad: &'static str,
}
const CPU: Spec = Spec {
    table: "metrics_cpu",
    resource: "cpu",
    key: None,
    fields: CPU_FIELDS,
    pad: "",
};
const MEMORY: Spec = Spec {
    table: "metrics_memory",
    resource: "memory",
    key: None,
    fields: MEMORY_FIELDS,
    pad: "",
};
const DISK: Spec = Spec {
    table: "metrics_disk",
    resource: "disk",
    key: Some("mount_point"),
    fields: DISK_FIELDS,
    pad: "",
};
const NETWORK: Spec = Spec {
    table: "metrics_network",
    resource: "network",
    key: Some("interface_name"),
    fields: NETWORK_FIELDS,
    pad: "",
};
// Shares network's rollup cursor and retention policy.
const NETWORK_TOTAL: Spec = Spec {
    table: "metrics_network_total",
    resource: "network",
    key: None,
    fields: NETWORK_FIELDS,
    pad: "'' AS interface_name,",
};

/// Raw rows store only the inputs of a derived percentage (same as rollup).
fn raw_expr(table: &str, field: &'static str) -> &'static str {
    match (table, field) {
        ("metrics_memory", "used_percent") => {
            "100.0 * MAX(total_bytes - available_bytes, 0) / NULLIF(total_bytes, 0)"
        }
        ("metrics_disk", "used_percent") => "100.0 * used_bytes / NULLIF(total_bytes, 0)",
        _ => field,
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ChartWindow {
    pub start: i64,
    pub end: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ChartSource {
    pub resolution: String,
    pub from: i64,
    pub to: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ChartMetadata {
    pub requested: ChartWindow,
    pub aligned: ChartWindow,
    /// Snapshot time; neither this nor data_through asserts uninterrupted sampling.
    pub as_of: i64,
    /// Last represented source instant (raw) or processed bucket boundary (rollup).
    pub data_through: Option<i64>,
    pub bucket_seconds: i64,
    pub max_points: u32,
    pub sources: Vec<ChartSource>,
    /// Unavailable source coverage, distinct from empty processed buckets.
    pub unavailable: Vec<ChartWindow>,
    pub degraded: bool,
}
#[derive(Clone, Debug)]
struct Tier {
    name: String,
    interval: i64,
    from: i64,
    to: i64,
}

fn ceil(ts: i64, step: i64) -> i64 {
    (ts / step + i64::from(ts % step != 0)) * step
}
fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}
fn nice_step(ideal: i64) -> i64 {
    const STEPS: &[i64] = &[
        2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 10800, 21600, 43200, 86400,
    ];
    STEPS
        .iter()
        .copied()
        .find(|s| *s >= ideal)
        .unwrap_or_else(|| ceil(ideal, 86400))
}
fn edges(tiers: &[Tier], start: i64, end: i64) -> Vec<i64> {
    let mut edges = vec![start, end];
    for t in tiers {
        if t.from > start && t.from < end {
            edges.push(t.from);
        }
        if t.to > start && t.to < end {
            edges.push(t.to);
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}
fn segments(
    tiers: &[Tier],
    start: i64,
    end: i64,
    step: i64,
) -> (Vec<ChartSource>, Vec<ChartWindow>) {
    let mut sources: Vec<ChartSource> = Vec::new();
    let mut gaps: Vec<ChartWindow> = Vec::new();
    for pair in edges(tiers, start, end).windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let chosen = tiers
            .iter()
            .filter(|t| t.from <= from && t.to >= to && (t.name == "raw" || step % t.interval == 0))
            .max_by_key(|t| t.interval);
        if let Some(t) = chosen {
            if let Some(last) = sources
                .last_mut()
                .filter(|s| s.resolution == t.name && s.to == from)
            {
                last.to = to;
            } else {
                sources.push(ChartSource {
                    resolution: t.name.clone(),
                    from,
                    to,
                });
            }
        } else if let Some(last) = gaps.last_mut().filter(|s| s.end == from) {
            last.end = to;
        } else {
            gaps.push(ChartWindow {
                start: from,
                end: to,
            });
        }
    }
    (sources, gaps)
}

fn plan(tiers: &[Tier], start: i64, end: i64, now: i64, budget: u32) -> AppResult<ChartMetadata> {
    let ideal = nice_step((end - start + i64::from(budget) - 1) / i64::from(budget));
    // Minimum source width needed anywhere in the requested window. Intervals
    // are read from configuration, not inferred from the tier names.
    let mut multiple = 1;
    for pair in edges(tiers, start, end.min(now).max(start)).windows(2) {
        if let Some(t) = tiers
            .iter()
            .filter(|t| t.from <= pair[0] && t.to >= pair[1])
            .min_by_key(|t| t.interval)
        {
            let interval = if t.name == "raw" { 1 } else { t.interval };
            multiple = (multiple / gcd(multiple, interval))
                .checked_mul(interval)
                .filter(|width| *width <= 10 * 366 * 86400)
                .ok_or_else(|| AppError::Internal("incompatible chart source intervals".into()))?;
        }
    }
    let mut step = ceil(ideal, multiple);
    // Epoch alignment may add a bucket. Check the actual count; never LIMIT
    // the newest rows and silently lose the beginning of the requested window.
    while (ceil(end, step) - start / step * step) / step > i64::from(budget) {
        step = ceil(nice_step(step + 1), multiple);
    }
    let aligned = ChartWindow {
        start: start / step * step,
        end: ceil(end, step),
    };
    let (sources, unavailable) = segments(
        tiers,
        aligned.start,
        aligned.end.min(now).max(aligned.start),
        step,
    );
    let degraded = multiple > ideal || !unavailable.is_empty();
    Ok(ChartMetadata {
        requested: ChartWindow { start, end },
        aligned,
        as_of: now,
        data_through: None,
        bucket_seconds: step,
        max_points: budget,
        sources,
        unavailable,
        degraded,
    })
}

async fn tiers(conn: &mut SqliteConnection, spec: &Spec, now: i64) -> AppResult<Vec<Tier>> {
    // One snapshot contains policies, coverage and samples. Disabled tiers may
    // still serve their certified historical interval; they cannot invent a tail.
    let rows = sqlx::query(
        "SELECT r.name, r.interval_seconds, p.keep_seconds,
        s.processed_from, s.last_bucket_ts FROM resolutions r
        JOIN retention_policy p ON p.resolution = r.name AND p.resource = ?1
        LEFT JOIN rollup_state s ON s.resolution = r.name AND s.resource = ?1
        ORDER BY r.sort_order",
    )
    .bind(spec.resource)
    .fetch_all(&mut *conn)
    .await?;
    let raw_first: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT MIN(timestamp) FROM {} WHERE resolution = 'raw'",
        spec.table
    )))
    .fetch_one(&mut *conn)
    .await?;
    let mut tiers = Vec::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        let interval: i64 = row.try_get("interval_seconds")?;
        if interval <= 0 || interval > 31_536_000 {
            continue;
        }
        let keep: i64 = row.try_get("keep_seconds")?;
        let retained = now.saturating_sub(keep).max(0);
        let (from, to) = if name == "raw" {
            let Some(first) = raw_first else {
                continue;
            };
            (first.max(retained), now)
        } else {
            let Some(from) = row.try_get::<Option<i64>, _>("processed_from")? else {
                continue;
            };
            let Some(last) = row.try_get::<Option<i64>, _>("last_bucket_ts")? else {
                continue;
            };
            (
                ceil(from.max(retained), interval),
                (last + interval).min(now / interval * interval),
            )
        };
        if from < to {
            tiers.push(Tier {
                name,
                interval,
                from,
                to,
            });
        }
    }
    Ok(tiers)
}

/// Points per key, so a keyed table stays within MAX_ROWS in total.
async fn key_budget(
    conn: &mut SqliteConnection,
    spec: &Spec,
    tiers: &[Tier],
    start: i64,
    end: i64,
    budget: u32,
) -> AppResult<u32> {
    let Some(key) = spec.key else {
        return Ok(budget);
    };
    let table = spec.table;
    let sql = format!(
        "SELECT COUNT(DISTINCT {key}) FROM {table} WHERE resolution = ?1 AND timestamp = (
        SELECT MAX(timestamp) FROM {table} WHERE resolution = ?1 AND timestamp >= ?2 AND timestamp < ?3)"
    );
    let mut keys = 1;
    for t in tiers {
        let (from, to) = (t.from.max(start), t.to.min(end));
        if from < to {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
                .bind(&t.name)
                .bind(from)
                .bind(to)
                .fetch_one(&mut *conn)
                .await?;
            keys = keys.max(n);
        }
    }
    Ok(budget
        .min(u32::try_from(MAX_ROWS / keys).unwrap_or(u32::MAX))
        .max(16))
}

async fn chart_plan(
    conn: &mut SqliteConnection,
    spec: &Spec,
    start: i64,
    end: i64,
    max_points: u32,
) -> AppResult<ChartMetadata> {
    let now = chrono::Utc::now().timestamp();
    // Bound arithmetic and work, independently of the caller's chosen budget.
    if start < 0 || end <= start || end > now + 86400 || end - start > 10 * 366 * 86400 {
        return Err(AppError::BadRequest(
            "chart range must be positive, at most ten years, and no more than a day in the future"
                .into(),
        ));
    }
    let tiers = tiers(conn, spec, now).await?;
    let budget = key_budget(conn, spec, &tiers, start, end, max_points.clamp(16, 2000)).await?;
    // Actual sample count is authoritative. Sampling intervals are only nominal.
    if tiers
        .iter()
        .any(|t| t.name == "raw" && t.from <= start && t.to >= end.min(now))
    {
        let samples: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM (SELECT DISTINCT timestamp FROM {} WHERE resolution = 'raw'
            AND timestamp >= ? AND timestamp < ? LIMIT ?)",
            spec.table
        )))
        .bind(start)
        .bind(end)
        .bind(i64::from(budget) + 1)
        .fetch_one(&mut *conn)
        .await?;
        if samples <= i64::from(budget) {
            return Ok(ChartMetadata {
                requested: ChartWindow { start, end },
                aligned: ChartWindow { start, end },
                as_of: now,
                data_through: None,
                bucket_seconds: 0,
                max_points: budget,
                sources: vec![ChartSource {
                    resolution: "raw".into(),
                    from: start,
                    to: end,
                }],
                unavailable: vec![],
                degraded: false,
            });
        }
    }
    plan(&tiers, start, end, now, budget)
}

/// Rows for a plan; raw rows when `bucket_seconds` is zero, else query-time buckets.
async fn chart_rows(
    conn: &mut SqliteConnection,
    spec: &Spec,
    meta: &mut ChartMetadata,
) -> AppResult<Vec<SqliteRow>> {
    let (table, pad) = (spec.table, spec.pad);
    let (key_sel, key_order) = match spec.key {
        Some(k) => (format!("{k}, "), format!(", {k}")),
        None => (String::new(), String::new()),
    };
    if meta.bucket_seconds == 0 {
        let derived: String = spec
            .fields
            .iter()
            .filter(|&&f| raw_expr(table, f) != f)
            .map(|&f| format!("{} AS effective_{f}, ", raw_expr(table, f)))
            .collect();
        let sql = format!(
            "SELECT *, {derived}{pad} 0 AS bucket_seconds FROM {table}
            WHERE resolution = 'raw' AND timestamp >= ? AND timestamp < ?
            ORDER BY timestamp{key_order}"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(meta.requested.start)
            .bind(meta.requested.end)
            .fetch_all(&mut *conn)
            .await?;
        meta.data_through = rows.last().map(|r| r.try_get("timestamp")).transpose()?;
        return Ok(rows);
    }
    if meta.sources.is_empty() {
        return Ok(vec![]);
    }
    let step = meta.bucket_seconds;
    // All generated identifiers are constants. Only parameter values come from
    // requests/configuration; segment intervals are disjoint half-open ranges.
    let complete = "MIN(CASE WHEN resolution = 'raw' OR summary_version = 1 THEN 1 ELSE 0 END) = 1";
    let mut values = Vec::new();
    for &field in spec.fields {
        let raw = raw_expr(table, field);
        let sum = format!(
            "SUM(CASE WHEN resolution = 'raw' THEN COALESCE(1.0 * ({raw}), 0.0) ELSE {field}_sum END)"
        );
        let count = format!(
            "SUM(CASE WHEN resolution = 'raw' THEN CASE WHEN ({raw}) IS NULL THEN 0 ELSE 1 END ELSE {field}_valid_count END)"
        );
        let value = format!("(CASE WHEN resolution = 'raw' THEN ({raw}) ELSE {field} END)");
        let legacy = format!(
            "SUM(1.0 * {value} * COALESCE(sample_count, 1)) / NULLIF(SUM(CASE WHEN {value} IS NULL THEN 0 ELSE COALESCE(sample_count, 1) END), 0)"
        );
        let mean =
            format!("CASE WHEN {complete} THEN 1.0 * {sum} / NULLIF({count}, 0) ELSE {legacy} END");
        let mean = if REAL_FIELDS.contains(&field) {
            mean
        } else {
            format!("CAST({mean} AS INTEGER)")
        };
        if raw != field {
            values.push(format!("{mean} AS effective_{field}"));
        }
        values.push(format!("{mean} AS {field}"));
        for (suffix, value) in [
            (
                "min",
                format!(
                    "MIN(CASE WHEN resolution = 'raw' THEN 1.0 * ({raw}) ELSE {field}_min END)"
                ),
            ),
            (
                "max",
                format!(
                    "MAX(CASE WHEN resolution = 'raw' THEN 1.0 * ({raw}) ELSE {field}_max END)"
                ),
            ),
            ("sum", sum),
            ("valid_count", count),
        ] {
            values.push(format!(
                "CASE WHEN {complete} THEN {value} END AS {field}_{suffix}"
            ));
        }
    }
    let select = format!(
        "SELECT c.*, CASE WHEN c.resolution = 'raw' THEN 0 ELSE r.interval_seconds END AS source_width FROM {table} c JOIN resolutions r ON r.name = c.resolution WHERE c.resolution = ? AND c.timestamp >= ? AND c.timestamp < ?"
    );
    let selects = vec![select; meta.sources.len()];
    let sql = format!(
        "WITH input AS ({}) SELECT (timestamp / {step}) * {step} AS timestamp, {key_sel}{pad}
        {step} AS bucket_seconds, CASE WHEN {complete} THEN 1 END AS summary_version,
        MAX(timestamp + source_width) AS data_through, {} FROM input
        GROUP BY (timestamp / {step}){key_order} ORDER BY timestamp{key_order}",
        selects.join(" UNION ALL "),
        values.join(", ")
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for source in &meta.sources {
        query = query
            .bind(&source.resolution)
            .bind(source.from)
            .bind(source.to);
    }
    let rows = query.fetch_all(&mut *conn).await?;
    for row in &rows {
        let through: i64 = row.try_get("data_through")?;
        meta.data_through = Some(meta.data_through.unwrap_or(0).max(through));
    }
    Ok(rows)
}

async fn read_chart<T>(
    pool: &SqlitePool,
    spec: &Spec,
    start: i64,
    end: i64,
    max_points: u32,
    map: fn(SqliteRow, bool) -> AppResult<T>,
) -> AppResult<(Vec<T>, ChartMetadata)> {
    let mut tx = pool.begin().await?;
    let mut meta = chart_plan(&mut tx, spec, start, end, max_points).await?;
    let rows = chart_rows(&mut tx, spec, &mut meta).await?;
    tx.commit().await?;
    let raw = meta.bucket_seconds == 0;
    let points = rows
        .into_iter()
        .map(|r| map(r, raw))
        .collect::<AppResult<_>>()?;
    Ok((points, meta))
}

pub async fn read_cpu_chart(
    pool: &SqlitePool,
    start: i64,
    end: i64,
    max_points: u32,
) -> AppResult<(Vec<CpuHistoryRow>, ChartMetadata)> {
    read_chart(pool, &CPU, start, end, max_points, cpu_history_row).await
}

pub async fn read_memory_chart(
    pool: &SqlitePool,
    start: i64,
    end: i64,
    max_points: u32,
) -> AppResult<(Vec<MemoryHistoryRow>, ChartMetadata)> {
    read_chart(pool, &MEMORY, start, end, max_points, memory_history_row).await
}

pub async fn read_disk_chart(
    pool: &SqlitePool,
    start: i64,
    end: i64,
    max_points: u32,
) -> AppResult<(Vec<DiskHistoryRow>, ChartMetadata)> {
    read_chart(pool, &DISK, start, end, max_points, disk_history_row).await
}

/// Interfaces and totals share one plan, so both land on the same buckets.
pub async fn read_network_chart(
    pool: &SqlitePool,
    start: i64,
    end: i64,
    max_points: u32,
) -> AppResult<(
    Vec<NetworkHistoryRow>,
    Vec<NetworkHistoryRow>,
    ChartMetadata,
)> {
    let mut tx = pool.begin().await?;
    let mut meta = chart_plan(&mut tx, &NETWORK, start, end, max_points).await?;
    let mut totals_meta = meta.clone();
    let rows = chart_rows(&mut tx, &NETWORK, &mut meta).await?;
    let totals = chart_rows(&mut tx, &NETWORK_TOTAL, &mut totals_meta).await?;
    tx.commit().await?;
    let raw = meta.bucket_seconds == 0;
    let map = |rows: Vec<SqliteRow>| {
        rows.into_iter()
            .map(|r| network_history_row(r, raw))
            .collect::<AppResult<Vec<_>>>()
    };
    Ok((map(rows)?, map(totals)?, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_include_alignment_and_long_windows() {
        for budget in [16, 300, 600, 2000] {
            for span in [900, 3600, 21600, 86400, 604800, 31_536_000] {
                for offset in [0, 1, 17, 59] {
                    let p = plan(
                        &[],
                        1_000_000 + offset,
                        1_000_000 + offset + span,
                        100_000_000,
                        budget,
                    )
                    .unwrap();
                    assert!(
                        (p.aligned.end - p.aligned.start) / p.bucket_seconds <= i64::from(budget)
                    );
                    assert!(
                        p.aligned.start <= p.requested.start && p.aligned.end >= p.requested.end
                    );
                }
            }
        }
    }
    #[test]
    fn coarse_middle_has_fine_head_and_tail_without_overlap() {
        let tiers = vec![
            Tier {
                name: "raw".into(),
                interval: 2,
                from: 0,
                to: 900,
            },
            Tier {
                name: "1m".into(),
                interval: 60,
                from: 120,
                to: 780,
            },
        ];
        let (s, gaps) = segments(&tiers, 0, 900, 300);
        assert!(gaps.is_empty());
        assert_eq!(s.len(), 3);
        assert_eq!((&*s[0].resolution, s[0].from, s[0].to), ("raw", 0, 120));
        assert_eq!((&*s[1].resolution, s[1].from, s[1].to), ("1m", 120, 780));
        assert_eq!((&*s[2].resolution, s[2].from, s[2].to), ("raw", 780, 900));
    }
}
