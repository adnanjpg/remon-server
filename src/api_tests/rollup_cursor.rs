//! Rollup cursor advancement.
//!
//! Two failure modes pull in opposite directions: never advancing past an
//! empty bucket puts the sweep on a treadmill, always advancing loses buckets
//! a parent tier has not filled yet. Both are silent.

use super::TestApp;

async fn set_cursor(app: &TestApp, resource: &str, resolution: &str, ts: i64) {
    sqlx::query(
        "INSERT INTO rollup_state (resource, resolution, processed_from, last_bucket_ts, last_run_at)
         VALUES (?, ?, 0, ?, 0)
         ON CONFLICT(resource, resolution) DO UPDATE SET last_bucket_ts = excluded.last_bucket_ts",
    )
    .bind(resource)
    .bind(resolution)
    .bind(ts)
    .execute(&app.state.db)
    .await
    .expect("set cursor");
}

async fn cursor(app: &TestApp, resource: &str, resolution: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT last_bucket_ts FROM rollup_state WHERE resource = ? AND resolution = ?",
    )
    .bind(resource)
    .bind(resolution)
    .fetch_one(&app.state.db)
    .await
    .expect("read cursor")
}

/// A resource that produced rows and then stopped: empty buckets, non-zero
/// cursor. Without advancing on empty the sweep widens each tick to the
/// back-fill clamp and re-runs that range forever, writing nothing.
#[tokio::test]
async fn empty_buckets_advance_the_cursor_when_nothing_can_fill_them() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    // Parked 500 buckets back, with no `metrics_docker` rows anywhere.
    let stale = (now / 60 - 500) * 60;
    set_cursor(&app, "docker", "1m", stale).await;

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let after = cursor(&app, "docker", "1m").await;
    assert!(
        after > stale,
        "cursor stayed at {stale} after sweeping empty buckets"
    );
    assert!(
        after >= (now / 60 - 2) * 60,
        "cursor advanced to {after} but not to the latest closed bucket; the \
         backlog would be re-swept next tick"
    );
}

/// A gap longer than one tick's budget is caught up over several ticks rather
/// than skipped. The budget used to clamp how far *back* a tick reached, and
/// buckets below the clamp were not deferred but stepped over: 720 buckets is
/// 12 hours at `1m`, and `raw` — what `1m` is built from — is kept for a day,
/// so an outage between the two left a hole with the rows to fill it in place.
#[tokio::test]
async fn a_gap_longer_than_one_ticks_budget_is_deferred_not_skipped() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    let latest_closed = (now / 60 - 1) * 60;
    let stale = latest_closed - 1000 * 60;
    set_cursor(&app, "docker", "1m", stale).await;

    // A raw sample two buckets past the cursor: inside the window the clamp
    // jumped over, and far enough back that no later tick would revisit it.
    let sampled_at = stale + 120;
    sqlx::query(
        "INSERT INTO metrics_docker
             (resolution, timestamp, container_id, cpu_percent, memory_used_bytes,
              memory_limit_bytes, network_rx_bytes, network_tx_bytes,
              block_read_bytes, block_write_bytes, pids)
         VALUES ('raw', ?, 'web', 1.0, 1, 2, 3, 4, 5, 6, 7)",
    )
    .bind(sampled_at)
    .execute(&app.state.db)
    .await
    .expect("raw sample");

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("first tick");

    let rolled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM metrics_docker WHERE resolution = '1m' AND timestamp = ?",
    )
    .bind(sampled_at / 60 * 60)
    .fetch_one(&app.state.db)
    .await
    .expect("count rolled bucket");
    assert_eq!(
        rolled, 1,
        "the bucket holding the raw sample was stepped over instead of aggregated"
    );

    let after_first = cursor(&app, "docker", "1m").await;
    assert!(
        after_first < latest_closed,
        "one tick swallowed a 1000-bucket gap; the per-tick budget did not bound it"
    );

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("second tick");
    assert!(
        cursor(&app, "docker", "1m").await > after_first,
        "the second tick did not resume where the first one stopped"
    );
}

/// The counter-case: a `5m` bucket empty today can still be filled once
/// `1m` catches up, so stepping over it drops that window permanently.
/// `1m` is disabled so it cannot advance during the tick.
#[tokio::test]
async fn a_child_tier_waits_for_buckets_its_parent_has_not_filled() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    sqlx::query("UPDATE resolutions SET enabled = 0 WHERE name = '1m'")
        .execute(&app.state.db)
        .await
        .expect("disable 1m");

    // 1m is settled only up to `parent_at`; 5m is far behind it.
    let parent_at = (now / 60 - 300) * 60;
    set_cursor(&app, "cpu", "1m", parent_at).await;
    let child_start = (now / 300 - 400) * 300;
    set_cursor(&app, "cpu", "5m", child_start).await;

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let after = cursor(&app, "cpu", "5m").await;
    assert!(
        after <= parent_at,
        "5m cursor moved to {after}, past where 1m is settled ({parent_at})"
    );
}

/// A bucket the parent has only partly filled must not be treated as finished.
/// `aggregate_one_bucket` reports rows written, not a completed range, so a
/// window the parent is still working through aggregates from what is there so
/// far — and if the cursor moves past it, that partial average is permanent.
#[tokio::test]
async fn a_partly_filled_bucket_does_not_advance_the_cursor() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    sqlx::query("UPDATE resolutions SET enabled = 0 WHERE name = '1m'")
        .execute(&app.state.db)
        .await
        .expect("disable 1m");

    // One 5m window, with 1m settled only two minutes into it.
    let window = (now / 300 - 20) * 300;
    set_cursor(&app, "cpu", "1m", window + 60).await;
    set_cursor(&app, "cpu", "5m", window - 300).await;

    for minute in [window, window + 60] {
        sqlx::query(
            "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
             VALUES ('1m', ?, 10.0, 1.0, 1.0, 1.0)",
        )
        .bind(minute)
        .execute(&app.state.db)
        .await
        .expect("seed 1m row");
    }

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let after = cursor(&app, "cpu", "5m").await;
    assert!(
        after < window,
        "5m cursor moved to {after}, past a window 1m has only filled to \
         {}: that bucket keeps an average of two samples out of five",
        window + 60
    );
}

/// Advancing must not skip a gap: a later bucket that does have rows still
/// leaves the cursor before the pending one.
#[tokio::test]
async fn a_written_bucket_after_a_gap_does_not_drag_the_cursor_over_it() {
    let app = TestApp::spawn().await;
    let now = chrono::Utc::now().timestamp();

    sqlx::query("UPDATE resolutions SET enabled = 0 WHERE name = '1m'")
        .execute(&app.state.db)
        .await
        .expect("disable 1m");

    let parent_at = (now / 60 - 200) * 60;
    set_cursor(&app, "cpu", "1m", parent_at).await;
    let child_start = (now / 300 - 300) * 300;
    set_cursor(&app, "cpu", "5m", child_start).await;

    // A 1m row inside a bucket the parent has *not* settled through, so the
    // 5m sweep finds rows there while earlier buckets are still pending.
    let late_bucket = (now / 300 - 5) * 300;
    sqlx::query(
        "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('1m', ?, 42.0, 1.0, 1.0, 1.0)",
    )
    .bind(late_bucket + 60)
    .execute(&app.state.db)
    .await
    .expect("seed 1m row");

    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup tick");

    let after = cursor(&app, "cpu", "5m").await;
    assert!(
        after <= parent_at,
        "5m cursor jumped to {after}: a later bucket that happened to have \
         rows dragged it over buckets the parent still owes"
    );
}

pub(super) async fn samples(app: &TestApp, start: i64, end: i64) {
    let mut tx = app.state.db.begin().await.unwrap();
    for ts in (start..end).step_by(2) {
        sqlx::query(
            "INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m,
            load_5m, load_15m, steal_percent, context_switches_per_sec)
            VALUES ('raw', ?, ?, 1.0, 2.0, 3.0, ?, ?)",
        )
        .bind(ts)
        .bind((ts % 101) as f64 / 3.0)
        .bind(if ts % 6 == 0 { Some(0.25) } else { None })
        .bind(ts % 17)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn rollup_first_bucket_is_inside_parent_coverage_at_every_tier() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 3600 - 3) * 3600;
    samples(&app, base + 37, base + 7200).await;
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT resolution, processed_from FROM rollup_state WHERE resource = 'cpu' ORDER BY processed_from"
    ).fetch_all(&app.state.db).await.unwrap();
    assert_eq!(
        rows,
        vec![
            ("1m".into(), base + 60),
            ("5m".into(), base + 300),
            ("1h".into(), base + 3600)
        ]
    );
    for (resolution, first) in rows {
        let earliest: i64 =
            sqlx::query_scalar("SELECT MIN(timestamp) FROM metrics_cpu WHERE resolution = ?")
                .bind(resolution)
                .fetch_one(&app.state.db)
                .await
                .unwrap();
        assert_eq!(earliest, first);
    }
}

#[tokio::test]
async fn rollup_unknown_legacy_coverage_is_rebuilt_not_inferred_from_first_row() {
    let app = TestApp::spawn().await;
    let base = (chrono::Utc::now().timestamp() / 60 - 30) * 60;
    samples(&app, base + 17, base + 600).await;
    // A stale derived row in an empty raw bucket must not survive rebuilding.
    sqlx::query("INSERT INTO metrics_cpu (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m) VALUES ('1m', ?, 99.0, 1.0, 1.0, 1.0)")
        .bind(base + 660).execute(&app.state.db).await.unwrap();
    sqlx::query(
        "INSERT INTO rollup_state (resource, resolution, last_bucket_ts) VALUES ('cpu', '1m', ?)",
    )
    .bind(base + 600)
    .execute(&app.state.db)
    .await
    .unwrap();
    crate::services::rollup::run_once(&app.state).await.unwrap();
    let first: i64 = sqlx::query_scalar(
        "SELECT processed_from FROM rollup_state WHERE resource = 'cpu' AND resolution = '1m'",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    assert_eq!(first, base + 60);
    let count: i64 = sqlx::query_scalar("SELECT usage_percent_valid_count FROM metrics_cpu WHERE resolution = '1m' AND timestamp = ?")
        .bind(base + 60).fetch_one(&app.state.db).await.unwrap();
    assert_eq!(count, 30);
    let stale: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM metrics_cpu WHERE resolution = '1m' AND timestamp = ?",
    )
    .bind(base + 660)
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    assert_eq!(
        stale, 0,
        "empty rebuilt bucket must not retain an old derived value"
    );
}
