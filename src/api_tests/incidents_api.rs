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

async fn settle_episode(app: &TestApp, value: f64) {
    let (key, ep) = app
        .state
        .incident_episodes
        .read()
        .await
        .iter()
        .next()
        .map(|(k, e)| (k.clone(), e.clone()))
        .expect("open episode");
    let rule = crate::storage::repositories::AlertRepository::new(app.state.db.clone())
        .get(key.0)
        .await
        .unwrap()
        .unwrap();
    let expr = crate::services::alerting::expression::parse(&rule.expression).unwrap();
    let hold = (ep.eval_interval * 2).max(60);
    let step = ep.eval_interval.max(1);
    let mut now = ep.last_seen_at;
    while now < ep.last_seen_at + hold {
        now += step;
        incidents::observe_at(
            &app.state,
            incidents::AlertPhase::Resolved,
            &rule,
            &expr,
            &key.1,
            value,
            now,
        )
        .await
        .unwrap();
    }
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
    assert!(
        IncidentRepository::new(app.state.db.clone())
            .get(id)
            .await
            .unwrap()
            .unwrap()
            .closed_at
            .is_none()
    );
    settle_episode(&app, 0.5).await;
    let frames = wait_for_frames(&app, id, 4).await;
    assert_eq!(frames[3], "resolution", "resolution frame was never taken");

    // And exactly one episode holds all three.
    let rows = IncidentRepository::new(app.state.db.clone())
        .list(10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the episode was split in two");
    assert_eq!(rows[0].frame_count, 4);
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

    incidents::on_alert_transition(
        &app.state,
        AlertPhase::Escalation,
        rule.id,
        &rule.name,
        "",
        "cpu",
        50.0,
    )
    .await;

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
    let frames = wait_for_frames(&app, id, 3).await;
    assert_eq!(frames[2], "peak");

    let rows = IncidentRepository::new(app.state.db.clone())
        .list(1)
        .await
        .unwrap();
    assert_eq!(rows[0].worst_value, Some(99.0));
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
            AlertPhase::Pending,
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
    assert_eq!(row.worst_value, Some(91.0));
}

/// Notification suppression must not erase a distinct observed violation.
#[tokio::test]
async fn a_nearby_confirmed_recurrence_is_grouped_without_discarding_it() {
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

    // Cross again immediately: a distinct violation still needs its own record.
    seed_cpu(&app, 95.0).await;
    evaluator::evaluate_once(&rule, &expr, &app.state)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let rows = IncidentRepository::new(app.state.db.clone())
        .list(10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "a nearby recurrence split the episode");
    assert_eq!(rows[0].violation_count, 2);
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

/// No polling between transitions: logical ordering cannot depend on enrichment speed.
#[tokio::test]
async fn immediate_transitions_preserve_order_and_frozen_rule() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    for (phase, value) in [(Onset, 50.0), (Escalation, 95.0), (Resolved, 0.0)] {
        incidents::on_alert_transition(&app.state, phase, rule.id, &rule.name, "", "cpu", value)
            .await;
    }
    settle_episode(&app, 0.0).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.frames
            .iter()
            .map(|f| f.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["onset", "escalation", "recovery", "resolution"]
    );
    assert_eq!(row.worst_value, Some(95.0));
    assert_eq!(row.close_reason.as_deref(), Some("resolved"));
    let context: Value = serde_json::from_str(row.trigger_context.as_ref().unwrap()).unwrap();
    assert_eq!(context["expression"], rule.expression);
    assert_eq!(context["comparator"], ">");
    assert!(repo.append_frame(row.id, "peak", 0, "{}").await.is_err());
}

#[tokio::test]
async fn gradual_worsening_compares_against_recorded_frame() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    incidents::on_alert_transition(&app.state, Onset, rule.id, &rule.name, "", "cpu", 80.0).await;
    incidents::on_alert_transition(&app.state, Escalation, rule.id, &rule.name, "", "cpu", 80.0)
        .await;
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_peak_frame_at = 0;
    }
    for value in [81.0, 82.0, 83.0, 84.0] {
        incidents::on_alert_transition(
            &app.state, Sustained, rule.id, &rule.name, "", "cpu", value,
        )
        .await;
    }
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.frames.last().unwrap().kind, "peak");
    assert_eq!(row.worst_value, Some(84.0));
}

#[tokio::test]
async fn low_threshold_records_the_minimum_and_not_the_recovery() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    sqlx::query("UPDATE alert_rules SET expression='cpu.usage_percent < 10' WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    for (phase, value) in [(Onset, 9.0), (Escalation, 8.0)] {
        incidents::on_alert_transition(&app.state, phase, rule.id, &rule.name, "", "cpu", value)
            .await;
    }
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_peak_frame_at = 0;
    }
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 2.0)
        .await;
    incidents::on_alert_transition(&app.state, Resolved, rule.id, &rule.name, "", "cpu", 20.0)
        .await;
    settle_episode(&app, 20.0).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.worst_value, Some(2.0));
    assert_eq!(row.frames[2].kind, "peak");
    let last: Value = serde_json::from_str(&row.frames.last().unwrap().payload).unwrap();
    assert_eq!(last["trigger_value"], 20.0);
}

#[tokio::test]
async fn missing_data_closes_without_claiming_recovery_and_can_resume() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    incidents::on_alert_transition(&app.state, Onset, rule.id, &rule.name, "", "cpu", 80.0).await;
    let now = chrono::Utc::now().timestamp();
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_seen_at = now - 10000;
    }
    incidents::maintain(&app.state, now).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.close_reason.as_deref(), Some("data_gap"));
    assert_eq!(row.frames.last().unwrap().kind, "interrupted");
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 81.0)
        .await;
    let rows = repo.list(2).await.unwrap();
    assert_eq!(rows.len(), 2);
    let resumed = repo.get(rows[0].id).await.unwrap().unwrap();
    assert_eq!(resumed.frames[0].kind, "continuation");
    let context: Value = serde_json::from_str(resumed.trigger_context.as_ref().unwrap()).unwrap();
    assert_eq!(context["previous_incident_id"], row.id);
}

#[tokio::test]
async fn rule_changes_and_recording_limit_are_interruptions() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    incidents::on_alert_transition(&app.state, Onset, rule.id, &rule.name, "", "cpu", 80.0).await;
    let now = chrono::Utc::now().timestamp();
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.opened_at = now - 21601;
    }
    incidents::maintain(&app.state, now).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.close_reason.as_deref(), Some("expired"));
    assert_eq!(row.frames.last().unwrap().kind, "interrupted");
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 80.0)
        .await;
    sqlx::query("UPDATE alert_rules SET expression='cpu.usage_percent > 90' WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    incidents::maintain(&app.state, now).await;
    assert_eq!(
        repo.list(1).await.unwrap()[0].close_reason.as_deref(),
        Some("rule_changed")
    );
}

#[tokio::test]
async fn checkpoints_and_peak_budget_leave_room_for_resolution() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    incidents::on_alert_transition(&app.state, Onset, rule.id, &rule.name, "", "cpu", 10.0).await;
    incidents::on_alert_transition(&app.state, Escalation, rule.id, &rule.name, "", "cpu", 10.0)
        .await;
    for at in [600, 1800, 3600, 7200, 14400] {
        for ep in app.state.incident_episodes.write().await.values_mut() {
            ep.opened_at = chrono::Utc::now().timestamp() - at;
            ep.last_peak_frame_at = 0;
        }
        incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 10.0)
            .await;
    }
    for v in [20.0, 30.0, 40.0, 50.0, 60.0] {
        for ep in app.state.incident_episodes.write().await.values_mut() {
            ep.last_peak_frame_at = 0;
        }
        incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", v)
            .await;
    }
    incidents::on_alert_transition(&app.state, Resolved, rule.id, &rule.name, "", "cpu", 0.0).await;
    settle_episode(&app, 0.0).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.frames.len() <= 12);
    assert_eq!(
        row.frames.iter().filter(|f| f.kind == "checkpoint").count(),
        4
    );
    assert_eq!(row.frames.last().unwrap().kind, "resolution");
    assert_eq!(row.worst_value, Some(60.0));
}

#[tokio::test]
async fn state_comparisons_do_not_invent_numeric_worsening() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    sqlx::query("UPDATE alert_rules SET expression='cpu.usage_percent != 0' WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    incidents::on_alert_transition(&app.state, Onset, rule.id, &rule.name, "", "cpu", 1.0).await;
    incidents::on_alert_transition(&app.state, Escalation, rule.id, &rule.name, "", "cpu", 2.0)
        .await;
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_peak_frame_at = 0;
    }
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 100.0)
        .await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert!(!row.frames.iter().any(|f| f.kind == "peak"));
    assert_eq!(row.worst_value, None);
}

#[tokio::test]
async fn closing_frame_and_envelope_commit_together() {
    use crate::storage::repositories::NewIncident;
    let app = TestApp::spawn().await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    repo.append_frame(id, "onset", 1, "{}").await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_close BEFORE UPDATE OF closed_at ON incidents BEGIN SELECT RAISE(ABORT,'test close failure'); END").execute(&app.state.db).await.unwrap();
    assert!(
        repo.write_frame(id, "resolution", 2, "{}", Some("resolved"))
            .await
            .is_err()
    );
    let row = repo.get(id).await.unwrap().unwrap();
    assert_eq!(row.frames.len(), 1);
    assert!(row.closed_at.is_none());
}

#[tokio::test]
async fn disabled_rules_and_abandoned_manual_recordings_are_closed() {
    use crate::services::incidents::AlertPhase;
    use crate::storage::repositories::NewIncident;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    incidents::on_alert_transition(
        &app.state,
        AlertPhase::Onset,
        rule.id,
        &rule.name,
        "",
        "cpu",
        80.0,
    )
    .await;
    sqlx::query("UPDATE alert_rules SET enabled=0 WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    incidents::maintain(&app.state, chrono::Utc::now().timestamp()).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    assert_eq!(
        repo.list(1).await.unwrap()[0].close_reason.as_deref(),
        Some("rule_disabled")
    );
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    incidents::maintain(&app.state, chrono::Utc::now().timestamp() + 121).await;
    assert_eq!(
        repo.get(id).await.unwrap().unwrap().close_reason.as_deref(),
        Some("data_gap")
    );
}

#[tokio::test]
async fn trigger_snapshot_matches_evaluated_definition_during_rule_edit() {
    use crate::services::alerting::expression;
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 0).await;
    sqlx::query("UPDATE alert_rules SET expression='cpu.usage_percent > 90' WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    incidents::on_rule_observation(&app.state, Onset, &rule, &expr, "", 50.0).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let first = repo.list(1).await.unwrap()[0].clone();
    let ctx: Value = serde_json::from_str(first.trigger_context.as_ref().unwrap()).unwrap();
    assert_eq!(ctx["expression"], rule.expression);
    let mut edited = rule.clone();
    edited.expression = "cpu.usage_percent > 90".into();
    incidents::on_rule_observation(
        &app.state,
        Sustained,
        &edited,
        &expression::parse(&edited.expression).unwrap(),
        "",
        95.0,
    )
    .await;
    let old = repo.get(first.id).await.unwrap().unwrap();
    assert_eq!(old.close_reason.as_deref(), Some("rule_changed"));
    assert_eq!(old.worst_value, Some(50.0));
    assert_eq!(repo.list(10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_rebound_below_the_recorded_worst_is_not_a_new_peak() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, _) = arm_cpu_rule(&app, &token, 0).await;
    for (p, v) in [(Onset, 80.0), (Escalation, 80.0)] {
        incidents::on_alert_transition(&app.state, p, rule.id, &rule.name, "", "cpu", v).await;
    }
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_peak_frame_at = 0;
    }
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 100.0)
        .await;
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.opened_at = chrono::Utc::now().timestamp() - 600;
        ep.last_peak_frame_at = 0;
    }
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 50.0)
        .await;
    for ep in app.state.incident_episodes.write().await.values_mut() {
        ep.last_peak_frame_at = 0;
    }
    incidents::on_alert_transition(&app.state, Sustained, rule.id, &rule.name, "", "cpu", 70.0)
        .await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo
        .get(repo.list(1).await.unwrap()[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.frames.iter().filter(|f| f.kind == "peak").count(), 1);
    assert_eq!(row.worst_value, Some(100.0));
}

#[tokio::test]
async fn restart_settles_pending_enrichment() {
    use crate::storage::repositories::NewIncident;
    let app = TestApp::spawn().await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let id = repo
        .open(&NewIncident {
            trigger_kind: "manual",
            category: "custom".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    repo.append_frame(id, "onset", 1, "{\"enrichment\":\"pending\"}")
        .await
        .unwrap();
    incidents::close_orphaned_episodes(&app.state).await;
    let row = repo.get(id).await.unwrap().unwrap();
    let payload: Value = serde_json::from_str(&row.frames[0].payload).unwrap();
    assert_eq!(payload["enrichment"], "interrupted");
    assert_eq!(row.close_reason.as_deref(), Some("daemon_restart"));
}

#[tokio::test]
async fn a_day_of_unconfirmed_flapping_is_bounded_and_counts_every_crossing() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (mut rule, _) = arm_cpu_rule(&app, &token, 30).await;
    rule.expression = "cpu.usage_percent > 80".into();
    sqlx::query("UPDATE alert_rules SET expression=? WHERE id=?")
        .bind(&rule.expression)
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    let expr = crate::services::alerting::expression::parse(&rule.expression).unwrap();
    let base = chrono::Utc::now().timestamp();
    for tick in 0..8640 {
        let (phase, value) = if tick % 2 == 0 {
            (Onset, 82.0)
        } else {
            (Resolved, 79.0)
        };
        incidents::observe_at(&app.state, phase, &rule, &expr, "", value, base + tick * 10)
            .await
            .unwrap();
    }
    let repo = IncidentRepository::new(app.state.db.clone());
    let rows = repo.list(100).await.unwrap();
    assert_eq!(
        rows.len(),
        4,
        "flapping must only split at the six-hour recording boundary"
    );
    assert_eq!(rows.iter().map(|r| r.violation_count).sum::<i64>(), 4320);
    assert!(
        rows.iter()
            .all(|r| r.confirmation_count == 0 && r.frame_count <= 12)
    );
    assert_eq!(rows.iter().filter(|r| r.closed_at.is_none()).count(), 1);
    assert!(
        rows.iter()
            .filter(|r| r.closed_at.is_some())
            .all(|r| r.close_reason.as_deref() == Some("expired"))
    );
}

#[tokio::test]
async fn relapse_resets_recovery_without_reopening_a_closed_record() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 30).await;
    let t = chrono::Utc::now().timestamp();
    for (offset, phase, value) in [
        (0, Onset, 82.0),
        (10, Resolved, 0.5),
        (20, Onset, 81.0),
        (30, Resolved, 0.5),
        (50, Resolved, 0.5),
        (70, Resolved, 0.5),
    ] {
        incidents::observe_at(&app.state, phase, &rule, &expr, "", value, t + offset)
            .await
            .unwrap();
    }
    let repo = IncidentRepository::new(app.state.db.clone());
    let rows = repo.list(10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].closed_at.is_none());
    assert_eq!(rows[0].recovery_started_at, Some(t + 30));
    assert_eq!(rows[0].violation_count, 2);
    incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + 90)
        .await
        .unwrap();
    let row = repo.get(rows[0].id).await.unwrap().unwrap();
    assert_eq!(row.closed_at, Some(t + 90));
    assert_eq!(row.recovery_started_at, Some(t + 30));
    assert_eq!(
        row.frames
            .iter()
            .map(|f| f.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["onset", "recovery", "relapse", "cleared"]
    );
    incidents::observe_at(&app.state, Onset, &rule, &expr, "", 95.0, t + 100)
        .await
        .unwrap();
    assert_eq!(repo.list(10).await.unwrap().len(), 2);
    assert_eq!(
        repo.get(row.id).await.unwrap().unwrap().closed_at,
        row.closed_at
    );
}

#[tokio::test]
async fn recovery_needs_healthy_evaluations_and_cannot_bridge_missing_data() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 30).await;
    let t = chrono::Utc::now().timestamp();
    incidents::observe_at(&app.state, Onset, &rule, &expr, "", 82.0, t)
        .await
        .unwrap();
    incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + 10)
        .await
        .unwrap();
    incidents::maintain(&app.state, t + 80).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let id = repo.list(1).await.unwrap()[0].id;
    assert!(
        repo.get(id).await.unwrap().unwrap().closed_at.is_none(),
        "a timer is not evidence of health"
    );
    incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + 140)
        .await
        .unwrap();
    let row = repo.get(id).await.unwrap().unwrap();
    assert_eq!(row.close_reason.as_deref(), Some("data_gap"));
    assert_eq!(row.frames.last().unwrap().kind, "interrupted");
}

#[tokio::test]
async fn repeated_confirmations_are_counted_without_spending_a_frame_each_time() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 0).await;
    let t = chrono::Utc::now().timestamp();
    for (offset, phase, value) in [
        (0, Onset, 82.0),
        (10, Escalation, 82.0),
        (20, Resolved, 0.5),
        (30, Onset, 81.0),
        (40, Escalation, 81.0),
        (50, Resolved, 0.5),
        (70, Resolved, 0.5),
        (90, Resolved, 0.5),
        (110, Resolved, 0.5),
    ] {
        incidents::observe_at(&app.state, phase, &rule, &expr, "", value, t + offset)
            .await
            .unwrap();
    }
    let repo = IncidentRepository::new(app.state.db.clone());
    let rows = repo.list(10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].violation_count, 2);
    assert_eq!(rows[0].confirmation_count, 2);
    let row = repo.get(rows[0].id).await.unwrap().unwrap();
    assert_eq!(row.close_reason.as_deref(), Some("resolved"));
    assert_eq!(
        row.frames.iter().filter(|f| f.kind == "escalation").count(),
        1
    );
    assert_eq!(row.frames.last().unwrap().kind, "resolution");
}

#[tokio::test]
async fn rule_changes_interrupt_a_recovery_window() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (rule, expr) = arm_cpu_rule(&app, &token, 30).await;
    let t = chrono::Utc::now().timestamp();
    incidents::observe_at(&app.state, Onset, &rule, &expr, "", 82.0, t)
        .await
        .unwrap();
    incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + 10)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_rules SET expression='cpu.usage_percent > 90' WHERE id=?")
        .bind(rule.id)
        .execute(&app.state.db)
        .await
        .unwrap();
    incidents::maintain(&app.state, t + 20).await;
    let repo = IncidentRepository::new(app.state.db.clone());
    let row = repo.list(1).await.unwrap().remove(0);
    assert_eq!(row.close_reason.as_deref(), Some("rule_changed"));
    assert_eq!(row.closed_at, Some(t + 20));
}

#[tokio::test]
async fn recovery_hold_uses_evaluation_cadence_not_alarm_confirmation_duration() {
    use crate::services::incidents::AlertPhase::*;
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (mut rule, expr) = arm_cpu_rule(&app, &token, 600).await;
    rule.eval_interval_secs = 45;
    let t = chrono::Utc::now().timestamp();
    incidents::observe_at(&app.state, Onset, &rule, &expr, "", 82.0, t)
        .await
        .unwrap();
    for offset in [10, 55, 99] {
        incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + offset)
            .await
            .unwrap();
    }
    let repo = IncidentRepository::new(app.state.db.clone());
    let rows = repo.list(1).await.unwrap();
    assert!(rows[0].closed_at.is_none());
    let context: Value = serde_json::from_str(rows[0].trigger_context.as_ref().unwrap()).unwrap();
    assert_eq!(context["capture_policy"]["recovery_hold_secs"], 90);
    incidents::observe_at(&app.state, Resolved, &rule, &expr, "", 0.5, t + 100)
        .await
        .unwrap();
    assert_eq!(
        repo.get(rows[0].id).await.unwrap().unwrap().closed_at,
        Some(t + 100)
    );
}
