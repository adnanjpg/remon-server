//! Query-plan audit — a static guard against the full-scan / GROUP-BY-sorter
//! class of regression (the alert resolver's temp-b-tree over `metrics_disk`,
//! fixed by `(resolution, key, timestamp)` indexes in 0.15.2).
//!
//! Every compile-time-checked query in the `.sqlx` cache, plus the resolver's
//! dynamic "latest per key" queries, is run through `EXPLAIN QUERY PLAN`
//! against the real migrated (indexed) schema. A `SCAN <growing-table>` (full
//! table scan) or a `USE TEMP B-TREE FOR GROUP BY` (group-wise sort) over a
//! growing table fails the test — those are the plans that scale linearly with
//! retained history. (Index-served ORDER BY tiebreaks and small bounded sorts
//! are not flagged; a full ORDER BY over unbounded data surfaces as the scan.)
//!
//! sqlx only checks queries *compile*; it never looks at their plan. This
//! test is that missing efficiency gate.

use std::fs;
use std::path::Path;

use sqlx::{Row, SqlitePool};

use super::TestApp;
use crate::services::alerting::resolver::keyed_latest_sql;

/// Append-only / rolled-up tables that grow with retained history — a full
/// scan or a sort over these is the smell we guard against. Small config /
/// lookup tables (server_config, resolutions, devices, …) are exempt:
/// scanning a handful of rows is free.
const GROWING_TABLES: &[&str] = &[
    "metrics_cpu",
    "metrics_cpu_cores",
    "metrics_memory",
    "metrics_disk",
    "metrics_network",
    "metrics_docker",
    "metrics_process",
    "metrics_components",
    "metrics_pressure",
    "metrics_probe",
    "metrics_smart",
    "logs",
    "alert_events",
    "host_events",
    "probe_runs",
    "heartbeat_pings",
    "incident_snapshots",
];

/// Queries whose plan legitimately scans/sorts a growing table — reviewed and
/// accepted, keyed by a distinctive substring of the SQL with the rationale.
const ALLOWLIST: &[(&str, &str)] = &[];

/// Replace `?` / `?N` placeholders with `NULL` so `EXPLAIN` needs no bindings.
/// The plan is independent of the bound value, so `NULL` is fine.
fn placeholders_to_null(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '?' {
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
            out.push_str("NULL");
        } else {
            out.push(c);
        }
    }
    out
}

async fn plan_details(pool: &SqlitePool, sql: &str) -> Result<Vec<String>, String> {
    let explain = format!("EXPLAIN QUERY PLAN {}", placeholders_to_null(sql));
    // Auditing our own query text — the whole point of this test.
    let rows = sqlx::query(sqlx::AssertSqlSafe(explain))
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<String, _>("detail")).collect())
}

/// Word-boundary table mention (so `metrics_disk` doesn't match a longer name).
fn sql_mentions_table(sql: &str, table: &str) -> bool {
    sql.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| w == table)
}

/// The offending plan lines for one query, or empty if clean.
fn offenders(sql: &str, plan: &[String]) -> Vec<String> {
    if ALLOWLIST.iter().any(|(sub, _)| sql.contains(sub)) {
        return Vec::new();
    }
    let touches_growing = GROWING_TABLES.iter().any(|t| sql_mentions_table(sql, t));
    let mut out = Vec::new();
    for line in plan {
        // "SCAN <table>" without "USING <index>" is a full table scan — this
        // also catches a full ORDER BY sort over unbounded data, since that
        // materialises the base-table scan here.
        if let Some(rest) = line.strip_prefix("SCAN ") {
            let table = rest.split_whitespace().next().unwrap_or("");
            if GROWING_TABLES.contains(&table) && !line.contains("USING") {
                out.push(line.clone());
            }
        }
        // A GROUP BY sorter — the exact 0.15.2 regression class (the resolver
        // grouping over a full raw-partition scan). We do NOT flag ORDER BY
        // temp-b-trees: "LAST TERM OF ORDER BY" is an index-served sort with a
        // cheap tiebreak, and the remaining ones sort small bounded results
        // (e.g. one row per device) — neither scales with retained history.
        if line.contains("USE TEMP B-TREE FOR GROUP BY") && touches_growing {
            out.push(line.clone());
        }
    }
    out
}

#[tokio::test]
async fn sqlx_cached_queries_have_no_full_scan_or_sorter() {
    let app = TestApp::spawn().await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".sqlx");

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for entry in fs::read_dir(&dir).expect("read .sqlx directory") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let Some(sql) = json["query"].as_str() else {
            continue;
        };

        // A few statements can't be EXPLAIN QUERY PLAN'd (or carry no
        // interesting plan) — skip those rather than fail.
        let Ok(plan) = plan_details(&app.state.db, sql).await else {
            continue;
        };
        checked += 1;
        let off = offenders(sql, &plan);
        if !off.is_empty() {
            let compact: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            failures.push(format!(
                "{}\n  sql:  {}\n  plan: {:?}",
                path.file_name().unwrap().to_string_lossy(),
                compact.chars().take(160).collect::<String>(),
                off
            ));
        }
    }

    assert!(
        checked > 20,
        "expected to audit many cached queries, only saw {checked} — did the .sqlx path resolve?"
    );
    assert!(
        failures.is_empty(),
        "query-plan audit found {} offending query(ies) — full scan or sorter over a growing \
         table. Add an index, or (if genuinely unavoidable) add to ALLOWLIST with a reason:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[tokio::test]
async fn resolver_latest_per_key_uses_index_not_sorter() {
    let app = TestApp::spawn().await;

    // (table, a representative column, label column) — one per keyed namespace
    // the alert evaluator resolves from the DB.
    let cases = [
        ("metrics_disk", "used_bytes", "mount_point"),
        ("metrics_network", "rx_bytes_per_sec", "interface_name"),
        ("metrics_pressure", "some_avg10", "resource"),
        ("metrics_components", "temperature_c", "label"),
        ("metrics_smart", "health_passed", "device"),
        ("metrics_docker", "cpu_percent", "container_id"),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (table, col, label) in cases {
        // Both the unfiltered and label-filtered shapes the evaluator builds.
        for where_label in ["", &format!("AND {} = ?", label)] {
            let sql = keyed_latest_sql(table, col, label, where_label);
            let plan = plan_details(&app.state.db, &sql).await.expect("explain");
            let off = offenders(&sql, &plan);
            if !off.is_empty() {
                failures.push(format!("{table} (where_label={where_label:?}): {off:?}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "the resolver's latest-per-key query regressed to a scan/sorter — the \
         (resolution, key, timestamp) index is missing or unused:\n{}",
        failures.join("\n")
    );
}

/// Guard the guard: the same latest-per-key query against a metrics-shaped
/// table WITHOUT the `(resolution, key, timestamp)` index must produce the
/// GROUP BY sorter — proving the index is what removes it, and that the
/// detector is looking for the right plan line. If this ever stops firing, the
/// audit above has gone blind.
#[tokio::test]
async fn audit_detects_the_sorter_when_the_index_is_missing() {
    let app = TestApp::spawn().await;
    sqlx::query(
        "CREATE TABLE metrics_unindexed
           (resolution TEXT, timestamp INTEGER, mount_point TEXT, used_bytes INTEGER)",
    )
    .execute(&app.state.db)
    .await
    .unwrap();

    let sql = keyed_latest_sql("metrics_unindexed", "used_bytes", "mount_point", "");
    let plan = plan_details(&app.state.db, &sql).await.unwrap();
    assert!(
        plan.iter()
            .any(|l| l.contains("USE TEMP B-TREE FOR GROUP BY")),
        "an unindexed latest-per-key query must sort; plan was {plan:?}"
    );
}
