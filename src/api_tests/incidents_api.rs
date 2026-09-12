//! Incident flight-recorder tests: manual capture, the assistant tools, the
//! REST surface, and the episode lifecycle the evaluator drives.
//!
//! The lifecycle tests are the ones that matter. The shape this replaced took
//! one frame at `ok→pending` and one 60 s later, and the `pending→firing`
//! capture was always swallowed by the flap cooldown — so a long incident was
//! described entirely by its opening minute, and nothing at all was recorded
//! when it cleared. `escalation_and_resolution_land_in_one_episode` and
//! `a_new_worst_value_earns_a_peak_frame` exist to keep that from returning.
//!
//! Frames here are built from whatever the harness has — empty caches degrade
//! slices to null, which is exactly the contract.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::TestApp;
use crate::assistant::tools::dispatch;
use crate::services::incidents;
use crate::storage::repositories::IncidentRepository;

fn call(result: String) -> Value {
    serde_json::from_str(&result).expect("tool returned valid json")
}

/// Frames are written from spawned tasks; poll rather than sleep a fixed span,
/// so a slow machine does not turn these into a timing lottery.
async fn wait_for_frames(app: &TestApp, id: i64, want: usize) -> Vec<String> {
    let repo = IncidentRepository::new(app.state.db.clone());
    for _ in 0..80 {
        if let Ok(Some(row)) = repo.get(id).await
            && row.frames.len() >= want
        {
            return row.frames.into_iter().map(|f| f.kind).collect();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let got = repo
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|r| r.frames.into_iter().map(|f| f.kind).collect::<Vec<_>>())
        .unwrap_or_default();
    panic!("expected {want} frame(s) on incident {id}, got {got:?}");
}

async fn wait_for_episode(app: &TestApp) -> i64 {
    let repo = IncidentRepository::new(app.state.db.clone());
    for _ in 0..80 {
        if let Ok(rows) = repo.list(10).await
            && let Some(r) = rows.first()
        {
            return r.id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no incident episode was opened");
}

async fn seed_cpu(app: &TestApp, usage: f64) {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR REPLACE INTO metrics_cpu
           (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m)
         VALUES ('raw', ?, ?, 1.0, 1.0, 1.0)",
    )
    .bind(now)
    .bind(usage)
    .execute(&app.state.db)
    .await
    .unwrap();
}

/// Create a rule any cpu sample violates, seed one, and hand back the pair the
/// evaluator needs.
async fn arm_cpu_rule(
    app: &TestApp,
    token: &str,
    for_duration_secs: i64,
) -> (
    crate::models::alert::AlertRule,
    crate::services::alerting::expression::Expression,
) {
    use crate::services::alerting::expression;
    use crate::storage::repositories::AlertRepository;

    let (st, _) = app
        .request(
            "POST",
            "/alerts",
            Some(token),
            Some(json!({
                "name": "test-cpu", "expression": "cpu.usage_percent > 1",
                "severity": "warn", "for_duration_secs": for_duration_secs
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);

    seed_cpu(app, 95.0).await;

    let rule = AlertRepository::new(app.state.db.clone())
        .list_enabled()
        .await
        .unwrap()
        .pop()
        .expect("rule");
    let expr = expression::parse(&rule.expression).expect("parse");
    (rule, expr)
}

#[tokio::test]
async fn manual_capture_persists_and_tools_read_it() {
    let app = TestApp::spawn().await;

    let id = incidents::capture_manual(&app.state, "operator smoke test", "security")
        .await
        .expect("capture");
    assert!(id > 0);

    // list_incidents sees it.
    let out = call(dispatch(&app.state, "list_incidents", &json!({})).await);
    assert_eq!(out["count"], 1, "got: {out}");
    assert_eq!(out["incidents"][0]["trigger"], "manual");
    assert_eq!(out["incidents"][0]["category"], "security");
    assert_eq!(out["incidents"][0]["reason"], "operator smoke test");

    // incident_detail returns the reel, with the opening frame parsed.
    let out = call(dispatch(&app.state, "incident_detail", &json!({ "id": id })).await);
    assert_eq!(out["id"], id);
    let frames = out["frames"].as_array().expect("frames array");
    assert_eq!(frames.len(), 1, "got: {out}");
    assert_eq!(frames[0]["kind"], "onset");
    let payload = &frames[0]["payload"];
    assert!(payload.is_object(), "payload should parse: {out}");
    assert!(payload.get("captured_at").is_some());
    assert!(payload.get("recent_daemon_errors").is_some());
    assert!(payload.get("co_active_alerts").is_some());
}

#[tokio::test]
async fn incident_detail_unknown_id_errors() {
    let app = TestApp::spawn().await;
    let out = call(dispatch(&app.state, "incident_detail", &json!({ "id": 424242 })).await);
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("424242")),
        "got: {out}"
    );
}

#[tokio::test]
async fn capture_incident_tool_requires_reason_and_valid_category() {
    let app = TestApp::spawn().await;

    let out = call(dispatch(&app.state, "capture_incident", &json!({})).await);
    assert!(out["error"].is_string(), "got: {out}");

    let out = call(
        dispatch(
            &app.state,
            "capture_incident",
            &json!({ "reason": "x", "category": "catastrophe" }),
        )
        .await,
    );
    assert!(out["error"].is_string(), "got: {out}");

    let out = call(
        dispatch(
            &app.state,
            "capture_incident",
            &json!({ "reason": "weird connection storm" }),
        )
        .await,
    );
    assert_eq!(out["captured"], true, "got: {out}");
    assert!(out["id"].as_i64().is_some_and(|id| id > 0));
}

#[tokio::test]
async fn rest_capture_requires_auth_and_opens_an_episode() {
    let app = TestApp::spawn().await;

    let (st, _) = app
        .request(
            "POST",
            "/incidents/capture",
            None,
            Some(json!({ "reason": "external hook" })),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let token = app.pair_and_login().await;
    let (st, body) = app
        .request(
            "POST",
            "/incidents/capture",
            Some(&token),
            Some(json!({ "reason": "external hook", "category": "security" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    let id = body["id"].as_i64().expect("id");

    let row = IncidentRepository::new(app.state.db.clone())
        .get(id)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.trigger_kind, "manual");
    assert_eq!(row.category, "security");

    // Empty reason is rejected.
    let (st, _) = app
        .request(
            "POST",
            "/incidents/capture",
            Some(&token),
            Some(json!({ "reason": "  " })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// The regression this redesign exists for. The escalation and the resolution
/// must both be recorded, in the *same* episode as the onset — the old shape
/// dropped the first to the flap cooldown and never took the second at all.
#[tokio::test]
async fn escalation_and_resolution_land_in_one_episode() {
    use crate::services::alerting::evaluator;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 0).await;

    // ok → pending: opens the episode.
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    let id = wait_for_episode(&app).await;
    wait_for_frames(&app, id, 1).await;

    // pending → firing: appends, and must NOT be swallowed by the cooldown.
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    let frames = wait_for_frames(&app, id, 2).await;
    assert_eq!(frames[0], "onset");
    assert_eq!(frames[1], "escalation", "escalation frame was dropped");

    // The violation clears: firing → ok closes the episode with a final frame.
    // A *non-violating* sample, not an empty table — with nothing to read the
    // evaluator has no sample to judge and never transitions at all.
    seed_cpu(&app, 0.5).await;
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    let frames = wait_for_frames(&app, id, 3).await;
    assert_eq!(frames[2], "resolution", "resolution frame was never taken");

    // And exactly one episode holds all three.
    let rows = IncidentRepository::new(app.state.db.clone())
        .list(10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the episode was split in two");
    assert_eq!(rows[0].frame_count, 3);
    assert!(rows[0].closed_at.is_some(), "episode left open");
    assert_eq!(rows[0].close_reason.as_deref(), Some("resolved"));
    assert_eq!(rows[0].trigger_value, Some(95.0));
}

/// A materially worse value during a sustained episode earns its own frame and
/// raises the recorded peak — the moment the old shape could never capture.
#[tokio::test]
async fn a_new_worst_value_earns_a_peak_frame() {
    use crate::services::incidents::AlertPhase;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _expr) = arm_cpu_rule(&app, &token, 0).await;

    incidents::on_alert_transition(
        &app.state,
        AlertPhase::Onset,
        rule.id,
        &rule.name,
        "",
        "cpu",
        50.0,
    )
    .await;
    let id = wait_for_episode(&app).await;
    wait_for_frames(&app, id, 1).await;

    // Backdate the last peak frame so the spacing rule cannot veto this one;
    // the margin is what is under test, not the clock.
    {
        let mut map = app.state.incident_episodes.write().await;
        for ep in map.values_mut() {
            ep.last_peak_frame_at = 0;
        }
    }

    incidents::on_alert_transition(
        &app.state,
        AlertPhase::Sustained,
        rule.id,
        &rule.name,
        "",
        "cpu",
        99.0,
    )
    .await;
    let frames = wait_for_frames(&app, id, 2).await;
    assert_eq!(frames[1], "peak");

    let rows = IncidentRepository::new(app.state.db.clone())
        .list(1)
        .await
        .unwrap();
    assert_eq!(rows[0].peak_value, Some(99.0));
    assert_eq!(rows[0].trigger_value, Some(50.0));
}

/// A tick that is not materially worse must write nothing: a frame per
/// evaluator tick would be load added during the incident, which the flight
/// recorder refuses to do.
#[tokio::test]
async fn an_unremarkable_tick_writes_no_frame() {
    use crate::services::incidents::AlertPhase;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _expr) = arm_cpu_rule(&app, &token, 0).await;

    incidents::on_alert_transition(
        &app.state,
        AlertPhase::Onset,
        rule.id,
        &rule.name,
        "",
        "cpu",
        90.0,
    )
    .await;
    let id = wait_for_episode(&app).await;
    wait_for_frames(&app, id, 1).await;

    for v in [88.0, 90.5, 91.0] {
        incidents::on_alert_transition(
            &app.state,
            AlertPhase::Sustained,
            rule.id,
            &rule.name,
            "",
            "cpu",
            v,
        )
        .await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let row = IncidentRepository::new(app.state.db.clone())
        .get(id)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.frames.len(), 1, "a quiet tick wrote a frame");
    // The peak still tracks the highest seen, frame or no frame.
    assert_eq!(row.peak_value, Some(91.0));
}

/// The cooldown still exists, but it now governs *episodes*: a rule that flaps
/// must not open a fresh one each time.
#[tokio::test]
async fn the_cooldown_suppresses_a_second_episode_not_a_frame() {
    use crate::services::alerting::evaluator;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 0).await;

    for _ in 0..2 {
        evaluator::evaluate_once(&rule, &expr, &app.state)
            .await
            .unwrap();
    }
    let id = wait_for_episode(&app).await;
    wait_for_frames(&app, id, 2).await;

    seed_cpu(&app, 0.5).await;
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    wait_for_frames(&app, id, 3).await;

    // Cross again immediately: inside the cooldown, so no new episode.
    seed_cpu(&app, 95.0).await;
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let rows = IncidentRepository::new(app.state.db.clone())
        .list(10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the flap opened a second episode");
}

/// A frame records the instant it was taken and does not carry a frozen copy
/// of the gauges. What they did across the episode is read back from
/// `metrics_*`, which is only sound while those rows outlive the episode —
/// hence the companion assertion in `host_gauge_tiers_outlive_an_episode`.
#[tokio::test]
async fn a_frame_does_not_duplicate_the_gauge_history() {
    let app = TestApp::spawn().await;
    let id = incidents::capture_manual(&app.state, "shape check", "custom")
        .await
        .expect("capture");
    wait_for_frames(&app, id, 1).await;

    let row = IncidentRepository::new(app.state.db.clone())
        .get(id)
        .await
        .unwrap()
        .expect("row");
    let payload: Value = serde_json::from_str(&row.frames[0].payload).expect("payload json");
    assert!(
        payload.get("window").is_none(),
        "frames should not carry a gauge summary any more: {payload}"
    );
    assert!(payload.get("captured_at").is_some());
    assert!(payload.get("vitals").is_some());
}

/// The load-bearing half of that decision. A frame points at `metrics_*` for
/// the episode's shape, so the host gauges must keep a sub-hourly tier for at
/// least as long as an episode is kept — otherwise a two-month-old incident
/// resolves to one hourly bucket whose extrema describe the whole hour, and
/// the reference silently stops answering the question it was written for.
#[tokio::test]
async fn host_gauge_tiers_outlive_an_episode() {
    let app = TestApp::spawn().await;

    let incident_keep: i64 = sqlx::query_scalar(
        "SELECT keep_seconds FROM retention_policy WHERE resource = 'incidents'",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();

    for resource in ["cpu", "memory", "disk", "network"] {
        let keep: i64 = sqlx::query_scalar(
            "SELECT keep_seconds FROM retention_policy
              WHERE resource = ? AND resolution = '5m'",
        )
        .bind(resource)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
        assert!(
            keep >= incident_keep,
            "{resource} keeps its 5m tier {keep}s but episodes live {incident_keep}s, \
             so an old incident cannot be read back at that resolution"
        );
    }
}

/// Retention must not reap a recording in progress — the frames that would
/// justify keeping it have not been written yet.
#[tokio::test]
async fn retention_keeps_an_open_episode() {
    use crate::storage::repositories::{MetricsRepository, NewIncident};

    let app = TestApp::spawn().await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let now = chrono::Utc::now().timestamp();

    let open = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".to_string(),
            reason: Some("still recording".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let closed = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".to_string(),
            reason: Some("done".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    repo.close(closed, now, "resolved").await.unwrap();

    // Age both past the cutoff.
    sqlx::query("UPDATE incidents SET opened_at = ?")
        .bind(now - 100_000)
        .execute(&app.state.db)
        .await
        .unwrap();

    MetricsRepository::new(app.state.db.clone())
        .delete_older_than("incidents", "raw", now - 1000)
        .await
        .unwrap();

    assert!(repo.get(open).await.unwrap().is_some(), "open one reaped");
    assert!(repo.get(closed).await.unwrap().is_none(), "closed one kept");
}

/// Frames belong to their episode and go with it.
#[tokio::test]
async fn deleting_an_episode_takes_its_frames() {
    use crate::storage::repositories::NewIncident;

    let app = TestApp::spawn().await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    repo.append_frame(id, "onset", 1, "{}").await.unwrap();
    repo.append_frame(id, "resolution", 2, "{}").await.unwrap();

    sqlx::query("DELETE FROM incidents WHERE id = ?")
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();

    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM incident_frames WHERE incident_id = ?")
            .bind(id)
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(left, 0, "orphaned frames left behind");
}

/// An episode the previous process left open must be closed at boot, or the
/// next crossing appends to it and reports a start from before the restart.
#[tokio::test]
async fn boot_closes_episodes_orphaned_by_a_restart() {
    use crate::storage::repositories::NewIncident;

    let app = TestApp::spawn().await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "alert",
            category: "resource".to_string(),
            // No rule_id: `alert_rules` is empty here and the column is a
            // foreign key. What is under test is the sweep, not the join.
            rule_name: Some("orphan".to_string()),
            label_set: Some(String::new()),
            ..Default::default()
        })
        .await
        .unwrap();

    incidents::close_orphaned_episodes(&app.state).await;

    let row = repo.get(id).await.unwrap().expect("row");
    assert!(row.closed_at.is_some());
    assert_eq!(row.close_reason.as_deref(), Some("daemon_restart"));
}

#[tokio::test]
async fn rest_list_and_get_read_episodes_back() {
    let app = TestApp::spawn().await;

    let (st, _) = app.request("GET", "/incidents", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let token = app.pair_and_login().await;

    // Nothing captured yet: an empty list, not a 404.
    let (st, body) = app.request("GET", "/incidents", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], 0);
    assert_eq!(body["incidents"].as_array().expect("array").len(), 0);

    let id = incidents::capture_manual(&app.state, "disk filling up", "resource")
        .await
        .expect("capture");
    wait_for_frames(&app, id, 1).await;

    let (st, body) = app.request("GET", "/incidents", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], 1);
    let row = &body["incidents"][0];
    assert_eq!(row["id"], id);
    assert_eq!(row["trigger_kind"], "manual");
    assert_eq!(row["category"], "resource");
    assert_eq!(row["reason"], "disk filling up");
    assert_eq!(row["frame_count"], 1);
    // Still recording: the follow-up lands a minute later.
    assert!(
        row.get("closed_at").is_none(),
        "a live episode should omit closed_at, not null it"
    );
    // Listing must not drag the payloads along.
    assert!(row.get("frames").is_none(), "listing leaked the frames");

    // The detail carries the reel, payloads as JSON rather than quoted strings.
    let (st, body) = app
        .request("GET", &format!("/incidents/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    assert_eq!(body["id"], id);
    let frames = body["frames"].as_array().expect("frames");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["kind"], "onset");
    assert_eq!(frames[0]["seq"], 0);
    assert!(frames[0]["payload"].is_object(), "payload should be json");

    let (st, _) = app
        .request("GET", "/incidents/999999", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rest_list_is_newest_first_and_honours_limit() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let mut ids = Vec::new();
    for reason in ["first", "second", "third"] {
        ids.push(
            incidents::capture_manual(&app.state, reason, "custom")
                .await
                .expect("capture"),
        );
    }

    let (st, body) = app.request("GET", "/incidents", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    let got: Vec<i64> = body["incidents"]
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["id"].as_i64().expect("id"))
        .collect();
    let mut newest_first = ids.clone();
    newest_first.reverse();
    assert_eq!(got, newest_first);

    let (st, body) = app
        .request("GET", "/incidents?limit=1", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], 1);
    assert_eq!(body["incidents"][0]["id"], *ids.last().expect("last"));
}
