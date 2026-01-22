use axum::Router;
use log::{error, info};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::LatencyUnit;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

mod config;
mod logger;
mod notification_service;
pub mod persistence;
mod routes;

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

    logger::LogService::new().set_level(log_filter).build();

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

    // Build the Axum router using modular route registration
    let app = Router::new()
        // REST API routes (both public and protected)
        .merge(routes::rest::create_routes())
        // SSE routes (Server-Sent Events)
        .nest("/sse", routes::sse::create_routes())
        // WebSocket routes
        .nest("/ws", routes::ws::create_routes())
        // HTTP request/response logging
        .layer(
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
