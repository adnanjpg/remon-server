//! Freshness of the resolver's in-memory snapshot path.
//!
//! `resolve_with_state` answers hot namespaces from `stats_latest` instead of
//! the DB. Nothing clears that snapshot, so its age has to be checked or a
//! stalled collector keeps every rule evaluating one frozen tick.

use std::sync::Arc;

use super::TestApp;
use crate::models::stats::{AllStats, CpuStats, LoadAverage, MemoryStats};
use crate::services::alerting::expression::MetricRef;
use crate::services::alerting::resolver;

fn cpu_metric() -> MetricRef {
    MetricRef {
        namespace: "cpu".to_string(),
        field: "usage_percent".to_string(),
        labels: Default::default(),
    }
}

/// A snapshot stamped `age_secs` ago, reading `usage_percent`.
async fn set_snapshot(app: &TestApp, age_secs: i64, usage_percent: f64) {
    let ts = chrono::Utc::now().timestamp() - age_secs;
    let snap = AllStats {
        cpu: Arc::new(CpuStats {
            usage_percent,
            per_core: vec![],
            load_avg: LoadAverage {
                one: 0.0,
                five: 0.0,
                fifteen: 0.0,
            },
            timestamp: ts,
            steal_percent: None,
            iowait_percent: None,
            guest_percent: None,
            user_percent: None,
            system_percent: None,
            context_switches_per_sec: None,
            process_forks_per_sec: None,
        }),
        memory: Arc::new(MemoryStats {
            total_bytes: 0,
            used_bytes: 0,
            available_bytes: 0,
            cached_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            timestamp: ts,
            page_faults_minor_per_sec: None,
            page_faults_major_per_sec: None,
            swap_in_pages_per_sec: None,
            swap_out_pages_per_sec: None,
        }),
        disks: Arc::new(vec![]),
        network: Arc::new(vec![]),
        pressure: None,
        components: None,
    };
    *app.state.stats_latest.write().await = Some(snap);
}

/// One raw `metrics_cpu` row at `now`, so the DB path has a distinguishable
/// answer.
async fn seed_cpu_row(app: &TestApp, usage_percent: f64) {
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, ?, 0, 0, 0)",
    )
    .bind(chrono::Utc::now().timestamp())
    .bind(usage_percent)
    .execute(&app.state.db)
    .await
    .expect("seed cpu row");
}

#[tokio::test]
async fn a_current_snapshot_answers_without_touching_the_database() {
    let app = TestApp::spawn().await;
    seed_cpu_row(&app, 10.0).await;
    set_snapshot(&app, 0, 90.0).await;

    let samples = resolver::resolve_with_state(&app.state, &cpu_metric())
        .await
        .expect("resolve");

    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].value, 90.0);
}

#[tokio::test]
async fn a_stale_snapshot_falls_through_to_the_database() {
    let app = TestApp::spawn().await;
    seed_cpu_row(&app, 10.0).await;
    // Past five stats intervals at the default cadence, and past the floor.
    set_snapshot(&app, 3600, 90.0).await;

    let samples = resolver::resolve_with_state(&app.state, &cpu_metric())
        .await
        .expect("resolve");

    assert_eq!(samples.len(), 1);
    assert_eq!(
        samples[0].value, 10.0,
        "a stale snapshot must not answer for a live rule"
    );
}

/// With nothing in the table either, a stale snapshot resolves to no samples —
/// which is what lets the evaluator's prune path see the target as gone.
#[tokio::test]
async fn a_stale_snapshot_with_an_empty_table_resolves_to_nothing() {
    let app = TestApp::spawn().await;
    set_snapshot(&app, 3600, 90.0).await;

    let samples = resolver::resolve_with_state(&app.state, &cpu_metric())
        .await
        .expect("resolve");

    assert!(samples.is_empty());
}
