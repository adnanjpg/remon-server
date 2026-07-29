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
