use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use log::{error, info};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::Level;

mod api;
mod config;
mod logger;
mod notification_service;
pub mod persistence;

mod auth;
mod grpc;
mod logs;
mod monitor;

use local_ip_address::local_ip;
use std::convert::TryInto;

fn get_ip_array(_config: &config::Config) -> Option<[u8; 4]> {
    match local_ip() {
        Ok(ip) => {
            let ip_str = ip.to_string();

            let ip_array: [u8; 4] = ip_str
                .split(".")
                .map(|x| x.parse::<u8>().unwrap())
                .collect::<Vec<u8>>()
                .try_into()
                .unwrap();

            Some(ip_array)
        }
        Err(_) => {
            error!("Failed to get local IP address.");
            None
        }
    }
}

fn get_socket_addr(config: &config::Config) -> Option<SocketAddr> {
    match get_ip_array(config) {
        Some(ip_array) => Some(SocketAddr::from((ip_array, config.server.port))),
        None => None,
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

#[cfg(test)]
#[ctor::ctor]
fn init_tests() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Trace)
        .is_test(true)
        .try_init()
        .expect("Failed to initialize logger for tests");
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

    // Initialize tracing subscriber for HTTP request logging
    let log_level = match config.logging.level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    // Initialize tracing first (for HTTP logging)
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .compact()
        .init();

    // Then initialize application logger
    let log_filter = match config.logging.level.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };

    logger::LogService::new()
        .set_level(log_filter)
        .build();

    // Validate JWT secret strength at startup
    if let Err(e) = auth::token::validate() {
        error!("{}", e);
        std::process::exit(1);
    }

    let socket_addr = match get_socket_addr(&config) {
        Some(addr) => addr,
        None => {
            error!("Failed to get local IP address.");
            SocketAddr::from(([127, 0, 0, 1], config.server.port))
        }
    };

    match crate::persistence::init_db().await {
        Ok(val) => val,
        Err(e) => {
            error!("Database initialization failed: {:?}", e);
            return;
        }
    };

    match monitor::init().await {
        Ok(_) => {}
        Err(_) => {
            error!("Failed to initialize monitor.");
            return;
        }
    }

    match grpc::grpc_service::init().await {
        Ok(_) => {}
        Err(_) => {
            error!("Failed to initialize gRPC.");
            return;
        }
    }

    // Build the Axum router
    // Public routes (no authentication)
    let public_routes = Router::new()
        .route("/hello", get(api::handlers::misc::hello))
        .route("/teapot", get(api::handlers::misc::teapot))
        .route("/healthcheck", get(api::handlers::misc::healthcheck))
        .route("/get-otp-qr", post(api::handlers::auth::get_otp_qr))
        .route("/login", post(api::handlers::auth::login));

    // Protected routes (require authentication)
    let protected_routes = Router::new()
        .route("/get-desc", get(api::handlers::monitor::get_desc))
        .route(
            "/get-hardware-info",
            get(api::handlers::monitor::get_hardware_info),
        )
        .route(
            "/get-cpu-status",
            get(api::handlers::monitor::get_cpu_status),
        )
        .route(
            "/get-mem-status",
            get(api::handlers::monitor::get_mem_status),
        )
        .route(
            "/get-disk-status",
            get(api::handlers::monitor::get_disk_status),
        )
        .route(
            "/get-processes",
            get(api::handlers::process::get_processes),
        )
        .route(
            "/kill-process",
            get(api::handlers::process::kill_process),
        )
        .route("/update-info", post(api::handlers::monitor::update_info))
        .route(
            "/validate-token-test",
            get(api::handlers::monitor::validate_token_test),
        )
        .route(
            "/logs/get-app-ids",
            get(api::handlers::logs::get_app_ids_handler),
        )
        .route(
            "/logs/get-app-logs",
            get(api::handlers::logs::get_app_logs_handler),
        )
        // Docker routes
        .route(
            "/docker/status",
            get(api::handlers::docker::get_docker_status),
        )
        .route(
            "/docker/containers",
            get(api::handlers::docker::list_containers),
        )
        .route(
            "/docker/containers/:id",
            get(api::handlers::docker::get_container),
        )
        .route(
            "/docker/stats",
            get(api::handlers::docker::get_stats),
        )
        .route(
            "/docker/containers/:id/start",
            post(api::handlers::docker::start_container),
        )
        .route(
            "/docker/containers/:id/stop",
            post(api::handlers::docker::stop_container),
        )
        .route(
            "/docker/containers/:id/restart",
            post(api::handlers::docker::restart_container),
        )
        .route(
            "/docker/containers/:id/logs",
            get(api::handlers::docker::get_logs),
        )
        .layer(middleware::from_fn(api::middleware::auth_middleware));

    // Merge routes with HTTP request/response logging
    let app = public_routes.merge(protected_routes).layer(
        TraceLayer::new_for_http()
            .make_span_with(
                DefaultMakeSpan::new()
                    .level(Level::INFO)
                    .include_headers(true),
            )
            .on_response(
                DefaultOnResponse::new()
                    .level(Level::INFO)
                    .latency_unit(LatencyUnit::Millis),
            ),
    );

    if cfg!(debug_assertions) {
        // In debug mode, run two servers
        let debug_socket_addr = SocketAddr::from(([127, 0, 0, 1], config.server.port));

        info!("Listening on http://{}", socket_addr);
        info!("Listening on http://{}", debug_socket_addr);

        let listener_main = TcpListener::bind(socket_addr).await.unwrap();
        let listener_debug = TcpListener::bind(debug_socket_addr).await.unwrap();

        let app_main = app.clone();
        let app_debug = app;

        tokio::select! {
            _ = axum::serve(listener_main, app_main).with_graceful_shutdown(shutdown_signal()) => {},
            _ = axum::serve(listener_debug, app_debug).with_graceful_shutdown(shutdown_signal()) => {},
        }
    } else {
        info!("Listening on http://{}", socket_addr);

        let listener = TcpListener::bind(socket_addr).await.unwrap();

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            error!("server error: {}", e);
        }
    }
}
