use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server};

use std::convert::Infallible;
use std::net::SocketAddr;

mod api;
mod notification_service;
pub mod persistence;

use env_logger;
use log::{error, info};

mod auth;
mod monitor;

use local_ip_address::local_ip;

use std::convert::TryInto;

// https://stackoverflow.com/a/39175997/12555423
#[macro_use]
extern crate lazy_static;

// TODO(adnanjpg): get port from env var
const DEFAULT_PORT: u16 = 8080;

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
        Some(ip_array) => Some(SocketAddr::from((ip_array, DEFAULT_PORT))),
        None => None,
    }
}

async fn req_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => api::hello::hello(req),
        (&Method::GET, "/teapot") => api::teapot::teapot(req),
        (&Method::GET, "/healthcheck") => api::healthcheck::healthcheck(req),
        (&Method::POST, "/get-otp-qr") => api::get_otp_qr::get_otp_qr(req).await,
        (&Method::POST, "/login") => api::login::login(req).await,
        (&Method::POST, "/update-info") => api::update_info::update_info(req).await,
        (&Method::GET, "/get-desc") => api::get_desc::get_desc(req),
        (&Method::GET, "/get-hardware-info") => {
            api::get_hardware_info::get_hardware_info(req).await
        }
        (&Method::GET, "/get-cpu-status") => api::get_cpu_status::get_cpu_status(req).await,
        (&Method::GET, "/get-mem-status") => api::get_mem_status::get_mem_status(req).await,
        (&Method::GET, "/get-disk-status") => api::get_disk_status::get_disk_status(req).await,
        (&Method::GET, "/validate-token-test") => {
            api::validate_token_test::validate_token_test(req).await
        }
        (_, _) => api::_404::_404(req),
    }
}

fn init_logger(test_assertions: bool) {
    if cfg!(debug_assertions) {
        env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .init();
    } else if test_assertions {
        env_logger::builder()
            .filter_level(log::LevelFilter::Info)
            .init();
    } else {
        env_logger::builder()
            .filter_level(log::LevelFilter::Info)
            .init();
    }
}

async fn shutdown_signal() {
    // Wait for the CTRL+C signal for graceful shutdown
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

// https://stackoverflow.com/a/63442117/12555423
#[cfg(test)]
#[ctor::ctor]
fn init_tests() {
    init_logger(true);
}

#[tokio::main]
async fn main() {
    init_logger(false);

    let socket_addr = match get_socket_addr() {
        Some(addr) => addr,
        None => {
            error!("Failed to get local IP address.");
            return;
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

    let server = Server::bind(&socket_addr)
        .serve(make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(req_handler))
        }))
        .with_graceful_shutdown(shutdown_signal());

    let mut socket_addrs = vec![socket_addr];

    if cfg!(debug_assertions) {
        let debug_socket_addr = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT));
        let server_local = Server::bind(&debug_socket_addr)
            .serve(make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(req_handler))
            }))
            .with_graceful_shutdown(shutdown_signal());

        socket_addrs.push(debug_socket_addr);

        for addr in socket_addrs {
            info!("Listening on http://{}", addr);
        }

        tokio::select! {
            _ = server_local => {},
            _ = server => {},
        }
    } else {
        for addr in socket_addrs {
            info!("Listening on http://{}", addr);
        }

        if let Err(e) = server.await {
            error!("server error: {}", e);
        }
    }
}
