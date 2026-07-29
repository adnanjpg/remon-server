//! Rollup cursor advancement.
//!
//! Two failure modes pull in opposite directions: never advancing past an
//! empty bucket puts the sweep on a treadmill, always advancing loses buckets
//! a parent tier has not filled yet. Both are silent.

use super::TestApp;

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
