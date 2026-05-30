use axum::{
    Router,
    http::{HeaderName, HeaderValue, Method, StatusCode},
};
use log::{error, info};
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::LatencyUnit;
use tower_http::compression::{
    CompressionLayer,
    predicate::{DefaultPredicate, NotForContentType, Predicate},
};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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
            Ok(mut sig) => {
                sig.recv().await;
            }
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
    // Keep `log::*` output visible under `cargo test`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace")),
        )
        .with_test_writer()
        .try_init();
}

#[tokio::main]
async fn main() {
    let config = match config::Config::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // One tracing registry handles stdout and DB persistence. The default
    // filter keeps dependency logs quiet unless RUST_LOG overrides it.
    let level_str = config.logging.level.to_lowercase();
    let default_filter = format!(
        "remon_server={lvl},sqlx=warn,hyper=warn,h2=warn,rustls=warn,tower_http={lvl}",
        lvl = level_str
    );
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    // Drained by `start_db_writer` once the DB connection is ready.
    let (log_tx, log_rx) = tokio::sync::mpsc::channel::<services::logging::AppLog>(100);
    let persist_level =
        services::logging::parse_persist_level(&config.monitoring.log_insertion_level);
    let db_layer =
        services::logging::DbLayer::new(log_tx, persist_level, config.monitoring.app_name.clone());

    // Avoid ANSI escapes in redirected logs.
    let stdout_ansi = std::io::stdout().is_terminal();

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(db_layer);

    let format = config.logging.format.to_lowercase();
    let install_result = match format.as_str() {
        "json" => registry
            .with(tracing_subscriber::fmt::layer().with_ansi(false).json())
            .try_init(),
        "pretty" => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(stdout_ansi)
                    .pretty(),
            )
            .try_init(),
        _ => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(stdout_ansi)
                    .compact(),
            )
            .try_init(),
    };
    if let Err(e) = install_result {
        eprintln!("Failed to install tracing subscriber: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = auth::token::validate() {
        error!("{}", e);
        std::process::exit(1);
    }

    #[cfg(feature = "docker")]
    services::docker::set_socket_path(&config.docker.socket_path);

    if let Err(e) = tokio::fs::create_dir_all(&config.database.folder_path).await {
        error!(
            "Failed to create database folder {}: {:?}",
            config.database.folder_path, e
        );
        std::process::exit(1);
    }

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

    let local_hardware = Arc::new(system_svc::get_hardware_info());

    // DB-backed runtime config overrides selected TOML defaults at boot.
    let overrides = match db.config().load().await {
        Ok(o) => o,
        Err(e) => {
            error!("Failed to load runtime config from DB: {:?}", e);
            std::process::exit(1);
        }
    };

    let effective_config = state::EffectiveConfig {
        server_name: overrides.server_name,
        rollup_tick_interval_ms: overrides.rollup_tick_interval_ms,
        retention_tick_interval_ms: overrides.retention_tick_interval_ms,
    };

    let init_system = platform::init::detect();
    info!("Init system detected: {:?}", init_system);
    let service_manager = platform::services::factory::create(&init_system).await;

    let probe_registry = probes::registry::new_registry();

    // Push delivery cannot work without a VAPID keypair.
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
        config.server.trusted_proxy,
        effective_config,
        overrides.collector_stats_interval_ms,
        overrides.processes_cache_ttl_ms,
        #[cfg(feature = "docker")]
        config.docker.exec_enabled,
        local_hardware,
        service_manager,
        probe_registry,
        notify,
        vapid_keys,
    ));
    info!("AppState initialized with broadcast channels and layered config");

    services::logging::start_db_writer(log_rx, db.pool().clone());

    collectors::spawn_all(app_state.clone());

    services::rollup::spawn(app_state.clone());
    services::retention::spawn(app_state.clone());
    services::alerts::spawn(app_state.clone());
    services::sessions::spawn(app_state.clone());
    info!("Rollup, retention, alert evaluator, and session cleanup workers spawned");

    let probe_dir = std::path::PathBuf::from("probes");
    let _ = probes::scheduler::load_and_spawn(
        &probe_dir,
        Arc::clone(&app_state.probe_registry),
        app_state.db.clone(),
    )
    .await;

    let cors_layer = build_cors_layer(&config.cors);

    // REST gets request timeouts; SSE/WS streams stay long-lived.
    let rest_router = routes::rest::create_routes(app_state.clone()).layer(
        TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(30)),
    );
    let compression = CompressionLayer::new()
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("text/event-stream")));
    let app = Router::new()
        .merge(rest_router)
        .nest("/sse", routes::sse::create_routes(app_state.clone()))
        .nest("/ws", routes::ws::create_routes(app_state.clone()))
        .with_state(app_state)
        .layer(cors_layer)
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(compression)
        // SSE/WS browser clients authenticate via query string, so redact
        // access_token before the URI reaches stdout or DB-backed logs.
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
            error!(
                "Invalid server.host '{}', falling back to 0.0.0.0:{}",
                config.server.host, config.server.port
            );
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

/// Redact `access_token` query values before request URIs are logged.
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

/// Build CORS from config, failing fast when production allow-listing is empty.
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
