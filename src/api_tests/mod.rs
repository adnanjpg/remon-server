//! In-process HTTP integration tests.
//!
//! Each test builds a fully-wired [`AppState`] over a fresh in-memory SQLite
//! database (migrations applied) and drives the *real* REST router — auth
//! middleware, rate limiter, validation and all — through
//! [`tower::ServiceExt::oneshot`]. No TCP socket is opened, so the suite is
//! fast and order-independent: every `TestApp::spawn` is fully isolated.
//!
//! This is the integration tier of the test pyramid; the per-module `#[cfg]`
//! unit tests cover pure logic (parsing, expressions, filters) underneath.

mod alerts_api;
mod assistant_api;
mod assistant_tools;
mod auth_flow;
mod config_api;
mod events_api;
mod heartbeats_api;
mod incidents_api;
mod logs_api;
mod probes_api;
mod process_api;
mod public;
mod push_api;
mod query_plan_audit;
mod smart_api;
mod summary_api;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use serde_json::Value;
use tower::ServiceExt;

use crate::config::{AuthConfig, NotificationsConfig};
use crate::notify::NotificationManager;
use crate::state::{AppState, EffectiveConfig};

/// Install a test-only tracing subscriber once, before any test runs, so
/// `log::*` / `tracing::*` output stays visible under `cargo test`. Runs via
/// `ctor` at test-binary load; `try_init` keeps it idempotent.
#[ctor::ctor(unsafe)]
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace")),
        )
        .with_test_writer()
        .try_init();
}

/// A wired-up server under test: the router plus the shared [`AppState`], so
/// tests can both issue HTTP requests and inspect/seed internal state (e.g.
/// read the active pairing code the server only prints to its terminal).
pub struct TestApp {
    pub router: Router,
    pub state: Arc<AppState>,
}

impl TestApp {
    /// Build an isolated app backed by a fresh in-memory SQLite DB.
    ///
    /// `max_connections = 1` keeps the whole `:memory:` database on a single
    /// connection so migrations and every subsequent query see the same data.
    pub async fn spawn() -> TestApp {
        Self::spawn_with_assistant(crate::config::AssistantConfig::default()).await
    }

    /// Same as [`spawn`], but with an explicit assistant config. The live
    /// assistant smoke test (`assistant_api`, `#[ignore]`) uses this to inject
    /// a real provider key from the environment; every other test takes the
    /// default (no key → the endpoint is inert).
    pub async fn spawn_with_assistant(assistant_config: crate::config::AssistantConfig) -> TestApp {
        let db = crate::storage::Database::connect("sqlite::memory:", 1)
            .await
            .expect("connect in-memory sqlite");
        db.migrate().await.expect("run migrations");

        let auth_config = AuthConfig {
            jwt_secret: "test-jwt-secret-at-least-32-chars-long-0123456789".to_string(),
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 2_592_000,
            pairing_code_ttl_secs: 300,
        };

        let effective_config = EffectiveConfig {
            server_name: "test-server".to_string(),
            rollup_tick_interval_ms: 60_000,
            retention_tick_interval_ms: 3_600_000,
        };

        let hardware = Arc::new(crate::services::system::get_hardware_info());
        let service_manager =
            crate::platform::services::factory::create(&crate::platform::init::detect()).await;
        let probe_registry = crate::probes::registry::new_registry();

        let vapid = Arc::new(
            crate::services::webpush::load_or_generate(db.pool())
                .await
                .expect("vapid keypair"),
        );
        let notify = NotificationManager::new(
            db.pool().clone(),
            NotificationsConfig::default(),
            Arc::clone(&vapid),
        )
        .await
        .expect("notification manager");

        let state = Arc::new(AppState::new(
            db.pool().clone(),
            auth_config,
            assistant_config,
            false,
            effective_config,
            2000,
            5000,
            3000,
            1_800_000,
            #[cfg(feature = "docker")]
            false,
            hardware,
            service_manager,
            probe_registry,
            notify,
            vapid,
        ));

        // Mirror `build_app`: the assistant lives outside `create_routes`
        // (it gets a longer timeout there), so mount it here too — with the
        // same auth middleware — or the `/assistant` route tests 404.
        let assistant = crate::routes::rest::create_assistant_routes().layer(
            axum::middleware::from_fn_with_state(state.clone(), crate::middleware::auth_middleware),
        );
        let router = crate::routes::rest::create_routes(state.clone())
            .merge(assistant)
            .with_state(state.clone());
        TestApp { router, state }
    }

    /// Fire one request through the router. Returns the status and the parsed
    /// JSON body (`Value::Null` when the body is empty or not JSON).
    ///
    /// A stable loopback `ConnectInfo` is always attached: both the auth
    /// rate-limiter (`PeerIpKeyExtractor`) and the login handler read the
    /// client IP from it, and a missing extension would 500 those routes.
    pub async fn request(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req_body = match &body {
            Some(v) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let mut req = builder.body(req_body).expect("build request");
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));

        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router oneshot");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, json)
    }

    /// Run the full pairing → login flow and return a valid access token.
    ///
    /// Three calls hit the rate-limited anonymous subrouter (burst 5), so a
    /// single `pair_and_login` per test stays well within budget.
    pub async fn pair_and_login(&self) -> String {
        let (st, _) = self
            .request("POST", "/auth/pair/initiate", None, None)
            .await;
        assert_eq!(st, StatusCode::OK, "pair/initiate should succeed");

        // The code is only printed to the server terminal; as the trusted
        // host, the test reads it straight from shared state.
        let code = self
            .state
            .pairing_state
            .read()
            .await
            .as_ref()
            .expect("pairing window active")
            .code
            .clone();

        let (st, body) = self
            .request(
                "POST",
                "/auth/pair/complete",
                None,
                Some(serde_json::json!({
                    "pairing_code": code,
                    "device_name": "integration-test",
                })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "pair/complete should succeed: {body}");
        let device_id = body["device_id"].as_str().expect("device_id").to_string();
        let device_token = body["device_token"]
            .as_str()
            .expect("device_token")
            .to_string();

        let (st, body) = self
            .request(
                "POST",
                "/auth/login",
                None,
                Some(serde_json::json!({
                    "device_id": device_id,
                    "device_token": device_token,
                })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "login should succeed: {body}");
        body["access_token"]
            .as_str()
            .expect("access_token")
            .to_string()
    }
}
