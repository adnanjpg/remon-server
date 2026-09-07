//! Authentication: pairing, login, token enforcement, logout revocation.

use super::TestApp;
use axum::http::StatusCode;

use crate::auth::service::AuthService;
use crate::models::auth::StoredDevice;
use crate::storage::repositories::DeviceRepository;
use serde_json::{Value, json};

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

async fn login_fixture(app: &TestApp) -> (Value, Value) {
    let credentials = json!({ "device_id": "auth-test", "device_token": "test-device-secret" });
    DeviceRepository::new(app.state.db.clone())
        .create(&StoredDevice {
            id: "auth-test".into(),
            name: "auth-test".into(),
            token_hash: AuthService::hash_token("test-device-secret").unwrap(),
            last_ip: None,
            last_seen: 0,
            created_at: 0,
            is_active: true,
        })
        .await
        .unwrap();
    let (status, tokens) = app
        .request("POST", "/auth/login", None, Some(credentials.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    (credentials, tokens)
}

async fn sessions(app: &TestApp) -> Vec<(String, String, i64)> {
    sqlx::query_as("SELECT id, device_id, expires_at FROM sessions ORDER BY id")
        .fetch_all(&app.state.db)
        .await
        .unwrap()
}

async fn reject_refresh_insert(app: &TestApp) {
    // Fail the SECOND insert, after the access row has already been written.
    sqlx::query(
        "CREATE TRIGGER reject_refresh_insert BEFORE INSERT ON sessions
         WHEN NEW.expires_at > unixepoch() + 86400
         BEGIN SELECT RAISE(ABORT, 'injected refresh insert failure'); END",
    )
    .execute(&app.state.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn refresh_insert_failure_preserves_old_sessions_and_allows_retry() {
    let app = TestApp::spawn().await;
    let (_, old) = login_fixture(&app).await;
    let before = sessions(&app).await;
    let access = old["access_token"].as_str().unwrap();
    let body = json!({ "refresh_token": old["refresh_token"] });
    // Populate the positive auth cache before attempting rotation.
    assert_eq!(
        app.request("GET", "/system/info", Some(access), None)
            .await
            .0,
        StatusCode::OK
    );
    reject_refresh_insert(&app).await;

    let (status, _) = app
        .request("POST", "/auth/refresh", None, Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        sessions(&app).await,
        before,
        "failed rotation must restore every old row"
    );
    app.state.session_cache.clear();
    assert_eq!(
        app.request("GET", "/system/info", Some(access), None)
            .await
            .0,
        StatusCode::OK
    );

    sqlx::query("DROP TRIGGER reject_refresh_insert")
        .execute(&app.state.db)
        .await
        .unwrap();
    let (status, rotated) = app.request("POST", "/auth/refresh", None, Some(body)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the old refresh token must remain retryable: {rotated}"
    );
    assert_eq!(sessions(&app).await.len(), 2);
    assert_eq!(
        app.request("GET", "/system/info", Some(access), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request(
            "GET",
            "/system/info",
            rotated["access_token"].as_str(),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn login_insert_failure_leaves_no_partial_pair() {
    let app = TestApp::spawn().await;
    let (credentials, _) = login_fixture(&app).await;
    let before = sessions(&app).await;
    reject_refresh_insert(&app).await;
    let (status, _) = app
        .request("POST", "/auth/login", None, Some(credentials))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(sessions(&app).await, before);
}

#[tokio::test]
async fn concurrent_refresh_has_one_winner_and_a_valid_pair() {
    // Separate connections over a real WAL database exercise SQLite's writer
    // lock, rather than relying on the default harness's single connection.
    let path = std::env::temp_dir().join(format!("remon-auth-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}", path.to_string_lossy().replace('\\', "/"));
    // Set WAL once before opening multiple connections: racing the initial
    // journal-mode change can itself fail with SQLITE_BUSY on Windows.
    let bootstrap = crate::storage::Database::connect(&url, 1).await.unwrap();
    bootstrap.pool().close().await;
    let app = TestApp::spawn_at(&url, 2).await;
    let (_, old) = login_fixture(&app).await;
    let body = json!({ "refresh_token": old["refresh_token"] });
    let (first, second) = tokio::join!(
        app.request("POST", "/auth/refresh", None, Some(body.clone())),
        app.request("POST", "/auth/refresh", None, Some(body.clone())),
    );
    let (winner, loser) = if first.0 == StatusCode::OK {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(winner.0, StatusCode::OK, "{winner:?}");
    assert_eq!(loser.0, StatusCode::UNAUTHORIZED, "{loser:?}");
    assert_eq!(sessions(&app).await.len(), 2);
    assert_eq!(
        app.request(
            "GET",
            "/system/info",
            winner.1["access_token"].as_str(),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    // A replay cannot remove the winning pair.
    assert_eq!(
        app.request("POST", "/auth/refresh", None, Some(body))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(sessions(&app).await.len(), 2);
    app.state.db.close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn inactive_device_cannot_rotate_or_change_its_sessions() {
    let app = TestApp::spawn().await;
    let (_, old) = login_fixture(&app).await;
    let before = sessions(&app).await;
    DeviceRepository::new(app.state.db.clone())
        .deactivate("auth-test")
        .await
        .unwrap();
    let (status, body) = app
        .request(
            "POST",
            "/auth/refresh",
            None,
            Some(json!({ "refresh_token": old["refresh_token"] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(sessions(&app).await, before);
}

#[tokio::test]
async fn session_count_counts_token_rows_not_open_browsers() {
    let app = TestApp::spawn().await;
    let (credentials, mut tokens) = login_fixture(&app).await;
    // Reopening the client logs in again with its persisted device credential.
    // Each login adds an access AND a refresh row, even for the same device.
    for _ in 0..2 {
        let (status, next) = app
            .request("POST", "/auth/login", None, Some(credentials.clone()))
            .await;
        assert_eq!(status, StatusCode::OK);
        tokens = next;
    }
    let (status, listing) = app
        .request("GET", "/me/sessions", tokens["access_token"].as_str(), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["sessions"][0]["active_sessions"], 6);
    let (status, rotated) = app
        .request(
            "POST",
            "/auth/refresh",
            None,
            Some(json!({ "refresh_token": tokens["refresh_token"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listing) = app
        .request(
            "GET",
            "/me/sessions",
            rotated["access_token"].as_str(),
            None,
        )
        .await;
    assert_eq!(listing["sessions"][0]["active_sessions"], 2);
}
