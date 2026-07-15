//! Incident flight-recorder tests: manual capture, assistant tools over the
//! snapshots, the REST trigger, and the evaluator hook (ok→pending captures,
//! cooldown dedupes). Bundles here are built from whatever the harness has —
//! empty caches degrade slices to null, which is exactly the contract.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::TestApp;
use crate::assistant::tools::dispatch;
use crate::services::incidents;
use crate::storage::repositories::IncidentRepository;

fn call(result: String) -> Value {
    serde_json::from_str(&result).expect("tool returned valid json")
}

#[tokio::test]
async fn manual_capture_persists_and_tools_read_it() {
    let app = TestApp::spawn().await;

    let id = incidents::capture_manual(&app.state, "operator smoke test", "security")
        .await
        .expect("capture");
    assert!(id > 0);

    // list_incidents sees it.
    let out = call(dispatch(&app.state, "list_incidents", &json!({})).await);
    assert_eq!(out["count"], 1, "got: {out}");
    assert_eq!(out["incidents"][0]["trigger"], "manual");
    assert_eq!(out["incidents"][0]["category"], "security");
    assert_eq!(out["incidents"][0]["reason"], "operator smoke test");

    // incident_detail returns a parsed bundle with the expected slices.
    let out = call(dispatch(&app.state, "incident_detail", &json!({ "id": id })).await);
    assert_eq!(out["id"], id);
    let bundle = &out["bundle"];
    assert!(bundle.is_object(), "bundle should parse: {out}");
    assert!(bundle.get("captured_at").is_some());
    assert!(bundle.get("recent_daemon_errors").is_some());
    assert!(bundle.get("co_active_alerts").is_some());
}

#[tokio::test]
async fn incident_detail_unknown_id_errors() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "incident_detail", &json!({ "id": 424242 })).await);
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("424242")),
        "got: {out}"
    );
}

#[tokio::test]
async fn capture_incident_tool_requires_reason_and_valid_category() {
    let app = TestApp::spawn().await;

    let out = call(dispatch(&app.state, "capture_incident", &json!({})).await);
    assert!(out["error"].is_string(), "got: {out}");

    let out = call(
        dispatch(
            &app.state,
            "capture_incident",
            &json!({ "reason": "x", "category": "catastrophe" }),
        )
        .await,
    );
    assert!(out["error"].is_string(), "got: {out}");

    let out = call(
        dispatch(
            &app.state,
            "capture_incident",
            &json!({ "reason": "weird connection storm" }),
        )
        .await,
    );
    assert_eq!(out["captured"], true, "got: {out}");
    assert!(out["id"].as_i64().is_some_and(|id| id > 0));
}

#[tokio::test]
async fn rest_capture_requires_auth_and_creates_snapshot() {
    let app = TestApp::spawn().await;

    let (st, _) = app
        .request(
            "POST",
            "/incidents/capture",
            None,
            Some(json!({ "reason": "external hook" })),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let token = app.pair_and_login().await;
    let (st, body) = app
        .request(
            "POST",
            "/incidents/capture",
            Some(&token),
            Some(json!({ "reason": "external hook", "category": "security" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    let id = body["id"].as_i64().expect("id");

    let row = IncidentRepository::new(app.state.db.clone())
        .get(id)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.trigger_kind, "manual");
    assert_eq!(row.category, "security");

    // Empty reason is rejected.
    let (st, _) = app
        .request(
            "POST",
            "/incidents/capture",
            Some(&token),
            Some(json!({ "reason": "  " })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn alert_transition_captures_once_with_cooldown() {
    use crate::services::alerting::{evaluator, expression};
    use crate::storage::repositories::AlertRepository;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // A rule that is instantly violated by any cpu sample.
    let (st, _) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(json!({
                "name": "test-cpu", "expression": "cpu.usage_percent > 1",
                "severity": "warn", "for_duration_secs": 300
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);

    // Seed one violating raw sample.
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, 95.0, 1.0, 1.0, 1.0)",
    )
    .bind(now)
    .execute(&app.state.db)
    .await
    .unwrap();

    let repo = AlertRepository::new(app.state.db.clone());
    let rule = repo.list_enabled().await.unwrap().pop().expect("rule");
    let expr = expression::parse(&rule.expression).expect("parse");

    // Tick 1: ok → pending fires the capture (spawned); tick 2 stays pending.
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();

    // The capture runs on a spawned task — poll briefly for it to land.
    let inc_repo = IncidentRepository::new(app.state.db.clone());
    let mut rows = Vec::new();
    for _ in 0..40 {
        rows = inc_repo.list(10).await.unwrap();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 1, "exactly one capture expected (cooldown)");
    let r = &rows[0];
    assert_eq!(r.trigger_kind, "alert");
    assert_eq!(r.category, "resource");
    assert_eq!(r.rule_name.as_deref(), Some("test-cpu"));
    assert_eq!(r.metric_value, Some(95.0));

    // A third tick after the capture exists must not create a second row.
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(inc_repo.list(10).await.unwrap().len(), 1);
}
