use super::{TestApp, rollup_cursor::samples};
use crate::storage::repositories::read_cpu_chart;

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
