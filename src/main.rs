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
use tracing_core::Level;

mod api;
mod logger;
mod notification_service;
pub mod persistence;

mod auth;
mod grpc;
mod logs;
mod monitor;

use local_ip_address::local_ip;
use std::convert::TryInto;

fn get_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080)
}

fn get_ip_array() -> Option<[u8; 4]> {
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

fn get_socket_addr() -> Option<SocketAddr> {
    match get_ip_array() {
        Some(ip_array) => Some(SocketAddr::from((ip_array, get_port()))),
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
    dotenv::dotenv().ok();

    if cfg!(debug_assertions) {
        logger::LogService::new()
            .set_level(log::LevelFilter::Debug)
            .build();
    } else {
        logger::LogService::new()
            .set_level(log::LevelFilter::Info)
            .build();
    }

    let socket_addr = match get_socket_addr() {
        Some(addr) => addr,
        None => {
            error!("Failed to get local IP address.");
            SocketAddr::from(([127, 0, 0, 1], get_port()))
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
        .layer(middleware::from_fn(api::middleware::auth_middleware));

    // Merge routes
    let app = public_routes
        .merge(protected_routes)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::DEBUG))
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::DEBUG)
                        .latency_unit(LatencyUnit::Millis),
                ),
        );

    if cfg!(debug_assertions) {
        // In debug mode, run two servers
        let debug_socket_addr = SocketAddr::from(([127, 0, 0, 1], get_port()));

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
