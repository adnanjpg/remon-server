//! `GET /system/smart` — SMART disk health.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn smart_requires_auth() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/system/smart", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn smart_empty_when_collector_never_ran() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (status, body) = app
        .request("GET", "/system/smart", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK, "smart should succeed: {body}");
    // No collector in the test harness → unavailable, no devices.
    assert_eq!(body["available"], false);
    assert_eq!(body["devices"], serde_json::json!([]));
}

#[tokio::test]
async fn smart_returns_latest_reading_per_device() {
    use crate::storage::repositories::{SmartDeviceRow, SmartRepository};

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let repo = SmartRepository::new(app.state.db.clone());
    let ata = SmartDeviceRow {
        device: "/dev/sda".to_string(),
        model: Some("WDC WD40EFRX".to_string()),
        serial: Some("WD-1234".to_string()),
        health_passed: Some(true),
        temperature_c: Some(34.0),
        power_on_hours: Some(21345),
        power_cycles: Some(87),
        reallocated_sectors: Some(0),
        pending_sectors: Some(0),
        uncorrectable_sectors: Some(0),
        udma_crc_errors: Some(13),
        percentage_used: None,
        available_spare_percent: None,
        media_errors: None,
    };
    repo.insert_tick(100, std::slice::from_ref(&ata))
        .await
        .expect("seed tick 1");
    // Second tick: health flips — the endpoint must serve this one.
    let mut ata2 = ata.clone();
    ata2.health_passed = Some(false);
    ata2.reallocated_sectors = Some(12);
    repo.insert_tick(200, &[ata2]).await.expect("seed tick 2");

    let (status, body) = app
        .request("GET", "/system/smart", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let devices = body["devices"].as_array().expect("devices array");
    assert_eq!(devices.len(), 1, "one row per device, not per tick");
    let d = &devices[0];
    assert_eq!(d["device"], "/dev/sda");
    assert_eq!(d["model"], "WDC WD40EFRX");
    assert_eq!(d["health_passed"], false);
    assert_eq!(d["reallocated_sectors"], 12);
    assert_eq!(d["timestamp"], 200);
    // NVMe-only fields are null on an ATA disk but the keys exist.
    assert!(d["percentage_used"].is_null());
}
