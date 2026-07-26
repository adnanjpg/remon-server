use anyhow::Context;
use log::{error, info};
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod notify;
mod routes;
mod state;

mod assistant;
mod auth;

#[cfg(test)]
mod api_tests;

mod cli;
mod collectors;
mod doctor;
mod error;
mod middleware;
mod models;
mod onboarding;
mod paths;
mod platform;
mod probes;
mod request_log;
mod services;
mod shutdown;
mod storage;

use crate::services::system as system_svc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let invocation = match cli::parse() {
        cli::Parsed::Run(cli) => cli,
        cli::Parsed::Exit(code) => return code,
    };

    // Resolve the filesystem layout before anything reads a path from it.
    paths::init(invocation.config_dir, invocation.data_dir);

    // Diagnostics report on their own and never boot the server, so they run
    // before the subscriber is installed — their output is the product, and
    // log lines interleaved with it would only be noise.
    match invocation.command {
        cli::Command::Doctor => {
            return if doctor::run() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            };
        }
        cli::Command::ConfigCheck => return config_check(),
        cli::Command::Run => {}
    }

    // Config load and logging install both run before the tracing subscriber
    // exists, so their failures go to stderr + exit rather than through the
    // log macros. Everything past this point logs via `error!`.
    let config = match config::Config::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            return std::process::ExitCode::FAILURE;
        }
    };

    let log_rx = match init_logging(&config) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("Failed to initialize logging: {:#}", e);
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Err(e) = run(config, log_rx).await {
        error!("fatal during startup: {:#}", e);
        return std::process::ExitCode::FAILURE;
    }

    std::process::ExitCode::SUCCESS
}

/// `config check` — parse every layer, validate, and report where the values
/// came from. Exits non-zero on anything that would stop a boot, so an
/// installer can gate `systemctl enable` on it.
fn config_check() -> std::process::ExitCode {
    let paths = paths::get();
    match config::Config::new() {
        Ok(cfg) => {
            println!("configuration ok");
            println!("  config dir   {}", paths.config_dir.display());
            println!("  data dir     {}", paths.data_dir.display());
            println!("  probes dir   {}", paths.probes_dir.display());
            println!(
                "  database     {}",
                paths.resolve_data(&cfg.database.path).display()
            );
            println!("  listen       {}:{}", cfg.server.host, cfg.server.port);
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("configuration invalid: {e}");
            std::process::ExitCode::FAILURE
        }
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
    mut config: config::Config,
    log_rx: mpsc::Receiver<services::logging::AppLog>,
) -> anyhow::Result<()> {
    info!(
        "starting remon-server v{} (env={})",
        env!("CARGO_PKG_VERSION"),
        std::env::var("RUN_ENV").unwrap_or_else(|_| "development".into())
    );

    #[cfg(feature = "docker")]
    services::docker::set_socket_path(&config.docker.socket_path);

    let paths = paths::get();
    info!(
        "config dir: {} | data dir: {} | probes dir: {}",
        paths.config_dir.display(),
        paths.data_dir.display(),
        paths.probes_dir.display()
    );

    let db_folder = paths.resolve_data(&config.database.folder_path);
    tokio::fs::create_dir_all(&db_folder)
        .await
        .with_context(|| format!("create database folder {}", db_folder.display()))?;

    let db_path = paths.resolve_data(&config.database.path);
    let db_url = format!("sqlite:{}", db_path.display());
    let db = storage::Database::connect(&db_url, config.database.max_connections)
        .await
        .context("database connection")?;

    db.migrate().await.context("database migration")?;

    // Resolve the effective JWT secret now that the DB is migrated: an
    // operator-provided strong secret wins, otherwise fall back to the
    // per-install secret generated and persisted on first boot.
    let jwt_secret = auth::secret::resolve(&config.auth.jwt_secret, db.pool())
        .await
        .context("resolve JWT secret")?;
    config.auth.jwt_secret = jwt_secret;

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
    info!("init system detected: {:?}", init_system);
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
        config.assistant.clone(),
        config.server.trusted_proxy,
        effective_config,
        overrides.collector_stats_interval_ms,
        overrides.processes_cache_ttl_ms,
        overrides.collector_docker_interval_ms,
        overrides.collector_smart_interval_ms,
        #[cfg(feature = "docker")]
        config.docker.exec_enabled,
        local_hardware,
        service_manager,
        probe_registry,
        notify,
        vapid_keys,
    ));
    info!("app state initialized with broadcast channels and layered config");

    services::logging::start_db_writer(log_rx, db.pool().clone());

    // Reboot / unclean-exit detection first, so the boot event (stamped
    // with the actual boot time) exists before anything else this run
    // writes to the ledger.
    services::events::detect_boot_on_startup(&app_state).await;
    services::events::spawn_system_event_sweep(app_state.clone());

    collectors::spawn_all(app_state.clone());
    collectors::smart::spawn(app_state.clone(), config.smart.clone());

    services::rollup::spawn(app_state.clone());
    services::retention::spawn(app_state.clone());
    services::alerts::spawn(app_state.clone());
    services::sessions::spawn(app_state.clone());
    info!("rollup, retention, alert evaluator, and session cleanup workers spawned");

    let _ = probes::scheduler::load_and_spawn(
        &paths.probes_dir,
        Arc::clone(&app_state.probe_registry),
        app_state.db.clone(),
    )
    .await;

    let app = routes::build_app(app_state.clone(), &config)?;

    // Already validated in Config::new — a bad host never reaches here.
    let bind_addr: SocketAddr = config.server.bind_addr().context("bind address")?;

    info!("listening on http://{}", bind_addr);

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind on {bind_addr}"))?;

    // Printed once the socket is up, so the addresses it names are ones that
    // actually answer.
    onboarding::print_if_unpaired(&app_state.db, bind_addr).await;

    // Flip `state.shutdown` inside the future handed to axum, before its
    // graceful-drain phase starts waiting on in-flight responses. The
    // infinite SSE streams race their next item against this signal (see
    // `routes::sse::until_shutdown`) so they end promptly instead of
    // blocking shutdown forever.
    let shutdown_state = app_state.clone();
    let graceful_shutdown = async move {
        shutdown::signal().await;
        let _ = shutdown_state.shutdown.send(true);
    };

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(graceful_shutdown)
    .await
    {
        error!("server error: {}", e);
    }

    // Reached only on orderly drain — a crash/kill skips this, which is
    // exactly what the next boot's unclean-exit detection keys on.
    services::events::mark_clean_shutdown(&app_state).await;

    info!("shutdown complete");
    Ok(())
}
