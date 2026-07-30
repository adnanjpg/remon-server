//! What the rollup does to each column, as opposed to which buckets it visits.
//!
//! Picking the wrong aggregate is the quietest failure this task has: the row
//! is written, the count is right, the chart draws, and the number is one the
//! host never reported.

use super::TestApp;

async fn raw_docker_sample(app: &TestApp, ts: i64, cpu: f64, mem: i64, rx: i64, block_read: i64) {
    sqlx::query(
        "INSERT INTO metrics_docker
             (resolution, timestamp, container_id, cpu_percent, memory_used_bytes,
              memory_limit_bytes, network_rx_bytes, network_tx_bytes,
              block_read_bytes, block_write_bytes, pids)
         VALUES ('raw', ?, 'web', ?, ?, 4096, ?, ?, ?, ?, 3)",
    )
    .bind(ts)
    .bind(cpu)
    .bind(mem)
    .bind(rx)
    .bind(rx * 2)
    .bind(block_read)
    .bind(block_read * 2)
    .execute(&app.state.db)
    .await
    .expect("raw docker sample");
}

/// Docker's byte columns are counters, cumulative since the container started —
/// the collector stores `networks.*.rx_bytes` and the blkio totals as read.
/// Averaging one produces a value between the bucket's first and last reading,
/// which the container never reported and every coarser tier then averages
/// again. The reading that represents the bucket is its maximum.
#[tokio::test]
async fn docker_counters_roll_up_by_max_and_gauges_by_average() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    // A closed bucket, with the cursor parked immediately before it.
    let bucket = (now / 60 - 3) * 60;
    sqlx::query(
        "INSERT INTO rollup_state (resource, resolution, last_bucket_ts, last_run_at)
         VALUES ('docker', '1m', ?, 0)
         ON CONFLICT(resource, resolution) DO UPDATE SET last_bucket_ts = excluded.last_bucket_ts",
    )
    .bind(bucket - 60)
    .execute(&app.state.db)
    .await
    .expect("set cursor");

    raw_docker_sample(&app, bucket + 10, 10.0, 1_000, 100, 5).await;
    raw_docker_sample(&app, bucket + 40, 30.0, 3_000, 300, 25).await;

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let row: (f64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT cpu_percent, memory_used_bytes, network_rx_bytes, network_tx_bytes,
                block_read_bytes, block_write_bytes
           FROM metrics_docker
          WHERE resolution = '1m' AND timestamp = ? AND container_id = 'web'",
    )
    .bind(bucket)
    .fetch_one(&app.state.db)
    .await
    .expect("rolled-up bucket");

    assert_eq!(
        row.2, 300,
        "network_rx_bytes averaged to {} instead of the bucket's end value",
        row.2
    );
    assert_eq!(row.3, 600, "network_tx_bytes is a counter too");
    assert_eq!(row.4, 25, "block_read_bytes is a counter too");
    assert_eq!(row.5, 50, "block_write_bytes is a counter too");

    // The gauges keep averaging — a container's memory over the minute is a
    // mean, not a peak, and cpu_percent is already a rate when it is stored.
    assert!(
        (row.0 - 20.0).abs() < f64::EPSILON,
        "cpu_percent should be the mean, got {}",
        row.0
    );
    assert_eq!(row.1, 2_000, "memory_used_bytes should be the mean");
}

async fn set_cursor(app: &TestApp, resource: &str, resolution: &str, ts: i64) {
    sqlx::query(
        "INSERT INTO rollup_state (resource, resolution, last_bucket_ts, last_run_at)
         VALUES (?, ?, ?, 0)
         ON CONFLICT(resource, resolution) DO UPDATE SET last_bucket_ts = excluded.last_bucket_ts",
    )
    .bind(resource)
    .bind(resolution)
    .bind(ts)
    .execute(&app.state.db)
    .await
    .expect("set cursor");
}

/// One already-rolled 1m row, standing for `samples` raw samples.
async fn child_cpu(app: &TestApp, ts: i64, usage: f64, samples: i64, steal: Option<f64>) {
    sqlx::query(
        "INSERT INTO metrics_cpu
             (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m,
              steal_percent, sample_count)
         VALUES ('1m', ?, ?, 1.0, 1.0, 1.0, ?, ?)",
    )
    .bind(ts)
    .bind(usage)
    .bind(steal)
    .bind(samples)
    .execute(&app.state.db)
    .await
    .expect("child bucket");
}

/// A child that covered 30 raw samples and one that covered a single sample are
/// not equal evidence. `AVG` treats them as equal, and chaining raw→1m→5m→1h
/// compounds that at every tier — the error is invisible in the output because
/// the row is written and the chart draws.
#[tokio::test]
async fn a_chained_tier_weights_children_by_the_samples_behind_them() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    let bucket = (now / 300 - 3) * 300;

    // The 1m tier has to be settled past the 5m bucket, or the 5m tier
    // correctly declines to fold a window its parent has not finished.
    set_cursor(&app, "cpu", "1m", bucket + 300).await;
    set_cursor(&app, "cpu", "5m", bucket - 300).await;

    // A full minute at 10%, then a minute the collector barely sampled at 100%.
    child_cpu(&app, bucket, 10.0, 30, Some(10.0)).await;
    child_cpu(&app, bucket + 60, 100.0, 1, None).await;

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let (usage, steal, samples): (f64, Option<f64>, i64) = sqlx::query_as(
        "SELECT usage_percent, steal_percent, sample_count
           FROM metrics_cpu WHERE resolution = '5m' AND timestamp = ?",
    )
    .bind(bucket)
    .fetch_one(&app.state.db)
    .await
    .expect("rolled-up bucket");

    // Unweighted this reads 55.0 — the stray sample outvoting a full minute.
    let expected = (10.0 * 30.0 + 100.0) / 31.0;
    assert!(
        (usage - expected).abs() < 1e-9,
        "usage_percent {usage} is not the weighted mean {expected}"
    );
    assert_eq!(samples, 31, "sample_count must carry the total, not the count");

    // The second child has no steal_percent at all. Counting it in the
    // denominator would dilute a field it never carried.
    let steal = steal.expect("steal_percent should survive one NULL child");
    assert!(
        (steal - 10.0).abs() < 1e-9,
        "steal_percent {steal} was diluted by a child that carried none"
    );
}
