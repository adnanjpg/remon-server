//! Runtime config: GET reflects effective state, PATCH validates and persists.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn get_config_returns_effective_state() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app.request("GET", "/config", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["server_name"], "test-server");
    assert_eq!(body["collector_stats_interval_ms"], 2000);
    // The container-stats collector interval is now a live config field.
    assert_eq!(body["collector_docker_interval_ms"], 3000);
}

#[tokio::test]
async fn patch_config_rejects_subsecond_interval() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({ "collector_stats_interval_ms": 500 })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_config_rejects_overlong_server_name() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let long_name = "n".repeat(200);
    let (st, _) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({ "server_name": long_name })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_config_applies_and_persists() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({
                "server_name": "renamed",
                "collector_stats_interval_ms": 3000,
            })),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["server_name"], "renamed");
    assert_eq!(body["collector_stats_interval_ms"], 3000);

    // Reflected on a subsequent GET (read-back through the DB + live state).
    let (_, body) = app.request("GET", "/config", Some(&token), None).await;
    assert_eq!(body["server_name"], "renamed");
    assert_eq!(body["collector_stats_interval_ms"], 3000);
}
