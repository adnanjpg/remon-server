//! Hermetic tests for the assistant's read-only tools, exercised directly
//! through `assistant::tools::dispatch` — no provider, no network. This is
//! where the tool logic (shape, sorting, error handling) is verified; the
//! `#[ignore]` live test in `assistant_api` only proves the provider loop.

use super::TestApp;
use serde_json::{Value, json};

use crate::assistant::ProposedAction;
use crate::assistant::tools::{dispatch, dispatch_collecting};

/// Parse a tool's JSON string result. Tools never return non-JSON.
fn call(result: String) -> Value {
    serde_json::from_str(&result).expect("tool returned valid json")
}

/// Run a tool, returning both its JSON result and any drafted proposals.
async fn propose(app: &TestApp, name: &str, args: Value) -> (Value, Vec<ProposedAction>) {
    let mut proposals = Vec::new();
    let out = dispatch_collecting(&app.state, name, &args, &mut proposals).await;
    (call(out), proposals)
}

#[tokio::test]
async fn get_summary_reports_seeded_stats() {
    use crate::models::stats::{AllStats, CpuStats, DiskStats, LoadAverage, MemoryStats};
    use std::sync::Arc;

    let app = TestApp::spawn().await;

    let stats = AllStats {
        cpu: Arc::new(CpuStats {
            usage_percent: 55.5,
            per_core: vec![],
            load_avg: LoadAverage {
                one: 1.0,
                five: 1.0,
                fifteen: 1.0,
            },
            timestamp: 1_700_000_000,
            steal_percent: None,
            iowait_percent: None,
            guest_percent: None,
            user_percent: None,
            system_percent: None,
            context_switches_per_sec: None,
            process_forks_per_sec: None,
        }),
        memory: Arc::new(MemoryStats {
            total_bytes: 16_000_000_000,
            used_bytes: 8_000_000_000,
            available_bytes: 8_000_000_000,
            cached_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            timestamp: 1_700_000_000,
            page_faults_minor_per_sec: None,
            page_faults_major_per_sec: None,
            swap_in_pages_per_sec: None,
            swap_out_pages_per_sec: None,
        }),
        disks: Arc::new(vec![DiskStats {
            mount_point: "/data".to_string(),
            total_bytes: 100,
            used_bytes: 80,
            available_bytes: 20,
            read_bytes_per_sec: 0,
            write_bytes_per_sec: 0,
            timestamp: 1_700_000_000,
            inode_used_percent: None,
            read_iops: None,
            write_iops: None,
            io_util_percent: None,
        }]),
        network: Arc::new(vec![]),
        pressure: None,
        components: None,
    };
    *app.state.stats_latest.write().await = Some(stats);

    let out = call(dispatch(&app.state, "get_summary", &json!({})).await);
    assert_eq!(out["cpu_usage_percent"], 55.5);
    assert_eq!(out["memory_used_bytes"], 8_000_000_000u64);
    assert_eq!(out["fullest_disk"]["mount_point"], "/data");
    assert_eq!(out["fullest_disk"]["used_percent"], 80.0);
    assert_eq!(out["alerts_firing"], 0);
}

#[tokio::test]
async fn active_alerts_empty_on_fresh_db() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "active_alerts", &json!({})).await);
    assert_eq!(out["count"], 0);
    assert!(out["alerts"].as_array().is_some_and(|a| a.is_empty()));
}

#[tokio::test]
async fn query_metric_rejects_unknown_field() {
    let app = TestApp::spawn().await;
    let out = call(
        dispatch(
            &app.state,
            "query_metric",
            &json!({ "namespace": "cpu", "field": "definitely_not_a_field" }),
        )
        .await,
    );
    // A bad field surfaces as a readable error the model can react to.
    assert!(out["error"].is_string(), "expected error, got: {out}");
}

#[tokio::test]
async fn query_metric_missing_namespace_errors() {
    let app = TestApp::spawn().await;
    let out = call(
        dispatch(
            &app.state,
            "query_metric",
            &json!({ "field": "usage_percent" }),
        )
        .await,
    );
    assert!(out["error"].is_string(), "expected error, got: {out}");
}

#[tokio::test]
async fn list_processes_returns_ranked_list() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "list_processes", &json!({ "limit": 5 })).await);
    assert_eq!(out["sorted_by"], "cpu");
    let procs = out["processes"].as_array().expect("processes array");
    assert!(procs.len() <= 5);
    // The test process itself is running, so the snapshot is never empty.
    assert!(!procs.is_empty());
    assert!(procs[0]["pid"].is_u64());
    assert!(procs[0]["name"].is_string());
}

#[tokio::test]
async fn read_logs_returns_shape() {
    let app = TestApp::spawn().await;
    let out = call(
        dispatch(
            &app.state,
            "read_logs",
            &json!({ "level": "trace", "limit": 10 }),
        )
        .await,
    );
    assert!(out["count"].is_u64());
    assert!(out["logs"].is_array());
}

#[tokio::test]
async fn unknown_tool_is_reported() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "no_such_tool", &json!({})).await);
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("no_such_tool")),
        "expected unknown-tool error, got: {out}"
    );
}

#[tokio::test]
async fn metric_history_empty_on_fresh_db() {
    let app = TestApp::spawn().await;
    let out = call(
        dispatch(
            &app.state,
            "metric_history",
            &json!({ "namespace": "cpu", "field": "usage_percent", "window_secs": 3600 }),
        )
        .await,
    );
    // No metrics collected in the harness → a well-formed but empty series.
    assert_eq!(out["metric"], "cpu.usage_percent");
    assert!(
        out["series"].as_array().is_some_and(|s| s.is_empty()),
        "got: {out}"
    );
}

#[tokio::test]
async fn metric_history_rejects_unsupported_namespace() {
    let app = TestApp::spawn().await;
    let out = call(
        dispatch(
            &app.state,
            "metric_history",
            &json!({ "namespace": "service", "field": "up" }),
        )
        .await,
    );
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("history is not available")),
        "got: {out}"
    );
}

#[tokio::test]
async fn recent_alert_events_empty_on_fresh_db() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "recent_alert_events", &json!({})).await);
    assert_eq!(out["count"], 0);
    assert!(out["events"].as_array().is_some_and(|e| e.is_empty()));
}

#[tokio::test]
async fn prometheus_query_errors_when_unconfigured() {
    // Default config has no prometheus_url, so the tool is not advertised and,
    // if called anyway, refuses with a clear message.
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "prometheus_query", &json!({ "query": "up" })).await);
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("prometheus_url")),
        "got: {out}"
    );
}

// ===== propose-only actions =====

#[tokio::test]
async fn propose_alert_rule_drafts_but_does_not_create() {
    let app = TestApp::spawn().await;
    let (out, proposals) = propose(
        &app,
        "propose_alert_rule",
        json!({ "name": "high-cpu", "expression": "cpu.usage_percent > 90", "severity": "crit", "for_secs": 300 }),
    )
    .await;

    assert!(out["proposed"].is_string(), "got: {out}");
    assert_eq!(proposals.len(), 1);
    let p = &proposals[0];
    assert_eq!(p.kind, "create_alert");
    assert_eq!(p.method, "POST");
    assert_eq!(p.path, "/alerts");
    let body = p.body.as_ref().expect("body");
    assert_eq!(body["expression"], "cpu.usage_percent > 90");
    assert_eq!(body["severity"], "crit");
    assert_eq!(body["for_duration_secs"], 300);

    // Nothing was actually created.
    let rules = call(dispatch(&app.state, "list_alert_rules", &json!({})).await);
    assert_eq!(rules["count"], 0);
}

#[tokio::test]
async fn propose_alert_rule_rejects_bad_expression() {
    let app = TestApp::spawn().await;
    let (out, proposals) = propose(
        &app,
        "propose_alert_rule",
        json!({ "name": "bad", "expression": "cpu.usage_percent GREATER 90" }),
    )
    .await;
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("invalid expression")),
        "got: {out}"
    );
    assert!(
        proposals.is_empty(),
        "a rejected draft must not propose anything"
    );
}

#[tokio::test]
async fn propose_kill_process_rejects_bad_signal() {
    let app = TestApp::spawn().await;
    let (out, proposals) = propose(
        &app,
        "propose_kill_process",
        json!({ "pid": 1234, "signal": 2 }),
    )
    .await;
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("signal")),
        "got: {out}"
    );
    assert!(proposals.is_empty());
}

#[tokio::test]
async fn propose_kill_process_builds_delete_with_signal() {
    let app = TestApp::spawn().await;
    let (_out, proposals) = propose(
        &app,
        "propose_kill_process",
        json!({ "pid": 4321, "name": "stuck", "signal": 9 }),
    )
    .await;
    assert_eq!(proposals.len(), 1);
    let p = &proposals[0];
    assert_eq!(p.kind, "kill_process");
    assert_eq!(p.method, "DELETE");
    assert_eq!(p.path, "/processes/4321?signal=9");
    assert!(p.body.is_none());
}

#[tokio::test]
async fn propose_service_action_builds_path() {
    let app = TestApp::spawn().await;
    let (_out, proposals) = propose(
        &app,
        "propose_service_action",
        json!({ "name": "nginx.service", "action": "restart" }),
    )
    .await;
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].path, "/services/nginx.service/restart");
    assert_eq!(proposals[0].method, "POST");
}

#[tokio::test]
async fn propose_silence_alert_unknown_rule_errors() {
    let app = TestApp::spawn().await;
    let (out, proposals) = propose(
        &app,
        "propose_silence_alert",
        json!({ "name": "nope", "minutes": 30 }),
    )
    .await;
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("no alert rule")),
        "got: {out}"
    );
    assert!(proposals.is_empty());
}
