//! Heartbeat checks: CRUD, capability-URL ping surface, pause semantics,
//! and the alert-engine integration (resolver + vanished-target prune).

use axum::http::StatusCode;
use serde_json::json;

use super::TestApp;

async fn create_check(app: &TestApp, token: &str, name: &str, body_extra: serde_json::Value) -> (i64, String) {
    let mut body = json!({
        "name": name,
        "period_secs": 60,
        "grace_secs": 30,
    });
    body.as_object_mut()
        .unwrap()
        .extend(body_extra.as_object().cloned().unwrap_or_default());
    let (st, body) = app
        .request("POST", "/heartbeats", Some(token), Some(body))
        .await;
    assert_eq!(st, StatusCode::CREATED, "create failed: {body}");
    let id = body["id"].as_i64().expect("check id");
    let slug = body["slug"].as_str().expect("slug shown once").to_string();
    assert_eq!(body["ping_path"], format!("/ping/{slug}"));
    (id, slug)
}

async fn check_state(app: &TestApp, token: &str, id: i64) -> String {
    let (st, body) = app
        .request("GET", &format!("/heartbeats/{id}"), Some(token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    body["state"].as_str().expect("state").to_string()
}

#[tokio::test]
async fn create_ping_lifecycle() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (id, slug) = create_check(&app, &token, "db-backup", json!({})).await;
    assert_eq!(check_state(&app, &token, id).await, "waiting");

    // Anonymous success ping flips it up.
    let (st, body) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK, "ping failed: {body}");
    assert_eq!(check_state(&app, &token, id).await, "up");

    // The ping is on the log, newest first.
    let (st, body) = app
        .request("GET", &format!("/heartbeats/{id}/pings"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let pings = body["pings"].as_array().expect("pings");
    assert_eq!(pings.len(), 1);
    assert_eq!(pings[0]["kind"], "success");
    assert_eq!(pings[0]["source_ip"], "127.0.0.1");
}

#[tokio::test]
async fn unknown_and_malformed_slugs_404_uniformly() {
    let app = TestApp::spawn().await;

    // Well-formed but unknown.
    let (st, _) = app
        .request("GET", "/ping/00112233445566778899aabbccddeeff", None, None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Malformed shapes — same 404, no oracle.
    for slug in [
        "short",
        "ZZ112233445566778899AABBCCDDEEFF",
        "00112233445566778899aabbccddeeff0", // 33 chars
    ] {
        let (st, _) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "slug {slug:?} must 404");
    }
}

#[tokio::test]
async fn exit_code_path_and_fail_latch() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "cron-job", json!({})).await;

    // Nonzero exit latches failed; body is captured on the log row.
    let (st, _) = app
        .request(
            "POST",
            &format!("/ping/{slug}/7"),
            None,
            Some(json!({"trace": "boom"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "failed");

    // Healthy window elapsing does not clear an explicit fail…
    let (st, body) = app
        .request("GET", &format!("/heartbeats/{id}/pings"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let pings = body["pings"].as_array().unwrap();
    assert_eq!(pings[0]["kind"], "fail");
    assert_eq!(pings[0]["exit_code"], 7);
    assert!(
        pings[0]["body"].as_str().unwrap().contains("boom"),
        "fail body must be captured"
    );

    // …but exit 0 (success spelling) does.
    let (st, _) = app.request("GET", &format!("/ping/{slug}/0"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "up");

    // /fail without body also latches.
    let (st, _) = app.request("POST", &format!("/ping/{slug}/fail"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "failed");

    // Junk tail = uniform 404.
    let (st, _) = app
        .request("GET", &format!("/ping/{slug}/not-a-code"), None, None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn service_pause_window_and_resume() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "deployer", json!({})).await;

    // Declared window.
    let (st, body) = app
        .request(
            "POST",
            &format!("/ping/{slug}/pause?duration=2h&reason=deploy"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "pause failed: {body}");
    assert_eq!(body["status"], "paused");
    assert_eq!(body["clamped"], false);
    assert_eq!(check_state(&app, &token, id).await, "paused");

    // Reason and origin surface on the check while active.
    let (_, body) = app
        .request("GET", &format!("/heartbeats/{id}"), Some(&token), None)
        .await;
    assert_eq!(body["pause_origin"], "service");
    assert_eq!(body["pause_reason"], "deploy");

    // A ping during a DECLARED window is recorded but does not resume.
    let (st, _) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "paused");

    // Explicit early resume ends it; fresh window → up, not down.
    let (st, _) = app
        .request("POST", &format!("/ping/{slug}/resume"), None, None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "up");

    // Resume with nothing active is idempotent.
    let (st, _) = app
        .request("POST", &format!("/ping/{slug}/resume"), None, None)
        .await;
    assert_eq!(st, StatusCode::OK);

    // Over-cap magnitude clamps and says so.
    let (st, body) = app
        .request(
            "POST",
            &format!("/ping/{slug}/pause?duration=48h"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["clamped"], true);

    // Malformed shape is a hard 400.
    let (st, _) = app
        .request(
            "POST",
            &format!("/ping/{slug}/pause?duration=3x"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = app
        .request(
            "POST",
            &format!("/ping/{slug}/pause?duration=1h&until=99999999999"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn bare_service_pause_auto_resumes_on_ping() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "flappy", json!({})).await;

    let (st, body) = app
        .request("POST", &format!("/ping/{slug}/pause"), None, None)
        .await;
    assert_eq!(st, StatusCode::OK, "bare pause failed: {body}");
    assert_eq!(check_state(&app, &token, id).await, "paused");

    // "Quiet until I ping again" — the next success ping lifts it.
    let (st, _) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "up");
}

#[tokio::test]
async fn operator_pause_beats_service() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "guarded", json!({})).await;

    // Operator pause, indefinite (empty body).
    let (st, body) = app
        .request(
            "POST",
            &format!("/heartbeats/{id}/pause"),
            Some(&token),
            Some(json!({})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "operator pause failed: {body}");
    assert_eq!(body["state"], "paused");
    assert_eq!(body["pause_origin"], "operator");
    assert!(body["paused_until"].is_null(), "indefinite has no end");

    // Service can neither pause over it nor resume it.
    let (st, _) = app
        .request("POST", &format!("/ping/{slug}/pause?duration=1h"), None, None)
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .request("POST", &format!("/ping/{slug}/resume"), None, None)
        .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // A success ping does not lift an operator pause either.
    let (st, _) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "paused");

    // Operator resume: fresh window, not instant-down.
    let (st, _) = app
        .request("DELETE", &format!("/heartbeats/{id}/pause"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(check_state(&app, &token, id).await, "up");
}

#[tokio::test]
async fn operator_pause_validation() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _) = create_check(&app, &token, "strict", json!({})).await;

    for bad in [
        json!({"until": 100, "duration_secs": 100}),
        json!({"until": 100}),          // in the past
        json!({"duration_secs": 0}),
        json!({"duration_secs": 100 * 86_400}), // over the 30d ceiling
    ] {
        let (st, _) = app
            .request(
                "POST",
                &format!("/heartbeats/{id}/pause"),
                Some(&token),
                Some(bad.clone()),
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "body {bad} must 400");
    }
}

#[tokio::test]
async fn disabled_checks_404_and_reenable_grants_a_fresh_window() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "seasonal", json!({})).await;

    // Latch a failure, then disable with the ping clock deep in the past.
    let (st, _) = app.request("POST", &format!("/ping/{slug}/fail"), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let past = chrono::Utc::now().timestamp() - 86_400;
    sqlx::query("UPDATE heartbeat_checks SET last_ping_at = ?, last_fail_at = ? WHERE id = ?")
        .bind(past)
        .bind(past)
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();
    let (st, _) = app
        .request(
            "PUT",
            &format!("/heartbeats/{id}"),
            Some(&token),
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(check_state(&app, &token, id).await, "disabled");

    // Disabled = the capability URL is dead, uniformly.
    for uri in [
        format!("/ping/{slug}"),
        format!("/ping/{slug}/fail"),
        format!("/ping/{slug}/7"),
    ] {
        let (st, _) = app.request("POST", &uri, None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} must 404 while disabled");
    }

    // Re-enable: fresh period+grace and a cleared fail latch — not an
    // instant down/failed page for a week-old silence.
    let (st, body) = app
        .request(
            "PUT",
            &format!("/heartbeats/{id}"),
            Some(&token),
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["state"], "up", "re-enable must grant a fresh window");
}

#[tokio::test]
async fn operator_pause_accepts_an_empty_body() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _) = create_check(&app, &token, "bodyless", json!({})).await;

    // No body at all — the documented indefinite form.
    let (st, body) = app
        .request("POST", &format!("/heartbeats/{id}/pause"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK, "empty body must pause: {body}");
    assert_eq!(body["state"], "paused");
    assert!(body["paused_until"].is_null());
}

#[tokio::test]
async fn update_clears_description_on_explicit_null() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _) = create_check(&app, &token, "described", json!({"description": "old"})).await;

    // Omitting the field leaves it alone…
    let (st, body) = app
        .request(
            "PUT",
            &format!("/heartbeats/{id}"),
            Some(&token),
            Some(json!({"period_secs": 90})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["description"], "old");

    // …an explicit null clears it.
    let (st, body) = app
        .request(
            "PUT",
            &format!("/heartbeats/{id}"),
            Some(&token),
            Some(json!({"description": null})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["description"].is_null(), "explicit null must clear");
}

#[tokio::test]
async fn oversized_fail_body_truncates_instead_of_413() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "verbose", json!({})).await;

    // 10 KiB of trace — over the old 8 KiB route limit, under the global
    // 64 KiB one. Must latch the failure and store a 4 KiB prefix.
    let big = "x".repeat(10 * 1024);
    let (st, _) = app
        .request(
            "POST",
            &format!("/ping/{slug}/fail"),
            None,
            Some(serde_json::Value::String(big)),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "oversized fail body must not 413");
    assert_eq!(check_state(&app, &token, id).await, "failed");

    let (_, body) = app
        .request("GET", &format!("/heartbeats/{id}/pings"), Some(&token), None)
        .await;
    let stored = body["pings"][0]["body"].as_str().expect("captured body");
    assert_eq!(stored.len(), 4096, "stored body must be capped at 4 KiB");
}

#[tokio::test]
async fn absurd_pause_duration_clamps_instead_of_overflowing() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "immortal", json!({})).await;

    let (st, body) = app
        .request(
            "POST",
            &format!("/ping/{slug}/pause?duration=9223372036854775807"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["clamped"], true);
    assert_eq!(check_state(&app, &token, id).await, "paused");
}

#[tokio::test]
async fn create_paused_skips_the_first_window() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _) = create_check(&app, &token, "prewired", json!({"paused": true})).await;
    assert_eq!(check_state(&app, &token, id).await, "paused");
}

#[tokio::test]
async fn crud_validation_and_conflicts() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _) = create_check(&app, &token, "unique-name", json!({})).await;

    // Duplicate name → 409, not a 500.
    let (st, _) = app
        .request(
            "POST",
            "/heartbeats",
            Some(&token),
            Some(json!({"name": "unique-name", "period_secs": 60})),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // Bad names / timings → 400.
    for body in [
        json!({"name": "Bad Name", "period_secs": 60}),
        json!({"name": "9starts-with-digit", "period_secs": 60}),
        json!({"name": "ok-name", "period_secs": 1}),
        json!({"name": "ok-name", "period_secs": 60, "grace_secs": -1}),
    ] {
        let (st, _) = app
            .request("POST", "/heartbeats", Some(&token), Some(body.clone()))
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "body {body} must 400");
    }

    // Selective update: only the sent fields change.
    let (st, body) = app
        .request(
            "PUT",
            &format!("/heartbeats/{id}"),
            Some(&token),
            Some(json!({"period_secs": 120})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["period_secs"], 120);
    assert_eq!(body["name"], "unique-name");
    assert_eq!(body["grace_secs"], 30);

    // Delete cascades the log and 404s afterwards.
    let (st, _) = app
        .request("DELETE", &format!("/heartbeats/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = app
        .request("GET", &format!("/heartbeats/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rotate_slug_invalidates_the_old_url() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, old_slug) = create_check(&app, &token, "rotated", json!({})).await;

    let (st, body) = app
        .request(
            "POST",
            &format!("/heartbeats/{id}/rotate-slug"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let new_slug = body["slug"].as_str().expect("new slug").to_string();
    assert_ne!(new_slug, old_slug);

    let (st, _) = app.request("GET", &format!("/ping/{old_slug}"), None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "old slug must be dead");
    let (st, _) = app.request("GET", &format!("/ping/{new_slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn heartbeat_endpoints_require_auth() {
    let app = TestApp::spawn().await;
    for (method, uri) in [
        ("GET", "/heartbeats"),
        ("POST", "/heartbeats"),
        ("GET", "/heartbeats/1"),
        ("DELETE", "/heartbeats/1"),
        ("POST", "/heartbeats/1/pause"),
        ("POST", "/heartbeats/1/rotate-slug"),
        ("GET", "/heartbeats/1/pings"),
    ] {
        let (st, _) = app.request(method, uri, None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{method} {uri} must 401");
    }
}

/// End-to-end alerting: `heartbeat.up < 1` fires when a check goes deaf
/// and the vanished-target prune resolves the Firing row when the check
/// is deleted (regression for the strand-forever bug).
#[tokio::test]
async fn alert_fires_on_down_and_prune_resolves_on_delete() {
    use crate::services::alerting::{evaluator, expression};
    use crate::storage::repositories::AlertRepository;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, slug) = create_check(&app, &token, "watched", json!({})).await;

    // A rule can exist before its check has ever pinged.
    let (st, body) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(json!({
                "name": "heartbeat down",
                "expression": "heartbeat.up < 1",
                "severity": "crit",
                "for_duration_secs": 0,
                "eval_interval_secs": 3,
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "rule create failed: {body}");

    let (st, _) = app.request("GET", &format!("/ping/{slug}"), None, None).await;
    assert_eq!(st, StatusCode::OK);

    // Push the last ping deep into the past — deaf for an hour on a
    // 60s+30s check.
    let past = chrono::Utc::now().timestamp() - 3600;
    sqlx::query("UPDATE heartbeat_checks SET last_ping_at = ? WHERE id = ?")
        .bind(past)
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();

    let repo = AlertRepository::new(app.state.db.clone());
    let rule = repo.list_enabled().await.unwrap().pop().expect("rule");
    let expr = expression::parse(&rule.expression).expect("parse");

    // Tick 1: ok → pending. Tick 2 (for=0): pending → firing.
    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    let states = repo.list_state_for_rule(rule.id).await.unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].label_set, r#"{"check":"watched"}"#);
    assert_eq!(
        states[0].state,
        crate::models::alert::AlertLifecycle::Firing
    );
    let events = repo.events_for_rule(rule.id, 10, 0).await.unwrap();
    assert_eq!(events.len(), 1, "one fired event");

    // Delete the check: its label_set vanishes from resolver output. The
    // prune must resolve the Firing row instead of stranding it.
    let (st, _) = app
        .request("DELETE", &format!("/heartbeats/{id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    let states = repo.list_state_for_rule(rule.id).await.unwrap();
    assert!(states.is_empty(), "firing row must be pruned, not stranded");
    let events = repo.events_for_rule(rule.id, 10, 0).await.unwrap();
    assert_eq!(events.len(), 2, "fired + synthetic resolved");
    assert_eq!(
        events[0].event_type,
        crate::models::alert::AlertEventType::Resolved
    );
}

/// Paused checks keep emitting up=1, so a firing alert resolves when the
/// operator declares maintenance instead of stranding.
#[tokio::test]
async fn declaring_pause_resolves_a_firing_alert() {
    use crate::services::alerting::{evaluator, expression};
    use crate::storage::repositories::AlertRepository;

    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let (id, _slug) = create_check(&app, &token, "maintained", json!({})).await;

    let (st, _) = app
        .request(
            "POST",
            "/alerts",
            Some(&token),
            Some(json!({
                "name": "hb", "expression": "heartbeat.up < 1", "severity": "warn",
                "for_duration_secs": 0, "eval_interval_secs": 3,
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);

    // Never pinged + created_at pushed past → down.
    let past = chrono::Utc::now().timestamp() - 3600;
    sqlx::query("UPDATE heartbeat_checks SET created_at = ? WHERE id = ?")
        .bind(past)
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();

    let repo = AlertRepository::new(app.state.db.clone());
    let rule = repo.list_enabled().await.unwrap().pop().unwrap();
    let expr = expression::parse(&rule.expression).unwrap();
    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    assert_eq!(
        repo.list_state_for_rule(rule.id).await.unwrap()[0].state,
        crate::models::alert::AlertLifecycle::Firing
    );

    // Operator declares maintenance → resolver emits up=1 → resolve.
    let (st, _) = app
        .request(
            "POST",
            &format!("/heartbeats/{id}/pause"),
            Some(&token),
            Some(json!({"duration_secs": 3600})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    evaluator::evaluate_once(&rule, &expr, &app.state).await.unwrap();
    assert_eq!(
        repo.list_state_for_rule(rule.id).await.unwrap()[0].state,
        crate::models::alert::AlertLifecycle::Ok
    );
}
