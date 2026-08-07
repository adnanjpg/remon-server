//! Windowed aggregation: reading the samples between the evaluation ticks.
//!
//! A bare metric resolves to the value at the tick, so a spike shorter than
//! `eval_interval_secs` cannot satisfy a rule that needs two consecutive
//! violating ticks, whatever its threshold.

use std::collections::BTreeMap;

use super::TestApp;
use crate::services::alerting::expression::{self, Aggregate, MetricRef, Window};
use crate::services::alerting::resolver;

fn cpu_metric() -> MetricRef {
    MetricRef {
        namespace: "cpu".to_string(),
        field: "usage_percent".to_string(),
        labels: BTreeMap::new(),
    }
}

/// Samples every 2s over the last minute: quiet, with one 2-second spike.
/// `spike_ago_secs` places the spike; everything else sits at 30%.
async fn seed_spike(app: &TestApp, spike: f64, spike_ago_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    for age in (0..60).step_by(2) {
        let age = age as i64;
        let value = if age == spike_ago_secs { spike } else { 30.0 };
        sqlx::query(
            "INSERT INTO metrics_cpu
               (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
             VALUES ('raw', ?, ?, 0, 0, 0)",
        )
        .bind(now - age)
        .bind(value)
        .execute(&app.state.db)
        .await
        .expect("seed cpu row");
    }
}

/// The whole point: the same data, the same threshold, opposite verdicts.
#[tokio::test]
async fn a_window_catches_the_spike_the_instantaneous_path_misses() {
    let app = TestApp::spawn().await;
    seed_spike(&app, 94.2, 30).await;

    // What a poll landing now sees — the spike is 30 seconds behind it.
    let instant = resolver::resolve_with_state(&app.state, &cpu_metric())
        .await
        .expect("resolve");
    assert_eq!(instant.len(), 1);
    assert_eq!(
        instant[0].value, 30.0,
        "the poll reads the current gauge, not the spike"
    );
    assert!(
        !expression::Comparator::Gt.evaluate(instant[0].value, 80.0),
        "so a `> 80` rule sees nothing to fire on"
    );

    let windowed = resolver::resolve_windowed(
        &app.state,
        &cpu_metric(),
        &Window {
            agg: Aggregate::Max,
            secs: 60,
        },
    )
    .await
    .expect("resolve windowed");
    assert_eq!(windowed.len(), 1);
    assert_eq!(windowed[0].value, 94.2);
    assert!(
        expression::Comparator::Gt.evaluate(windowed[0].value, 80.0),
        "max over the window sees every sample between the polls"
    );
}

/// `avg` must not inherit `max`'s answer — a lone spike in a quiet minute is
/// exactly the case where the two have to disagree.
#[tokio::test]
async fn avg_and_min_aggregate_over_the_same_window() {
    let app = TestApp::spawn().await;
    seed_spike(&app, 94.2, 30).await;

    let avg = resolver::resolve_windowed(
        &app.state,
        &cpu_metric(),
        &Window {
            agg: Aggregate::Avg,
            secs: 60,
        },
    )
    .await
    .expect("resolve avg");
    // 29 samples at 30.0 and one at 94.2.
    assert!(
        avg[0].value > 30.0 && avg[0].value < 35.0,
        "avg was {}, expected one spike diluted across thirty samples",
        avg[0].value
    );

    let min = resolver::resolve_windowed(
        &app.state,
        &cpu_metric(),
        &Window {
            agg: Aggregate::Min,
            secs: 60,
        },
    )
    .await
    .expect("resolve min");
    assert_eq!(min[0].value, 30.0);
}

/// A window shorter than the spike's age must not reach back past its own edge.
#[tokio::test]
async fn a_window_only_sees_its_own_span() {
    let app = TestApp::spawn().await;
    seed_spike(&app, 94.2, 50).await;

    let short = resolver::resolve_windowed(
        &app.state,
        &cpu_metric(),
        &Window {
            agg: Aggregate::Max,
            secs: 10,
        },
    )
    .await
    .expect("resolve");
    assert_eq!(
        short[0].value, 30.0,
        "a 10s window must not see a spike 50s back"
    );
}

/// Keyed namespaces aggregate per key, and a label filter still narrows it.
#[tokio::test]
async fn keyed_namespaces_aggregate_per_key() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();
    for (age, mount, used) in [
        (2i64, "/", 10u64),
        (4, "/", 90),
        (2, "/data", 40),
        (4, "/data", 20),
    ] {
        sqlx::query(
            "INSERT INTO metrics_disk
               (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes)
             VALUES ('raw', ?, ?, 100, ?, 0)",
        )
        .bind(now - age)
        .bind(mount)
        .bind(used as i64)
        .execute(&app.state.db)
        .await
        .expect("seed disk row");
    }

    let metric = |labels: BTreeMap<String, String>| MetricRef {
        namespace: "disk".to_string(),
        field: "used_percent".to_string(),
        labels,
    };
    let window = Window {
        agg: Aggregate::Max,
        secs: 60,
    };

    let mut all = resolver::resolve_windowed(&app.state, &metric(BTreeMap::new()), &window)
        .await
        .expect("resolve keyed");
    all.sort_by(|a, b| a.label_set.cmp(&b.label_set));
    assert_eq!(all.len(), 2, "one sample per mount");
    assert_eq!(all[0].label_set, r#"{"mount_point":"/"}"#);
    assert_eq!(all[0].value, 90.0);
    assert_eq!(all[1].value, 40.0);

    let filtered = resolver::resolve_windowed(
        &app.state,
        &metric(BTreeMap::from([(
            "mount_point".to_string(),
            "/data".to_string(),
        )])),
        &window,
    )
    .await
    .expect("resolve filtered");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].value, 40.0);
}

/// Namespaces with no sample history cannot be aggregated, and saying so at
/// parse/validate time beats a rule that warns on every tick and never fires.
#[tokio::test]
async fn namespaces_without_history_are_refused() {
    let app = TestApp::spawn().await;
    let err = resolver::resolve_windowed(
        &app.state,
        &MetricRef {
            namespace: "service".to_string(),
            field: "up".to_string(),
            labels: BTreeMap::from([("unit".to_string(), "nginx.service".to_string())]),
        },
        &Window {
            agg: Aggregate::Max,
            secs: 60,
        },
    )
    .await
    .expect_err("service has no sample history");
    assert!(
        err.message.contains("no sample history"),
        "unexpected message: {}",
        err.message
    );
}

/// An empty window yields no sample rather than a zero — the evaluator's prune
/// reads "no sample" as stale and holds state, where a 0 would resolve the rule.
#[tokio::test]
async fn an_empty_window_yields_no_sample() {
    let app = TestApp::spawn().await;
    let samples = resolver::resolve_windowed(
        &app.state,
        &cpu_metric(),
        &Window {
            agg: Aggregate::Max,
            secs: 60,
        },
    )
    .await
    .expect("resolve");
    assert!(samples.is_empty());
}
