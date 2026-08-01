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
            "cpu crit",
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

/// Every store caps at `limit` rows, so a filter applied to the merged result
/// instead of in SQL returns an empty page as soon as the unwanted kind fills
/// the window on its own — while matching events sit just outside it. The
/// resolved rows are seeded last so they are the newest and would take the
/// whole limit for themselves.
#[tokio::test]
async fn kind_filter_survives_a_window_full_of_the_other_kind() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let rule_id = seed_rule(&app, "cpu crit").await;
    let repo = AlertRepository::new(app.state.db.clone());

    for _ in 0..3 {
        repo.insert_event(
            rule_id,
            "cpu crit",
            "{}",
            AlertEventType::Fired,
            AlertSeverity::Crit,
            Some(93.5),
            true,
        )
        .await
        .expect("insert fired");
    }
    for _ in 0..12 {
        repo.insert_event(
            rule_id,
            "cpu crit",
            "{}",
            AlertEventType::Resolved,
            AlertSeverity::Crit,
            Some(10.0),
            true,
        )
        .await
        .expect("insert resolved");
    }

    let (st, body) = app
        .request(
            "GET",
            "/events?kinds=alert_fired&limit=10",
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["count"], 3,
        "the fired rows must survive the cap: {body}"
    );
    assert!(
        body["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] == "alert_fired"),
        "got: {body}"
    );
}

/// Same shape for the incident projection, where the filter is on the trigger
/// rather than the kind: `system` means alert-triggered, `operator` everything
/// else. The manual rows are seeded last so they would fill the window.
#[tokio::test]
async fn source_filter_survives_a_window_full_of_the_other_source() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    for (trigger, count) in [("alert", 3), ("manual", 12)] {
        for _ in 0..count {
            sqlx::query(
                "INSERT INTO incident_snapshots (trigger_kind, category, bundle)
                 VALUES (?, 'resource', '{}')",
            )
            .bind(trigger)
            .execute(&app.state.db)
            .await
            .expect("insert incident");
        }
    }

    let (st, body) = app
        .request(
            "GET",
            "/events?kinds=incident_captured&sources=system&limit=10",
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["count"], 3,
        "the alert-triggered rows must survive: {body}"
    );
    assert!(
        body["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["source"] == "system" && e["details"]["trigger"] == "alert"),
        "got: {body}"
    );
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

/// Paging a merged timeline cannot cut on a timestamp alone. The three stores
/// have independent id spaces and routinely write the same second — an alert
/// firing and the capture it triggers are the standard case — so a `ts`-only
/// cursor either drops whatever shares the boundary or serves it twice.
///
/// `limit=1` puts the cursor *inside* the shared second twice over, which is
/// the shape that breaks. The loop is bounded because a cursor that fails to
/// advance does not return a wrong answer, it hangs.
#[tokio::test]
async fn paging_does_not_drop_or_repeat_events_sharing_a_second() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let now = chrono::Utc::now().timestamp();
    let shared = now - 100;

    // Three stores, one second.
    HostEventRepository::new(app.state.db.clone())
        .insert(&NewHostEvent {
            created_at: Some(shared),
            source: "system",
            kind: "boot",
            severity: "warn",
            message: "boot at the shared second".to_string(),
            ..Default::default()
        })
        .await
        .expect("insert ledger row");

    let rule_id = seed_rule(&app, "shared-second rule").await;
    sqlx::query(
        "INSERT INTO alert_events
           (rule_id, rule_name, label_set, event_type, severity, occurred_at, metric_value, notified)
         VALUES (?, 'shared-second rule', '{}', 'fired', 'crit', ?, 99.0, 1)",
    )
    .bind(rule_id)
    .bind(shared)
    .execute(&app.state.db)
    .await
    .expect("insert alert event");

    sqlx::query(
        "INSERT INTO incident_snapshots (created_at, trigger_kind, category, bundle)
         VALUES (?, 'manual', 'resource', '{}')",
    )
    .bind(shared)
    .execute(&app.state.db)
    .await
    .expect("insert incident");

    // Two more seconds either side, so the cursor also has to cross a boundary
    // where only one store has anything.
    for (offset, msg) in [(1i64, "older ledger row"), (-1, "newer ledger row")] {
        HostEventRepository::new(app.state.db.clone())
            .insert(&NewHostEvent {
                created_at: Some(shared - offset),
                source: "system",
                kind: "boot",
                severity: "warn",
                message: msg.to_string(),
                ..Default::default()
            })
            .await
            .expect("insert ledger row");
    }

    let path = "/events?kinds=boot,alert_fired,incident_captured&limit=1";
    let mut seen: Vec<(i64, String, String)> = Vec::new();
    let mut cursor: Option<String> = None;

    for _ in 0..20 {
        let url = match &cursor {
            Some(c) => format!("{path}&cursor={c}"),
            None => path.to_string(),
        };
        let (st, body) = app.request("GET", &url, Some(&token), None).await;
        assert_eq!(st, StatusCode::OK, "got: {body}");

        for e in body["events"].as_array().expect("events array") {
            seen.push((
                e["ts"].as_i64().expect("ts"),
                e["kind"].as_str().expect("kind").to_string(),
                e["message"].as_str().unwrap_or_default().to_string(),
            ));
        }
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }

    assert_eq!(
        seen.len(),
        5,
        "expected every seeded event exactly once, got {seen:?}"
    );
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        seen.len(),
        "an event was served twice: {seen:?}"
    );

    // Newest first, and the shared second is contiguous rather than split.
    let ts: Vec<i64> = seen.iter().map(|(t, _, _)| *t).collect();
    assert_eq!(
        ts,
        vec![shared + 1, shared, shared, shared, shared - 1],
        "page boundaries reordered the timeline"
    );
}

/// The event log outlives the rule it audits. Deleting a rule used to cascade
/// its events away, which meant an operator could erase the evidence that a
/// rule had ever fired by removing the rule; the timeline would also have
/// hidden them regardless, because it reached the rule's name through an inner
/// join. Both are why the name is on the row now.
#[tokio::test]
async fn deleting_a_rule_keeps_the_alerts_it_fired() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let repo = AlertRepository::new(app.state.db.clone());

    let rule_id = seed_rule(&app, "doomed rule").await;
    repo.insert_event(
        rule_id,
        "doomed rule",
        "{}",
        AlertEventType::Fired,
        AlertSeverity::Crit,
        Some(93.5),
        true,
    )
    .await
    .expect("insert alert event");

    assert!(repo.delete(rule_id).await.expect("delete rule"));

    let (st, body) = app
        .request("GET", "/events?kinds=alert_fired", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK, "got: {body}");
    assert_eq!(body["count"], 1, "the event went with the rule: {body}");

    let fired = &body["events"][0];
    assert!(
        fired["message"].as_str().unwrap().contains("doomed rule"),
        "the event no longer says what it was about: {fired}"
    );
    assert!(
        fired["ref"].is_null(),
        "a deleted rule must not be offered as a link: {fired}"
    );
}
