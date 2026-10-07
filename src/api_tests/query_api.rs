//! `POST /query` — the data endpoint behind composed screens.

use super::TestApp;
use axum::http::StatusCode;
use serde_json::{Value, json};

async fn seed_cpu(app: &TestApp, ts: i64, usage: f64) {
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, ?, 0, 0, 0)",
    )
    .bind(ts)
    .bind(usage)
    .execute(&app.state.db)
    .await
    .expect("seed cpu");
}

async fn seed_process(
    app: &TestApp,
    resolution: &str,
    ts: i64,
    name: &str,
    cpu: f64,
    n: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO metrics_process
           (resolution, timestamp, name, pid_count, cpu_percent, memory_bytes, sample_count)
         VALUES (?, ?, ?, 1, ?, 1000, ?)",
    )
    .bind(resolution)
    .bind(ts)
    .bind(name)
    .bind(cpu)
    .bind(n)
    .execute(&app.state.db)
    .await
    .expect("seed process");
}

async fn query(app: &TestApp, token: &str, body: Value) -> (StatusCode, Value) {
    app.request("POST", "/query", Some(token), Some(body)).await
}

fn values(series: &Value) -> Vec<f64> {
    series["points"]
        .as_array()
        .expect("points")
        .iter()
        .filter_map(|p| p[1].as_f64())
        .collect()
}

#[tokio::test]
async fn query_requires_auth() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .request("POST", "/query", None, Some(json!({ "queries": [] })))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn series_reads_a_host_field_with_its_unit() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    for (ago, usage) in [(120, 10.0), (60, 50.0), (10, 90.0)] {
        seed_cpu(&app, now - ago, usage).await;
    }

    let (status, body) = query(
        &app,
        &token,
        json!({ "range": "30m", "queries": [
            { "id": "cpu", "namespace": "cpu", "field": "usage_percent" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["results"][0];
    assert_eq!(r["id"], "cpu");
    assert_eq!(r["unit"], "percent");
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["series"].as_array().unwrap().len(), 1);
    assert_eq!(r["series"][0]["labels"], json!({}));
    assert_eq!(values(&r["series"][0]), vec![10.0, 50.0, 90.0]);
    assert_eq!(
        body["end"].as_i64().unwrap() - body["start"].as_i64().unwrap(),
        1800
    );
}

#[tokio::test]
async fn keyed_series_rank_by_average_and_filter_by_label() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    for (name, cpu) in [("quiet", 5.0), ("busy", 50.0), ("medium", 20.0)] {
        seed_process(&app, "raw", now - 30, name, cpu, None).await;
        seed_process(&app, "raw", now - 20, name, cpu, None).await;
    }

    let (status, body) = query(
        &app,
        &token,
        json!({ "range": "1h", "queries": [
            { "id": "top", "namespace": "process", "field": "cpu_percent", "limit": 2 },
            { "id": "one", "namespace": "process", "field": "cpu_percent", "labels": { "name": "quiet" } },
            { "id": "bad", "namespace": "process", "field": "cpu_percent", "labels": { "pid": "1" } }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let top = body["results"][0]["series"].as_array().unwrap();
    let names: Vec<&str> = top
        .iter()
        .map(|s| s["labels"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["busy", "medium"]);

    let one = body["results"][1]["series"].as_array().unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0]["labels"]["name"], "quiet");

    // One bad query fails alone; the others above still answered.
    let bad = &body["results"][2];
    assert!(bad["error"].as_str().unwrap().contains("'name'"), "{bad}");
    assert_eq!(bad["series"], json!([]));
}

#[tokio::test]
async fn rollup_tiers_without_summaries_use_the_sample_weighted_mean() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    // Two 1m rows inside one 12h output bucket, standing for 1 and 3 samples.
    let bucket = (now - 2 * 86_400) / 43_200 * 43_200;
    seed_process(&app, "1m", bucket + 60, "nginx", 10.0, Some(1)).await;
    seed_process(&app, "1m", bucket + 120, "nginx", 40.0, Some(3)).await;
    sqlx::query(
        "INSERT INTO rollup_state (resource, resolution, processed_from, last_bucket_ts)
         VALUES ('process', '1m', ?, ?)",
    )
    .bind(now - 6 * 86_400)
    .bind(now - 120)
    .execute(&app.state.db)
    .await
    .expect("seed rollup state");

    let (status, body) = query(
        &app,
        &token,
        json!({ "range": "7d", "max_points": 16, "queries": [
            { "id": "p", "namespace": "process", "field": "cpu_percent", "labels": { "name": "nginx" } }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["results"][0];
    assert_eq!(r["chart"]["bucket_seconds"], 43_200, "{r}");
    // (10*1 + 40*3) / 4, not the plain average of the two rows.
    assert_eq!(values(&r["series"][0]), vec![32.5]);
}

#[tokio::test]
async fn latest_reads_through_the_alert_resolver() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    seed_cpu(&app, chrono::Utc::now().timestamp(), 64.0).await;

    let (status, body) = query(
        &app,
        &token,
        json!({ "queries": [
            { "id": "now", "namespace": "cpu", "field": "usage_percent", "mode": "latest" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(values(&body["results"][0]["series"][0]), vec![64.0]);
}

#[tokio::test]
async fn unknown_names_say_what_is_valid() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (status, body) = query(
        &app,
        &token,
        json!({ "queries": [
            { "id": "a", "namespace": "cpu", "field": "nope" },
            { "id": "b", "namespace": "gpu", "field": "usage" }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let a = body["results"][0]["error"].as_str().unwrap();
    assert!(a.contains("usage_percent"), "{a}");
    let b = body["results"][1]["error"].as_str().unwrap();
    assert!(b.contains("process"), "{b}");
}

#[tokio::test]
async fn malformed_requests_are_rejected_whole() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let q = json!({ "id": "a", "namespace": "cpu", "field": "usage_percent" });

    for body in [
        json!({ "queries": [] }),
        json!({ "range": "2h", "queries": [q] }),
        json!({ "range": "1h", "start": 1, "queries": [q] }),
        json!({ "queries": [q, q] }),
    ] {
        let (status, resp) = query(&app, &token, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {resp}");
    }
}

#[tokio::test]
async fn a_burst_does_not_outrank_steady_load() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    // `db` holds 20% for ten ticks; `build` shows up in one at 150%.
    for i in 0..10 {
        seed_process(&app, "raw", now - 100 + i * 2, "db", 20.0, None).await;
    }
    seed_process(&app, "raw", now - 100, "build", 150.0, None).await;

    let (status, body) = query(
        &app,
        &token,
        json!({ "range": "1h", "queries": [
            { "id": "top", "namespace": "process", "field": "cpu_percent", "limit": 1 },
            { "id": "sum", "namespace": "process", "field": "cpu_percent", "mode": "summary" },
            { "id": "one", "namespace": "process", "field": "cpu_percent", "mode": "summary",
              "labels": { "name": "build" } }
        ] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let top = &body["results"][0]["series"];
    assert_eq!(top.as_array().unwrap().len(), 1);
    assert_eq!(top[0]["labels"]["name"], "db", "{top}");
    assert!(
        top[0].get("stats").is_none(),
        "series mode draws points only"
    );

    let sum = body["results"][1]["series"].as_array().unwrap();
    let names: Vec<&str> = sum
        .iter()
        .map(|s| s["labels"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["db", "build"]);
    assert_eq!(sum[0]["points"], json!([]));
    assert_eq!(
        sum[0]["stats"],
        json!({ "avg": 20.0, "min": 20.0, "max": 20.0 })
    );
    // One tick of ten at 150 averages 15 over the window, not 150.
    assert_eq!(
        sum[1]["stats"],
        json!({ "avg": 15.0, "min": 150.0, "max": 150.0 })
    );

    // A labelled summary still averages over the namespace's ticks.
    let one = body["results"][2]["series"].as_array().unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0]["stats"]["avg"], 15.0);
}
