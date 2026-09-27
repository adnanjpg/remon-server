//! Integration coverage for the web-push subscription endpoint: the SSRF
//! guard on the relay endpoint, and validation of what the browser brings.
//! IP-literal targets short-circuit the policy without a DNS lookup, so every
//! case here is hermetic (no network).

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::TestApp;
use crate::notify::Severity;
use crate::storage::repositories::DeviceRepository;

/// A valid subscription body; `endpoint` and extra fields vary per test.
fn subscription(endpoint: &str, extra: Value) -> Value {
    let mut body = json!({
        "endpoint": endpoint,
        "p256dh": "BBingBogus",
        "auth": "c2VjcmV0",
        "vapid_private_key": crate::services::webpush::test_key(),
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    body
}

async fn subscribe(app: &TestApp, token: &str, body: Value) -> StatusCode {
    app.request("POST", "/me/push-subscription", Some(token), Some(body))
        .await
        .0
}

#[tokio::test]
async fn push_subscription_rejects_metadata_endpoint() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // The cloud-metadata address (169.254.169.254) is the classic SSRF
    // target. The endpoint must be rejected before the row is persisted.
    let body = subscription("http://169.254.169.254/latest/meta-data/", json!({}));
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn push_subscription_rejects_loopback_endpoint() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let body = subscription("http://127.0.0.1/push", json!({}));
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn push_subscription_requires_all_fields() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Empty field is rejected before the SSRF check even runs.
    let body = subscription("https://8.8.8.8/x", json!({ "p256dh": "" }));
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::BAD_REQUEST);

    // No browser key at all: there is nothing to sign its pushes with.
    let mut body = subscription("https://8.8.8.8/x", json!({}));
    body.as_object_mut().unwrap().remove("vapid_private_key");
    assert!(subscribe(&app, &token, body).await.is_client_error());
}

#[tokio::test]
async fn push_subscription_stores_what_the_browser_brings() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Public IP literal: passes the SSRF policy without a DNS lookup. The
    // handler only validates and stores, so placeholder p256dh/auth are fine.
    let body = subscription(
        "https://8.8.8.8/wpush/v2/abc",
        json!({ "ref": "profile-1", "min_severity": "crit" }),
    );
    let key = body["vapid_private_key"].as_str().unwrap().to_string();
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::NO_CONTENT);

    let targets = DeviceRepository::new(app.state.db.clone())
        .list_active_web_push_targets()
        .await
        .unwrap();
    assert_eq!(targets.len(), 1);
    let sub = &targets[0].subscription;
    assert_eq!(sub.vapid_key, key);
    assert_eq!(sub.reference.as_deref(), Some("profile-1"));
    assert_eq!(sub.min_severity.as_deref(), Some("crit"));
}

#[tokio::test]
async fn push_subscription_rejects_a_bad_key_ref_or_severity() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    for extra in [
        json!({ "vapid_private_key": "not a key" }),
        json!({ "min_severity": "info" }),
        json!({ "ref": "x".repeat(200) }),
    ] {
        let body = subscription("https://8.8.8.8/wpush/v2/abc", extra.clone());
        assert_eq!(
            subscribe(&app, &token, body).await,
            StatusCode::BAD_REQUEST,
            "{extra} must be rejected"
        );
    }
}

#[tokio::test]
async fn push_unsubscribe_clears_the_subscription() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let body = subscription(
        "https://8.8.8.8/wpush/v2/abc",
        json!({ "ref": "profile-1" }),
    );
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::NO_CONTENT);
    let (st, _) = app
        .request("DELETE", "/me/push-subscription", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let targets = DeviceRepository::new(app.state.db.clone())
        .list_active_web_push_targets()
        .await
        .unwrap();
    assert!(targets.is_empty());
}

#[tokio::test]
async fn a_subscribed_browser_counts_as_somewhere_to_send() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let repo = DeviceRepository::new(app.state.db.clone());

    assert!(!repo.has_web_push_target(Severity::Crit).await.unwrap());

    let body = subscription(
        "https://8.8.8.8/wpush/v2/abc",
        json!({ "min_severity": "crit" }),
    );
    assert_eq!(subscribe(&app, &token, body).await, StatusCode::NO_CONTENT);

    assert!(repo.has_web_push_target(Severity::Crit).await.unwrap());
    assert!(!repo.has_web_push_target(Severity::Warn).await.unwrap());
}
