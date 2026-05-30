//! Public (unauthenticated) surface + routing basics.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn health_is_public() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn hello_is_public() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/hello", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn teapot_returns_418() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/teapot", None, None).await;
    assert_eq!(status, StatusCode::IM_A_TEAPOT);
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
