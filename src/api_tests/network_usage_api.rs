//! `GET /metrics/network/usage` — bytes moved, integrated from stored rates.
//!
//! The arithmetic is the whole feature, so these pin it down: a rate stored at
//! one resolution must come back multiplied by that resolution's bucket, a
//! tunnel must be listed without being counted twice, and a window the daemon
//! slept through must say so through `coverage` rather than quietly reporting
//! a total that looks complete.

use axum::http::StatusCode;
use serde_json::Value;

use super::TestApp;

/// One `metrics_network` row: a rate, not a total.
async fn seed(app: &TestApp, resolution: &str, ts: i64, iface: &str, rx: i64, tx: i64) {
    sqlx::query(
        "INSERT INTO metrics_network
           (resolution, timestamp, interface_name, rx_bytes_per_sec, tx_bytes_per_sec,
            rx_packets_per_sec, tx_packets_per_sec)
         VALUES (?, ?, ?, ?, ?, 0, 0)",
    )
    .bind(resolution)
    .bind(ts)
    .bind(iface)
    .bind(rx)
    .bind(tx)
    .execute(&app.state.db)
    .await
    .expect("seed network row");
}

fn iface<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["interfaces"]
        .as_array()
        .expect("interfaces array")
        .iter()
        .find(|i| i["name"] == name)
        .unwrap_or_else(|| panic!("no interface {name} in {body}"))
}

/// A rate held for a bucket is that rate times the bucket's width. Seeded at
/// `1h`, so the multiplier is 3600 and nothing else.
#[tokio::test]
async fn rates_integrate_into_bytes_over_the_bucket() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let start = now - 3 * 3600;

    for i in 0..3 {
        seed(&app, "1h", start + i * 3600, "eth0", 1000, 500).await;
    }

    let (status, body) = app
        .request(
            "GET",
            &format!("/metrics/network/usage?start={start}&end={now}&resolution=1h"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // 3 buckets × 3600 s × 1000 B/s.
    assert_eq!(body["total_rx_bytes"], 3 * 3600 * 1000, "got: {body}");
    assert_eq!(body["total_tx_bytes"], 3 * 3600 * 500);
    assert_eq!(body["resolution"], "1h");
}

/// The same rate stored at a finer resolution stands for less wall clock, so
/// the multiplier has to follow the resolution rather than being a constant.
#[tokio::test]
async fn the_bucket_width_follows_the_resolution() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let start = now - 600;

    for i in 0..10 {
        seed(&app, "1m", start + i * 60, "eth0", 100, 0).await;
    }

    let (status, body) = app
        .request(
            "GET",
            &format!("/metrics/network/usage?start={start}&end={now}&resolution=1m"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_rx_bytes"], 10 * 60 * 100, "got: {body}");
}

/// A WireGuard byte is also an eth0 byte. The tunnel stays visible — an
/// operator wants to know what crossed the VPN — but adding it to the total
/// would report a host that moved twice the traffic it did.
#[tokio::test]
async fn a_tunnel_is_listed_but_left_out_of_the_total() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let start = now - 3600;

    seed(&app, "1h", start, "eth0", 1000, 1000).await;
    seed(&app, "1h", start, "wg0", 900, 900).await;

    let (status, body) = app
        .request(
            "GET",
            &format!("/metrics/network/usage?start={start}&end={now}&resolution=1h"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["total_rx_bytes"], 3600 * 1000, "got: {body}");
    assert_eq!(iface(&body, "wg0")["is_tunnel"], true);
    assert_eq!(iface(&body, "wg0")["rx_bytes"], 3600 * 900);
    assert_eq!(iface(&body, "eth0")["is_tunnel"], false);
}

/// Silence and zero traffic produce the same total, and only `coverage` can
/// tell them apart. Half the window seeded must read as roughly half covered.
#[tokio::test]
async fn a_gap_in_the_window_shows_up_as_coverage() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let start = now - 10 * 3600;

    // Five of the ten hours carry rows; the daemon was down for the rest.
    for i in 0..5 {
        seed(&app, "1h", start + i * 3600, "eth0", 1000, 0).await;
    }

    let (status, body) = app
        .request(
            "GET",
            &format!("/metrics/network/usage?start={start}&end={now}&resolution=1h"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let coverage = body["coverage"].as_f64().expect("coverage is a number");
    assert!(
        (0.4..=0.6).contains(&coverage),
        "half a window of rows should read as about half covered, got {coverage} in {body}"
    );
    // The traffic that was measured is still reported in full.
    assert_eq!(body["total_rx_bytes"], 5 * 3600 * 1000);
}

/// An empty window is a real answer — zero bytes, no coverage — not a 500.
#[tokio::test]
async fn an_empty_window_answers_zero() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();

    let (status, body) = app
        .request(
            "GET",
            &format!("/metrics/network/usage?start={}&end={now}", now - 86_400),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_rx_bytes"], 0, "got: {body}");
    assert_eq!(body["coverage"], 0.0);
    assert_eq!(body["interfaces"].as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn usage_requires_a_token() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .request("GET", "/metrics/network/usage", None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Naming, not numbering, is what separates a tunnel from a NIC: `tun0` is a
/// tunnel, but a card that merely starts with the same letters is not.
#[test]
fn tunnel_detection_does_not_swallow_real_nics() {
    use crate::services::system::is_tunnel_interface;

    for name in ["wg0", "tun0", "ppp0", "tailscale0", "utun3", "gre1"] {
        assert!(is_tunnel_interface(name), "{name} should read as a tunnel");
    }
    for name in ["eth0", "enp3s0", "wlan0", "tunnelbroker", "grebond", "bond0"] {
        assert!(!is_tunnel_interface(name), "{name} is not a tunnel");
    }
}
