//! Alert actions: binding CRUD and validation, the guardrail gauntlet, the
//! propose → confirm → run lifecycle, and the circuit breaker.
//!
//! Execution tests build an `ActionManifest` in memory rather than dropping a
//! file in a temp directory: the runner takes argv, so a portable "succeed" /
//! "fail" command is one line per platform and the suite stays honest on
//! Windows, where a `.sh` would simply not run.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use super::TestApp;
use crate::actions::manifest::ActionManifest;
use crate::config::ActionsConfig;
use crate::models::action::ActionTrigger;
use crate::models::alert::AlertSeverity;
use crate::services::actions::{AlertContext, dispatch};

/// argv that exits 0 on this host.
fn ok_command() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), "exit 0".into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]
    }
}

/// argv that exits non-zero on this host.
fn fail_command() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), "exit 3".into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), "exit 3".into()]
    }
}

/// Put a script action in the registry without touching the filesystem.
async fn register_action(app: &TestApp, name: &str, command: Vec<String>) {
    let manifest = ActionManifest {
        name: name.to_string(),
        description: Some("test action".into()),
        enabled: true,
        timeout: std::time::Duration::from_secs(10),
        command,
        platforms: Vec::new(),
        env: HashMap::new(),
        run_as_user: None,
        memory_limit_mb: None,
        source_path: std::path::PathBuf::from("<test>"),
    };
    app.state
        .action_registry
        .write()
        .await
        .actions
        .insert(name.to_string(), manifest);
}

async fn create_rule(app: &TestApp, token: &str, name: &str) -> i64 {
    let (st, body) = app
        .request(
            "POST",
            "/alerts",
            Some(token),
            Some(json!({
                "name": name,
                "expression": "cpu.usage_percent > 80",
                "severity": "warn",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "create rule failed: {body}");
    body["id"].as_i64().expect("rule id")
}

async fn create_binding(
    app: &TestApp,
    token: &str,
    rule_id: i64,
    extra: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut body = json!({ "kind": "script", "target": "restart-worker" });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    app.request(
        "POST",
        &format!("/alerts/{rule_id}/actions"),
        Some(token),
        Some(body),
    )
    .await
}

fn ctx_for(rule_id: i64, trigger: ActionTrigger) -> AlertContext {
    AlertContext {
        rule_id,
        rule_name: "test-rule".into(),
        severity: AlertSeverity::Warn,
        label_set: "{}".into(),
        trigger,
        metric_value: Some(93.0),
    }
}

/// Every run row for a binding, newest first.
async fn runs(app: &TestApp, token: &str, action_id: i64) -> Vec<serde_json::Value> {
    let (st, body) = app
        .request(
            "GET",
            &format!("/actions/runs?action_id={action_id}"),
            Some(token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "list runs failed: {body}");
    body["runs"].as_array().cloned().unwrap_or_default()
}

// ===== catalogue =====

#[tokio::test]
async fn catalogue_lists_scripts_and_the_builtin_kinds() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;

    let (st, body) = app.request("GET", "/actions", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);
    // The host's ceilings travel with the catalogue so a client can explain
    // why an `auto` binding is not going to run.
    assert_eq!(body["enabled"], true);
    assert_eq!(body["auto"], false, "auto must be opt-in");

    let entries = body["entries"].as_array().expect("entries");
    let script = entries
        .iter()
        .find(|e| e["target"] == "restart-worker")
        .expect("the registered script is listed");
    assert_eq!(script["kind"], "script");
    assert!(script["verbs"].as_array().unwrap().is_empty());

    let service = entries
        .iter()
        .find(|e| e["kind"] == "service")
        .expect("service catalogue entry");
    let verbs: Vec<&str> = service["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(verbs, vec!["start", "stop", "restart", "reload"]);
}

// ===== binding CRUD + validation =====

#[tokio::test]
async fn binding_defaults_to_manual() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;

    let (st, body) = create_binding(&app, &token, rule_id, json!({})).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    // The safe default is the whole safety story: a binding created without
    // an opinion asks before it acts.
    assert_eq!(body["mode"], "manual");
    assert_eq!(body["on_event"], "fired");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["summary"], "run script restart-worker");
}

#[tokio::test]
async fn binding_to_an_unloaded_script_is_rejected_at_write_time() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;

    // A typo that only announces itself the next time the rule fires is the
    // worst possible time to learn about it.
    let (st, body) =
        create_binding(&app, &token, rule_id, json!({ "target": "restrt-workr" })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("restrt-workr"),
        "the error should name the missing script: {body}"
    );
}

#[tokio::test]
async fn catalogue_bindings_need_a_verb_and_scripts_refuse_one() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;

    let (st, body) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "kind": "service", "target": "nginx.service" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    let (st, body) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "kind": "script", "target": "restart-worker", "verb": "restart" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a script has no verb: {body}");

    // `reload` is a service verb; a container has no equivalent.
    let (st, body) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "kind": "container", "target": "api", "verb": "reload" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    let (st, body) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "kind": "service", "target": "nginx.service", "verb": "restart" }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["summary"], "restart service nginx.service");
}

#[tokio::test]
async fn guardrail_ranges_are_enforced() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;

    for bad in [
        json!({ "max_runs_per_hour": 0 }),
        json!({ "max_runs_per_hour": 1000 }),
        json!({ "failure_limit": 0 }),
        json!({ "cooldown_secs": -1 }),
        json!({ "cooldown_secs": 999_999 }),
    ] {
        let (st, body) = create_binding(&app, &token, rule_id, bad.clone()).await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "{bad} should be refused: {body}"
        );
    }
}

#[tokio::test]
async fn a_binding_dies_with_its_rule() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let binding_id = binding["id"].as_i64().unwrap();

    let (st, _) = app
        .request("DELETE", &format!("/alerts/{rule_id}"), Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, _) = app
        .request(
            "GET",
            &format!("/actions/bindings/{binding_id}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "a binding with no rule can never fire again"
    );
}

// ===== the guardrail gauntlet =====

#[tokio::test]
async fn a_manual_binding_proposes_instead_of_acting() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows.len(), 1, "one proposal per transition: {rows:?}");
    assert_eq!(rows[0]["status"], "pending");
    assert!(
        rows[0]["expires_at"].as_i64().is_some(),
        "a proposal nobody answers must not sit there forever"
    );
}

#[tokio::test]
async fn an_auto_binding_will_not_run_while_the_host_switch_is_off() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "mode": "auto" })).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows[0]["status"], "skipped");
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("actions.auto"),
        "the skip has to say which switch stopped it: {rows:?}"
    );
}

#[tokio::test]
async fn an_armed_auto_binding_runs_and_records_its_outcome() {
    let app = TestApp::spawn_with_actions(ActionsConfig {
        auto: true,
        ..Default::default()
    })
    .await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "mode": "auto" })).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows[0]["status"], "succeeded", "{rows:?}");
    assert_eq!(rows[0]["origin"], "auto");
    assert_eq!(rows[0]["exit_code"], 0);
    assert!(rows[0]["finished_at"].as_i64().is_some());
}

#[tokio::test]
async fn on_event_decides_which_transition_a_binding_hears() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) =
        create_binding(&app, &token, rule_id, json!({ "on_event": "resolved" })).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    assert!(
        runs(&app, &token, action_id).await.is_empty(),
        "a resolved-only binding must not even leave a skip on a fire"
    );

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Resolved))
        .await
        .expect("dispatch");
    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["trigger_event"], "resolved");
}

#[tokio::test]
async fn a_second_fire_does_not_stack_a_second_proposal() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows.len(), 2, "the refusal is recorded, not silent");
    let pending: Vec<_> = rows.iter().filter(|r| r["status"] == "pending").collect();
    assert_eq!(pending.len(), 1, "single-flight: {rows:?}");
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("in flight"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn the_hourly_ceiling_stops_a_flapping_rule() {
    let app = TestApp::spawn_with_actions(ActionsConfig {
        auto: true,
        ..Default::default()
    })
    .await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    // No cooldown, so the ceiling is the only thing left holding the line.
    let (_, binding) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "mode": "auto", "cooldown_secs": 0, "max_runs_per_hour": 2 }),
    )
    .await;
    let action_id = binding["id"].as_i64().unwrap();

    for _ in 0..4 {
        dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
            .await
            .expect("dispatch");
    }

    let rows = runs(&app, &token, action_id).await;
    let executed = rows.iter().filter(|r| r["status"] == "succeeded").count();
    assert_eq!(executed, 2, "exactly the budget, no more: {rows:?}");
    let ceilinged = rows
        .iter()
        .filter(|r| {
            r["message"]
                .as_str()
                .unwrap_or_default()
                .contains("hourly ceiling")
        })
        .count();
    assert_eq!(ceilinged, 2, "and the rest say why: {rows:?}");
}

#[tokio::test]
async fn cooldown_holds_a_binding_between_runs() {
    let app = TestApp::spawn_with_actions(ActionsConfig {
        auto: true,
        ..Default::default()
    })
    .await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "mode": "auto", "cooldown_secs": 600, "max_runs_per_hour": 10 }),
    )
    .await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(
        rows.iter().filter(|r| r["status"] == "succeeded").count(),
        1
    );
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cooldown"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn cooldown_is_per_target_not_per_binding() {
    let app = TestApp::spawn_with_actions(ActionsConfig {
        auto: true,
        ..Default::default()
    })
    .await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "disk-full").await;
    let (_, binding) = create_binding(
        &app,
        &token,
        rule_id,
        json!({ "mode": "auto", "cooldown_secs": 600, "max_runs_per_hour": 10 }),
    )
    .await;
    let action_id = binding["id"].as_i64().unwrap();

    // One rule, two disks: acting on `/` must not cool off `/var`.
    for mount in ["/", "/var"] {
        let mut ctx = ctx_for(rule_id, ActionTrigger::Fired);
        ctx.label_set = format!(r#"{{"mount_point":"{mount}"}}"#);
        dispatch(&app.state, &ctx).await.expect("dispatch");
    }

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(
        rows.iter().filter(|r| r["status"] == "succeeded").count(),
        2,
        "each target has its own lifecycle: {rows:?}"
    );
}

#[tokio::test]
async fn a_dry_run_records_what_it_would_have_done_and_stops() {
    let app = TestApp::spawn_with_actions(ActionsConfig {
        auto: true,
        ..Default::default()
    })
    .await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "mode": "dry_run" })).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows[0]["status"], "skipped");
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("would run script restart-worker"),
        "{rows:?}"
    );
    // A dry run consumed nothing, so it must not have started a cooldown.
    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["status"] == "skipped"));
}

#[tokio::test]
async fn a_disabled_binding_is_not_even_considered() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "enabled": false })).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    assert!(runs(&app, &token, action_id).await.is_empty());
}

// ===== propose → confirm =====

#[tokio::test]
async fn confirming_a_proposal_runs_it_once() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    let run_id = runs(&app, &token, action_id).await[0]["id"]
        .as_i64()
        .unwrap();

    let (st, body) = app
        .request(
            "POST",
            &format!("/actions/runs/{run_id}/confirm"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // The response is the finished run, not an acknowledgement — the operator
    // who pressed the button sees the outcome.
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["origin"], "confirmed");
    assert!(body["requested_by"].as_str().is_some());

    // Second confirm is a conflict, not a second restart.
    let (st, _) = app
        .request(
            "POST",
            &format!("/actions/runs/{run_id}/confirm"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_dismissed_proposal_cannot_then_be_confirmed() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");
    let run_id = runs(&app, &token, action_id).await[0]["id"]
        .as_i64()
        .unwrap();

    let (st, body) = app
        .request(
            "POST",
            &format!("/actions/runs/{run_id}/dismiss"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "dismissed");

    let (st, _) = app
        .request(
            "POST",
            &format!("/actions/runs/{run_id}/confirm"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn pending_runs_are_the_operators_inbox() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    create_binding(&app, &token, rule_id, json!({})).await;

    dispatch(&app.state, &ctx_for(rule_id, ActionTrigger::Fired))
        .await
        .expect("dispatch");

    let (st, body) = app
        .request("GET", "/actions/runs?status=pending", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["runs"].as_array().unwrap().len(), 1);

    let (st, body) = app
        .request("GET", "/actions/runs?status=nonsense", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

// ===== run-now and the breaker =====

#[tokio::test]
async fn run_now_executes_regardless_of_the_unattended_switches() {
    // `actions.auto` is off and the binding is `manual`: neither restrains a
    // run a person asked for by id.
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    let (st, body) = app
        .request(
            "POST",
            &format!("/actions/bindings/{action_id}/run"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["trigger_event"], "manual");
    assert_eq!(body["origin"], "confirmed");
}

#[tokio::test]
async fn a_failing_script_fails_the_run_and_keeps_its_output() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", fail_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    let (st, body) = app
        .request(
            "POST",
            &format!("/actions/bindings/{action_id}/run"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["exit_code"], 3);
    assert!(
        body["message"].as_str().unwrap_or_default().contains("3"),
        "the message should carry the exit code: {body}"
    );
}

#[tokio::test]
async fn consecutive_failures_disarm_the_binding() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", fail_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "failure_limit": 2 })).await;
    let action_id = binding["id"].as_i64().unwrap();

    for _ in 0..2 {
        let (st, _) = app
            .request(
                "POST",
                &format!("/actions/bindings/{action_id}/run"),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
    }

    let (st, body) = app
        .request(
            "GET",
            &format!("/actions/bindings/{action_id}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["enabled"], false, "the breaker must trip: {body}");
    assert_eq!(body["consecutive_failures"], 2);
    assert!(
        body["disabled_reason"].as_str().is_some(),
        "and say so, so it reads differently from an operator switching it off"
    );
}

#[tokio::test]
async fn a_success_clears_the_failure_streak() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", fail_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "failure_limit": 3 })).await;
    let action_id = binding["id"].as_i64().unwrap();

    let path = format!("/actions/bindings/{action_id}/run");
    app.request("POST", &path, Some(&token), None).await;
    // Swap the script for one that works, then run again.
    register_action(&app, "restart-worker", ok_command()).await;
    app.request("POST", &path, Some(&token), None).await;

    let (_, body) = app
        .request(
            "GET",
            &format!("/actions/bindings/{action_id}"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(
        body["consecutive_failures"], 0,
        "the streak is *consecutive*: {body}"
    );
    assert_eq!(body["enabled"], true);
}

#[tokio::test]
async fn re_enabling_a_tripped_binding_clears_the_breaker() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", fail_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "failure_limit": 1 })).await;
    let action_id = binding["id"].as_i64().unwrap();

    app.request(
        "POST",
        &format!("/actions/bindings/{action_id}/run"),
        Some(&token),
        None,
    )
    .await;

    let (st, body) = app
        .request(
            "PUT",
            &format!("/actions/bindings/{action_id}"),
            Some(&token),
            Some(json!({
                "kind": "script",
                "target": "restart-worker",
                "enabled": true,
                "failure_limit": 1,
            })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    // Turning it back on is itself the operator's judgement that the past
    // failures are no longer the current situation.
    assert_eq!(body["consecutive_failures"], 0);
    assert!(body["disabled_reason"].is_null());
}

#[tokio::test]
async fn a_run_whose_script_vanished_fails_loudly() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    let (_, binding) = create_binding(&app, &token, rule_id, json!({})).await;
    let action_id = binding["id"].as_i64().unwrap();

    // A reload that drops the file leaves the binding pointing at nothing.
    app.state.action_registry.write().await.actions.clear();

    let (st, body) = app
        .request(
            "POST",
            &format!("/actions/bindings/{action_id}/run"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "a no-op that looks like success is the one outcome to avoid: {body}"
    );
}

#[tokio::test]
async fn orphaned_runs_are_released_at_startup() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;
    register_action(&app, "restart-worker", ok_command()).await;
    let rule_id = create_rule(&app, &token, "cpu-hot").await;
    // No cooldown, so the only thing that could hold the next fire back is the
    // stuck latch this test is about.
    let (_, binding) = create_binding(&app, &token, rule_id, json!({ "cooldown_secs": 0 })).await;
    let action_id = binding["id"].as_i64().unwrap();

    // Stand in for a process that died mid-execution: a `running` row nothing
    // is going to finish. Left alone it holds the single-flight latch shut.
    sqlx::query(
        "INSERT INTO action_runs
            (action_id, rule_id, rule_name, label_set, action_kind, action_target,
             trigger_event, origin, status, created_at)
         VALUES (?, ?, 'cpu-hot', '{}', 'script', 'restart-worker', 'fired', 'auto',
                 'running', unixepoch())",
    )
    .bind(action_id)
    .bind(rule_id)
    .execute(&app.state.db)
    .await
    .expect("seed an orphaned run");

    crate::services::actions::recover_orphaned_runs(&app.state.db).await;

    let rows = runs(&app, &token, action_id).await;
    assert_eq!(rows[0]["status"], "failed");
    assert!(
        rows[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("restarted"),
        "{rows:?}"
    );

    // And the binding can act again.
    dispatch(
        &Arc::clone(&app.state),
        &ctx_for(rule_id, ActionTrigger::Fired),
    )
    .await
    .expect("dispatch");
    let rows = runs(&app, &token, action_id).await;
    assert!(rows.iter().any(|r| r["status"] == "pending"), "{rows:?}");
}
