use axum::{
    Router,
    http::{HeaderName, HeaderValue, Method},
};
use log::{error, info};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::LatencyUnit;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

mod config;
mod notify;
mod routes;
mod state;

mod auth;

mod collectors;
mod error;
mod middleware;
mod models;
mod platform;
mod probes;
mod services;
mod storage;

use crate::services::system as system_svc;


async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Ctrl+C handler error: {}", e);
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => { sig.recv().await; }
            Err(e) => {
                error!("Failed to install SIGTERM handler: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_tests() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Trace)
        .is_test(true)
        .try_init();
}

#[tokio::main]
async fn main() {
    // Load configuration
    let config = match config::Config::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Logger init: ORDER MATTERS.
    //
    // `tracing-subscriber` (with default features) pulls in `tracing-log`,
    // whose `LogTracer::init()` claims `log::set_logger()`. If we let it
    // run first, env_logger's `try_init()` later silently no-ops and our
    // pipe → DB writer never sees a record.
    //
    // So: install env_logger FIRST (claims `set_logger`), then install
    // tracing as the global *tracing* subscriber. Both end up coexisting:
    // - `log::*!` macros → env_logger pipe → DB writer + stdout
    // - `tracing::*!` (incl. tower-http TraceLayer spans) → tracing → stdout
    let log_filter = match config.logging.level.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };

    let log_rx = services::logging::LogService::new()
        .set_level(log_filter)
        .build();

    let log_level = match config.logging.level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    // `set_global_default` instead of `init()` here — `init()` would also
    // try to claim `log::set_logger` via tracing-log, panicking because
    // env_logger already owns it. This keeps the two facades cleanly
    // separated: log:: → env_logger → DB, tracing:: → stdout.
    let format = config.logging.format.to_lowercase();
    let format = format.as_str();
    match format {
        "json" => {
            let s = tracing_subscriber::fmt()
                .json()
                .with_max_level(log_level)
                .with_target(false)
                .finish();
            if let Err(e) = tracing::subscriber::set_global_default(s) {
                eprintln!("Failed to install tracing subscriber: {}", e);
                std::process::exit(1);
            }
        }
        "pretty" => {
            let s = tracing_subscriber::fmt()
                .pretty()
                .with_max_level(log_level)
                .with_target(false)
                .finish();
            if let Err(e) = tracing::subscriber::set_global_default(s) {
                eprintln!("Failed to install tracing subscriber: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            let s = tracing_subscriber::fmt()
                .compact()
                .with_max_level(log_level)
                .with_target(false)
                .finish();
            if let Err(e) = tracing::subscriber::set_global_default(s) {
                eprintln!("Failed to install tracing subscriber: {}", e);
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = auth::token::validate() {
        error!("{}", e);
        std::process::exit(1);
    }

    #[cfg(feature = "docker")]
    services::docker::set_socket_path(&config.docker.socket_path);

    // Ensure database folder exists before opening the SQLite file
    if let Err(e) = tokio::fs::create_dir_all(&config.database.folder_path).await {
        error!(
            "Failed to create database folder {}: {:?}",
            config.database.folder_path, e
        );
        std::process::exit(1);
    }

    // Initialize storage layer
    let db_url = format!("sqlite:{}", config.database.path);
    let db = match storage::Database::connect(&db_url, config.database.max_connections).await {
        Ok(db) => db,
        Err(e) => {
            error!("Database connection failed: {:?}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = db.migrate().await {
        error!("Database migration failed: {:?}", e);
        std::process::exit(1);
    }

    let local_hardware = system_svc::get_hardware_info();

    // Layered config: TOML defaults are already in `config`; DB row in
    // `server_config` overrides selected fields at boot. The DB row is
    // seeded by 0001_schema.sql so this never returns an empty result.
    let overrides = match db.config().load().await {
        Ok(o) => o,
        Err(e) => {
            error!("Failed to load runtime config from DB: {:?}", e);
            std::process::exit(1);
        }
    };

    let effective_config = state::EffectiveConfig {
        server_name: overrides.server_name,
        collector_stats_base_interval_ms: overrides.collector_stats_interval_ms,
        rollup_tick_interval_ms: overrides.rollup_tick_interval_ms,
        retention_tick_interval_ms: overrides.retention_tick_interval_ms,
    };

    // Detect init system, build the appropriate `ServiceManager` backend.
    // Linux → systemd (full) or OpenRC (full). Windows → SCM via PowerShell.
    // Other targets get the `Unsupported` fallback that returns NotSupported
    // for every operation. Selection happens once at boot; changing init
    // systems requires a restart (which is also a kernel-restart anyway).
    let init_system = platform::init::detect();
    info!("Init system detected: {:?}", init_system);
    let service_manager = platform::services::factory::create(&init_system).await;

    // Probe registry. Empty at construction; the loader pass below
    // populates it from `probes/*.yaml` and spawns per-probe
    // tasks. Lives in AppState so REST handlers can serve the registry
    // without a DB round-trip.
    let probe_registry = probes::registry::new_registry();

    // VAPID keypair: load from `vapid_keys` row, generate + persist on
    // first boot. Failure here is fatal — the server identifies itself
    // to push relays with this keypair, and we won't be able to deliver
    // alerts without it. Built BEFORE NotificationManager so the
    // `web-push` channel can take an Arc clone at construction time.
    let vapid_keys = match services::webpush::load_or_generate(db.pool()).await {
        Ok(k) => Arc::new(k),
        Err(e) => {
            error!("Failed to load/generate VAPID keypair: {:?}", e);
            std::process::exit(1);
        }
    };

    let notify = match notify::NotificationManager::new(
        db.pool().clone(),
        config.notifications.clone(),
        Arc::clone(&vapid_keys),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to initialize notification manager: {:?}", e);
            std::process::exit(1);
        }
    };

    let app_state = Arc::new(state::AppState::new(
        db.pool().clone(),
        config.auth.clone(),
        effective_config,
        overrides.collector_stats_interval_ms,
        overrides.collector_processes_interval_ms,
        #[cfg(feature = "docker")]
        overrides.collector_docker_interval_ms,
        #[cfg(feature = "docker")]
        config.docker.exec_enabled,
        local_hardware,
        service_manager,
        probe_registry,
        notify,
        vapid_keys,
    ));
    info!("AppState initialized with broadcast channels and layered config");

    // Now that the DB is up, attach the log channel's consumer so future
    // log records land in the `logs` table.
    services::logging::start_db_writer(log_rx, db.pool().clone());

    // Spawn collectors (broadcast + raw metrics writer)
    collectors::spawn_all(app_state.clone());

    // Spawn rollup, retention, adaptive sampling, alert evaluator, service watcher
    services::rollup::spawn(app_state.clone());
    services::retention::spawn(app_state.clone());
    services::sampling::spawn(app_state.clone());
    services::alerts::spawn(app_state.clone());
    services::service_watcher::spawn(app_state.clone());
    services::sessions::spawn(app_state.clone());
    info!(
        "Rollup, retention, sampling, alert evaluator, service watcher, and session cleanup workers spawned"
    );

    // Probe loader: scan probes/, validate manifests, spawn one
    // task per enabled+platform-matched probe. A missing directory is
    // not an error — fresh installs may not ship any examples.
    let probe_dir = std::path::PathBuf::from("probes");
    let report = probes::scheduler::load_and_spawn(
        &probe_dir,
        Arc::clone(&app_state.probe_registry),
        app_state.db.clone(),
    )
    .await;
    info!(
        "Probe loader: {} loaded, {} disabled, {} skipped(platform), {} failed",
        report.loaded.len(),
        report.skipped_disabled.len(),
        report.skipped_platform.len(),
        report.failed.len()
    );

    // CORS layer for the browser-based web UI. Resolved from config; in
    // dev `allow_any_origin = true` and we hand the browser `Access-Control-
    // Allow-Origin: *`. In production an explicit origin list is required
    // — we exit on misconfiguration rather than silently locking the UI out.
    let cors_layer = build_cors_layer(&config.cors);

    // Build the Axum router. State is passed through to each module so the
    // auth middleware (a layer attached inside each module) can run
    // session/jti revocation checks against the database.
    let app = Router::new()
        .merge(routes::rest::create_routes(app_state.clone()))
        .nest("/sse", routes::sse::create_routes(app_state.clone()))
        .nest("/ws", routes::ws::create_routes(app_state.clone()))
        .with_state(app_state)
        .layer(cors_layer)
        .layer(tower_http::compression::CompressionLayer::new())
        // HTTP request/response trace span.
        //
        // Two redactions matter here:
        // 1. `include_headers` is implicitly off — we don't ship them into
        //    the span. This kept Authorization headers out of the LogService
        //    pipe (B1 sprint).
        // 2. The URI is sanitized via `redact_access_token` because browser
        //    SSE/WS clients pass the JWT as `?access_token=...` (the only
        //    way EventSource / `new WebSocket(...)` can authenticate). Left
        //    alone, that token would land in stdout/journald with every
        //    request line — same risk class as the header leak.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let uri = redact_access_token(&req.uri().to_string());
                    tracing::info_span!(
                        "request",
                        method = %req.method(),
                        uri = %uri,
                    )
                })
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::INFO)
                        .latency_unit(LatencyUnit::Millis),
                ),
        );

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .unwrap_or_else(|_| {
            error!("Invalid server.host '{}', falling back to 0.0.0.0:{}", config.server.host, config.server.port);
            SocketAddr::from(([0, 0, 0, 0], config.server.port))
        });

    info!("Listening on http://{}", bind_addr);

    let listener = match TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind on {}: {}", bind_addr, e);
            std::process::exit(1);
        }
    };

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    {
        error!("server error: {}", e);
    }
}

/// Replace the value of an `access_token=…` query parameter with `REDACTED`
/// so the token doesn't end up in span fields / stdout / journald logs.
/// Preserves the rest of the URL exactly.
fn redact_access_token(uri: &str) -> String {
    let Some((path, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let scrubbed: Vec<String> = query
        .split('&')
        .map(|pair| {
            if pair.starts_with("access_token=") || pair == "access_token" {
                "access_token=REDACTED".to_string()
            } else {
                pair.to_string()
            }
        })
        .collect();
    format!("{}?{}", path, scrubbed.join("&"))
}

#[cfg(test)]
mod redact_tests {
    use super::redact_access_token;

    #[test]
    fn no_query_passes_through() {
        assert_eq!(redact_access_token("/sse/stats"), "/sse/stats");
    }

    #[test]
    fn token_first_param_redacted() {
        assert_eq!(
            redact_access_token("/sse/stats?access_token=abc.def.ghi"),
            "/sse/stats?access_token=REDACTED"
        );
    }

    #[test]
    fn token_with_other_params_redacted() {
        assert_eq!(
            redact_access_token("/sse/stats?foo=1&access_token=abc&bar=2"),
            "/sse/stats?foo=1&access_token=REDACTED&bar=2"
        );
    }

    #[test]
    fn unrelated_query_untouched() {
        assert_eq!(
            redact_access_token("/processes?limit=10"),
            "/processes?limit=10"
        );
    }
}

/// Build the CORS layer from configuration.
///
/// - `allow_any_origin = true` → `Access-Control-Allow-Origin: *`. Safe
///   here because the API uses `Authorization: Bearer ...` (no cookies
///   ever), so the wildcard is not in conflict with credentialed mode.
/// - Otherwise the explicit `allowed_origins` list is parsed; invalid
///   entries are skipped with a warning so a single malformed URL doesn't
///   break boot.
/// - Empty list with `allow_any_origin = false` is a misconfiguration —
///   the browser would reject every preflight. We exit so the operator
///   notices instead of debugging "why does the UI hang".
fn build_cors_layer(cfg: &config::CorsConfig) -> CorsLayer {
    let methods = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ];
    let allowed_headers = [
        HeaderName::from_static("authorization"),
        HeaderName::from_static("content-type"),
    ];

    let base = CorsLayer::new()
        .allow_methods(methods)
        .allow_headers(allowed_headers);

    if cfg.allow_any_origin {
        return base.allow_origin(AllowOrigin::any());
    }

    if cfg.allowed_origins.is_empty() {
        error!(
            "FATAL: cors.allow_any_origin = false but cors.allowed_origins is empty. \
             Set REMON__CORS__ALLOW_ANY_ORIGIN=true (dev) or populate allowed_origins."
        );
        std::process::exit(1);
    }

    let parsed: Vec<HeaderValue> = cfg
        .allowed_origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                error!("Skipping invalid CORS origin '{}'", o);
                None
            }
        })
        .collect();

    if parsed.is_empty() {
        error!("FATAL: cors.allowed_origins had no valid entries");
        std::process::exit(1);
    }

    info!(
        "CORS: explicit allow-list ({} origin{})",
        parsed.len(),
        if parsed.len() == 1 { "" } else { "s" }
    );
    base.allow_origin(AllowOrigin::list(parsed))
}

