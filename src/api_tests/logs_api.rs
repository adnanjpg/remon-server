//! `GET /logs` — application-log read API.

use super::TestApp;
use axum::http::StatusCode;

use crate::storage::repositories::LogRepository;
use crate::storage::repositories::logs::NewLogRow;

fn row(timestamp: i64, level: i32, message: &str) -> NewLogRow {
    NewLogRow {
        timestamp,
        level,
        source: "test-app".to_string(),
        target: "remon_server::test".to_string(),
        message: message.to_string(),
    }
}

#[tokio::test]
async fn logs_require_auth() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/logs", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logs_filter_by_level_and_sort_newest_first() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let now = chrono::Utc::now().timestamp();
    let repo = LogRepository::new(app.state.db.clone());
    repo.insert_batch(&[
        row(now - 30, 1, "boom"),
        row(now - 20, 3, "started"),
        row(now - 10, 2, "flaky"),
    ])
    .await
    .expect("seed logs");

    // Default: everything, newest first.
    let (status, body) = app.request("GET", "/logs", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "logs should succeed: {body}");
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["message"], "flaky");
    assert_eq!(entries[0]["level"], "warn");
    assert_eq!(entries[2]["message"], "boom");

    // level=warn keeps warn + error only.
    let (status, body) = app
        .request("GET", "/logs?level=warn", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|e| e["level"] == "warn" || e["level"] == "error")
    );
}

#[tokio::test]
async fn logs_validate_query_params() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (status, _) = app
        .request("GET", "/logs?level=loud", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = app
        .request("GET", "/logs?start=200&end=100", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logs_honor_range_and_limit() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let repo = LogRepository::new(app.state.db.clone());
    repo.insert_batch(&[row(100, 3, "old"), row(200, 3, "mid"), row(300, 3, "new")])
        .await
        .expect("seed logs");

    let (status, body) = app
        .request("GET", "/logs?start=150&end=250", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["message"], "mid");

    let (status, body) = app
        .request("GET", "/logs?start=0&end=400&limit=2", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2, "limit caps the page");
    assert_eq!(entries[0]["message"], "new");
}
