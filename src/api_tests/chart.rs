use super::{TestApp, rollup_cursor::samples};
use crate::storage::repositories::{
    read_cpu_chart, read_disk_chart, read_memory_chart, read_network_chart,
};

#[tokio::test]
async fn cpu_chart_stitched_statistics_match_raw_with_seams_inside_output_buckets() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 600 - 20) * 600;
    samples(&app, base, base + 6000).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    // A recently enabled, lagging minute tier, with no certified coarser tier.
    sqlx::query("UPDATE rollup_state SET processed_from = NULL WHERE resolution IN ('5m', '1h')")
        .execute(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE rollup_state SET processed_from = ?, last_bucket_ts = ? WHERE resource = 'cpu' AND resolution = '1m'")
        .bind(base + 120).bind(base + 4740).execute(&app.state.db).await.unwrap();
    let (points, meta) = read_cpu_chart(&app.state.db, base + 17, base + 5983, 16)
        .await
        .unwrap();
    assert!(points.len() <= 16);
    assert_eq!(meta.sources.len(), 3);
    assert_eq!(meta.sources[0].resolution, "raw");
    assert_eq!(meta.sources[1].resolution, "1m");
    assert_eq!(meta.sources[2].resolution, "raw");
    assert!(meta.sources[0].to % meta.bucket_seconds != 0);
    for p in &points {
        for field in ["usage_percent", "steal_percent", "context_switches_per_sec"] {
            let sql = format!(
                "SELECT MIN(1.0 * {field}), MAX(1.0 * {field}), COALESCE(SUM(1.0 * {field}), 0.0), COUNT({field}) FROM metrics_cpu WHERE resolution = 'raw' AND timestamp >= ? AND timestamp < ?"
            );
            let expected: (Option<f64>, Option<f64>, f64, i64) =
                sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(p.timestamp)
                    .bind(p.timestamp + meta.bucket_seconds)
                    .fetch_one(&app.state.db)
                    .await
                    .unwrap();
            let actual = &p.statistics.as_ref().unwrap()[field];
            assert_eq!(actual.valid_count, expected.3);
            assert_eq!(actual.min, expected.0);
            assert_eq!(actual.max, expected.1);
            assert!((actual.sum - expected.2).abs() < 1e-7);
        }
    }
    let all_count: i64 = points
        .iter()
        .map(|p| p.statistics.as_ref().unwrap()["usage_percent"].valid_count)
        .sum();
    assert_eq!(
        all_count, 3000,
        "aligned full range, without duplicates or dropped head"
    );
    // A single legacy source row invalidates extrema for its output bucket.
    sqlx::query(
        "UPDATE metrics_cpu SET summary_version = NULL WHERE resolution = '1m' AND timestamp = ?",
    )
    .bind(base + 180)
    .execute(&app.state.db)
    .await
    .unwrap();
    let (points, _) = read_cpu_chart(&app.state.db, base + 17, base + 5983, 16)
        .await
        .unwrap();
    assert!(points[0].statistics.is_none());
    assert!(points[1].statistics.is_some());
}

#[tokio::test]
async fn cpu_chart_raw_probe_preserves_whole_window_or_rebuckets() {
    let app = TestApp::spawn().await;
    let base = chrono::Utc::now().timestamp() - 1000;
    samples(&app, base, base + 200).await;
    let (points, meta) = read_cpu_chart(&app.state.db, base, base + 200, 100)
        .await
        .unwrap();
    assert_eq!(points.len(), 100);
    assert_eq!(meta.bucket_seconds, 0);
    let (points, meta) = read_cpu_chart(&app.state.db, base, base + 200, 16)
        .await
        .unwrap();
    assert!(points.len() <= 16);
    assert!(meta.bucket_seconds > 0);
    assert_eq!(
        points
            .iter()
            .map(|p| p.statistics.as_ref().unwrap()["usage_percent"].valid_count)
            .sum::<i64>(),
        100
    );
}

#[tokio::test]
async fn cpu_chart_raw_probe_tolerates_client_clock_ahead() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    samples(&app, now - 100, now).await;
    let (_, meta) = read_cpu_chart(&app.state.db, now - 100, now + 5, 300)
        .await
        .unwrap();
    assert_eq!(meta.bucket_seconds, 0);
}

#[tokio::test]
async fn cpu_chart_old_short_window_uses_retained_coarse_data_and_reports_missing_coverage() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 300 - 10) * 300;
    samples(&app, base, base + 1200).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    sqlx::query("DELETE FROM metrics_cpu WHERE resolution IN ('raw', '1m')")
        .execute(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE rollup_state SET processed_from = NULL WHERE resolution = '1m'")
        .execute(&app.state.db)
        .await
        .unwrap();
    let (points, meta) = read_cpu_chart(&app.state.db, base + 20, base + 800, 300)
        .await
        .unwrap();
    assert!(!points.is_empty());
    assert_eq!(meta.bucket_seconds, 300);
    assert!(meta.degraded);
    assert!(meta.sources.iter().all(|s| s.resolution == "5m"));
    let (points, meta) = read_cpu_chart(&app.state.db, base - 10000, base - 9000, 300)
        .await
        .unwrap();
    assert!(points.is_empty());
    assert!(meta.degraded);
    assert!(!meta.unavailable.is_empty());
}

#[tokio::test]
async fn cpu_chart_rest_and_batch_expose_per_resource_metadata() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let base = chrono::Utc::now().timestamp() - 1000;
    samples(&app, base, base + 200).await;
    for route in [
        format!("/metrics/cpu?start={base}&end={}&max_points=16", base + 200),
        format!(
            "/metrics/batch?resources=cpu,memory&start={base}&end={}&max_points=16",
            base + 200
        ),
    ] {
        let (status, body) = app.request("GET", &route, Some(&token), None).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let cpu = if body.get("series").is_some() {
            &body["series"][0]
        } else {
            &body
        };
        assert_eq!(cpu["chart"]["max_points"], 16);
        assert!(cpu["points"].as_array().unwrap().len() <= 16);
    }
}

#[tokio::test]
async fn cpu_chart_week_old_short_window_ignores_expired_raw_even_before_pruning() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 300 - 20) * 300;
    samples(&app, base, base + 1800).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let age = 8 * 86400;
    sqlx::query("UPDATE metrics_cpu SET timestamp = timestamp - ?")
        .bind(age)
        .execute(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE rollup_state SET processed_from = processed_from - ?, last_bucket_ts = last_bucket_ts - ?")
        .bind(age).bind(age).execute(&app.state.db).await.unwrap();
    let (points, meta) = read_cpu_chart(&app.state.db, base - age + 21, base - age + 921, 300)
        .await
        .unwrap();
    assert!(!points.is_empty());
    assert_eq!(meta.bucket_seconds, 300);
    assert!(meta.degraded);
    assert!(meta.sources.iter().all(|s| s.resolution == "5m"));
}

#[tokio::test]
async fn cpu_chart_retention_boundary_never_reads_a_partial_source_bucket() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 300 - 20) * 300;
    samples(&app, base, base + 1800).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    sqlx::query("DELETE FROM metrics_cpu WHERE resolution = 'raw'")
        .execute(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE rollup_state SET processed_from = NULL WHERE resolution = '1m'")
        .execute(&app.state.db)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE retention_policy SET keep_seconds = ? WHERE resource = 'cpu' AND resolution = '5m'",
    )
    .bind(now - base - 17)
    .execute(&app.state.db)
    .await
    .unwrap();
    let (_, meta) = read_cpu_chart(&app.state.db, base, base + 900, 300)
        .await
        .unwrap();
    assert_eq!(meta.sources[0].from, base + 300);
    assert_eq!(meta.unavailable[0].start, base);
    assert_eq!(meta.unavailable[0].end, base + 300);
}

async fn host_samples(app: &TestApp, start: i64, end: i64) {
    let mut tx = app.state.db.begin().await.unwrap();
    for ts in (start..end).step_by(2) {
        let used = 1000 + ts % 97;
        sqlx::query(
            "INSERT INTO metrics_memory (resolution, timestamp, total_bytes, used_bytes,
            available_bytes, cached_bytes, swap_used_bytes) VALUES ('raw', ?, 4000, ?, ?, 10, 0)",
        )
        .bind(ts)
        .bind(used)
        .bind(4000 - used)
        .execute(&mut *tx)
        .await
        .unwrap();
        for (mount, total) in [("/", 1000), ("/data", 5000)] {
            sqlx::query(
                "INSERT INTO metrics_disk (resolution, timestamp, mount_point, total_bytes,
                used_bytes, available_bytes) VALUES ('raw', ?, ?, ?, ?, 0)",
            )
            .bind(ts)
            .bind(mount)
            .bind(total)
            .bind(ts % 89)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        for iface in ["eth0", "eth1"] {
            sqlx::query(
                "INSERT INTO metrics_network (resolution, timestamp, interface_name,
                rx_bytes_per_sec, tx_bytes_per_sec, rx_packets_per_sec, tx_packets_per_sec)
                VALUES ('raw', ?, ?, ?, 1, 1, 1)",
            )
            .bind(ts)
            .bind(iface)
            .bind(ts % 53)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO metrics_network_total (resolution, timestamp, rx_bytes_per_sec,
            tx_bytes_per_sec, rx_packets_per_sec, tx_packets_per_sec, errors_in_per_sec,
            errors_out_per_sec) VALUES ('raw', ?, ?, 2, 2, 2, 0, 0)",
        )
        .bind(ts)
        .bind(2 * (ts % 53))
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn host_charts_bucket_every_key_and_derive_raw_percentages() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 600 - 20) * 600;
    host_samples(&app, base, base + 6000).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();

    let (points, meta) = read_memory_chart(&app.state.db, base, base + 6000, 50)
        .await
        .unwrap();
    assert!(meta.bucket_seconds > 0 && points.len() <= 50);
    let valid: i64 = points
        .iter()
        .map(|p| p.statistics.as_ref().unwrap()["used_percent"].valid_count)
        .sum();
    assert_eq!(valid, 3000);
    for p in &points {
        let s = &p.statistics.as_ref().unwrap()["used_percent"];
        let mean = p.used_percent.unwrap();
        assert!(s.min.unwrap() <= mean && mean <= s.max.unwrap());
        assert!((mean - s.sum / s.valid_count as f64).abs() < 1e-9);
    }

    let (points, meta) = read_disk_chart(&app.state.db, base, base + 6000, 50)
        .await
        .unwrap();
    let buckets = (meta.aligned.end - meta.aligned.start) / meta.bucket_seconds;
    assert_eq!(points.len() as i64, 2 * buckets);
    for p in points.iter().filter(|p| p.mount_point == "/data") {
        let s = &p.statistics.as_ref().unwrap()["used_percent"];
        assert!(s.max.unwrap() <= 100.0 * 88.0 / 5000.0 + 1e-9);
    }

    let (rows, totals, meta) = read_network_chart(&app.state.db, base, base + 6000, 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2 * totals.len());
    for t in &totals {
        assert_eq!(t.bucket_seconds, meta.bucket_seconds);
        let pair: Vec<_> = rows.iter().filter(|r| r.timestamp == t.timestamp).collect();
        assert_eq!(pair.len(), 2);
        let rx = &t.statistics.as_ref().unwrap()["rx_bytes_per_sec"];
        let sum: f64 = pair
            .iter()
            .map(|r| r.statistics.as_ref().unwrap()["rx_bytes_per_sec"].sum)
            .sum();
        assert!((rx.sum - sum).abs() < 1e-6);
    }
}

#[tokio::test]
async fn host_charts_serve_short_windows_as_raw_samples() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    host_samples(&app, now - 200, now).await;
    let (points, meta) = read_disk_chart(&app.state.db, now - 200, now + 5, 300)
        .await
        .unwrap();
    assert_eq!(meta.bucket_seconds, 0);
    assert_eq!(points.len(), 200);
    let root = points.iter().find(|p| p.mount_point == "/").unwrap();
    assert_eq!(
        root.used_percent,
        Some(100.0 * root.used_bytes as f64 / root.total_bytes as f64)
    );
    let (_, meta) = read_memory_chart(&app.state.db, now - 200, now, 300)
        .await
        .unwrap();
    assert_eq!(meta.bucket_seconds, 0);
}

#[tokio::test]
async fn host_charts_exposed_by_rest_and_batch() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let base = chrono::Utc::now().timestamp() - 1000;
    host_samples(&app, base, base + 200).await;
    let q = format!("start={base}&end={}&max_points=16", base + 200);
    for route in ["memory", "disk", "network"] {
        let (status, body) = app
            .request("GET", &format!("/metrics/{route}?{q}"), Some(&token), None)
            .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(body["chart"]["max_points"], 16, "{route}");
    }
    let (_, body) = app
        .request(
            "GET",
            &format!("/metrics/batch?resources=memory,disk,network&{q}"),
            Some(&token),
            None,
        )
        .await;
    for s in body["series"].as_array().unwrap() {
        assert!(s["chart"]["bucket_seconds"].as_i64().unwrap() > 0, "{s}");
    }
    let (_, body) = app
        .request(
            "GET",
            &format!("/metrics/memory?{q}&resolution=raw"),
            Some(&token),
            None,
        )
        .await;
    assert!(body.get("chart").is_none());
    assert_eq!(body["resolution"], "raw");
}
