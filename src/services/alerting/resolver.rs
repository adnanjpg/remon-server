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

use super::expression::MetricRef;
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
    "used_bytes",
    "available_bytes",
    "cached_bytes",
    "swap_used_bytes",
    "page_faults_minor_per_sec",
    "page_faults_major_per_sec",
    "swap_in_pages_per_sec",
    "swap_out_pages_per_sec",
];
const MEMORY_I64: &[&str] = MEMORY_FIELDS; // every memory field is INTEGER

const DISK_FIELDS: &[&str] = &[
    "total_bytes",
    "used_bytes",
    "available_bytes",
    "used_percent",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
    "inode_used_percent",
];
const DISK_I64: &[&str] = &[
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

// Live-check namespace; resolved via ServiceManager, not the DB.
const SERVICE_FIELDS: &[&str] = &["up"];

// Heartbeat checks; state derived from heartbeat_checks timestamps at
// resolve time — the rule's eval tick IS the deadline check.
const HEARTBEAT_FIELDS: &[&str] = &["up", "late"];

// ===== Public entry =====

/// DB-only entry. Service-namespace rules error out here; use
/// [`resolve_with_state`] for those.
#[cfg(test)]
pub async fn resolve(
    pool: &SqlitePool,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    resolve_inner(pool, None, metric).await
}

pub async fn resolve_with_state(
    state: &crate::state::AppState,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    resolve_inner(&state.db, Some(&state.service_manager), metric).await
}

async fn resolve_inner(
    pool: &SqlitePool,
    services: Option<&Arc<dyn ServiceManager>>,
    metric: &MetricRef,
) -> Result<Vec<ResolvedSample>, ResolveError> {
    match metric.namespace.as_str() {
        "cpu" => resolve_unkeyed(pool, metric, "metrics_cpu", CPU_FIELDS, CPU_I64).await,
        "memory" => {
            resolve_unkeyed(pool, metric, "metrics_memory", MEMORY_FIELDS, MEMORY_I64).await
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

async fn resolve_unkeyed(
    pool: &SqlitePool,
    metric: &MetricRef,
    table: &str,
    valid_fields: &[&str],
    i64_fields: &[&str],
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let column = check_field(metric, valid_fields)?;
    if !metric.labels.is_empty() {
        return Err(ResolveError::msg(format!(
            "namespace '{}' has no label dimensions; remove the label set",
            metric.namespace
        )));
    }
    let sql = format!(
        "SELECT {col} FROM {table}
          WHERE resolution = 'raw' AND {col} IS NOT NULL
          ORDER BY timestamp DESC LIMIT 1",
        col = column,
        table = table
    );

    let value_opt: Option<f64> = if i64_fields.contains(&column) {
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_optional(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
            .map(|v| v as f64)
    } else {
        sqlx::query_scalar::<_, f64>(sqlx::AssertSqlSafe(sql.as_str()))
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

async fn resolve_keyed(
    pool: &SqlitePool,
    metric: &MetricRef,
    table: &str,
    valid_fields: &[&str],
    i64_fields: &[&str],
    label_column: &str,
    computed: &[(&str, &str)],
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

    let sql = format!(
        "SELECT {label}, {col}
           FROM {table}
          WHERE resolution = 'raw' AND {col} IS NOT NULL
            {where_label}
            AND ({label}, timestamp) IN (
              SELECT {label}, MAX(timestamp)
                FROM {table}
               WHERE resolution = 'raw' AND {col} IS NOT NULL
               GROUP BY {label}
            )",
        label = label_column,
        col = select_expr,
        table = table,
        where_label = where_label,
    );

    let rows: Vec<(String, f64)> = if i64_fields.contains(&column) {
        let mut q2 = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql.as_str()));
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
        let mut q2 = sqlx::query_as::<_, (String, f64)>(sqlx::AssertSqlSafe(sql.as_str()));
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

// ===== Probe namespace =====

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
    let sql = format!(
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
    );

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
        let out = resolve(&pool, &metric("cpu", "usage_percent", &[]))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label_set, "{}");
        assert_eq!(out[0].value, 75.5);
    }

    #[tokio::test]
    async fn cpu_unknown_field_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("cpu", "no_such_field", &[]))
            .await
            .unwrap_err();
        assert!(err.message.contains("not valid"));
    }

    #[tokio::test]
    async fn cpu_with_labels_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("cpu", "usage_percent", &[("x", "y")]))
            .await
            .unwrap_err();
        assert!(err.message.contains("no label dimensions"));
    }

    #[tokio::test]
    async fn cpu_no_data_returns_empty() {
        let pool = fixture().await;
        let out = resolve(&pool, &metric("cpu", "usage_percent", &[]))
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
        let out = resolve(&pool, &metric("cpu", "context_switches_per_sec", &[]))
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
        let out = resolve(&pool, &metric("memory", "used_bytes", &[]))
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
        let mut out = resolve(&pool, &metric("disk", "used_bytes", &[]))
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
        let out = resolve(&pool, &metric("disk", "used_percent", &[]))
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
        let out = resolve(&pool, &metric("disk", "inode_used_percent", &[]))
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
        let mut out = resolve(&pool, &metric("probe", "banned", &[]))
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
        let mut out = resolve(&pool, &metric("smart", "health_passed", &[]))
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

        let mut out = resolve(&pool, &metric("heartbeat", "up", &[]))
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

        let mut out = resolve(&pool, &metric("heartbeat", "late", &[]))
            .await
            .unwrap();
        out.sort_by(|a, b| a.label_set.cmp(&b.label_set));
        assert_eq!(out[0].meta.as_deref(), Some("down"));
        assert_eq!(out[0].value, 1.0);
        assert_eq!(out[1].meta.as_deref(), Some("late"));
        assert_eq!(out[1].value, 1.0);

        // The graceful one is still up=1 while late.
        let out = resolve(&pool, &metric("heartbeat", "up", &[("check", "graceful")]))
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
        )
        .await
        .unwrap();
        assert!(out.is_empty(), "rules must be creatable before their check");
    }

    #[tokio::test]
    async fn heartbeat_unknown_field_and_label_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("heartbeat", "age_secs", &[]))
            .await
            .unwrap_err();
        assert!(err.message.contains("not valid"));
        let err = resolve(&pool, &metric("heartbeat", "up", &[("unit", "x")]))
            .await
            .unwrap_err();
        assert!(err.message.contains("'check'"));
    }

    #[tokio::test]
    async fn unknown_namespace_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("nonsense", "x", &[]))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown namespace"));
    }
}
