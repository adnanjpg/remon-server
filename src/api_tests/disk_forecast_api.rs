//! `GET /metrics/disk/forecast` — when each volume runs out of room.
//!
//! The estimator has its own unit tests in `services::forecast`; these cover
//! the part only the endpoint can get wrong: reading the series per mount,
//! taking capacity as of now rather than as of the oldest row, and passing
//! through the refusal to answer when a volume's drift is lost in its churn.

use axum::http::StatusCode;
use serde_json::Value;

use super::TestApp;

/// One hourly `metrics_disk` row.
async fn seed(app: &TestApp, ts: i64, mount: &str, used: i64, total: i64) {
    sqlx::query(
        "INSERT INTO metrics_disk
           (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes,
            read_bytes_per_sec, write_bytes_per_sec)
         VALUES ('1h', ?, ?, ?, ?, ?, 0, 0)",
    )
    .bind(ts)
    .bind(mount)
    .bind(total)
    .bind(used)
    .bind(total - used)
    .execute(&app.state.db)
    .await
    .expect("seed disk row");
}

fn mount<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["mounts"]
        .as_array()
        .expect("mounts array")
        .iter()
        .find(|m| m["mount_point"] == name)
        .unwrap_or_else(|| panic!("no mount {name} in {body}"))
}

/// 14 days of hourly rows ending now, filling by `per_hour` bytes each tick.
async fn seed_ramp(app: &TestApp, mount_point: &str, start_used: i64, per_hour: i64, total: i64) {
    let now = chrono::Utc::now().timestamp();
    for i in 0..336i64 {
        let ts = now - (336 - i) * 3600;
        seed(app, ts, mount_point, start_used + per_hour * i, total).await;
    }
}

#[tokio::test]
async fn a_steadily_filling_volume_gets_a_date() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // 1 GB/h = 24 GB/day; ends at 760 GB of a 1 TB volume, so ~10 days left.
    seed_ramp(&app, "/", 424_000_000_000, 1_000_000_000, 1_000_000_000_000).await;

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let m = mount(&body, "/");
    assert_eq!(m["verdict"], "filling", "got: {body}");
    let days = m["days_until_full"].as_f64().expect("a date");
    assert!((8.0..12.0).contains(&days), "got {days} days");
    // ~24 GB/day, allowing for the hourly grid.
    let per_day = m["bytes_per_day"].as_f64().expect("rate");
    assert!(
        (per_day - 24.0e9).abs() < 1.0e9,
        "got {per_day} bytes/day in {body}"
    );
}

/// The case the whole feature lives or dies on: a volume that churns without
/// going anywhere must refuse to name a day rather than guess one.
#[tokio::test]
async fn a_churning_volume_refuses_to_name_a_day() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let swing = [
        0i64,
        40_000_000_000,
        -30_000_000_000,
        20_000_000_000,
        -35_000_000_000,
    ];

    for i in 0..336i64 {
        let ts = now - (336 - i) * 3600;
        let used = 500_000_000_000 + swing[(i % 5) as usize];
        seed(&app, ts, "/var", used, 1_000_000_000_000).await;
    }

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let m = mount(&body, "/var");
    assert_eq!(m["verdict"], "unclear", "got: {body}");
    assert!(m["days_until_full"].is_null(), "got: {body}");
}

/// Every mount is fitted on its own series — one filling volume must not drag
/// a quiet one along with it.
#[tokio::test]
async fn mounts_are_forecast_independently() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    seed_ramp(&app, "/", 424_000_000_000, 1_000_000_000, 1_000_000_000_000).await;
    seed_ramp(&app, "/boot", 300_000_000, 0, 1_000_000_000).await;

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mounts"].as_array().expect("mounts").len(), 2);

    assert_eq!(mount(&body, "/")["verdict"], "filling", "got: {body}");
    // A volume that has not moved in a fortnight is `stable`, not `unclear`:
    // the two mean different things, and only one of them is an admission of
    // ignorance. Either way it must carry no date of its own.
    assert_eq!(mount(&body, "/boot")["verdict"], "stable", "got: {body}");
    assert!(mount(&body, "/boot")["days_until_full"].is_null());
    assert_eq!(mount(&body, "/boot")["bytes_per_day"], 0);
}

/// A volume grown mid-window must be forecast against the size it has now, not
/// the one it had a fortnight ago — otherwise resizing a disk reports it as
/// about to fill.
#[tokio::test]
async fn capacity_is_read_as_of_the_newest_row() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();

    for i in 0..336i64 {
        let ts = now - (336 - i) * 3600;
        // Doubled at the halfway mark.
        let total = if i < 168 {
            500_000_000_000
        } else {
            1_000_000_000_000
        };
        seed(&app, ts, "/data", 400_000_000_000 + 100_000_000 * i, total).await;
    }

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mount(&body, "/data")["total_bytes"],
        1_000_000_000_000i64,
        "got: {body}"
    );
}

/// Freeing space is a real verdict, and it carries no date.
#[tokio::test]
async fn a_draining_volume_says_so() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    seed_ramp(
        &app,
        "/",
        800_000_000_000,
        -1_000_000_000,
        1_000_000_000_000,
    )
    .await;

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let m = mount(&body, "/");
    assert_eq!(m["verdict"], "draining", "got: {body}");
    assert!(m["days_until_full"].is_null());
    assert!(m["bytes_per_day"].as_f64().expect("rate") < 0.0);
}

/// A horizon the caller narrows must move the line between "dated" and
/// "stable" rather than being ignored.
#[tokio::test]
async fn the_horizon_bounds_what_gets_a_date() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    seed_ramp(&app, "/", 424_000_000_000, 1_000_000_000, 1_000_000_000_000).await;

    let (_, wide) = app
        .request(
            "GET",
            "/metrics/disk/forecast?horizon_days=60",
            Some(&token),
            None,
        )
        .await;
    assert_eq!(mount(&wide, "/")["verdict"], "filling");

    let (_, narrow) = app
        .request(
            "GET",
            "/metrics/disk/forecast?horizon_days=2",
            Some(&token),
            None,
        )
        .await;
    assert_eq!(mount(&narrow, "/")["verdict"], "stable", "got: {narrow}");
    assert!(mount(&narrow, "/")["days_until_full"].is_null());
}

/// An empty database is a real answer, not a 500.
#[tokio::test]
async fn no_history_answers_with_no_mounts() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (status, body) = app
        .request("GET", "/metrics/disk/forecast", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mounts"].as_array().expect("mounts").len(), 0);
}

#[tokio::test]
async fn forecast_requires_a_token() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .request("GET", "/metrics/disk/forecast", None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
