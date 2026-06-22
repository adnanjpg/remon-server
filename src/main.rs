use anyhow::Context;
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
use tokio::sync::mpsc;
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

#[cfg(test)]
mod api_tests;

mod collectors;
mod error;
mod middleware;
mod models;
mod platform;
mod probes;
mod services;
mod shutdown;
mod storage;

use crate::services::system as system_svc;

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
    // Config load and logging install both run before the tracing subscriber
    // exists, so their failures go to stderr + exit rather than through the
    // log macros. Everything past this point logs via `error!`.
    let config = match config::Config::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    let log_rx = match init_logging(&config) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("Failed to initialize logging: {:#}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = run(config, log_rx).await {
        error!("fatal during startup: {:#}", e);
        std::process::exit(1);
    }
}

/// Install the tracing subscriber and return the receiver half of the
/// DB-log channel (drained later by `start_db_writer`). One registry handles
/// stdout and DB persistence; the default filter keeps dependency logs quiet
/// unless `RUST_LOG` overrides it.
fn init_logging(
    config: &config::Config,
) -> anyhow::Result<mpsc::Receiver<services::logging::AppLog>> {
    let level_str = config.logging.level.to_lowercase();
    let default_filter = format!(
        "remon_server={lvl},sqlx=warn,hyper=warn,h2=warn,rustls=warn,tower_http={lvl}",
        lvl = level_str
    );
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    // Drained by `start_db_writer` once the DB connection is ready.
    let (log_tx, log_rx) = mpsc::channel::<services::logging::AppLog>(100);
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
    match format.as_str() {
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
    }
    .map_err(|e| anyhow::anyhow!("install tracing subscriber: {e}"))?;

    Ok(log_rx)
}

/// Boot the server: validate config, open the database, wire shared state,
/// spawn background workers, and serve until shutdown. Runs with the tracing
/// subscriber already installed, so every fatal here is surfaced via the
/// `?`-propagated error that `main` logs once.
async fn run(
    config: config::Config,
    log_rx: mpsc::Receiver<services::logging::AppLog>,
) -> anyhow::Result<()> {
    auth::token::validate().map_err(anyhow::Error::msg)?;

    #[cfg(feature = "docker")]
    services::docker::set_socket_path(&config.docker.socket_path);

    tokio::fs::create_dir_all(&config.database.folder_path)
        .await
        .with_context(|| format!("create database folder {}", config.database.folder_path))?;

    let db_url = format!("sqlite:{}", config.database.path);
    let db = storage::Database::connect(&db_url, config.database.max_connections)
        .await
        .context("database connection")?;

    db.migrate().await.context("database migration")?;

    let local_hardware = Arc::new(system_svc::get_hardware_info());

    // DB-backed runtime config overrides selected TOML defaults at boot.
    let overrides = db
        .config()
        .load()
        .await
        .context("load runtime config from DB")?;

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
    let vapid_keys = Arc::new(
        services::webpush::load_or_generate(db.pool())
            .await
            .context("load/generate VAPID keypair")?,
    );

    let notify = notify::NotificationManager::new(
        db.pool().clone(),
        config.notifications.clone(),
        Arc::clone(&vapid_keys),
    )
    .await
    .context("initialize notification manager")?;

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
    collectors::smart::spawn(app_state.clone(), config.smart.clone());

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

    let app = build_router(app_state, &config)?;

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

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind on {bind_addr}"))?;

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown::signal())
    .await
    {
        error!("server error: {}", e);
    }

    Ok(())
}

/// Assemble the full application router: REST (with request timeouts) merged
/// with the long-lived SSE/WS streams, wrapped in the shared tower-http stack.
fn build_router(
    app_state: Arc<state::AppState>,
    config: &config::Config,
) -> anyhow::Result<Router> {
    let cors_layer = build_cors_layer(&config.cors)?;

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
        )
        // Outermost so responses produced by inner layers — notably the
        // 413 from the body limit above — still carry CORS headers; without
        // this a browser sees an opaque CORS error instead of the real status.
        .layer(cors_layer);

    Ok(app)
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
fn build_cors_layer(cfg: &config::CorsConfig) -> anyhow::Result<CorsLayer> {
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
        return Ok(base.allow_origin(AllowOrigin::any()));
    }

    if cfg.allowed_origins.is_empty() {
        anyhow::bail!(
            "cors.allow_any_origin = false but cors.allowed_origins is empty. \
             Set REMON__CORS__ALLOW_ANY_ORIGIN=true (dev) or populate allowed_origins."
        );
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
        anyhow::bail!("cors.allowed_origins had no valid entries");
    }

    info!(
        "CORS: explicit allow-list ({} origin{})",
        parsed.len(),
        if parsed.len() == 1 { "" } else { "s" }
    );
    Ok(base.allow_origin(AllowOrigin::list(parsed)))
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
