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

/// The process list this endpoint kills from includes the server itself, and
/// the default signal is the one it shuts down cleanly on. Killing it here has
/// to be refused, or an operator tidying up a process list switches off the
/// monitoring they are looking at.
#[tokio::test]
async fn kill_refuses_the_server_itself() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let own_pid = std::process::id();
    let (st, body) = app
        .request(
            "DELETE",
            &format!("/processes/{own_pid}"),
            Some(&token),
            None,
        )
        .await;

    assert_eq!(st, StatusCode::CONFLICT);
    // The refusal has to name the way through, not just say no.
    let message = body.to_string();
    assert!(
        message.contains("/system/restart"),
        "expected the refusal to point at the deliberate endpoint, got: {message}"
    );
}

#[tokio::test]
async fn kill_refuses_pid_1() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request("DELETE", "/processes/1", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
}
