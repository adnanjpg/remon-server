//! `screen::validate` — the gate every composed screen passes.

use super::TestApp;
use serde_json::{Value, json};

use crate::screen::validate;

async fn seed_cpu(app: &TestApp, ago: i64, usage: f64) {
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, ?, 0, 0, 0)",
    )
    .bind(chrono::Utc::now().timestamp() - ago)
    .bind(usage)
    .execute(&app.state.db)
    .await
    .expect("seed cpu");
}

async fn seed_process(app: &TestApp, name: &str, cpu: f64) {
    sqlx::query(
        "INSERT INTO metrics_process
           (resolution, timestamp, name, pid_count, cpu_percent, memory_bytes)
         VALUES ('raw', ?, ?, 1, ?, 1000)",
    )
    .bind(chrono::Utc::now().timestamp() - 5)
    .bind(name)
    .bind(cpu)
    .execute(&app.state.db)
    .await
    .expect("seed process");
}

fn line(title: &str, namespace: &str, field: &str, labels: Value) -> Value {
    json!({ "type": "line", "title": title, "series": [
        { "query": { "namespace": namespace, "field": field, "labels": labels } }
    ] })
}

async fn errors(app: &TestApp, spec: Value) -> String {
    validate(&app.state, &spec)
        .await
        .expect_err("spec should be rejected")
}

#[tokio::test]
async fn a_valid_screen_comes_back_canonical() {
    let app = TestApp::spawn().await;
    seed_cpu(&app, 30, 40.0).await;
    seed_process(&app, "nginx", 12.0).await;
    seed_process(&app, "postgres", 30.0).await;

    let spec = json!({
        "title": "  web tier  ",
        "root": { "type": "grid", "children": [
            line("CPU", "cpu", "usage_percent", json!({})),
            line("nginx", "process", "cpu_percent", json!({ "name": "nginx" })),
            { "type": "stat", "title": "now", "query": { "namespace": "cpu", "field": "usage_percent" } },
            { "type": "table", "title": "procs", "limit": 5, "columns": [
                { "label": "cpu", "query": { "namespace": "process", "field": "cpu_percent" } },
                { "label": "mem", "query": { "namespace": "process", "field": "memory_bytes" } }
            ] },
            { "type": "widget", "config": { "kind": "history-chart", "resource": "cpu", "range": "6h" } }
        ] }
    });
    let out = validate(&app.state, &spec).await.expect("valid");
    assert_eq!(out["title"], "web tier");
    assert_eq!(out["range"], "1h");
    assert_eq!(out["root"]["columns"], 1);
    assert_eq!(out["root"]["children"].as_array().unwrap().len(), 5);
    // Empty labels are dropped, so equal screens compare equal.
    assert!(
        out["root"]["children"][0]["series"][0]["query"]
            .get("labels")
            .is_none()
    );
}

#[tokio::test]
async fn shape_errors_are_reported_together_with_their_paths() {
    let app = TestApp::spawn().await;
    let err = errors(
        &app,
        json!({ "title": "x", "range": "2h", "root": { "type": "grid", "columns": 9, "children": [
            { "type": "tabs", "tabs": [] },
            { "type": "widget", "config": { "kind": "cpu-detail", "range": "1h" } }
        ] } }),
    )
    .await;
    for want in [
        "range: unknown range '2h'",
        "root: columns must be 1-4",
        "root.children[0]: tabs holds 1-6 tabs",
        "root.children[1]: 'cpu-detail' does not take range",
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
}

#[tokio::test]
async fn unknown_nodes_and_fields_are_rejected_by_name() {
    let app = TestApp::spawn().await;
    let err = errors(&app, json!({ "title": "x", "root": { "type": "pie" } })).await;
    assert!(err.contains("pie"), "{err}");
    let err = errors(
        &app,
        json!({ "title": "x", "root": { "type": "stat", "title": "s", "color": "red",
                 "query": { "namespace": "cpu", "field": "usage_percent" } } }),
    )
    .await;
    assert!(err.contains("color"), "{err}");
}

#[tokio::test]
async fn data_errors_name_what_exists() {
    let app = TestApp::spawn().await;
    seed_process(&app, "nginx", 12.0).await;
    seed_process(&app, "postgres", 30.0).await;

    let err = errors(
        &app,
        json!({ "title": "x", "root": { "type": "grid", "children": [
            line("a", "process", "cpu_percent", json!({ "name": "apache" })),
            line("b", "smart", "temperature_c", json!({})),
            line("c", "process", "cpu", json!({})),
            { "type": "stat", "title": "d", "query": { "namespace": "process", "field": "cpu_percent" } }
        ] } }),
    )
    .await;
    for want in [
        "root.children[0].series[0]: no process with name='apache' in the last 1h; seen: nginx, postgres",
        "root.children[1].series[0]: no history for namespace 'smart'",
        "root.children[2].series[0]: unknown field 'cpu' for 'process'",
        "root.children[3]: process.cpu_percent matches 2 series",
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
}

#[tokio::test]
async fn query_and_panel_limits_hold() {
    let app = TestApp::spawn().await;
    seed_cpu(&app, 30, 40.0).await;
    let children: Vec<Value> = (0..17)
        .map(|i| {
            json!({ "type": "stat", "title": format!("s{i}"),
                    "query": { "namespace": "cpu", "field": "usage_percent" } })
        })
        .collect();
    // Twelve children per grid, so nest to fit seventeen.
    let err = errors(
        &app,
        json!({ "title": "x", "root": { "type": "tabs", "tabs": [
            { "title": "a", "child": { "type": "grid", "children": children[..12] } },
            { "title": "b", "child": { "type": "grid", "children": children[12..] } }
        ] } }),
    )
    .await;
    assert!(err.contains("17 queries; at most 16"), "{err}");
}

/// Register a probe that has already reported, without running it.
async fn seed_probe(app: &TestApp, name: &str, metrics: Vec<crate::models::probe::ProbeMetric>) {
    let dir =
        std::env::temp_dir().join(format!("remon-screen-probe-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp probes dir");
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(
        &path,
        format!("name: {name}\nenabled: true\ninterval: \"1h\"\ncommand: [\"true\"]\n"),
    )
    .expect("write manifest");
    let manifest = crate::probes::manifest::Manifest::load(&path)
        .await
        .expect("manifest");
    let _ = std::fs::remove_dir_all(&dir);
    app.state.probe_registry.write().await.probes.insert(
        name.to_string(),
        crate::probes::registry::ProbeEntry {
            manifest,
            last_run: None,
            last_metrics: metrics,
            task: None,
        },
    );
}

#[tokio::test]
async fn probe_widgets_resolve_labels_and_units() {
    use crate::models::probe::ProbeMetric;

    let app = TestApp::spawn().await;
    let metric = |site: &str| ProbeMetric {
        name: "rtt".to_string(),
        value: 1.0,
        unit: Some("ms".to_string()),
        labels: [("site".to_string(), site.to_string())].into(),
    };
    seed_probe(&app, "latency", vec![metric("eu"), metric("us")]).await;

    let widget =
        |config: Value| json!({ "title": "x", "root": { "type": "widget", "config": config } });
    let out = validate(
        &app.state,
        &widget(
            json!({ "kind": "probe-metric", "probe": "latency", "metric": "rtt",
                        "labels": { "site": "us" } }),
        ),
    )
    .await
    .expect("valid");
    assert_eq!(
        out["root"]["config"],
        json!({ "kind": "probe-metric", "probe": "latency", "metric": "rtt", "viz": "chart",
                "unit": "ms", "labelKey": r#"{"site":"us"}"# })
    );

    let err = errors(
        &app,
        widget(
            json!({ "kind": "probe-metric", "probe": "latency", "metric": "rtt",
                       "labels": { "site": "ap" } }),
        ),
    )
    .await;
    assert!(err.contains(r#"{"site":"eu"}"#), "{err}");
}

#[tokio::test]
async fn table_columns_aggregate_over_the_window() {
    let app = TestApp::spawn().await;
    seed_process(&app, "nginx", 12.0).await;

    let table = |columns: Value| {
        json!({ "title": "x", "root": { "type": "table", "title": "busiest today",
                "range": "24h", "limit": 5, "columns": columns } })
    };
    let out = validate(
        &app.state,
        &table(json!([
            { "label": "avg cpu", "agg": "avg", "query": { "namespace": "process", "field": "cpu_percent" } },
            { "label": "peak", "agg": "max", "query": { "namespace": "process", "field": "cpu_percent" } },
            { "label": "now", "agg": "current", "query": { "namespace": "process", "field": "memory_bytes" } }
        ])),
    )
    .await
    .expect("valid");
    let columns = &out["root"]["columns"];
    assert_eq!(columns[0]["agg"], "avg");
    assert!(
        columns[2].get("agg").is_none(),
        "current is the default, left implicit"
    );
    assert_eq!(out["root"]["range"], "24h");

    let err = errors(
        &app,
        table(json!([
            { "label": "a", "agg": "avg", "query": { "namespace": "smart", "field": "temperature_c" } },
            { "label": "b", "agg": "avg", "query": { "namespace": "process", "field": "cpu_percent", "limit": 3 } },
            { "label": "c", "agg": "median", "query": { "namespace": "process", "field": "cpu_percent" } }
        ])),
    )
    .await;
    assert!(err.contains("median"), "{err}");
    let err = errors(
        &app,
        table(json!([
            { "label": "a", "agg": "avg", "query": { "namespace": "smart", "field": "temperature_c" } },
            { "label": "b", "agg": "avg", "query": { "namespace": "process", "field": "cpu_percent", "limit": 3 } }
        ])),
    )
    .await;
    for want in [
        "root.columns[0]: no history for namespace 'smart'",
        "root.columns[1]: put limit on the table",
    ] {
        assert!(err.contains(want), "missing {want:?} in:\n{err}");
    }
}
