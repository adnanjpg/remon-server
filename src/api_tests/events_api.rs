//! Unified event-timeline tests: the `GET /events` union over the
//! host-event ledger, alert fire/resolve, and incident captures — merge
//! order, filters, validation — plus the operator-audit write path driven
//! through a real mutating endpoint.

use axum::http::StatusCode;
use serde_json::json;

use super::TestApp;
use crate::models::alert::{AlertEventType, AlertSeverity};
use crate::services::incidents;
use crate::storage::repositories::{
    AlertRepository, HostEventRepository, NewHostEvent, UpsertAlertRule,
};

async fn seed_rule(app: &TestApp, name: &str) -> i64 {
    AlertRepository::new(app.state.db.clone())
        .insert(&UpsertAlertRule {
            name: name.to_string(),
            description: None,
            enabled: true,
            expression: "cpu.usage_percent > 80".to_string(),
            severity: AlertSeverity::Crit,
            for_duration_secs: 30,
            eval_interval_secs: 10,
            cooldown_secs: 900,
            silenced_until: None,
        })
        .await
        .expect("create rule")
}

#[tokio::test]
async fn events_require_auth() {
    let app = TestApp::spawn().await;
    let (st, _) = app.request("GET", "/events", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn events_union_merges_all_three_stores() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();

    // Ledger row (system), stamped older than the rest.
    HostEventRepository::new(app.state.db.clone())
        .insert(&NewHostEvent {
            created_at: Some(now - 600),
            source: "system",
            kind: "boot",
            severity: "warn",
            message: "Host booted after unclean shutdown".to_string(),
            ..Default::default()
        })
        .await
        .expect("insert host event");

    // Alert transition (crit fired → error-severity event).
    let rule_id = seed_rule(&app, "cpu crit").await;
    AlertRepository::new(app.state.db.clone())
        .insert_event(
            rule_id,
            "{}",
            AlertEventType::Fired,
            AlertSeverity::Crit,
            Some(93.5),
            true,
        )
        .await
        .expect("insert alert event");

    // Incident capture (manual → operator source).
    incidents::capture_manual(&app.state, "smoke", "custom")
        .await
        .expect("capture");

    let (st, body) = app.request("GET", "/events", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK, "got: {body}");
    // Three seeded + the device_paired row pair_and_login itself audits.
    assert_eq!(body["count"], 4, "got: {body}");

    let events = body["events"].as_array().expect("events array");
    // Newest first.
    let ts: Vec<i64> = events.iter().map(|e| e["ts"].as_i64().unwrap()).collect();
    assert!(ts.windows(2).all(|w| w[0] >= w[1]), "not sorted: {ts:?}");
    // The old boot row sits last.
    let boot = events.last().expect("non-empty");
    assert_eq!(boot["kind"], "boot");
    assert_eq!(boot["source"], "system");
    assert_eq!(boot["severity"], "warn");

    let fired = events
        .iter()
        .find(|e| e["kind"] == "alert_fired")
        .expect("alert_fired present");
    assert_eq!(fired["severity"], "error", "crit fired maps to error");
    assert_eq!(fired["ref"]["type"], "alert_rule");
    assert_eq!(fired["ref"]["id"], rule_id.to_string());
    assert!(
        fired["message"].as_str().unwrap().contains("cpu crit"),
        "got: {fired}"
    );

    let incident = events
        .iter()
        .find(|e| e["kind"] == "incident_captured")
        .expect("incident present");
    assert_eq!(incident["source"], "operator", "manual trigger → operator");
    assert_eq!(incident["ref"]["type"], "incident");
    assert_eq!(incident["details"]["trigger"], "manual");
}

#[tokio::test]
async fn events_filter_by_kind_and_source() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let repo = HostEventRepository::new(app.state.db.clone());
    repo.insert(&NewHostEvent {
        source: "system",
        kind: "oom_kill",
        severity: "error",
        message: "Kernel OOM killer terminated 'chrome' (pid 1)".to_string(),
        ..Default::default()
    })
    .await
    .expect("insert");
    repo.insert(&NewHostEvent {
        source: "operator",
        kind: "service_action",
        severity: "info",
        message: "Service 'nginx' restarted".to_string(),
        actor_device_id: Some("dev-1".to_string()),
        actor_name: Some("phone".to_string()),
        ..Default::default()
    })
    .await
    .expect("insert");

    let (st, body) = app
        .request("GET", "/events?kinds=oom_kill", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 1, "got: {body}");
    assert_eq!(body["events"][0]["kind"], "oom_kill");

    // Operator source: the seeded service_action plus the device_paired
    // row from pairing itself — and nothing system-sourced.
    let (st, body) = app
        .request("GET", "/events?sources=operator", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 2, "got: {body}");
    let action = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "service_action")
        .expect("service_action present");
    assert_eq!(action["actor"]["name"], "phone");

    // A kinds filter that names no alert/incident projection must not
    // drag those stores into the response.
    seed_rule(&app, "quiet rule").await;
    let (_, body) = app
        .request("GET", "/events?kinds=service_action", Some(&token), None)
        .await;
    assert_eq!(body["count"], 1, "got: {body}");
}

#[tokio::test]
async fn events_validate_range_and_source() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request("GET", "/events?start=100&end=50", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, body) = app
        .request("GET", "/events?sources=bogus", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("bogus")),
        "got: {body}"
    );
}

#[tokio::test]
async fn events_range_excludes_outside_rows() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();

    let repo = HostEventRepository::new(app.state.db.clone());
    for (ts, kind) in [(now - 7200, "boot"), (now - 60, "server_started")] {
        repo.insert(&NewHostEvent {
            created_at: Some(ts),
            source: "system",
            kind: if kind == "boot" {
                "boot"
            } else {
                "server_started"
            },
            severity: "info",
            message: kind.to_string(),
            ..Default::default()
        })
        .await
        .expect("insert");
    }

    // Kinds-scoped so the pairing audit row can't leak into the count.
    let uri = format!(
        "/events?start={}&end={}&kinds=boot,server_started",
        now - 3600,
        now
    );
    let (st, body) = app.request("GET", &uri, Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 1, "got: {body}");
    assert_eq!(body["events"][0]["kind"], "server_started");
}

/// The operator-audit write path end-to-end: a real mutating endpoint
/// (alert silence) must land an attributed row in the ledger. The insert is
/// fire-and-forget, so poll briefly instead of asserting immediately.
#[tokio::test]
async fn alert_silence_writes_audit_event() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let rule_id = seed_rule(&app, "to silence").await;

    let (st, body) = app
        .request(
            "POST",
            &format!("/alerts/{rule_id}/silence"),
            Some(&token),
            Some(json!({ "duration_secs": 3600 })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "silence failed: {body}");

    let mut found = None;
    for _ in 0..100 {
        let (_, body) = app
            .request("GET", "/events?kinds=alert_silenced", Some(&token), None)
            .await;
        if body["count"] == 1 {
            found = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let body = found.expect("audit event should appear");
    let ev = &body["events"][0];
    assert_eq!(ev["source"], "operator");
    assert_eq!(ev["ref"]["type"], "alert_rule");
    assert_eq!(ev["ref"]["id"], rule_id.to_string());
    // Attribution: the paired device's name resolves onto the row.
    assert_eq!(ev["actor"]["name"], "integration-test", "got: {ev}");
    assert!(
        ev["message"].as_str().unwrap().contains("to silence"),
        "got: {ev}"
    );
}
