//! Alert rule CRUD + expression validation + schema catalogue.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn create_list_get_delete_rule() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Create.
    let (st, body) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(serde_json::json!({
                "name": "high cpu",
                "expression": "cpu.usage_percent > 80",
                "severity": "warn",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "create failed: {body}");
    let id = body["id"].as_i64().expect("rule id");
    assert_eq!(body["name"], "high cpu");
    assert_eq!(body["severity"], "warn");

    // List contains exactly the new rule.
    let (st, body) = app.request("GET", "/alerts", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["rules"].as_array().expect("rules array").len(), 1);

    // Fetch by id.
    let (st, _) = app
        .request("GET", &format!("/alerts/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);

    // Delete.
    let (st, _) = app
        .request("DELETE", &format!("/alerts/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Gone now.
    let (st, _) = app
        .request("GET", &format!("/alerts/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_expression_is_rejected() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(serde_json::json!({
                "name": "bad",
                "expression": "this is not a valid expression",
                "severity": "warn",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_missing_rule_is_404() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request("DELETE", "/alerts/999999", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn schema_endpoint_returns_catalogue() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request("GET", "/alerts/schema", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["namespaces"].is_array());
    assert!(body["comparators"].is_array());

    // The catalogue mirrors the resolver whitelists (resolver.rs) — a
    // namespace the evaluator accepts must be offered to the rule editor.
    let names: Vec<&str> = body["namespaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    for expected in [
        "cpu",
        "memory",
        "disk",
        "network",
        "pressure",
        "components",
        "smart",
        "docker",
        "process",
        "probe",
        "heartbeat",
        "service",
    ] {
        assert!(names.contains(&expected), "schema is missing '{expected}'");
    }
}

#[tokio::test]
async fn explicit_null_clears_description_and_silence() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(serde_json::json!({
                "name": "nullable",
                "description": "temporary note",
                "expression": "cpu.usage_percent > 80",
                "severity": "warn",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = body["id"].as_i64().unwrap();

    let (st, _) = app
        .request(
            "POST",
            &format!("/alerts/{id}/silence"),
            Some(&token),
            Some(serde_json::json!({"duration_secs": 600})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);

    // Omitting a field leaves it alone…
    let (st, body) = app
        .request(
            "PUT",
            &format!("/alerts/{id}"),
            Some(&token),
            Some(serde_json::json!({"name": "nullable2"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["description"], "temporary note");
    assert!(body["silenced_until"].is_i64());

    // …an explicit null clears it.
    let (st, body) = app
        .request(
            "PUT",
            &format!("/alerts/{id}"),
            Some(&token),
            Some(serde_json::json!({"description": null, "silenced_until": null})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["description"].is_null(), "null must clear description");
    assert!(body["silenced_until"].is_null(), "null must clear silence");
}

/// The event-driven evaluator keeps lifecycle in memory and writes
/// `alert_state` only on transitions / for non-Ok rows — a healthy Ok rule
/// must not touch the table, while a pending row must persist so
/// `GET /alerts/state` can show it.
#[tokio::test]
async fn evaluator_persists_selectively() {
    use crate::models::alert::AlertLifecycle;
    use crate::services::alerting::{evaluator, expression};
    use crate::storage::repositories::AlertRepository;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(serde_json::json!({
                "name": "cpu-threshold",
                "expression": "cpu.usage_percent > 80",
                "severity": "warn",
                "for_duration_secs": 0
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);

    let repo = AlertRepository::new(app.state.db.clone());
    let rule = repo.list_enabled().await.unwrap().pop().expect("rule");
    let expr = expression::parse(&rule.expression).expect("parse");
    let now = chrono::Utc::now().timestamp();

    // Below threshold → Ok. resolve_with_state falls through to the DB (no
    // live snapshot in tests), so seed a raw sample.
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, 10.0, 0, 0, 0)",
    )
    .bind(now)
    .execute(&app.state.db)
    .await
    .unwrap();

    let out = evaluator::evaluate_in_memory_once(&rule, &expr, &app.state, &[], now).await;
    assert_eq!(out, vec![("{}".to_string(), AlertLifecycle::Ok)]);
    assert!(
        repo.list_state_for_rule(rule.id).await.unwrap().is_empty(),
        "an Ok row stays in memory, never hits alert_state"
    );

    // Above threshold → ok→pending, which is non-Ok and must persist.
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, 95.0, 0, 0, 0)",
    )
    .bind(now + 1)
    .execute(&app.state.db)
    .await
    .unwrap();

    let out = evaluator::evaluate_in_memory_once(&rule, &expr, &app.state, &[], now + 1).await;
    assert_eq!(out, vec![("{}".to_string(), AlertLifecycle::Pending)]);
    let rows = repo.list_state_for_rule(rule.id).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a pending row must persist for /alerts/state"
    );
    assert_eq!(rows[0].state, AlertLifecycle::Pending);
}
