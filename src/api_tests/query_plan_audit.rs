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
use crate::services::alerting::resolver::{keyed_latest_sql, probe_latest_sql, unkeyed_latest_sql};
use crate::storage::repositories::logs::LIST_SQL as LOGS_LIST_SQL;

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
        // A GROUP BY or DISTINCT sorter — the 0.15.2 regression class (the
        // resolver grouping over a full raw-partition scan) and its twin,
        // since the resolver now reaches its keys through DISTINCT and an
        // index that stops serving it would be just as expensive. We do NOT
        // flag ORDER BY temp-b-trees: "LAST TERM OF ORDER BY" is an
        // index-served sort with a cheap tiebreak, and the remaining ones sort
        // small bounded results (e.g. one row per device) — neither scales
        // with retained history.
        if touches_growing
            && (line.contains("USE TEMP B-TREE FOR GROUP BY")
                || line.contains("USE TEMP B-TREE FOR DISTINCT"))
        {
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

/// A few hundred rows per keyed table, with far fewer keys than timestamps.
///
/// The ratio is the whole point: `ANALYZE` records average rows per distinct
/// index prefix, and the planner only prefers `(resolution, key, timestamp)`
/// over the primary key once it can see that fixing a key narrows the search.
/// Volume is irrelevant to that comparison, so this stays small.
async fn seed_for_statistics(pool: &SqlitePool) {
    insert_statistics_fixture(pool).await;

    sqlx::query("ANALYZE").execute(pool).await.expect("analyze");

    // An empty table produces no statistics and so silently reverts the planner
    // to its guesses — the audit would then report the guessed plan as the
    // finding. Confirm every table under audit actually has statistics.
    for table in [
        "metrics_disk",
        "metrics_network",
        "metrics_pressure",
        "metrics_components",
        "metrics_smart",
        "metrics_docker",
        "metrics_process",
    ] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl = ?")
            .bind(table)
            .fetch_one(pool)
            .await
            .expect("read sqlite_stat1");
        assert!(
            n > 0,
            "{table} has no statistics; the fixture did not populate it"
        );
    }
}

/// The rows only, with no `ANALYZE` — so a test can observe what happens to a
/// database that has data and no statistics, which is what production was.
async fn insert_statistics_fixture(pool: &SqlitePool) {
    const TICKS: i64 = 200;

    // Plain INSERT, never `OR IGNORE`: `resource` carries a CHECK constraint,
    // and a fixture that swallows the violation leaves the table empty, which
    // leaves it without statistics, which fails this test for a reason that has
    // nothing to do with the query being audited.
    let rows = |cols: &str, keys: i64, vals: &str| {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM t WHERE n < {TICKS} - 1),
                           k(j) AS (SELECT 0 UNION ALL SELECT j+1 FROM k WHERE j < {keys} - 1)
             INSERT INTO {cols} SELECT 'raw', n*2, {vals} FROM t CROSS JOIN k"
        )
    };

    for sql in [
        rows(
            "metrics_disk (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes)",
            4,
            "'mnt' || j, 1000, 500, 500",
        ),
        rows(
            "metrics_network (resolution, timestamp, interface_name, rx_bytes_per_sec, tx_bytes_per_sec, rx_packets_per_sec, tx_packets_per_sec)",
            4,
            "'if' || j, 1, 1, 1, 1",
        ),
        rows(
            "metrics_pressure (resolution, timestamp, resource, some_avg10, some_avg60, some_avg300, full_avg10, full_avg60, full_avg300)",
            3,
            "CASE j WHEN 0 THEN 'cpu' WHEN 1 THEN 'memory' ELSE 'io' END, 1, 1, 1, 1, 1, 1",
        ),
        rows(
            "metrics_components (resolution, timestamp, label, temperature_c)",
            4,
            "'sensor' || j, 40.0",
        ),
        rows(
            "metrics_smart (resolution, timestamp, device, health_passed)",
            4,
            "'sd' || j, 1",
        ),
        rows(
            "metrics_docker (resolution, timestamp, container_id, cpu_percent, memory_used_bytes, memory_limit_bytes, network_rx_bytes, network_tx_bytes)",
            4,
            "'c' || j, 1.0, 1, 1, 1, 1",
        ),
        rows(
            "metrics_process (resolution, timestamp, name, pid_count, cpu_percent, memory_bytes)",
            4,
            "'proc' || j, 1, 1.0, 1",
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .expect("seed for statistics");
    }
}

/// The audit above builds its own statistics, so it would go on passing even if
/// nothing in the server ever produced any. This is the half that ties the plan
/// to production: the retention pass runs at startup and hourly, and it has to
/// leave `sqlite_stat1` behind.
#[tokio::test]
async fn the_retention_pass_leaves_planner_statistics_behind() {
    let app = TestApp::spawn().await;
    insert_statistics_fixture(&app.state.db).await;

    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
    )
    .fetch_one(&app.state.db)
    .await
    .expect("read schema");
    assert_eq!(
        exists, 0,
        "statistics existed before anything ran ANALYZE; this test cannot prove what made them"
    );

    crate::services::retention::run_once(&app.state)
        .await
        .expect("retention pass");

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_stat1")
        .fetch_one(&app.state.db)
        .await
        .expect("sqlite_stat1 after a retention pass");
    assert!(
        rows > 0,
        "the retention pass left no planner statistics, so every plan comes from \
         SQLite's built-in guesses and the resolver's per-key seek falls back to \
         the primary key"
    );
}

/// The per-key seek must reach its key through the index built for it.
///
/// Separate from the scan/sorter audit because it asks a different question of a
/// different fixture. That audit runs against an empty schema, where there are
/// no statistics and every plan comes from SQLite's built-in guesses; the guess
/// for this shape is the primary key, which constrains only `resolution` and so
/// walks that whole partition once per key. Nothing in the other detectors sees
/// it — it is not a scan and it sorts nothing — so the cost the indexes were
/// added to remove came back while the gate stayed green.
///
/// The assertion is on the table accesses that are *not* covered by an index: a
/// covering index legitimately constrains only `resolution` when it is
/// collecting the DISTINCT key list, but anything that has to fetch rows must
/// have fixed the key first.
#[tokio::test]
async fn resolver_per_key_seek_constrains_the_key() {
    let app = TestApp::spawn().await;
    seed_for_statistics(&app.state.db).await;

    let cases = [
        ("metrics_disk", "used_bytes", "mount_point"),
        ("metrics_network", "rx_bytes_per_sec", "interface_name"),
        ("metrics_pressure", "some_avg10", "resource"),
        ("metrics_components", "temperature_c", "label"),
        ("metrics_smart", "health_passed", "device"),
        ("metrics_docker", "cpu_percent", "container_id"),
        ("metrics_process", "cpu_percent", "name"),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (table, col, label) in cases {
        for where_label in ["", &format!("AND {} = ?", label)] {
            let sql = keyed_latest_sql(table, col, label, where_label);
            let plan = plan_details(&app.state.db, &sql).await.expect("explain");
            let unbounded: Vec<&String> = plan
                .iter()
                .filter(|l| l.starts_with("SEARCH") && !l.contains("COVERING INDEX"))
                .filter(|l| !l.contains(&format!("{label}=?")))
                .collect();
            if !unbounded.is_empty() {
                failures.push(format!(
                    "{table} (where_label={where_label:?}): {unbounded:?}"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "a per-key seek fetches rows without constraining its key, so it walks the \
         whole resolution partition for every key. The (resolution, key, timestamp) \
         index exists — the planner is not choosing it, which needs statistics \
         (`PRAGMA optimize`, run by the retention pass):\n{}",
        failures.join("\n")
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
        ("metrics_process", "cpu_percent", "name"),
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

/// Queries assembled by hand never reach the `.sqlx` cache, so the sweep above
/// cannot see them — and an index dropped as unused would look unused here too.
/// Every such query is listed explicitly.
#[tokio::test]
async fn hand_built_queries_have_no_full_scan_or_sorter() {
    let app = TestApp::spawn().await;

    let mut sqls: Vec<(String, String)> = vec![
        ("logs list".into(), LOGS_LIST_SQL.to_string()),
        (
            "unkeyed cpu".into(),
            unkeyed_latest_sql("metrics_cpu", "usage_percent"),
        ),
        (
            "unkeyed memory".into(),
            unkeyed_latest_sql("metrics_memory", "used_bytes"),
        ),
    ];
    // The probe shape varies with how much the rule constrains: no filter, a
    // probe_name equality, and a JSON label predicate on top.
    for (label, probe_clause, json_clauses) in [
        ("probe unfiltered", "", ""),
        ("probe by name", " AND probe_name = ?", ""),
        (
            "probe by name+label",
            " AND probe_name = ?",
            " AND json_extract(labels, ?) = ?",
        ),
    ] {
        sqls.push((label.into(), probe_latest_sql(probe_clause, json_clauses)));
    }

    let mut failures: Vec<String> = Vec::new();
    for (label, sql) in &sqls {
        let plan = plan_details(&app.state.db, sql).await.expect("explain");
        let off = offenders(sql, &plan);
        if !off.is_empty() {
            failures.push(format!("{label}: {off:?}"));
        }
    }

    assert!(
        failures.is_empty(),
        "a hand-built query scans or sorts a growing table:\n{}",
        failures.join("\n")
    );
}

/// Guard the guard: the same latest-per-key query against a metrics-shaped
/// table WITHOUT the `(resolution, key, timestamp)` index must degenerate into
/// a scan and a sort — proving the index is what removes them, and that the
/// detector is looking for plan lines that actually appear. If this ever stops
/// firing, the audit above has gone blind.
///
/// The signal moved with the query: the group-wise-max form sorted for its
/// GROUP BY, the distinct-keys form sorts for its DISTINCT. Both are caught,
/// which is the point — a detector pinned to the old wording would have waved
/// the new shape through.
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
            .any(|l| l.contains("USE TEMP B-TREE FOR DISTINCT")
                || l.contains("USE TEMP B-TREE FOR GROUP BY")),
        "an unindexed latest-per-key query must sort; plan was {plan:?}"
    );
    assert!(
        plan.iter()
            .any(|l| l.starts_with("SCAN ") && !l.contains("USING")),
        "an unindexed latest-per-key query must scan; plan was {plan:?}"
    );
}
