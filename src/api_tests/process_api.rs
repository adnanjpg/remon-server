//! Process listing + kill validation.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn list_processes_requires_auth() {
    let app = TestApp::spawn().await;
    let (st, _) = app.request("GET", "/processes", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_processes_returns_snapshot() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request("GET", "/processes?limit=5", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["processes"].is_array());
    // The test runner itself is a process, so the snapshot is never empty.
    assert!(body["total"].as_u64().expect("total") > 0);
}

#[tokio::test]
async fn kill_rejects_invalid_signal() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Only 9 (SIGKILL) and 15 (SIGTERM) are accepted; 7 must be a 400 before
    // any OS call happens.
    let (st, _) = app
        .request(
            "DELETE",
            "/processes/4294967295?signal=7",
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}
