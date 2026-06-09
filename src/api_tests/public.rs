//! Public (unauthenticated) surface + routing basics.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn health_is_public() {
    let app = TestApp::spawn().await;
    let (status, body) = app.request("GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn ready_is_public_and_reports_ready() {
    let app = TestApp::spawn().await;
    let (status, body) = app.request("GET", "/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
}

#[tokio::test]
async fn unknown_route_requires_auth_then_404() {
    let app = TestApp::spawn().await;

    // Unauthenticated, an unknown path is caught by the protected router's
    // auth-wrapped fallback → 401. This is intentional: it doesn't leak
    // which routes exist to anonymous callers.
    let (status, _) = app.request("GET", "/no/such/route", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // With a valid token, auth passes and routing falls through to 404.
    let token = app.pair_and_login().await;
    let (status, _) = app
        .request("GET", "/no/such/route", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
