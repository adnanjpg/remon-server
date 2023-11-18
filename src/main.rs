use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server};

use std::convert::Infallible;
use std::net::SocketAddr;

mod notification_service;

use serde::{Deserialize, Serialize};
use serde_json;

use env_logger;
use log::{debug, error, info, warn};

mod auth;
mod monitor;

async fn req_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    #[derive(Serialize, Deserialize)]
    #[allow(non_camel_case_types)]
    enum ResponseBody {
        success(bool),
        error(String),
        token(String),
    }

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => {
            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "text/plain")
                .body(Body::from("Hello World!\r\n"))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/teapot") => {
            let response = Response::builder()
                .status(hyper::StatusCode::IM_A_TEAPOT)
                .header("Content-Type", "text/plain")
                .body(Body::from("I'm a teapot!\r\n"))
                .unwrap();
            Ok(response)
        }
        (&Method::POST, "/get-otp-qr") => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            let device_id = match serde_json::from_str::<serde_json::Value>(&body_str) {
                Ok(json) => match json["device_id"].as_str() {
                    Some(s) => s.to_string(),
                    None => {
                        let response = Response::builder()
                            .status(hyper::StatusCode::BAD_REQUEST)
                            .header("Content-Type", "application/json")
                            .body(Body::from(
                                serde_json::to_string(&ResponseBody::error(
                                    "Invalid Value.".to_string(),
                                ))
                                .unwrap(),
                            ))
                            .unwrap();
                        return Ok(response);
                    }
                },
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::BAD_REQUEST)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Invalid JSON.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            // TODO(isaidsari): handle invalid device_id cases

            let url = auth::otp::generate_otp_qr_url(&device_id);

            match auth::otp::outputqr(&url) {
                Ok(qr) => {
                    // Print the QR code to the terminal
                    println!("{}\r\n{}", url, qr);
                    let response = Response::builder()
                        .status(hyper::StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::success(true)).unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Failed to generate QR code.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
            }
        }
        (&Method::POST, "/login") => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            let login = match serde_json::from_str::<auth::token::LoginRequest>(&body_str) {
                Ok(req_json) => req_json,
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::BAD_REQUEST)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Invalid JSON.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            if auth::otp::check_totp_match(&login.otp, auth::otp::TOTP_KEY) {
                let token = auth::token::generate_token(&login.device_id).await.unwrap();

                let response = Response::builder()
                    .status(hyper::StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&ResponseBody::token(token)).unwrap(),
                    ))
                    .unwrap();
                Ok(response)
            } else {
                let response = Response::builder()
                    .status(hyper::StatusCode::UNAUTHORIZED)
                    .header("Content-Type", "text/plain")
                    .body(Body::from(
                        serde_json::to_string(&ResponseBody::error(
                            "Invalid OTP code.".to_string(),
                        ))
                        .unwrap(),
                    ))
                    .unwrap();
                Ok(response)
            }
        }
        (&Method::POST, "/update-info") => {
            let auth_header = req.headers().get("Authorization");

            let auth_header = match auth_header {
                Some(h) => h.to_str().unwrap(),
                None => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::FORBIDDEN)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Missing auth token.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            if !auth::token::validate_token(auth_header).await.is_ok() {
                let response = Response::builder()
                    .status(hyper::StatusCode::UNAUTHORIZED)
                    .header("Content-Type", "text/plain")
                    .body(Body::from(
                        serde_json::to_string(&ResponseBody::error(
                            "Invalid auth token.".to_string(),
                        ))
                        .unwrap(),
                    ))
                    .unwrap();
                return Ok(response);
            }

            // TODO(): check if the device_id in the token matches the device_id in the request body

            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            let update_info = match serde_json::from_str::<monitor::MonitorConfig>(&body_str) {
                Ok(req_json) => req_json,
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::BAD_REQUEST)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Invalid JSON.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            match monitor::insert_monitor_config(&update_info).await {
                Ok(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::success(true)).unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Failed to update monitor config.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
            }
        }
        (&Method::GET, "/get-desc") => {
            let desc = monitor::get_default_server_desc();

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&desc).unwrap()))
                .unwrap();
            Ok(response)
        }
        (&Method::POST, "/send-test-notification") => {
            // Read the request body into a byte buffer
            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
            let body_json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

            let device_ids = body_json["device_tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap())
                .collect::<Vec<&str>>();

            notification_service::send_notification_to_multi(&device_ids)
                .await
                .unwrap();

            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(""))
                .unwrap();

            Ok(response)
        }
        (&Method::GET, "/validate-token-test") => {
            let auth_header = req.headers().get("Authorization");

            let auth_header = match auth_header {
                Some(h) => h.to_str().unwrap(),
                None => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::FORBIDDEN)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Missing auth token.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            match auth::token::validate_token(auth_header).await {
                Ok(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::success(true)).unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::UNAUTHORIZED)
                        .header("Content-Type", "text/plain")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Invalid auth token.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    Ok(response)
                }
            }
        }
        (_, _) => {
            let response = Response::builder()
                .status(hyper::StatusCode::NOT_FOUND)
                .header("Content-Type", "text/plain")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::error(
                        "The requested resource was not found.".to_string(),
                    ))
                    .unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
    }
}

fn init_logger() {
    if cfg!(debug_assertions) {
        // use default configuration
        env_logger::init();
    } else {
        env_logger::builder()
            // remove debug level logs in prod
            .filter_level(log::LevelFilter::Debug)
            .init();
    }
}

#[tokio::main]
async fn main() {
    monitor::init_db().await;

    init_logger();

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    let server = Server::bind(&addr).serve(make_service_fn(|_conn| async {
        Ok::<_, Infallible>(service_fn(req_handler))
    }));

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
