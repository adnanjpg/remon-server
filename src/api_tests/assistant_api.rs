//! `POST /assistant` — read-only operator assistant.
//!
//! These are hermetic: the test harness builds `AssistantConfig::default()`,
//! which has no `api_key`, so the endpoint short-circuits to 503 before any
//! provider call. No network is touched.

use super::TestApp;
use axum::http::StatusCode;
use serde_json::json;

/// Live end-to-end smoke test against a real provider. Ignored by default so
/// the normal suite stays hermetic and offline. Run it explicitly with a key:
///
/// ```text
/// $env:REMON__ASSISTANT__API_KEY = "AIza..."      # Google AI Studio key
/// cargo test --bin remon-server assistant_live -- --ignored --nocapture
/// ```
///
/// Override `REMON__ASSISTANT__BASE_URL` / `REMON__ASSISTANT__MODEL` to point
/// at Groq / Ollama / another OpenAI-compatible endpoint. It seeds a stats
/// tick, then asks a health question so the model must call `get_summary`
/// (and likely `read_logs`) and fold the results into a plain-language answer.
#[tokio::test]
#[ignore = "live: set REMON__ASSISTANT__API_KEY to run"]
async fn assistant_live() {
    use crate::config::AssistantConfig;
    use crate::models::stats::{
        AllStats, CoreStats, CpuStats, DiskStats, LoadAverage, MemoryStats,
    };
    use std::sync::Arc;

    let api_key = std::env::var("REMON__ASSISTANT__API_KEY").unwrap_or_default();
    assert!(
        !api_key.trim().is_empty(),
        "set REMON__ASSISTANT__API_KEY to run the live assistant test"
    );

    let cfg = AssistantConfig {
        enabled: true,
        base_url: std::env::var("REMON__ASSISTANT__BASE_URL")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/v1beta/openai".into()),
        api_key,
        model: std::env::var("REMON__ASSISTANT__MODEL")
            .unwrap_or_else(|_| "gemini-2.5-flash".into()),
        max_tokens: 2048,
        prometheus_url: std::env::var("REMON__ASSISTANT__PROMETHEUS_URL").unwrap_or_default(),
    };

    let app = TestApp::spawn_with_assistant(cfg).await;

    // Seed a stats tick so `get_summary` returns real numbers to reason over.
    let stats = AllStats {
        cpu: Arc::new(CpuStats {
            usage_percent: 91.4,
            per_core: vec![CoreStats {
                core_index: 0,
                usage_percent: 91.4,
                freq_mhz: 2400,
            }],
            load_avg: LoadAverage {
                one: 7.8,
                five: 6.1,
                fifteen: 4.0,
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
            used_bytes: 7_600_000_000,
            available_bytes: 400_000_000,
            cached_bytes: 0,
            swap_total_bytes: 2_000_000_000,
            swap_used_bytes: 1_800_000_000,
            timestamp: 1_700_000_000,
            page_faults_minor_per_sec: None,
            page_faults_major_per_sec: None,
            swap_in_pages_per_sec: None,
            swap_out_pages_per_sec: None,
        }),
        disks: Arc::new(vec![DiskStats {
            mount_point: "/".to_string(),
            total_bytes: 100,
            used_bytes: 96,
            available_bytes: 4,
            read_bytes_per_sec: 0,
            write_bytes_per_sec: 0,
            timestamp: 1_700_000_000,
            inode_used_percent: None,
            read_iops: None,
            write_iops: None,
            io_util_percent: None,
        }]),
        network: Arc::new(vec![]),
        pressure: None,
        components: None,
    };
    *app.state.stats_latest.write().await = Some(stats);

    // Seed a rising CPU series so metric_history has a real trend to report:
    // a 1-minute rollup climbing 20% → 95% over the last hour, plus a live
    // `raw` sample for the current value.
    let now = chrono::Utc::now().timestamp();
    for i in 0..12i64 {
        let usage = 20.0 + (i as f64) * 6.8;
        let ts = now - 3600 + i * 300;
        sqlx::query(
            "INSERT INTO metrics_cpu
               (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m, iowait_percent)
             VALUES ('1m', ?, ?, 4.0, 3.0, 2.0, 12.5)",
        )
        .bind(ts)
        .bind(usage)
        .execute(&app.state.db)
        .await
        .expect("seed 1m cpu row");
    }
    sqlx::query(
        "INSERT INTO metrics_cpu
           (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m, iowait_percent)
         VALUES ('raw', ?, 94.0, 8.0, 6.0, 4.0, 15.0)",
    )
    .bind(now)
    .execute(&app.state.db)
    .await
    .expect("seed raw cpu row");

    let token = app.pair_and_login().await;
    let (status, body) = app
        .request(
            "POST",
            "/assistant",
            Some(&token),
            Some(json!({
                "question": "this host looks stressed. diagnose it: check the summary and \
the top cpu/memory processes, and tell me whether cpu has been climbing over the last hour \
and what the likely cause is."
            })),
        )
        .await;

    println!("--- assistant live status: {status}");
    println!(
        "--- assistant answer:\n{}",
        body["answer"].as_str().unwrap_or(&body.to_string())
    );
    assert_eq!(status, StatusCode::OK, "live assistant call failed: {body}");
    assert!(
        body["answer"]
            .as_str()
            .is_some_and(|a| !a.trim().is_empty()),
        "expected a non-empty answer, got: {body}"
    );
}

#[tokio::test]
async fn assistant_requires_auth() {
    let app = TestApp::spawn().await;
    let (status, _) = app
        .request(
            "POST",
            "/assistant",
            None,
            Some(json!({ "question": "hi" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn assistant_rejects_empty_question() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // The empty-question guard runs before the config gate, so this is a 400
    // regardless of whether a key is set.
    let (status, body) = app
        .request(
            "POST",
            "/assistant",
            Some(&token),
            Some(json!({ "question": "   " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn assistant_unconfigured_returns_503() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Default config carries no api_key, so a well-formed question still can't
    // run: the operator gets a 503 with a client-safe hint, not a 500.
    let (status, body) = app
        .request(
            "POST",
            "/assistant",
            Some(&token),
            Some(json!({ "question": "why is my server slow?" })),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(body["error"]["code"], "SERVICE_UNAVAILABLE");
}
