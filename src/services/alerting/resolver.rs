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
    "used_bytes",
    "available_bytes",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
    "inode_used_percent",
];
const DISK_I64: &[&str] = &[
    "used_bytes",
    "available_bytes",
    "read_bytes_per_sec",
    "write_bytes_per_sec",
];

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

// Live-check namespace; resolved via ServiceManager, not the DB.
const SERVICE_FIELDS: &[&str] = &["up"];

// ===== Public entry =====

/// DB-only entry. Service-namespace rules error out here; use
/// [`resolve_with_state`] for those.
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
            )
            .await
        }
        "probe" => resolve_probe(pool, metric).await,
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
        sqlx::query_scalar::<_, i64>(&sql)
            .fetch_optional(pool)
            .await
            .map_err(|e| ResolveError::msg(e.to_string()))?
            .map(|v| v as f64)
    } else {
        sqlx::query_scalar::<_, f64>(&sql)
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
) -> Result<Vec<ResolvedSample>, ResolveError> {
    let column = check_field(metric, valid_fields)?;

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
               WHERE resolution = 'raw'
               GROUP BY {label}
            )",
        label = label_column,
        col = column,
        table = table,
        where_label = where_label,
    );

    let rows: Vec<(String, f64)> = if i64_fields.contains(&column) {
        let mut q2 = sqlx::query_as::<_, (String, i64)>(&sql);
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
        let mut q2 = sqlx::query_as::<_, (String, f64)>(&sql);
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
    let mut json_clauses = String::new();
    for k in json_filters.keys() {
        json_clauses.push_str(&format!(" AND json_extract(labels, '$.{}') = ?", k));
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

    let mut q = sqlx::query_as::<_, (String, String, f64)>(&sql);
    q = q.bind(metric_name);
    if let Some(name) = probe_name_filter {
        q = q.bind(name);
    }
    for v in json_filters.values() {
        q = q.bind(*v);
    }
    q = q.bind(metric_name);
    if let Some(name) = probe_name_filter {
        q = q.bind(name);
    }
    for v in json_filters.values() {
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
    async fn unknown_namespace_rejected() {
        let pool = fixture().await;
        let err = resolve(&pool, &metric("nonsense", "x", &[]))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown namespace"));
    }
}
