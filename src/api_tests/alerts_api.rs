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
}
