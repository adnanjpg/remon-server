//! Runtime config: GET reflects effective state, PATCH validates and persists.

use super::TestApp;
use axum::http::StatusCode;

#[tokio::test]
async fn get_config_returns_effective_state() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app.request("GET", "/config", Some(&token), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["server_name"], "test-server");
    assert_eq!(body["collector_stats_interval_ms"], 2000);
    // The container-stats collector interval is now a live config field.
    assert_eq!(body["collector_docker_interval_ms"], 3000);
    assert_eq!(body["collector_smart_interval_ms"], 1_800_000);
    assert!(body["updated_at"].as_i64().is_some());
}

#[tokio::test]
async fn patch_config_rejects_subsecond_interval() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({ "collector_stats_interval_ms": 500 })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_config_rejects_overlong_server_name() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let long_name = "n".repeat(200);
    let (st, _) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({ "server_name": long_name })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_config_applies_and_persists() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({
                "server_name": "renamed",
                "collector_stats_interval_ms": 3000,
            })),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["server_name"], "renamed");
    assert_eq!(body["collector_stats_interval_ms"], 3000);

    // Reflected on a subsequent GET (read-back through the DB + live state).
    let (_, body) = app.request("GET", "/config", Some(&token), None).await;
    assert_eq!(body["server_name"], "renamed");
    assert_eq!(body["collector_stats_interval_ms"], 3000);
}

#[tokio::test]
async fn patch_config_rejects_subminute_smart_interval() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "PATCH",
            "/config",
            Some(&token),
            Some(serde_json::json!({ "collector_smart_interval_ms": 30_000 })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// ─── Retention policy ───────────────────────────────────────────────────────

#[tokio::test]
async fn get_retention_lists_seeded_policies() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request("GET", "/config/retention", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let policies = body["policies"].as_array().expect("policies array");
    assert!(!policies.is_empty());
    assert!(
        policies
            .iter()
            .any(|p| p["resource"] == "cpu" && p["resolution"] == "raw")
    );
}

#[tokio::test]
async fn patch_retention_updates_keep_seconds() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request(
            "PATCH",
            "/config/retention",
            Some(&token),
            Some(serde_json::json!({ "policies": [
                { "resource": "cpu", "resolution": "raw", "keep_seconds": 172_800 }
            ]})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let updated = body["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["resource"] == "cpu" && p["resolution"] == "raw")
        .expect("cpu/raw present");
    assert_eq!(updated["keep_seconds"], 172_800);
}

#[tokio::test]
async fn patch_retention_rejects_unknown_pair_without_partial_write() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // Valid first entry + bogus second: nothing may be applied.
    let (st, _) = app
        .request(
            "PATCH",
            "/config/retention",
            Some(&token),
            Some(serde_json::json!({ "policies": [
                { "resource": "cpu", "resolution": "raw", "keep_seconds": 172_800 },
                { "resource": "nope", "resolution": "raw", "keep_seconds": 172_800 }
            ]})),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (_, body) = app
        .request("GET", "/config/retention", Some(&token), None)
        .await;
    let cpu_raw = body["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["resource"] == "cpu" && p["resolution"] == "raw")
        .unwrap()
        .clone();
    assert_eq!(cpu_raw["keep_seconds"], 86_400);
}

#[tokio::test]
async fn patch_retention_rejects_out_of_range_keep() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, _) = app
        .request(
            "PATCH",
            "/config/retention",
            Some(&token),
            Some(serde_json::json!({ "policies": [
                { "resource": "cpu", "resolution": "raw", "keep_seconds": 60 }
            ]})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// ─── Resolutions ────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_resolutions_lists_chain() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    let (st, body) = app
        .request("GET", "/config/resolutions", Some(&token), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let names: Vec<&str> = body["resolutions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["raw", "1m", "5m", "1h"]);
}

#[tokio::test]
async fn patch_resolution_disables_leaf_only() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // '1m' feeds enabled '5m' — refuse.
    let (st, _) = app
        .request(
            "PATCH",
            "/config/resolutions/1m",
            Some(&token),
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // The leaf '1h' can go.
    let (st, body) = app
        .request(
            "PATCH",
            "/config/resolutions/1h",
            Some(&token),
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let one_h = body["resolutions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "1h")
        .unwrap()
        .clone();
    assert_eq!(one_h["enabled"], false);
}

#[tokio::test]
async fn patch_resolution_guards_raw_and_disabled_parent() {
    let app = TestApp::spawn().await;
    let token = app.pair_and_login().await;

    // raw is collector-written, not a rollup product.
    let (st, _) = app
        .request(
            "PATCH",
            "/config/resolutions/raw",
            Some(&token),
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Disable 1h, then 5m; re-enabling 1h under a disabled parent is refused.
    for name in ["1h", "5m"] {
        let (st, _) = app
            .request(
                "PATCH",
                &format!("/config/resolutions/{name}"),
                Some(&token),
                Some(serde_json::json!({ "enabled": false })),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
    }
    let (st, _) = app
        .request(
            "PATCH",
            "/config/resolutions/1h",
            Some(&token),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Unknown name → 404.
    let (st, _) = app
        .request(
            "PATCH",
            "/config/resolutions/2h",
            Some(&token),
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
