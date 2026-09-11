//! Metric resolver — turns a parsed `MetricRef` into one or more
//! `(label_set, value)` pairs by querying the right `metrics_*` table.
//!
//! Multi-target semantics: a `MetricRef` whose `labels` map doesn't
//! constrain the table's natural key (mount_point / interface_name /
//! resource / label / probe_name+labels) returns one sample per
//! distinct natural-key combination. The evaluator then runs the
//! comparator against each sample independently and tracks per-target
//! lifecycle. This is what makes "any disk over 90% fires" work
//! out-of-the-box.
//!
//! "Latest" = most recent `timestamp` per natural-key combo at
//! resolution `'raw'`. Future aggregation modes (avg over 5m, max over
//! 1h) live in v1.1; the v1 evaluator only needs current values.
//!
//! All field names and table names come from compile-time whitelists,
//! so the SQL we build with `format!` is injection-safe by construction
//! — we only string-concat `&'static str` we own. Label *values* always
//! flow through `bind`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sqlx::SqlitePool;

use super::expression::{MetricRef, Window};
use crate::models::stats::{
    AllStats, CpuStats, DiskStats, MemoryStats, NetworkStats, PressureStats,
};
use crate::platform::services::ServiceManager;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSample {
    /// Canonical labels JSON (sorted keys, no whitespace) keyed against
    /// the `alert_state` table's `label_set` column.
    pub label_set: String,
    pub value: f64,
    /// Optional human-readable detail (e.g. `service.up` carries the
    /// state name). Numeric namespaces leave this `None`.
    pub meta: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolveError {
    pub message: String,
}

impl ResolveError {
    fn msg(s: impl Into<String>) -> Self {
        Self { message: s.into() }
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metric resolve error: {}", self.message)
    }
}

impl std::error::Error for ResolveError {}

// ===== Per-namespace whitelists =====
//
// The `_I64` lists carry columns stored as INTEGER in SQLite. The
// resolver scalar-binds those as `i64` then casts to `f64`. Columns not
// in the i64 list are read as `f64` directly.

const CPU_FIELDS: &[&str] = &[
    "usage_percent",
    "load_1m",
    "load_5m",
    "load_15m",
    "steal_percent",
    "iowait_percent",
    "guest_percent",
    "context_switches_per_sec",
    "process_forks_per_sec",
];
const CPU_I64: &[&str] = &["context_switches_per_sec", "process_forks_per_sec"];

const MEMORY_FIELDS: &[&str] = &[
    "used_percent",
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "cached_bytes",
    "swap_used_bytes",
    "page_faults_minor_per_sec",
    "page_faults_major_per_sec",
    "swap_in_pages_per_sec",
    "swap_out_pages_per_sec",
];
const MEMORY_I64: &[&str] = &[
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "cached_bytes",
    "swap_used_bytes",
    "page_faults_minor_per_sec",
    "page_faults_major_per_sec",
    "swap_in_pages_per_sec",
    "swap_out_pages_per_sec",
];
const MEMORY_COMPUTED: &[(&str, &str)] = &[(
    "used_percent",
    "100.0 * MAX(total_bytes - available_bytes, 0) / NULLIF(total_bytes, 0)",
)];

const DISK_FIELDS: &[&str] = &[
    "read_iops",
    "write_iops",
    "io_util_percent",
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "used_percent",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
    "inode_used_percent",
];
const DISK_I64: &[&str] = &[
    "read_iops",
    "write_iops",
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
];
// Synthetic disk fields: a SQL expression stands in for a stored column.
// `used_percent` = used_bytes/total_bytes*100; NULLIF makes a 0-total
// mount resolve to NULL so the latest-non-null fallback skips it. Read as
// f64 (never in *_I64); the expression is a `&'static str` we own.
const DISK_COMPUTED: &[(&str, &str)] = &[(
    "used_percent",
    "CAST(used_bytes AS REAL) * 100.0 / NULLIF(total_bytes, 0)",
)];
// Namespaces whose fields are all real columns.
const NO_COMPUTED: &[(&str, &str)] = &[];

const NETWORK_FIELDS: &[&str] = &[
    "rx_bytes_per_sec",
    "tx_bytes_per_sec",
    "rx_packets_per_sec",
    "tx_packets_per_sec",
    "errors_in_per_sec",
    "errors_out_per_sec",
];
const NETWORK_I64: &[&str] = NETWORK_FIELDS; // all rate columns are INTEGER

const PRESSURE_FIELDS: &[&str] = &[
    "some_avg10",
    "some_avg60",
    "some_avg300",
    "full_avg10",
    "full_avg60",
    "full_avg300",
];
const PRESSURE_I64: &[&str] = &[];

const COMPONENTS_FIELDS: &[&str] = &["temperature_c", "max_c", "critical_c"];
const COMPONENTS_I64: &[&str] = &[];

const SMART_FIELDS: &[&str] = &[
    "health_passed",
    "temperature_c",
    "power_on_hours",
    "power_cycles",
    "reallocated_sectors",
    "pending_sectors",
    "uncorrectable_sectors",
    "udma_crc_errors",
    "percentage_used",
    "available_spare_percent",
    "media_errors",
];
// Everything except temperature_c is stored INTEGER (health_passed as 0/1).
const SMART_I64: &[&str] = &[
    "health_passed",
    "power_on_hours",
    "power_cycles",
    "reallocated_sectors",
    "pending_sectors",
    "uncorrectable_sectors",
    "udma_crc_errors",
    "percentage_used",
    "available_spare_percent",
    "media_errors",
];

const DOCKER_FIELDS: &[&str] = &[
    "cpu_percent",
    "memory_used_bytes",
    "memory_limit_bytes",
    "memory_percent",
    "network_rx_bytes",
    "network_tx_bytes",
    "block_read_bytes",
    "block_write_bytes",
    "pids",
];
const DOCKER_I64: &[&str] = &[
    "memory_used_bytes",
    "memory_limit_bytes",
    "network_rx_bytes",
    "network_tx_bytes",
    "block_read_bytes",
    "block_write_bytes",
    "pids",
];
// Synthetic: memory_used/limit*100; NULLIF makes an unlimited (0) limit
// resolve to NULL so the latest-non-null fallback skips it.
const DOCKER_COMPUTED: &[(&str, &str)] = &[(
    "memory_percent",
    "CAST(memory_used_bytes AS REAL) * 100.0 / NULLIF(memory_limit_bytes, 0)",
)];

// Name-grouped process series written by the processes collector. Only the
// top-K groups per write tick exist here, so a rule on a quiet process
// resolves to no samples until that process becomes hot — document rules
// against processes you expect to stay in the top set (or raise K).
const PROCESS_FIELDS: &[&str] = &[
    "cpu_percent",
    "memory_bytes",
    "pid_count",
    "disk_read_bps",
    "disk_write_bps",
];
const PROCESS_I64: &[&str] = &[
    "memory_bytes",
    "pid_count",
    "disk_read_bps",
    "disk_write_bps",
];

// Live-check namespace; resolved via ServiceManager, not the DB.
const SERVICE_FIELDS: &[&str] = &["up"];

// Heartbeat checks; state derived from heartbeat_checks timestamps at
// resolve time — the rule's eval tick IS the deadline check.
const HEARTBEAT_FIELDS: &[&str] = &["up", "late"];

// ===== Public entry =====

/// How many write intervals a sample may be behind and still count as current,
/// and the smallest window regardless. Multiplied by the namespace's own
/// cadence, never fixed: SMART is polled every half hour.
const FRESHNESS_INTERVALS: i64 = 5;
const FRESHNESS_FLOOR_SECS: i64 = 60;

/// Oldest timestamp that still counts as current, for a producer writing every
/// `write_interval_secs`. `i64::MIN` means unbounded — the query keeps its
/// placeholder either way, so the bind arity and the plan do not change with it.
fn freshness_since(now: i64, write_interval_secs: i64) -> i64 {
    now - (write_interval_secs * FRESHNESS_INTERVALS).max(FRESHNESS_FLOOR_SECS)
}

/// The write cadence of whatever fills a namespace's table, in seconds. `None`
/// where it does not apply: `heartbeat`/`service` are not time series. Takes
/// plain values rather than `AppState` so the mapping is testable on its own.
fn write_interval_secs(
    namespace: &str,
    stats_ms: u64,
    docker_ms: u64,
    smart_ms: u64,
) -> Option<i64> {
    let ms = match namespace {
        "cpu" | "memory" | "disk" | "network" | "network_total" | "pressure" => stats_ms,
        // Sensors are re-read every Nth tick and a row is written only then,
        // so the tick rate is not this series' cadence. Read as one, the newest
        // sample sits at the edge of its own window from the moment it lands,
        // and temperature rules go quiet with nothing reporting that they have.
        "components" => {
            stats_ms * crate::collectors::stats::COMPONENTS_REFRESH_EVERY_N_TICKS as u64
        }
        "docker" => docker_ms,
        "smart" => smart_ms,
        // Written once a minute whatever the tick rate is.
        "process" => {
            return Some(crate::collectors::processes::SERIES_WRITE_INTERVAL_SECS);
        }
        _ => return None,
    };
    Some((ms / 1000).max(1) as i64)
}

/// `(table, key column)` for the namespaces backed by a metrics series. The key
/// column is `None` where the series has one implicit key: the host itself.
fn series_of(namespace: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match namespace {
        "cpu" => ("metrics_cpu", None),
        "memory" => ("metrics_memory", None),
        "disk" => ("metrics_disk", Some("mount_point")),
        "network" => ("metrics_network", Some("interface_name")),
        "network_total" => ("metrics_network_total", None),
        "pressure" => ("metrics_pressure", Some("resource")),
        "components" => ("metrics_components", Some("label")),
        "smart" => ("metrics_smart", Some("device")),
        "docker" => ("metrics_docker", Some("container_id")),
        "process" => ("metrics_process", Some("name")),
        _ => return None,
    })
}

/// Whether a key the resolver stopped returning still exists in its series,
/// ignoring how old its newest sample is. A removed target has no condition
/// left to meet and its rule resolves; one that merely stopped being sampled
/// holds its state, since calling that a recovery would report an all-clear
/// nobody observed. Unparseable answers "gone".
pub(crate) async fn key_still_present(pool: &SqlitePool, namespace: &str, label_set: &str) -> bool {
    let Some((table, key_column)) = series_of(namespace) else {
        return false;
    };

    let sql = match key_column {
        Some(col) => {
            format!("SELECT 1 FROM {table} WHERE resolution = 'raw' AND {col} = ? LIMIT 1")
        }
        None => format!("SELECT 1 FROM {table} WHERE resolution = 'raw' LIMIT 1"),
    };
    let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.as_str()));

    if let Some(col) = key_column {
        let Ok(serde_json::Value::Object(labels)) = serde_json::from_str(label_set) else {
            return false;
        };
        let Some(value) = labels.get(col).and_then(|v| v.as_str()).map(str::to_owned) else {
            return false;
        };
        q = q.bind(value);
    }

    q.fetch_optional(pool).await.ok().flatten().is_some()
}

/// DB-only entry. Service-namespace rules error out here; use
/// [`resolve_with_state`] for those.
#[cfg(test)]
pub async fn resolve(
    pool: &SqlitePool,
    metric: &MetricRef,
    since: i64,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    resolve_inner(pool, None, metric, since).await
}

pub async fn resolve_with_state(
    state: &crate::state::AppState,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    use std::sync::atomic::Ordering;
    let since = match write_interval_secs(
        &metric.namespace,
        state.collector_stats_interval_ms.load(Ordering::Relaxed),
        state.collector_docker_interval_ms.load(Ordering::Relaxed),
        state.collector_smart_interval_ms.load(Ordering::Relaxed),
    ) {
        Some(secs) => freshness_since(chrono::Utc::now().timestamp(), secs),
        None => i64::MIN,
    };

    // Hot host-metric namespaces resolve from the live in-memory snapshot the
    // stats collector maintains (`stats_latest`, refreshed every tick) — the
    // current value for alerting is already in RAM, so the evaluator needn't
    // round-trip to the DB every tick. Only always-present fields take this
    // path; optional/enriched fields (steal, iowait, inode_used_percent, …)
    // and the boot window (snapshot not yet populated) fall through to the DB
    // query, which preserves the latest-non-null fallback the resolver
    // guarantees. See `resolve_from_snapshot`.
    //
    // Held to the same window as the DB path: nothing clears the snapshot, so
    // a collector that dies or stalls leaves its last tick readable forever,
    // and every rule on a hot namespace would keep evaluating that one value.
    // The whole bundle is written per tick, so `cpu` carries its age.
    if let Some(snap) = state.stats_latest.read().await.clone()
        && snap.cpu.timestamp >= since
        && let Some(result) = resolve_from_snapshot(metric, &snap)
    {
        return result;
    }

    resolve_inner(&state.db, Some(&state.service_manager), metric, since).await
}

/// Aggregate every raw sample in `window` into one value per natural key.
///
/// The instantaneous path reads whatever the gauge happens to say at the tick,
/// so a spike shorter than `eval_interval_secs` is invisible to a rule that
/// needs two consecutive violating ticks. This reads the samples between the
/// ticks instead.
///
/// Restricted to the namespaces with a history descriptor — probe / heartbeat /
/// service are not time series, and smart / components move far slower than any
/// window worth writing. A key with no sample in the window yields no sample at
/// all, which the evaluator's prune treats as "stale, hold state" rather than a
/// recovery.
pub async fn resolve_windowed(
    state: &crate::state::AppState,
    metric: &MetricRef,
    window: &Window,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let (table, fields, computed, label_col) =
        history_descriptor(&metric.namespace).ok_or_else(|| {
            ResolveError::msg(format!(
                "namespace '{}' has no sample history, so it cannot be aggregated over a window; \
                 drop the {}(…) wrapper",
                metric.namespace,
                window.agg.as_str()
            ))
        })?;

    let column = check_field(metric, fields)?;
    let expr: &str = computed
        .iter()
        .find_map(|(name, e)| (*name == column).then_some(*e))
        .unwrap_or(column);
    let agg = window.agg.sql_fn();
    let since = chrono::Utc::now().timestamp() - window.secs;

    let Some(keycol) = label_col else {
        if !metric.labels.is_empty() {
            return Err(ResolveError::msg(format!(
                "namespace '{}' has no label dimensions; remove the label set",
                metric.namespace
            )));
        }
        let sql = format!(
            "SELECT CAST({agg}({expr}) AS REAL) FROM {table}
              WHERE resolution = 'raw' AND ({expr}) IS NOT NULL AND timestamp >= ?"
        );
        let value: Option<f64> =
            sqlx::query_scalar::<_, Option<f64>>(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(since)
                .fetch_one(&state.db)
                .await
                .map_err(|e| ResolveError::msg(e.to_string()))?;
        return Ok(value
            .map(|v| ResolvedSample {
                label_set: "{}".to_string(),
                value: v,
                meta: None,
            })
            .into_iter()
            .collect());
    };

    let mut filter_value: Option<&str> = None;
    for (k, v) in &metric.labels {
        if k == keycol {
            filter_value = Some(v.as_str());
        } else {
            return Err(ResolveError::msg(format!(
                "namespace '{}' supports only the '{}' label, got '{}'",
                metric.namespace, keycol, k
            )));
        }
    }
    let where_label = if filter_value.is_some() {
        format!("AND {keycol} = ?")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT {keycol}, CAST({agg}({expr}) AS REAL) FROM {table}
          WHERE resolution = 'raw' AND ({expr}) IS NOT NULL
            AND timestamp >= ? {where_label}
          GROUP BY {keycol}"
    );
    let mut q = sqlx::query_as::<_, (String, f64)>(sqlx::AssertSqlSafe(sql.as_str())).bind(since);
    if let Some(v) = filter_value {
        q = q.bind(v);
    }
    let rows = q
        .fetch_all(&state.db)
        .await
        .map_err(|e| ResolveError::msg(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|(key, value)| {
            let mut labels = BTreeMap::new();
            labels.insert(keycol.to_string(), key);
            ResolvedSample {
                label_set: canonical_labels(&labels),
                value,
                meta: None,
            }
        })
        .collect())
}

async fn resolve_inner(
    pool: &SqlitePool,
    services: Option<&Arc<dyn ServiceManager>>,
    metric: &MetricRef,
    since: i64,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    match metric.namespace.as_str() {
        "cpu" => resolve_unkeyed(pool, metric, "metrics_cpu", CPU_FIELDS, CPU_I64, since).await,
        "network_total" => {
            resolve_unkeyed(
                pool,
                metric,
                "metrics_network_total",
                NETWORK_FIELDS,
                NETWORK_I64,
                since,
            )
            .await
        }
        "memory" => {
            resolve_unkeyed(
                pool,
                metric,
                "metrics_memory",
                MEMORY_FIELDS,
                MEMORY_I64,
                since,
            )
            .await
        }
        "disk" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_disk",
                DISK_FIELDS,
                DISK_I64,
                "mount_point",
                DISK_COMPUTED,
                since,
            )
            .await
        }
        "network" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_network",
                NETWORK_FIELDS,
                NETWORK_I64,
                "interface_name",
                NO_COMPUTED,
                since,
            )
            .await
        }
        "pressure" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_pressure",
                PRESSURE_FIELDS,
                PRESSURE_I64,
                "resource",
                NO_COMPUTED,
                since,
            )
            .await
        }
        "components" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_components",
                COMPONENTS_FIELDS,
                COMPONENTS_I64,
                "label",
                NO_COMPUTED,
                since,
            )
            .await
        }
        "smart" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_smart",
                SMART_FIELDS,
                SMART_I64,
                "device",
                NO_COMPUTED,
                since,
            )
            .await
        }
        "docker" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_docker",
                DOCKER_FIELDS,
                DOCKER_I64,
                "container_id",
                DOCKER_COMPUTED,
                since,
            )
            .await
        }
        "process" => {
            resolve_keyed(
                pool,
                metric,
                "metrics_process",
                PROCESS_FIELDS,
                PROCESS_I64,
                "name",
                NO_COMPUTED,
                since,
            )
            .await
        }
        "probe" => resolve_probe(pool, metric).await,
        "heartbeat" => resolve_heartbeat(pool, metric).await,
        "service" => match services {
            Some(sm) => resolve_service(sm.as_ref(), metric).await,
            None => Err(ResolveError::msg(
                "namespace 'service' requires runtime context; use resolve_with_state",
            )),
        },
        other => Err(ResolveError::msg(format!("unknown namespace '{}'", other))),
    }
}

// ===== Single-row tables (cpu, memory) =====

/// The single-row-table counterpart to [`keyed_latest_sql`]: newest non-NULL
/// sample of one column. Extracted for the same reason — the audit explains it
/// directly, since a query built by `format!` never reaches the `.sqlx` cache.
pub(crate) fn unkeyed_latest_sql(table: &str, column: &str) -> String {
    format!(
        "SELECT {col} FROM {table}
          WHERE resolution = 'raw' AND {col} IS NOT NULL AND timestamp >= ?
          ORDER BY timestamp DESC LIMIT 1",
        col = column,
        table = table
    )
}

async fn resolve_unkeyed(
    pool: &SqlitePool,
    metric: &MetricRef,
    table: &str,
    valid_fields: &[&str],
    i64_fields: &[&str],
    since: i64,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let column = check_field(metric, valid_fields)?;
    if !metric.labels.is_empty() {
        return Err(ResolveError::msg(format!(
            "namespace '{}' has no label dimensions; remove the label set",
            metric.namespace
        )));
    }
    let expression = if table == "metrics_memory" && column == "used_percent" {
        MEMORY_COMPUTED[0].1
    } else {
        column
    };
    let sql = unkeyed_latest_sql(table, expression);

    let value_opt: Option<f64> = if i64_fields.contains(&column) {
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(since)
            .fetch_optional(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
            .map(|v| v as f64)
    } else {
        sqlx::query_scalar::<_, f64>(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(since)
            .fetch_optional(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
    };

    Ok(value_opt
        .map(|v| ResolvedSample {
            label_set: "{}".to_string(),
            value: v,
            meta: None,
        })
        .into_iter()
        .collect())
}

// ===== Keyed tables (disk, network, pressure, components) =====

/// The "latest value per key" query the evaluator runs each tick: pick the
/// row with the greatest `timestamp` per `label_column` (skipping NULLs in
/// the selected column, so a currently-NULL field falls back to its last
/// non-NULL sample). Extracted so the query-plan audit test can `EXPLAIN` the
/// real SQL against the `(resolution, label_column, timestamp)` index.
///
/// Distinct keys plus a per-key seek, not a group-wise max: `{col} IS NOT
/// NULL` is not in the index, so `GROUP BY … MAX(timestamp)` needs a row
/// lookup per entry and walks the whole partition. Seeking each key from its
/// newest end stops at the first qualifying row.
///
/// The label filter goes in the key subquery — applied only in an outer WHERE
/// it still resolves every key on the host and discards all but one.
pub(crate) fn keyed_latest_sql(
    table: &str,
    select_expr: &str,
    label_column: &str,
    where_label: &str,
) -> String {
    format!(
        "SELECT lbl, val FROM (
           SELECT k.{label} AS lbl,
                  (SELECT {col}
                     FROM {table} t
                    WHERE t.resolution = 'raw'
                      AND t.{label} = k.{label}
                      AND ({col}) IS NOT NULL
                      AND t.timestamp >= ?
                    ORDER BY t.timestamp DESC
                    LIMIT 1) AS val
             FROM (SELECT DISTINCT {label}
                     FROM {table}
                    WHERE resolution = 'raw' {where_label}) k
         ) WHERE val IS NOT NULL",
        label = label_column,
        col = select_expr,
        table = table,
        where_label = where_label,
    )
}

// Five of these describe one namespace's table and never vary at a call site;
// they belong in a descriptor the dispatch passes by reference, which is the
// shape to move to when another parameter is needed.
#[allow(clippy::too_many_arguments)]
async fn resolve_keyed(
    pool: &SqlitePool,
    metric: &MetricRef,
    table: &str,
    valid_fields: &[&str],
    i64_fields: &[&str],
    label_column: &str,
    computed: &[(&str, &str)],
    since: i64,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let column = check_field(metric, valid_fields)?;

    // A synthetic field selects its SQL expression; every other field
    // selects its own (whitelisted) column name.
    let select_expr: &str = computed
        .iter()
        .find_map(|(name, expr)| (*name == column).then_some(*expr))
        .unwrap_or(column);

    let mut filter_value: Option<&str> = None;
    for (k, v) in &metric.labels {
        if k == label_column {
            filter_value = Some(v.as_str());
        } else {
            return Err(ResolveError::msg(format!(
                "namespace '{}' supports only the '{}' label, got '{}'",
                metric.namespace, label_column, k
            )));
        }
    }

    let where_label = if filter_value.is_some() {
        format!("AND {} = ?", label_column)
    } else {
        String::new()
    };

    let sql = keyed_latest_sql(table, select_expr, label_column, &where_label);

    // `since` first: the correlated subquery it belongs to appears before the
    // key subquery the label filter lands in.
    let rows: Vec<(String, f64)> = if i64_fields.contains(&column) {
        let mut q2 =
            sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql.as_str())).bind(since);
        if let Some(v) = filter_value {
            q2 = q2.bind(v);
        }
        q2.fetch_all(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
            .into_iter()
            .map(|(k, v)| (k, v as f64))
            .collect()
    } else {
        let mut q2 =
            sqlx::query_as::<_, (String, f64)>(sqlx::AssertSqlSafe(sql.as_str())).bind(since);
        if let Some(v) = filter_value {
            q2 = q2.bind(v);
        }
        q2.fetch_all(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
    };

    Ok(rows
        .into_iter()
        .map(|(label_value, v)| {
            let mut labels = BTreeMap::new();
            labels.insert(label_column.to_string(), label_value);
            ResolvedSample {
                label_set: canonical_labels(&labels),
                value: v,
                meta: None,
            }
        })
        .collect())
}

// ===== In-memory snapshot resolution (hot namespaces) =====
//
// The alert evaluator's dominant cost was a "latest value per key" DB query
// per rule per tick. But the collector already holds the current values in
// `state.stats_latest`, so for the host-metric namespaces we resolve straight
// from that snapshot — zero DB round-trips on the hot path.
//
// Only always-present fields take this path. Optional/enriched fields (steal,
// iowait, inode_used_percent, io_util, page faults, …) can legitimately be
// NULL for a tick, and the resolver's contract is to fall back to the last
// non-NULL sample; that history lives only in the DB, so those fields (and the
// boot window before the first tick populates the snapshot) fall through to
// the DB query. The common rules — cpu.usage_percent, memory.used_bytes,
// disk.used_percent, network rates, pressure — are all always-present and
// resolve entirely in memory.

/// Always-present CPU fields (subset of `CPU_FIELDS`).
const CPU_SNAP: &[&str] = &["usage_percent", "load_1m", "load_5m", "load_15m"];
/// Always-present memory fields (subset of `MEMORY_FIELDS`).
const MEMORY_SNAP: &[&str] = &[
    "used_bytes",
    "available_bytes",
    "cached_bytes",
    "swap_used_bytes",
];
/// Always-present disk fields (subset of `DISK_FIELDS`; `used_percent` is
/// computed and yields NaN only for a 0-total phantom mount).
const DISK_SNAP: &[&str] = &[
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "used_percent",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
];
// network + pressure fields are all always-present, so their whole whitelists
// (`NETWORK_FIELDS` / `PRESSURE_FIELDS`) are snapshot-resolvable.

/// Resolve a metric from the live `AllStats` snapshot.
/// - `Some(Ok(samples))` — resolved in memory.
/// - `Some(Err(_))` — snapshot namespace but malformed metric (bad field /
///   label), same error the DB path raises.
/// - `None` — not snapshot-resolvable (non-hot namespace or an optional
///   field); caller uses the DB path.
fn resolve_from_snapshot(
    metric: &MetricRef,
    snap: &AllStats,
) -> Option<Result<Vec<ResolvedSample>, ResolveError>> {
    let field = metric.field.as_str();
    match metric.namespace.as_str() {
        "cpu" if CPU_SNAP.contains(&field) => {
            Some(unkeyed_snap(metric, cpu_snap_value(&snap.cpu, field)))
        }
        "memory" if MEMORY_SNAP.contains(&field) => {
            Some(unkeyed_snap(metric, memory_snap_value(&snap.memory, field)))
        }
        "disk" if DISK_SNAP.contains(&field) => Some(keyed_snap(
            metric,
            "mount_point",
            snap.disks
                .iter()
                .map(|d| (d.mount_point.clone(), disk_snap_value(d, field))),
        )),
        "network_total" if NETWORK_FIELDS.contains(&field) => {
            let values: Vec<f64> = snap
                .network
                .iter()
                .filter(|n| !crate::services::system::is_tunnel_interface(&n.interface))
                .map(|n| network_snap_value(n, field))
                .collect();
            if values.is_empty() {
                None
            } else {
                Some(unkeyed_snap(metric, values.into_iter().sum()))
            }
        }
        "network" if NETWORK_FIELDS.contains(&field) => Some(keyed_snap(
            metric,
            "interface_name",
            snap.network
                .iter()
                .map(|n| (n.interface.clone(), network_snap_value(n, field))),
        )),
        "pressure" if PRESSURE_FIELDS.contains(&field) => {
            // No PSI on this host at all → fall to the DB path (also empty).
            let p = snap.pressure.as_ref()?;
            let items = [("cpu", &p.cpu), ("memory", &p.memory), ("io", &p.io)]
                .into_iter()
                .filter_map(|(res, opt)| {
                    opt.as_ref()
                        .map(|ps| (res.to_string(), pressure_snap_value(ps, field)))
                });
            Some(keyed_snap(metric, "resource", items))
        }
        _ => None,
    }
}

/// Build the single sample for an unkeyed namespace, rejecting stray labels
/// exactly as the DB path does.
fn unkeyed_snap(metric: &MetricRef, value: f64) -> Result<Vec<ResolvedSample>, ResolveError> {
    if !metric.labels.is_empty() {
        return Err(ResolveError::msg(format!(
            "namespace '{}' has no label dimensions; remove the label set",
            metric.namespace
        )));
    }
    Ok(vec![ResolvedSample {
        label_set: "{}".to_string(),
        value,
        meta: None,
    }])
}

/// Build one sample per key from a snapshot iterator, honouring an optional
/// single-label filter (same validation/semantics as `resolve_keyed`).
fn keyed_snap(
    metric: &MetricRef,
    label_column: &str,
    items: impl Iterator<Item = (String, f64)>,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let mut filter_value: Option<&str> = None;
    for (k, v) in &metric.labels {
        if k == label_column {
            filter_value = Some(v.as_str());
        } else {
            return Err(ResolveError::msg(format!(
                "namespace '{}' supports only the '{}' label, got '{}'",
                metric.namespace, label_column, k
            )));
        }
    }
    Ok(items
        .filter(|(key, _)| filter_value.is_none_or(|f| f == key))
        .map(|(key, value)| {
            let mut labels = BTreeMap::new();
            labels.insert(label_column.to_string(), key);
            ResolvedSample {
                label_set: canonical_labels(&labels),
                value,
                meta: None,
            }
        })
        .collect())
}

fn cpu_snap_value(c: &CpuStats, field: &str) -> f64 {
    match field {
        "usage_percent" => c.usage_percent,
        "load_1m" => c.load_avg.one,
        "load_5m" => c.load_avg.five,
        "load_15m" => c.load_avg.fifteen,
        _ => f64::NAN,
    }
}

fn memory_snap_value(m: &MemoryStats, field: &str) -> f64 {
    match field {
        "total_bytes" => m.total_bytes as f64,
        "used_percent" => {
            if m.total_bytes > 0 {
                100.0 * m.total_bytes.saturating_sub(m.available_bytes) as f64
                    / m.total_bytes as f64
            } else {
                f64::NAN
            }
        }
        "used_bytes" => m.used_bytes as f64,
        "available_bytes" => m.available_bytes as f64,
        "cached_bytes" => m.cached_bytes as f64,
        "swap_used_bytes" => m.swap_used_bytes as f64,
        _ => f64::NAN,
    }
}

fn disk_snap_value(d: &DiskStats, field: &str) -> f64 {
    match field {
        "total_bytes" => d.total_bytes as f64,
        "used_bytes" => d.used_bytes as f64,
        "available_bytes" => d.available_bytes as f64,
        "used_percent" => {
            if d.total_bytes > 0 {
                d.used_bytes as f64 * 100.0 / d.total_bytes as f64
            } else {
                f64::NAN
            }
        }
        "read_bytes_per_sec" => d.read_bytes_per_sec as f64,
        "write_bytes_per_sec" => d.write_bytes_per_sec as f64,
        _ => f64::NAN,
    }
}

fn network_snap_value(n: &NetworkStats, field: &str) -> f64 {
    match field {
        "rx_bytes_per_sec" => n.rx_bytes_per_sec as f64,
        "tx_bytes_per_sec" => n.tx_bytes_per_sec as f64,
        "rx_packets_per_sec" => n.rx_packets_per_sec as f64,
        "tx_packets_per_sec" => n.tx_packets_per_sec as f64,
        "errors_in_per_sec" => n.errors_in_per_sec as f64,
        "errors_out_per_sec" => n.errors_out_per_sec as f64,
        _ => f64::NAN,
    }
}

fn pressure_snap_value(p: &PressureStats, field: &str) -> f64 {
    match field {
        "some_avg10" => p.some_avg10,
        "some_avg60" => p.some_avg60,
        "some_avg300" => p.some_avg300,
        "full_avg10" => p.full_avg10,
        "full_avg60" => p.full_avg60,
        "full_avg300" => p.full_avg300,
        _ => f64::NAN,
    }
}

// ===== Probe namespace =====

/// Latest value per `(probe_name, labels)` stream. The label predicates are
/// caller-built (each carries its own placeholders), so the shape varies with
/// the rule; the audit explains the unfiltered and filtered variants.
pub(crate) fn probe_latest_sql(probe_clause: &str, json_clauses: &str) -> String {
    format!(
        "SELECT probe_name, labels, value
           FROM metrics_probe
          WHERE resolution = 'raw'
            AND metric_name = ?
            {probe_clause}
            {json_clauses}
            AND (probe_name, labels, timestamp) IN (
              SELECT probe_name, labels, MAX(timestamp)
                FROM metrics_probe
               WHERE resolution = 'raw'
                 AND metric_name = ?
                 {probe_clause}
                 {json_clauses}
               GROUP BY probe_name, labels
            )",
        probe_clause = probe_clause,
        json_clauses = json_clauses,
    )
}

async fn resolve_probe(
    pool: &SqlitePool,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    // Probe `field` is the metric_name the script emitted. We cannot
    // whitelist it — the script defines it. Charset check (already done
    // by the parser via the ident rule) is the only validation.
    let metric_name = metric.field.as_str();

    // `probe_name` is special: it's a column in metrics_probe, not part
    // of the JSON labels blob. Treat it as a separate equality filter.
    let mut probe_name_filter: Option<&str> = None;
    let mut json_filters: BTreeMap<&str, &str> = BTreeMap::new();
    for (k, v) in &metric.labels {
        if k == "probe_name" {
            probe_name_filter = Some(v.as_str());
        } else {
            json_filters.insert(k.as_str(), v.as_str());
        }
    }

    // Build per-JSON-label predicates. `json_extract(labels, '$.<key>')`
    // returns NULL when the key is missing — comparison against a
    // string evaluates to false in SQLite, which is the subset semantic
    // we want (rule's labels must be present in the row).
    // The JSON path is bound as a parameter (not interpolated) so this stays
    // injection-safe by construction even if the expression parser's ident
    // charset ever loosens. Each clause carries two placeholders: path, value.
    let mut json_clauses = String::new();
    for _ in json_filters.keys() {
        json_clauses.push_str(" AND json_extract(labels, ?) = ?");
    }

    let probe_clause = if probe_name_filter.is_some() {
        " AND probe_name = ?"
    } else {
        ""
    };

    // Group on (probe_name, labels) so a probe emitting multiple
    // labelled streams (`{jail=sshd}`, `{jail=ftp}`) returns each as a
    // separate sample.
    let sql = probe_latest_sql(probe_clause, &json_clauses);

    let mut q = sqlx::query_as::<_, (String, String, f64)>(sqlx::AssertSqlSafe(sql.as_str()));
    q = q.bind(metric_name);
    if let Some(name) = probe_name_filter {
        q = q.bind(name);
    }
    for (k, v) in &json_filters {
        q = q.bind(format!("$.{}", k));
        q = q.bind(*v);
    }
    q = q.bind(metric_name);
    if let Some(name) = probe_name_filter {
        q = q.bind(name);
    }
    for (k, v) in &json_filters {
        q = q.bind(format!("$.{}", k));
        q = q.bind(*v);
    }

    let rows = q
        .fetch_all(pool)
        .await
        .map_err(|e| ResolveError::msg(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|(probe_name, labels_json, value)| {
            // Parse the row's labels JSON, then add probe_name as a
            // synthetic label so the alert_state.label_set carries
            // enough identity for the evaluator to dedup.
            let mut labels: BTreeMap<String, String> =
                serde_json::from_str(&labels_json).unwrap_or_default();
            labels.insert("probe_name".to_string(), probe_name);
            ResolvedSample {
                label_set: canonical_labels(&labels),
                value,
                meta: None,
            }
        })
        .collect())
}

// ===== Heartbeat namespace =====

/// Resolve `heartbeat.up` / `heartbeat.late` from the check registry.
///
/// One sample per existing check, every tick, INCLUDING disabled and
/// paused ones — the evaluator strands a Firing label_set that stops
/// appearing, so "no sample" is reserved for checks that were deleted
/// (which the evaluator prune then resolves). A `{check=...}` filter
/// matching nothing yields an empty vec, never an error: rules must be
/// creatable before their check and must survive a rename underneath.
///
/// `up` is 0 only for down/failed. `late` rises at the grace boundary
/// and stays 1 through down/failed, so a warn-tier `late == 1` rule
/// doesn't resolve while things get worse.
async fn resolve_heartbeat(
    pool: &SqlitePool,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let field = check_field(metric, HEARTBEAT_FIELDS)?;

    let mut name_filter: Option<&str> = None;
    for (k, v) in &metric.labels {
        if k == "check" {
            name_filter = Some(v.as_str());
        } else {
            return Err(ResolveError::msg(format!(
                "namespace 'heartbeat' supports only the 'check' label, got '{}'",
                k
            )));
        }
    }

    let repo = crate::storage::repositories::HeartbeatRepository::new(pool.clone());
    let checks = repo
        .list_all()
        .await
        .map_err(|e| ResolveError::msg(format!("heartbeat lookup failed: {}", e)))?;

    use crate::models::heartbeat::HeartbeatState;
    let now = chrono::Utc::now().timestamp();

    Ok(checks
        .into_iter()
        .filter(|c| name_filter.is_none_or(|f| c.name == f))
        .map(|c| {
            let state = c.state(now);
            let up = !matches!(state, HeartbeatState::Down | HeartbeatState::Failed);
            let late = matches!(
                state,
                HeartbeatState::Late | HeartbeatState::Down | HeartbeatState::Failed
            );
            let value = match field {
                "late" => late as i64 as f64,
                _ => up as i64 as f64,
            };
            let meta = match state {
                HeartbeatState::Paused => match c.paused_until {
                    Some(until) => format!("paused until {}", until),
                    None => "paused".to_string(),
                },
                s => s.as_str().to_string(),
            };
            let mut labels = BTreeMap::new();
            labels.insert("check".to_string(), c.name);
            ResolvedSample {
                label_set: canonical_labels(&labels),
                value,
                meta: Some(meta),
            }
        })
        .collect())
}

// ===== Service namespace =====

async fn resolve_service(
    services: &dyn ServiceManager,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let _ = check_field(metric, SERVICE_FIELDS)?;

    let unit = metric.labels.get("unit").ok_or_else(|| {
        ResolveError::msg(
            "namespace 'service' requires a `unit` label, e.g. service.up{unit=\"nginx.service\"}",
        )
    })?;
    if metric.labels.len() > 1 {
        return Err(ResolveError::msg(
            "namespace 'service' supports only the `unit` label",
        ));
    }

    let svc = services
        .get(unit)
        .await
        .map_err(|e| ResolveError::msg(format!("service '{}' lookup failed: {}", unit, e)))?;

    use crate::platform::services::ServiceState;
    let up = matches!(svc.state, ServiceState::Running);
    let state_name = match svc.state {
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Stopping => "stopping",
        ServiceState::Paused => "paused",
        ServiceState::Failed => "failed",
        ServiceState::Reloading => "reloading",
        ServiceState::Unknown => "unknown",
    };

    let mut labels = BTreeMap::new();
    labels.insert("unit".to_string(), unit.clone());

    Ok(vec![ResolvedSample {
        label_set: canonical_labels(&labels),
        value: if up { 1.0 } else { 0.0 },
        meta: Some(state_name.to_string()),
    }])
}

// ===== helpers =====

fn check_field<'a>(metric: &'a MetricRef, valid: &[&str]) -> Result<&'a str, ResolveError> {
    if !valid.contains(&metric.field.as_str()) {
        return Err(ResolveError::msg(format!(
            "field '{}' is not valid for namespace '{}'; expected one of {:?}",
            metric.field, metric.namespace, valid
        )));
    }
    Ok(metric.field.as_str())
}

/// Canonical JSON for a sorted-key label map. `BTreeMap`'s iteration
/// already gives us the order; serde_json without spaces gives a
/// stable byte representation matching what the evaluator stores in
/// `alert_state.label_set`.
fn canonical_labels(labels: &BTreeMap<String, String>) -> String {
    serde_json::to_string(labels).unwrap_or_else(|_| "{}".to_string())
}

// ===== History summaries (assistant `metric_history`) =====

/// Rollup resolutions the history summary accepts. `raw` is the live tick;
/// `1m`/`5m`/`1h` are produced by the rollup worker.
pub const HISTORY_RESOLUTIONS: &[&str] = &["raw", "1m", "5m", "1h"];

/// One field's aggregate over a time window, per natural key. `last` is the
/// current value taken from the same "latest per key" path the evaluator uses,
/// so a summary and a live gauge never disagree. `last` is `NaN` when the key
/// has no current sample.
#[derive(Debug, Clone)]
pub struct FieldSummary {
    /// True only when min/max/avg/count describe observations rather than bucket means.
    pub observed_statistics: bool,
    pub label_set: String,
    pub count: i64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub last: f64,
}

/// History-capable numeric namespaces → `(table, valid_fields, computed,
/// label_column)`. Deliberately a subset of the resolver's namespaces: the
/// special ones (probe/heartbeat/service) and slow-moving smart/components are
/// excluded — history is about performance trends. Field *names* still validate
/// against the same whitelists the evaluator uses, keeping this injection-safe.
#[allow(clippy::type_complexity)]
fn history_descriptor(
    namespace: &str,
) -> Option<(
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, &'static str)],
    Option<&'static str>,
)> {
    match namespace {
        "cpu" => Some(("metrics_cpu", CPU_FIELDS, NO_COMPUTED, None)),
        "network_total" => Some(("metrics_network_total", NETWORK_FIELDS, NO_COMPUTED, None)),
        "memory" => Some(("metrics_memory", MEMORY_FIELDS, MEMORY_COMPUTED, None)),
        "disk" => Some((
            "metrics_disk",
            DISK_FIELDS,
            DISK_COMPUTED,
            Some("mount_point"),
        )),
        "network" => Some((
            "metrics_network",
            NETWORK_FIELDS,
            NO_COMPUTED,
            Some("interface_name"),
        )),
        "pressure" => Some((
            "metrics_pressure",
            PRESSURE_FIELDS,
            NO_COMPUTED,
            Some("resource"),
        )),
        "docker" => Some((
            "metrics_docker",
            DOCKER_FIELDS,
            DOCKER_COMPUTED,
            Some("container_id"),
        )),
        "process" => Some(("metrics_process", PROCESS_FIELDS, NO_COMPUTED, Some("name"))),
        _ => None,
    }
}

/// Aggregate a single metric field over `[start, end]` at `resolution`,
/// returning one summary per natural key (or a single summary for the unkeyed
/// namespaces). The `last` value is merged in from [`resolve_with_state`].
pub async fn history_summary(
    state: &crate::state::AppState,
    metric: &MetricRef,
    resolution: &str,
    start: i64,
    end: i64,
) -> Result<Vec<FieldSummary>, ResolveError> {
    let (table, fields, computed, label_col) =
        history_descriptor(&metric.namespace).ok_or_else(|| {
            ResolveError::msg(format!(
                "history is not available for namespace '{}'",
                metric.namespace
            ))
        })?;

    if !HISTORY_RESOLUTIONS.contains(&resolution) {
        return Err(ResolveError::msg(format!(
            "unknown resolution '{resolution}'; expected one of {HISTORY_RESOLUTIONS:?}"
        )));
    }

    let column = check_field(metric, fields)?;
    // Computed fields (e.g. disk.used_percent) aggregate over their SQL
    // expression; plain fields over their own column. CAST(... AS REAL) makes
    // MIN/MAX decode uniformly whether the column is INTEGER or REAL.
    let expr: &str = computed
        .iter()
        .find_map(|(name, e)| (*name == column).then_some(*e))
        .unwrap_or(column);

    // Current value(s) per key, straight from the evaluator's own path.
    let last_by_label: BTreeMap<String, f64> = resolve_with_state(state, metric)
        .await?
        .into_iter()
        .map(|s| (s.label_set, s.value))
        .collect();

    let pool = &state.db;
    let supports = matches!(
        metric.namespace.as_str(),
        "cpu" | "memory" | "disk" | "network" | "network_total"
    );
    let (count_expr, min_expr, max_expr, avg_expr) = if resolution != "raw" && supports {
        let complete = "MIN(CASE WHEN summary_version=1 THEN 1 ELSE 0 END)=1";
        (
            format!("CASE WHEN {complete} THEN SUM({column}_valid_count) ELSE COUNT(*) END"),
            format!("CASE WHEN {complete} THEN MIN({column}_min) END"),
            format!("CASE WHEN {complete} THEN MAX({column}_max) END"),
            format!(
                "CASE WHEN {complete} THEN 1.0 * SUM({column}_sum)/NULLIF(SUM({column}_valid_count),0) END"
            ),
        )
    } else {
        (
            format!("COUNT({expr})"),
            format!("CAST(MIN({expr}) AS REAL)"),
            format!("CAST(MAX({expr}) AS REAL)"),
            format!("AVG({expr})"),
        )
    };
    // A coarse bucket describes its complete interval, including a partially selected edge.
    let width: i64 = sqlx::query_scalar("SELECT interval_seconds FROM resolutions WHERE name=?")
        .bind(resolution)
        .fetch_one(pool)
        .await
        .map_err(|e| ResolveError::msg(e.to_string()))?;
    let start = if resolution == "raw" {
        start
    } else {
        start.div_euclid(width.max(1)) * width.max(1)
    };
    let mut out = Vec::new();

    if let Some(keycol) = label_col {
        let mut filter_value: Option<&str> = None;
        for (k, v) in &metric.labels {
            if k == keycol {
                filter_value = Some(v.as_str());
            } else {
                return Err(ResolveError::msg(format!(
                    "namespace '{}' supports only the '{}' label, got '{}'",
                    metric.namespace, keycol, k
                )));
            }
        }
        let where_label = if filter_value.is_some() {
            format!("AND {keycol} = ?")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {keycol},
                    {count_expr},
                    {min_expr},
                    {max_expr},
                    {avg_expr}
               FROM {table}
              WHERE resolution = ? AND {expr} IS NOT NULL
                AND timestamp >= ? AND timestamp <= ? {where_label}
              GROUP BY {keycol}"
        );
        let mut q = sqlx::query_as::<_, (String, i64, Option<f64>, Option<f64>, Option<f64>)>(
            sqlx::AssertSqlSafe(sql.as_str()),
        )
        .bind(resolution)
        .bind(start)
        .bind(end);
        if let Some(v) = filter_value {
            q = q.bind(v);
        }
        let rows = q
            .fetch_all(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?;
        for (keyval, count, min, max, avg) in rows {
            if count == 0 {
                continue;
            }
            let mut labels = BTreeMap::new();
            labels.insert(keycol.to_string(), keyval);
            let label_set = canonical_labels(&labels);
            let last = last_by_label.get(&label_set).copied().unwrap_or(f64::NAN);
            out.push(FieldSummary {
                observed_statistics: resolution == "raw"
                    || (supports && min.is_some() && max.is_some() && avg.is_some()),
                label_set,
                count,
                min: min.unwrap_or(f64::NAN),
                max: max.unwrap_or(f64::NAN),
                avg: avg.unwrap_or(f64::NAN),
                last,
            });
        }
    } else {
        if !metric.labels.is_empty() {
            return Err(ResolveError::msg(format!(
                "namespace '{}' has no label dimensions; remove the label set",
                metric.namespace
            )));
        }
        let sql = format!(
            "SELECT {count_expr},
                    {min_expr},
                    {max_expr},
                    {avg_expr}
               FROM {table}
              WHERE resolution = ? AND {expr} IS NOT NULL
                AND timestamp >= ? AND timestamp <= ?"
        );
        let row: (i64, Option<f64>, Option<f64>, Option<f64>) =
            sqlx::query_as::<_, (i64, Option<f64>, Option<f64>, Option<f64>)>(sqlx::AssertSqlSafe(
                sql.as_str(),
            ))
            .bind(resolution)
            .bind(start)
            .bind(end)
            .fetch_one(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?;
        let (count, min, max, avg) = row;
        if count > 0 {
            let last = last_by_label.get("{}").copied().unwrap_or(f64::NAN);
            out.push(FieldSummary {
                observed_statistics: resolution == "raw"
                    || (supports && min.is_some() && max.is_some() && avg.is_some()),
                label_set: "{}".to_string(),
                count,
                min: min.unwrap_or(f64::NAN),
                max: max.unwrap_or(f64::NAN),
                avg: avg.unwrap_or(f64::NAN),
                last,
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Executor;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fixture() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("memory pool");

        let schema = r#"
            CREATE TABLE metrics_cpu (
                resolution TEXT, timestamp INTEGER,
                usage_percent REAL,
                load_1m REAL, load_5m REAL, load_15m REAL,
                steal_percent REAL, iowait_percent REAL, guest_percent REAL,
                context_switches_per_sec INTEGER,
                process_forks_per_sec INTEGER
            );
            CREATE TABLE metrics_memory (
                resolution TEXT, timestamp INTEGER,
                used_bytes INTEGER, available_bytes INTEGER,
                cached_bytes INTEGER, swap_used_bytes INTEGER,
                page_faults_minor_per_sec INTEGER, page_faults_major_per_sec INTEGER,
                swap_in_pages_per_sec INTEGER, swap_out_pages_per_sec INTEGER
            );
            CREATE TABLE metrics_disk (
                resolution TEXT, timestamp INTEGER,
                mount_point TEXT,
                total_bytes INTEGER,
                used_bytes INTEGER, available_bytes INTEGER,
                read_bytes_per_sec INTEGER, write_bytes_per_sec INTEGER,
                inode_used_percent REAL
            );
            CREATE TABLE metrics_pressure (
                resolution TEXT, timestamp INTEGER,
                resource TEXT,
                some_avg10 REAL, some_avg60 REAL, some_avg300 REAL,
                full_avg10 REAL, full_avg60 REAL, full_avg300 REAL
            );
            CREATE TABLE metrics_components (
                resolution TEXT, timestamp INTEGER,
                label TEXT,
                temperature_c REAL, max_c REAL, critical_c REAL
            );
            CREATE TABLE metrics_probe (
                resolution TEXT, timestamp INTEGER,
                probe_name TEXT, metric_name TEXT,
                labels TEXT, value REAL
            );
            CREATE TABLE metrics_smart (
                resolution TEXT, timestamp INTEGER,
                device TEXT,
                health_passed INTEGER, temperature_c REAL,
                power_on_hours INTEGER, power_cycles INTEGER,
                reallocated_sectors INTEGER, pending_sectors INTEGER,
                uncorrectable_sectors INTEGER, udma_crc_errors INTEGER,
                percentage_used INTEGER, available_spare_percent INTEGER,
                media_errors INTEGER
            );
            CREATE TABLE heartbeat_checks (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT,
                slug_hash TEXT NOT NULL UNIQUE,
                period_secs INTEGER NOT NULL,
                grace_secs INTEGER NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_ping_at INTEGER,
                failed INTEGER NOT NULL DEFAULT 0,
                last_fail_at INTEGER,
                paused_at INTEGER,
                paused_until INTEGER,
                pause_origin TEXT,
                pause_reason TEXT,
                pause_until_ping INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
        "#;
        pool.execute(schema).await.expect("schema");
        pool
    }

    /// The discriminator the prune path leans on. A key whose samples are old
    /// still answers "present" — that is the whole point, since the alternative
    /// is telling an operator a rule recovered when the collector stopped.
    #[tokio::test]
    async fn a_stale_key_is_present_and_a_removed_one_is_not() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk
               (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes)
             VALUES ('raw', 100, '/', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed");

        assert!(
            key_still_present(&pool, "disk", r#"{"mount_point":"/"}"#).await,
            "a mount with only old samples is still a mount"
        );
        assert!(
            !key_still_present(&pool, "disk", r#"{"mount_point":"/gone"}"#).await,
            "a mount with no samples at all has been removed"
        );
        // The unkeyed series answers for the host as a whole; this fixture
        // never wrote a memory sample, so there is nothing to hold state for.
        assert!(!key_still_present(&pool, "memory", "{}").await);
        assert!(
            !key_still_present(&pool, "service", "{}").await,
            "namespaces with no metrics series cannot answer this"
        );
    }

    /// Every namespace must get a window wider than its own write cadence, or
    /// its newest sample is stale on arrival and rules go quiet. The failure
    /// that reached this: `components` writes a row every 30th stats tick, and
    /// was given the tick rate, putting the window exactly on the cadence.
    #[test]
    fn every_namespace_outlives_its_own_write_cadence() {
        const STATS_MS: u64 = 2_000;
        const DOCKER_MS: u64 = 3_000;
        const SMART_MS: u64 = 1_800_000;

        for ns in [
            "cpu",
            "memory",
            "disk",
            "network",
            "pressure",
            "components",
            "docker",
            "smart",
            "process",
        ] {
            let cadence = write_interval_secs(ns, STATS_MS, DOCKER_MS, SMART_MS)
                .unwrap_or_else(|| panic!("{ns} has no write cadence"));
            let now = 1_000_000;
            let window = now - freshness_since(now, cadence);
            assert!(
                window > cadence,
                "{ns}: window {window}s does not outlive its {cadence}s cadence"
            );
        }

        // The one that was wrong, pinned to the collector it comes from.
        assert_eq!(
            write_interval_secs("components", STATS_MS, DOCKER_MS, SMART_MS),
            Some(60),
            "components follows the sensor refresh, not the tick"
        );
        assert!(
            write_interval_secs("probe", STATS_MS, DOCKER_MS, SMART_MS).is_none(),
            "probe derives its own window from observed runs"
        );
    }

    fn metric(ns: &str, field: &str, labels: &[(&str, &str)]) -> MetricRef {
        let mut m = BTreeMap::new();
        for (k, v) in labels {
            m.insert((*k).to_string(), (*v).to_string());
        }
        MetricRef {
            namespace: ns.into(),
            field: field.into(),
            labels: m,
        }
    }

    #[tokio::test]
    async fn cpu_unkeyed_latest() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent)
             VALUES ('raw', 100, 10.0), ('raw', 200, 75.5)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(&pool, &metric("cpu", "usage_percent", &[]), i64::MIN)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_set, "{}");
        assert_eq!(out[0].value, 75.5);
    }

    #[tokio::test]
    async fn cpu_unknown_field_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("cpu", "no_such_field", &[]), i64::MIN)
            .await
            .unwrap_err();
        assert!(err.message.contains("not valid"));
    }

    #[tokio::test]
    async fn cpu_with_labels_rejected() {
        let pool = fixture().await;
        let err = resolve(
            &pool,
            &metric("cpu", "usage_percent", &[("x", "y")]),
            i64::MIN,
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("no label dimensions"));
    }

    #[tokio::test]
    async fn cpu_no_data_returns_empty() {
        let pool = fixture().await;
        let out = resolve(&pool, &metric("cpu", "usage_percent", &[]), i64::MIN)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn cpu_i64_field_coerced_to_f64() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_cpu (resolution, timestamp, context_switches_per_sec)
             VALUES ('raw', 100, 35469)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric("cpu", "context_switches_per_sec", &[]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out[0].value, 35469.0);
    }

    #[tokio::test]
    async fn memory_used_bytes_i64() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_memory (resolution, timestamp, used_bytes)
             VALUES ('raw', 100, 6291456000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(&pool, &metric("memory", "used_bytes", &[]), i64::MIN)
            .await
            .unwrap();
        assert_eq!(out[0].value, 6_291_456_000.0);
    }

    #[tokio::test]
    async fn disk_unfiltered_returns_all_mounts() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, used_bytes)
             VALUES
               ('raw', 100, '/',     1000),
               ('raw', 100, '/boot',  500),
               ('raw', 200, '/',     2000),
               ('raw', 200, '/boot',  500)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut out = resolve(&pool, &metric("disk", "used_bytes", &[]), i64::MIN)
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label_set, r#"{"mount_point":"/"}"#);
        assert_eq!(out[0].value, 2000.0);
        assert_eq!(out[1].label_set, r#"{"mount_point":"/boot"}"#);
        assert_eq!(out[1].value, 500.0);
    }

    #[tokio::test]
    async fn disk_with_filter_returns_one() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, used_bytes)
             VALUES ('raw', 100, '/', 1000), ('raw', 100, '/boot', 500)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric("disk", "used_bytes", &[("mount_point", "/")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 1000.0);
    }

    #[tokio::test]
    async fn disk_unknown_label_rejected() {
        let pool = fixture().await;
        let err = resolve(
            &pool,
            &metric("disk", "used_bytes", &[("interface_name", "eth0")]),
            i64::MIN,
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("'mount_point'"));
    }

    #[tokio::test]
    async fn disk_total_bytes_resolves() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, total_bytes, used_bytes)
             VALUES ('raw', 100, '/', 5000, 1000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric("disk", "total_bytes", &[("mount_point", "/")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 5000.0);
    }

    #[tokio::test]
    async fn disk_used_percent_computed() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, total_bytes, used_bytes)
             VALUES ('raw', 100, '/data', 1000, 900)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric("disk", "used_percent", &[("mount_point", "/data")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].value - 90.0).abs() < 1e-9,
            "expected 90.0, got {}",
            out[0].value
        );
        assert_eq!(out[0].label_set, r#"{"mount_point":"/data"}"#);
    }

    #[tokio::test]
    async fn disk_used_percent_zero_total_falls_back() {
        // total_bytes=0 -> NULLIF -> NULL, so the newest sample is skipped
        // and the resolver falls back to the last row whose computed value
        // is non-NULL (mirrors keyed_uses_latest_non_null for a synthetic
        // field).
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, total_bytes, used_bytes)
             VALUES ('raw', 100, '/', 200, 50), ('raw', 200, '/', 0, 50)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(&pool, &metric("disk", "used_percent", &[]), i64::MIN)
            .await
            .unwrap();
        assert_eq!(
            out.len(),
            1,
            "mount must not vanish when newest value is NULL"
        );
        assert_eq!(out[0].value, 25.0, "should fall back to last non-NULL");
    }

    #[tokio::test]
    async fn keyed_uses_latest_non_null_not_latest_row() {
        // Regression: the inner MAX(timestamp) subquery must apply the same
        // `<col> IS NOT NULL` filter the outer query does. Otherwise a mount
        // whose newest sample is NULL in the queried column (inode_used_percent
        // is NULL on non-Linux / before first enriched tick / on statvfs
        // timeout) vanishes from the result entirely instead of falling back
        // to its last non-NULL sample — which churns Ok rows and strands
        // Firing/Pending state.
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_disk (resolution, timestamp, mount_point, inode_used_percent)
             VALUES ('raw', 100, '/', 42.0), ('raw', 200, '/', NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(&pool, &metric("disk", "inode_used_percent", &[]), i64::MIN)
            .await
            .unwrap();
        assert_eq!(
            out.len(),
            1,
            "mount must not vanish when latest row is NULL"
        );
        assert_eq!(out[0].value, 42.0, "should fall back to latest non-NULL");
        assert_eq!(out[0].label_set, r#"{"mount_point":"/"}"#);
    }

    #[tokio::test]
    async fn pressure_resource_label() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_pressure
                (resolution, timestamp, resource,
                 some_avg10, some_avg60, some_avg300, full_avg10, full_avg60, full_avg300)
             VALUES
               ('raw', 100, 'cpu', 1.5, 1.6, 1.7, 0, 0, 0),
               ('raw', 100, 'memory', 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
               ('raw', 100, 'io', 0.0, 0.0, 0.0, 0.0, 0.0, 0.0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric("pressure", "some_avg10", &[("resource", "cpu")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 1.5);
        assert_eq!(out[0].label_set, r#"{"resource":"cpu"}"#);
    }

    #[tokio::test]
    async fn probe_with_labels_subset() {
        let pool = fixture().await;
        sqlx::query(
            r#"INSERT INTO metrics_probe
                 (resolution, timestamp, probe_name, metric_name, labels, value)
               VALUES
                 ('raw', 100, 'fail2ban', 'banned', '{"jail":"sshd"}', 5.0),
                 ('raw', 100, 'fail2ban', 'banned', '{"jail":"ftp"}',  2.0),
                 ('raw', 200, 'fail2ban', 'banned', '{"jail":"sshd"}', 7.0)"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let out = resolve(
            &pool,
            &metric(
                "probe",
                "banned",
                &[("probe_name", "fail2ban"), ("jail", "sshd")],
            ),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 7.0);
        assert_eq!(
            out[0].label_set,
            r#"{"jail":"sshd","probe_name":"fail2ban"}"#
        );
    }

    #[tokio::test]
    async fn probe_unfiltered_returns_each_label_combo() {
        let pool = fixture().await;
        sqlx::query(
            r#"INSERT INTO metrics_probe
                 (resolution, timestamp, probe_name, metric_name, labels, value)
               VALUES
                 ('raw', 200, 'fail2ban', 'banned', '{"jail":"sshd"}', 7.0),
                 ('raw', 200, 'fail2ban', 'banned', '{"jail":"ftp"}',  3.0)"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut out = resolve(&pool, &metric("probe", "banned", &[]), i64::MIN)
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn smart_keyed_by_device() {
        let pool = fixture().await;
        sqlx::query(
            "INSERT INTO metrics_smart
               (resolution, timestamp, device, health_passed, temperature_c, reallocated_sectors)
             VALUES
               ('raw', 100, '/dev/sda',   1, 34.0, 0),
               ('raw', 100, '/dev/nvme0', 1, 41.0, NULL),
               ('raw', 200, '/dev/sda',   0, 52.0, 1532)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // health_passed is INTEGER → coerced to f64; latest row per device wins.
        let mut out = resolve(&pool, &metric("smart", "health_passed", &[]), i64::MIN)
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label_set, r#"{"device":"/dev/nvme0"}"#);
        assert_eq!(out[0].value, 1.0);
        assert_eq!(out[1].label_set, r#"{"device":"/dev/sda"}"#);
        assert_eq!(out[1].value, 0.0);

        // Device filter narrows to one sample.
        let out = resolve(
            &pool,
            &metric("smart", "reallocated_sectors", &[("device", "/dev/sda")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 1532.0);
    }

    async fn seed_heartbeat(
        pool: &SqlitePool,
        name: &str,
        period: i64,
        grace: i64,
        enabled: bool,
        last_ping_at: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO heartbeat_checks
                (name, slug_hash, period_secs, grace_secs, enabled, last_ping_at,
                 created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(name)
        .bind(format!("hash-{}", name))
        .bind(period)
        .bind(grace)
        .bind(enabled)
        .bind(last_ping_at)
        .bind(0_i64)
        .bind(0_i64)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn heartbeat_up_emits_every_check() {
        let pool = fixture().await;
        let now = chrono::Utc::now().timestamp();
        // Fresh ping → up; silent for ages → down; disabled → still emitted, up.
        seed_heartbeat(&pool, "fresh", 3600, 300, true, Some(now)).await;
        seed_heartbeat(&pool, "silent", 60, 30, true, Some(now - 86_400)).await;
        seed_heartbeat(&pool, "off", 60, 30, false, Some(now - 86_400)).await;

        let mut out = resolve(&pool, &metric("heartbeat", "up", &[]), i64::MIN)
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 3, "disabled checks must keep emitting");
        assert_eq!(out[0].label_set, r#"{"check":"fresh"}"#);
        assert_eq!(out[0].value, 1.0);
        assert_eq!(out[1].label_set, r#"{"check":"off"}"#);
        assert_eq!(out[1].value, 1.0);
        assert_eq!(out[1].meta.as_deref(), Some("disabled"));
        assert_eq!(out[2].label_set, r#"{"check":"silent"}"#);
        assert_eq!(out[2].value, 0.0);
        assert_eq!(out[2].meta.as_deref(), Some("down"));
    }

    #[tokio::test]
    async fn heartbeat_late_stays_violated_through_down() {
        let pool = fixture().await;
        let now = chrono::Utc::now().timestamp();
        // Past period but inside grace → late; past deadline → down. Both
        // must read late=1 so a warn rule doesn't resolve as things worsen.
        seed_heartbeat(&pool, "graceful", 60, 3600, true, Some(now - 120)).await;
        seed_heartbeat(&pool, "gone", 60, 30, true, Some(now - 86_400)).await;

        let mut out = resolve(&pool, &metric("heartbeat", "late", &[]), i64::MIN)
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out[0].meta.as_deref(), Some("down"));
        assert_eq!(out[0].value, 1.0);
        assert_eq!(out[1].meta.as_deref(), Some("late"));
        assert_eq!(out[1].value, 1.0);

        // The graceful one is still up=1 while late.
        let out = resolve(
            &pool,
            &metric("heartbeat", "up", &[("check", "graceful")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 1.0);
    }

    #[tokio::test]
    async fn heartbeat_filter_miss_is_empty_not_error() {
        let pool = fixture().await;
        let out = resolve(
            &pool,
            &metric("heartbeat", "up", &[("check", "no-such-check")]),
            i64::MIN,
        )
        .await
        .unwrap();
        assert!(out.is_empty(), "rules must be creatable before their check");
    }

    #[tokio::test]
    async fn heartbeat_unknown_field_and_label_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("heartbeat", "age_secs", &[]), i64::MIN)
            .await
            .unwrap_err();
        assert!(err.message.contains("not valid"));
        let err = resolve(
            &pool,
            &metric("heartbeat", "up", &[("unit", "x")]),
            i64::MIN,
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("'check'"));
    }

    #[tokio::test]
    async fn unknown_namespace_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("nonsense", "x", &[]), i64::MIN)
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown namespace"));
    }

    // ===== In-memory snapshot resolution =====

    use crate::models::stats::{CoreStats, LoadAverage, PressureSnapshot};

    fn test_disk(mount: &str, total: u64, used: u64) -> DiskStats {
        DiskStats {
            mount_point: mount.to_string(),
            total_bytes: total,
            used_bytes: used,
            available_bytes: total.saturating_sub(used),
            read_bytes_per_sec: 0,
            write_bytes_per_sec: 0,
            timestamp: 100,
            inode_used_percent: None,
            read_iops: None,
            write_iops: None,
            io_util_percent: None,
        }
    }

    /// Build a snapshot with the fields the tests exercise; the rest carry
    /// harmless defaults.
    fn test_snapshot(disks: Vec<DiskStats>, pressure: Option<PressureSnapshot>) -> AllStats {
        AllStats {
            cpu: Arc::new(CpuStats {
                usage_percent: 75.0,
                per_core: vec![CoreStats {
                    core_index: 0,
                    usage_percent: 75.0,
                    freq_mhz: 3000,
                }],
                load_avg: LoadAverage {
                    one: 1.5,
                    five: 1.6,
                    fifteen: 1.7,
                },
                timestamp: 100,
                steal_percent: Some(0.5),
                iowait_percent: Some(0.2),
                guest_percent: None,
                user_percent: None,
                system_percent: None,
                context_switches_per_sec: Some(1000),
                process_forks_per_sec: Some(5),
            }),
            memory: Arc::new(MemoryStats {
                total_bytes: 8_000_000_000,
                used_bytes: 6_000_000_000,
                available_bytes: 2_000_000_000,
                cached_bytes: 1_000_000_000,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                timestamp: 100,
                page_faults_minor_per_sec: None,
                page_faults_major_per_sec: None,
                swap_in_pages_per_sec: None,
                swap_out_pages_per_sec: None,
            }),
            disks: Arc::new(disks),
            network: Arc::new(vec![NetworkStats {
                interface: "eth0".to_string(),
                rx_bytes_per_sec: 1234,
                tx_bytes_per_sec: 5678,
                rx_packets_per_sec: 10,
                tx_packets_per_sec: 20,
                errors_in_per_sec: 0,
                errors_out_per_sec: 0,
                rx_bytes_total: 0,
                tx_bytes_total: 0,
                timestamp: 100,
            }]),
            pressure: pressure.map(Arc::new),
            components: None,
        }
    }

    fn call_snap(
        ns: &str,
        field: &str,
        labels: &[(&str, &str)],
        snap: &AllStats,
    ) -> Vec<ResolvedSample> {
        resolve_from_snapshot(&metric(ns, field, labels), snap)
            .expect("snapshot-resolvable")
            .expect("no error")
    }

    #[test]
    fn snapshot_cpu_usage() {
        let snap = test_snapshot(vec![], None);
        let out = call_snap("cpu", "usage_percent", &[], &snap);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_set, "{}");
        assert_eq!(out[0].value, 75.0);
        // load fields map through load_avg.
        assert_eq!(call_snap("cpu", "load_5m", &[], &snap)[0].value, 1.6);
    }

    #[test]
    fn snapshot_optional_fields_fall_through_to_db() {
        let snap = test_snapshot(vec![test_disk("/", 1000, 900)], None);
        // Optional/enriched fields are not snapshot-resolvable → None (DB path).
        assert!(resolve_from_snapshot(&metric("cpu", "steal_percent", &[]), &snap).is_none());
        assert!(resolve_from_snapshot(&metric("disk", "inode_used_percent", &[]), &snap).is_none());
        assert!(
            resolve_from_snapshot(&metric("memory", "page_faults_major_per_sec", &[]), &snap)
                .is_none()
        );
        // Non-hot namespaces are never snapshot-resolvable.
        assert!(resolve_from_snapshot(&metric("smart", "health_passed", &[]), &snap).is_none());
    }

    #[test]
    fn snapshot_memory_used_bytes() {
        let snap = test_snapshot(vec![], None);
        let out = call_snap("memory", "used_bytes", &[], &snap);
        assert_eq!(out[0].value, 6_000_000_000.0);
    }

    #[test]
    fn snapshot_disk_used_percent_per_mount() {
        let snap = test_snapshot(
            vec![test_disk("/", 1000, 900), test_disk("/boot", 200, 50)],
            None,
        );
        let mut out = call_snap("disk", "used_percent", &[], &snap);
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label_set, r#"{"mount_point":"/"}"#);
        assert!((out[0].value - 90.0).abs() < 1e-9);
        assert_eq!(out[1].label_set, r#"{"mount_point":"/boot"}"#);
        assert!((out[1].value - 25.0).abs() < 1e-9);

        // Filter narrows to one mount.
        let one = call_snap("disk", "used_percent", &[("mount_point", "/")], &snap);
        assert_eq!(one.len(), 1);
        assert!((one[0].value - 90.0).abs() < 1e-9);
    }

    #[test]
    fn snapshot_disk_zero_total_is_nan_not_dropped() {
        // A 0-total phantom mount stays in the output (key present → no false
        // prune) with a NaN value the evaluator holds on.
        let snap = test_snapshot(vec![test_disk("/phantom", 0, 0)], None);
        let out = call_snap("disk", "used_percent", &[], &snap);
        assert_eq!(out.len(), 1);
        assert!(out[0].value.is_nan());
    }

    #[test]
    fn snapshot_network_keyed_by_interface() {
        let snap = test_snapshot(vec![], None);
        let out = call_snap("network", "rx_bytes_per_sec", &[], &snap);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_set, r#"{"interface_name":"eth0"}"#);
        assert_eq!(out[0].value, 1234.0);
    }

    #[test]
    fn snapshot_pressure_present_resources_only() {
        let ps = |v: f64| PressureStats {
            some_avg10: v,
            some_avg60: v,
            some_avg300: v,
            full_avg10: 0.0,
            full_avg60: 0.0,
            full_avg300: 0.0,
        };
        let pressure = PressureSnapshot {
            cpu: Some(ps(2.5)),
            memory: None, // absent resource is skipped, not emitted as NaN
            io: Some(ps(1.0)),
            timestamp: 100,
        };
        let snap = test_snapshot(vec![], Some(pressure));
        let mut out = call_snap("pressure", "some_avg10", &[], &snap);
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out.len(), 2, "memory (None) must be skipped");
        assert_eq!(out[0].label_set, r#"{"resource":"cpu"}"#);
        assert_eq!(out[0].value, 2.5);
        assert_eq!(out[1].label_set, r#"{"resource":"io"}"#);
        assert_eq!(out[1].value, 1.0);
    }

    #[test]
    fn snapshot_pressure_absent_falls_through() {
        // No PSI at all on the host → not snapshot-resolvable (DB path, empty).
        let snap = test_snapshot(vec![], None);
        assert!(resolve_from_snapshot(&metric("pressure", "some_avg10", &[]), &snap).is_none());
    }

    #[test]
    fn snapshot_rejects_bad_labels() {
        let snap = test_snapshot(vec![test_disk("/", 1000, 900)], None);
        // Unkeyed namespace with a label → error.
        let err = resolve_from_snapshot(&metric("cpu", "usage_percent", &[("x", "y")]), &snap)
            .unwrap()
            .unwrap_err();
        assert!(err.message.contains("no label dimensions"));
        // Keyed namespace with the wrong label → error naming the right one.
        let err = resolve_from_snapshot(
            &metric("disk", "used_bytes", &[("interface_name", "eth0")]),
            &snap,
        )
        .unwrap()
        .unwrap_err();
        assert!(err.message.contains("'mount_point'"));
    }
}
