//! Row budget on the keyed history reads.
//!
//! `limit` bounds distinct timestamps, not rows, so a keyed table answers with
//! `limit × keys`. The endpoint cap counts timestamps and so cannot see the
//! multiplier; these cover the budget that does.

use super::TestApp;
use crate::storage::repositories::MetricsRepository;

/// Write `cores` rows at each of `ticks` timestamps ending at `end`.
async fn seed_cores(app: &TestApp, end: i64, ticks: i64, cores: i64) {
    for t in 0..ticks {
        let ts = end - t * 2;
        let mut qb = sqlx::QueryBuilder::new(
            "INSERT INTO metrics_cpu_cores (timestamp, core_index, usage_percent, freq_mhz) ",
        );
        qb.push_values(0..cores, |mut b, core| {
            b.push_bind(ts).push_bind(core).push_bind(25.0).push_bind(0);
        });
        qb.build().execute(&app.state.db).await.expect("seed cores");
    }
}

/// A key count an ordinary host carries must not lose points. The budget is
/// there for the pathological case and would be a regression if it bit here.
#[tokio::test]
async fn an_ordinary_key_count_still_gets_every_point_it_asked_for() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    seed_cores(&app, now, 200, 8).await;

    let rows = MetricsRepository::new(app.state.db.clone())
        .read_cpu_cores(now - 100_000, now, 5000)
        .await
        .expect("read cores");

    let stamps: std::collections::HashSet<i64> = rows.iter().map(|r| r.0).collect();
    assert_eq!(stamps.len(), 200, "all seeded timestamps should survive");
    assert_eq!(rows.len(), 200 * 8);
}

/// The case the budget exists for: enough keys that `limit × keys` would
/// dwarf the response. Points are given up, keys are not.
#[tokio::test]
async fn a_large_key_count_bounds_the_response_instead_of_the_timestamps() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    // 128 cores × 5000 timestamps would be 640k rows.
    seed_cores(&app, now, 600, 128).await;

    let rows = MetricsRepository::new(app.state.db.clone())
        .read_cpu_cores(now - 100_000, now, 5000)
        .await
        .expect("read cores");

    assert!(
        rows.len() <= 50_000,
        "response carried {} rows, past the budget",
        rows.len()
    );

    // Every timestamp returned must carry its whole key set — trading points
    // for completeness is the entire point of the subquery pattern, so a
    // budget that cut mid-timestamp would defeat it.
    let mut per_ts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for r in &rows {
        *per_ts.entry(r.0).or_default() += 1;
    }
    assert!(!per_ts.is_empty(), "expected some points back");
    assert!(
        per_ts.values().all(|n| *n == 128),
        "a timestamp came back partially populated"
    );
}

/// `?limit=0` reaches the budget with a bound of zero; `clamp(1, limit)` panicked on it.
#[tokio::test]
async fn a_zero_limit_yields_an_empty_page_rather_than_a_panic() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    // Enough keys to get past the `keys <= 1` shortcut and into the budget.
    seed_cores(&app, now, 10, 8).await;

    let rows = MetricsRepository::new(app.state.db.clone())
        .read_cpu_cores(now - 3600, now, 0)
        .await
        .expect("read cores");
    assert!(rows.is_empty());
}

/// The same input over the wire.
#[tokio::test]
async fn the_cores_endpoint_survives_a_zero_limit() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    seed_cores(&app, now, 10, 8).await;

    let (status, body) = app
        .request(
            "GET",
            &format!(
                "/metrics/cpu/cores?start={}&end={}&limit=0",
                now - 3600,
                now
            ),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["points"].as_array().expect("points array").len(), 0);
}

/// An empty window reports zero keys; the caller's limit has to survive that
/// rather than collapse to a single point.
#[tokio::test]
async fn an_empty_window_does_not_collapse_the_limit() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    let rows = MetricsRepository::new(app.state.db.clone())
        .read_cpu_cores(now - 3600, now, 5000)
        .await
        .expect("read cores");
    assert!(rows.is_empty());
}
