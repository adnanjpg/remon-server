//! `GET /summary` — one-call host overview.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn summary_requires_auth() {
    let app = TestApp::spawn().await;
    let (status, _) = app.request("GET", "/summary", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn summary_returns_overview_shape() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (status, body) = app.request("GET", "/summary", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "summary should succeed: {body}");

    assert_eq!(body["server_name"], "test-server");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["hostname"].is_string());
    assert!(body["uptime_secs"].is_u64());

    // No collector runs in the test harness, so live-gauge fields are null
    // rather than absent — clients can rely on the keys existing.
    assert!(body["cpu_usage_percent"].is_null());
    assert!(body["memory_used_bytes"].is_null());
    assert!(body["stats_timestamp"].is_null());

    // Fresh DB → no alert state rows.
    assert_eq!(body["alerts_pending"], 0);
    assert_eq!(body["alerts_firing"], 0);
}

#[tokio::test]
async fn summary_reflects_latest_stats_tick() {
    use crate::models::stats::{
        AllStats, CoreStats, CpuStats, DiskStats, LoadAverage, MemoryStats,
    };
    use std::sync::Arc;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Seed the latest-stats cache the way the collector would.
    let stats = AllStats {
        cpu: Arc::new(CpuStats {
            usage_percent: 42.5,
            per_core: vec![CoreStats {
                core_index: 0,
                usage_percent: 42.5,
                freq_mhz: 2400,
            }],
            load_avg: LoadAverage {
                one: 0.5,
                five: 0.4,
                fifteen: 0.3,
            },
            timestamp: 1_700_000_000,
            steal_percent: None,
            iowait_percent: None,
            guest_percent: None,
            user_percent: None,
            system_percent: None,
            context_switches_per_sec: None,
            process_forks_per_sec: None,
        }),
        memory: Arc::new(MemoryStats {
            total_bytes: 8_000_000_000,
            used_bytes: 4_000_000_000,
            available_bytes: 4_000_000_000,
            cached_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            timestamp: 1_700_000_000,
            page_faults_minor_per_sec: None,
            page_faults_major_per_sec: None,
            swap_in_pages_per_sec: None,
            swap_out_pages_per_sec: None,
        }),
        disks: Arc::new(vec![
            DiskStats {
                mount_point: "/".to_string(),
                total_bytes: 100,
                used_bytes: 50,
                available_bytes: 50,
                read_bytes_per_sec: 0,
                write_bytes_per_sec: 0,
                timestamp: 1_700_000_000,
                inode_used_percent: None,
                read_iops: None,
                write_iops: None,
                io_util_percent: None,
            },
            DiskStats {
                mount_point: "/data".to_string(),
                total_bytes: 100,
                used_bytes: 90,
                available_bytes: 10,
                read_bytes_per_sec: 0,
                write_bytes_per_sec: 0,
                timestamp: 1_700_000_000,
                inode_used_percent: None,
                read_iops: None,
                write_iops: None,
                io_util_percent: None,
            },
        ]),
        network: Arc::new(vec![]),
        pressure: None,
        components: None,
    };
    *app.state.stats_latest.write().await = Some(stats);

    let (status, body) = app.request("GET", "/summary", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["cpu_usage_percent"], 42.5);
    assert_eq!(body["memory_used_bytes"], 4_000_000_000u64);
    assert_eq!(body["memory_total_bytes"], 8_000_000_000u64);
    assert_eq!(body["stats_timestamp"], 1_700_000_000);
    // The fullest mount wins the disk slot.
    assert_eq!(body["disk_max_mount"], "/data");
    assert_eq!(body["disk_max_used_percent"], 90.0);
}
