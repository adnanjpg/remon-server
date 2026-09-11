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
    assert_eq!(
        samples, 31,
        "sample_count must carry the total, not the count"
    );

    // The second child has no steal_percent at all. Counting it in the
    // denominator would dilute a field it never carried.
    let steal = steal.expect("steal_percent should survive one NULL child");
    assert!(
        (steal - 10.0).abs() < 1e-9,
        "steal_percent {steal} was diluted by a child that carried none"
    );
}

#[tokio::test]
async fn cpu_statistics_survive_all_tiers_nulls_rounding_and_retries() {
    let app = TestApp::spawn().await;
    let hour = (chrono::Utc::now().timestamp() / 3600 - 2) * 3600;
    set_cursor(&app, "cpu", "1m", hour - 60).await;
    set_cursor(&app, "cpu", "5m", hour - 300).await;
    set_cursor(&app, "cpu", "1h", hour - 3600).await;
    for (offset, usage, steal, ctxt) in [
        (0, 10.0, Some(100.0), 0),
        (2, 100.0, None, 1),
        (60, 10.0, Some(0.0), 1),
        (62, 10.0, Some(0.0), 2),
    ] {
        sqlx::query("INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m, steal_percent, context_switches_per_sec) VALUES ('raw', ?, ?, 0, 0, 0, ?, ?)")
            .bind(hour + offset).bind(usage).bind(steal).bind(ctxt)
            .execute(&app.state.db).await.unwrap();
    }
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let repo = crate::storage::repositories::MetricsRepository::new(app.state.db.clone());
    for resolution in ["5m", "1h"] {
        let rows = repo
            .read_cpu(resolution, hour, hour + 3599, 5000)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.usage_percent, 32.5);
        assert!((row.steal_percent.unwrap() - 100.0 / 3.0).abs() < 1e-9);
        assert_eq!(row.context_switches_per_sec, Some(1));
        let stats = row.statistics.as_ref().unwrap();
        assert_eq!(stats["usage_percent"].min, Some(10.0));
        assert_eq!(stats["usage_percent"].max, Some(100.0));
        assert_eq!(stats["usage_percent"].sum, 130.0);
        assert_eq!(stats["usage_percent"].valid_count, 4);
        assert_eq!(stats["steal_percent"].valid_count, 3);
        assert_eq!(stats["context_switches_per_sec"].sum, 4.0);
        assert_eq!(stats["guest_percent"].valid_count, 0);
        assert_eq!(stats["guest_percent"].min, None);
        assert_eq!(stats["guest_percent"].sum, 0.0);
    }
    // Retry a completed bucket: replacement must not count samples twice.
    set_cursor(&app, "cpu", "1h", hour - 3600).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let retried = repo.read_cpu("1h", hour, hour + 3599, 10).await.unwrap();
    assert_eq!(
        retried[0].statistics.as_ref().unwrap()["usage_percent"].valid_count,
        4
    );
    let raw = repo.read_cpu("raw", hour, hour + 2, 10).await.unwrap();
    assert_eq!(raw[0].bucket_seconds, 0);
    assert_eq!(
        raw[0].statistics.as_ref().unwrap()["context_switches_per_sec"].min,
        Some(0.0)
    );

    let token = app.pair_and_login().await;
    let query = format!("start={hour}&end={}&resolution=1h", hour + 3599);
    let (status, single) = app
        .request("GET", &format!("/metrics/cpu?{query}"), Some(&token), None)
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{single}");
    let (status, batch) = app
        .request(
            "GET",
            &format!("/metrics/batch?resources=cpu&{query}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{batch}");
    assert_eq!(single["points"], batch["series"][0]["points"]);
    assert_eq!(
        single["points"][0]["statistics"]["usage_percent"]["max"],
        100.0
    );
    assert_eq!(single["points"][0]["bucket_seconds"], 3600);
}

#[tokio::test]
async fn cpu_statistics_do_not_fabricate_extrema_for_mixed_legacy_buckets() {
    let app = TestApp::spawn().await;
    let bucket = (chrono::Utc::now().timestamp() / 300 - 3) * 300;
    set_cursor(&app, "cpu", "1m", bucket - 60).await;
    set_cursor(&app, "cpu", "5m", bucket - 300).await;
    sqlx::query("INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m) VALUES ('raw', ?, 100, 0, 0, 0)")
        .bind(bucket).execute(&app.state.db).await.unwrap();
    child_cpu(&app, bucket + 60, 10.0, 30, None).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let repo = crate::storage::repositories::MetricsRepository::new(app.state.db.clone());
    let rows = repo.read_cpu("5m", bucket, bucket + 299, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].statistics.is_none());
    assert!((rows[0].usage_percent - 400.0 / 31.0).abs() < 1e-9);
    let legacy = repo
        .read_cpu("1m", bucket + 60, bucket + 60, 10)
        .await
        .unwrap();
    assert!(legacy[0].statistics.is_none());
}

#[tokio::test]
async fn gauges_preserve_ratios_nulls_and_simultaneous_network_peaks_end_to_end() {
    use crate::models::stats::{CpuStats, DiskStats, MemoryStats, NetworkStats};
    use crate::services::alerting::{expression::MetricRef, resolver::history_summary};
    use crate::storage::repositories::MetricsRepository;
    use serde_json::json;
    let app = TestApp::spawn().await;
    let hour = (chrono::Utc::now().timestamp() / 3600 - 2) * 3600;
    for resource in ["memory", "disk", "network"] {
        set_cursor(&app, resource, "1m", hour - 60).await;
        set_cursor(&app, resource, "5m", hour - 300).await;
        set_cursor(&app, resource, "1h", hour - 3600).await;
    }
    let repo = MetricsRepository::new(app.state.db.clone());
    for (i, offset) in [0, 2, 60, 62].into_iter().enumerate() {
        let ts = hour + offset;
        let cpu:CpuStats=serde_json::from_value(json!({"usage_percent":10,"per_core":[],"load_avg":{"one":0,"five":0,"fifteen":0},"timestamp":ts})).unwrap();
        let (total, used, available) = if i < 2 { (100, 50, 50) } else { (200, 60, 140) };
        let memory:MemoryStats=serde_json::from_value(json!({"total_bytes":total,"used_bytes":used,"available_bytes":available,"cached_bytes":0,"swap_total_bytes":0,"swap_used_bytes":0,"timestamp":ts,"page_faults_major_per_sec":if i==0 {Some(100)} else if i==1 {None} else {Some(0)}})).unwrap();
        let disk:DiskStats=serde_json::from_value(json!({"mount_point":"/data","total_bytes":total,"used_bytes":used,"available_bytes":available,"read_bytes_per_sec":if i==0 {1000}else{10},"write_bytes_per_sec":0,"timestamp":ts,"read_iops":if i==0 {Some(100)}else if i==1 {None}else{Some(0)}})).unwrap();
        let network:Vec<NetworkStats>=[("eth0",if i%2==0 {100}else{0}),("eth1",if i%2==0 {0}else{100}),("wg0",999)].into_iter().map(|(name,rx)| serde_json::from_value(json!({"interface":name,"rx_bytes_per_sec":rx,"tx_bytes_per_sec":0,"rx_packets_per_sec":0,"tx_packets_per_sec":0,"errors_in_per_sec":0,"errors_out_per_sec":0,"rx_bytes_total":0,"tx_bytes_total":0,"timestamp":ts})).unwrap()).collect();
        repo.insert_raw_tick(&cpu, &memory, &[disk], &network, None, None)
            .await
            .unwrap();
    }
    crate::services::rollup::run_once(&app.state).await.unwrap();
    for resolution in ["5m", "1h"] {
        let memory = repo
            .read_memory(resolution, hour, hour + 3599, 5000)
            .await
            .unwrap();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].used_percent, Some(40.0));
        let stat = &memory[0].statistics.as_ref().unwrap()["used_percent"];
        assert_eq!(
            (stat.min, stat.max, stat.valid_count),
            (Some(30.0), Some(50.0), 4)
        );
        assert_eq!(
            memory[0].statistics.as_ref().unwrap()["page_faults_major_per_sec"].valid_count,
            3
        );
        let disk = repo
            .read_disk(resolution, hour, hour + 3599, 5000)
            .await
            .unwrap();
        assert_eq!(disk[0].used_percent, Some(40.0));
        assert_eq!(
            disk[0].statistics.as_ref().unwrap()["read_bytes_per_sec"].max,
            Some(1000.0)
        );
        let total = repo
            .read_network_totals(resolution, hour, hour + 3599, 5000)
            .await
            .unwrap();
        assert_eq!(total.len(), 1);
        assert_eq!(total[0].rx_bytes_per_sec, 100);
        assert_eq!(
            total[0].statistics.as_ref().unwrap()["rx_bytes_per_sec"].max,
            Some(100.0)
        );
        let interfaces = repo
            .read_network(resolution, hour, hour + 3599, 5000)
            .await
            .unwrap();
        assert_eq!(interfaces.len(), 3);
        let maxima: f64 = interfaces
            .iter()
            .filter(|r| r.interface_name != "wg0")
            .map(|r| {
                r.statistics.as_ref().unwrap()["rx_bytes_per_sec"]
                    .max
                    .unwrap()
            })
            .sum();
        assert_eq!(
            maxima, 200.0,
            "sum of independent maxima differs from simultaneous total"
        );
    }
    for (namespace, field, expected_min, expected_max, expected_avg, count) in [
        ("memory", "used_percent", 30.0, 50.0, 40.0, 4),
        (
            "memory",
            "page_faults_major_per_sec",
            0.0,
            100.0,
            100.0 / 3.0,
            3,
        ),
        ("disk", "used_percent", 30.0, 50.0, 40.0, 4),
        ("disk", "read_iops", 0.0, 100.0, 100.0 / 3.0, 3),
        ("network_total", "rx_bytes_per_sec", 100.0, 100.0, 100.0, 4),
    ] {
        let metric = MetricRef {
            namespace: namespace.into(),
            field: field.into(),
            labels: Default::default(),
        };
        let summary = history_summary(&app.state, &metric, "1h", hour + 10, hour + 3599)
            .await
            .unwrap();
        assert_eq!(summary.len(), 1);
        assert!(summary[0].observed_statistics);
        assert_eq!(summary[0].min, expected_min);
        assert_eq!(summary[0].max, expected_max);
        assert!((summary[0].avg - expected_avg).abs() < 1e-9);
        assert_eq!(summary[0].count, count);
    }
    let token = app.pair_and_login().await;
    let query = format!("start={hour}&end={}&resolution=1h", hour + 3599);
    let (status, batch) = app
        .request(
            "GET",
            &format!("/metrics/batch?resources=memory,disk,network&{query}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{batch}");
    for resource in ["memory", "disk", "network"] {
        let (status, single) = app
            .request(
                "GET",
                &format!("/metrics/{resource}?{query}"),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{single}");
        let batched = batch["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["resource"] == resource)
            .unwrap();
        assert_eq!(single["points"], batched["points"]);
        if resource == "network" {
            assert_eq!(single["totals"], batched["totals"]);
        }
    }
    repo.delete_older_than("network", "raw", hour + 3600)
        .await
        .unwrap();
    assert!(
        repo.read_network_totals("raw", hour, hour + 3599, 5000)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.read_network("raw", hour, hour + 3599, 5000)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.read_memory("raw", hour, hour + 3599, 5000)
            .await
            .unwrap()
            .len(),
        4
    );
}
