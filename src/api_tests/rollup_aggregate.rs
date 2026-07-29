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
