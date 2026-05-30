//! Authentication: pairing, login, token enforcement, logout revocation.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn protected_route_requires_token() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/system/info", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn garbage_token_is_rejected() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .request("GET", "/system/info", Some("not-a-real-jwt"), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn full_pairing_login_grants_access() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (status, _) = app.request("GET", "/system/info", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn wrong_pairing_code_is_rejected() {
    let app = TestApp::spawn().await;
    let (st, _) = app.request("POST", "/auth/pair/initiate", None, None).await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = app
        .request(
            "POST",
            "/auth/pair/complete",
            None,
            Some(serde_json::json!({
                "pairing_code": "00000000",
                "device_name": "attacker",
            })),
        )
        .await;
    // Wrong code → PairingExpired → 410 Gone.
    assert_eq!(st, StatusCode::GONE);
}

#[tokio::test]
async fn complete_without_active_window_is_rejected() {
    let app = TestApp::spawn().await;
    let (st, _) = app
        .request(
            "POST",
            "/auth/pair/complete",
            None,
            Some(serde_json::json!({
                "pairing_code": "12345678",
                "device_name": "x",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::GONE);
}

#[tokio::test]
async fn logout_revokes_the_session() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Valid before logout.
    let (st, _) = app.request("GET", "/system/info", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);

    // Logout revokes this access token's jti.
    let (st, _) = app
        .request("POST", "/auth/logout", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Same token is now rejected (jti no longer in `sessions`).
    let (st, _) = app.request("GET", "/system/info", Some(&token), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}
