//! Integration coverage for the web-push subscription endpoint, focused on
//! the SSRF guard wired onto `POST /me/push-subscription`. IP-literal targets
//! short-circuit the policy without a DNS lookup, so every case here is
//! hermetic (no network).

use axum::http::StatusCode;
use serde_json::json;

use super::TestApp;

#[tokio::test]
async fn push_subscription_rejects_metadata_endpoint() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // The cloud-metadata address (169.254.169.254) is the classic SSRF
    // target. The endpoint must be rejected before the row is persisted.
    let (st, _) = app
        .request(
            "POST",
            "/me/push-subscription",
            Some(&token),
            Some(json!({
                "endpoint": "http://169.254.169.254/latest/meta-data/",
                "p256dh": "BBingBogus",
                "auth": "c2VjcmV0",
            })),
        )
        .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "cloud-metadata endpoint must be blocked"
    );
}

#[tokio::test]
async fn push_subscription_rejects_loopback_endpoint() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "POST",
            "/me/push-subscription",
            Some(&token),
            Some(json!({
                "endpoint": "http://127.0.0.1/push",
                "p256dh": "BBingBogus",
                "auth": "c2VjcmV0",
            })),
        )
        .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "loopback endpoint must be blocked"
    );
}

#[tokio::test]
async fn push_subscription_accepts_public_endpoint() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Public IP literal: passes the SSRF policy without a DNS lookup, so the
    // happy path stays hermetic. The handler only validates + stores (no send),
    // so a placeholder p256dh/auth is fine here.
    let (st, _) = app
        .request(
            "POST",
            "/me/push-subscription",
            Some(&token),
            Some(json!({
                "endpoint": "https://8.8.8.8/wpush/v2/abc",
                "p256dh": "BBingBogus",
                "auth": "c2VjcmV0",
            })),
        )
        .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "public endpoint should be accepted"
    );
}

#[tokio::test]
async fn push_subscription_requires_all_fields() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Empty field is rejected before the SSRF check even runs.
    let (st, _) = app
        .request(
            "POST",
            "/me/push-subscription",
            Some(&token),
            Some(json!({
                "endpoint": "https://8.8.8.8/x",
                "p256dh": "",
                "auth": "c2VjcmV0",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}
