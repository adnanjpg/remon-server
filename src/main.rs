use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server};

use std::convert::Infallible;
use std::net::SocketAddr;

mod notification_service;

use serde::{Deserialize, Serialize};
use serde_json;

use env_logger;
use log::{debug, error, info};

mod auth;
mod monitor;

use local_ip_address::local_ip;

use std::convert::TryInto;
use std::time;

use crate::monitor::persistence;

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
                .body(Body::from("Hello World!"))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/teapot") => {
            let response = Response::builder()
                .status(hyper::StatusCode::IM_A_TEAPOT)
                .header("Content-Type", "text/plain")
                .body(Body::from("I'm a teapot!"))
                .unwrap();

            Ok(response)
        }
        (&Method::GET, "/healthcheck") => {
            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "text/plain")
                .body(Body::from("Running smoothly!"))
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
                        .header("Content-Type", "application/json")
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

            if auth::otp::check_totp_match_dev_id(&login.otp, &login.device_id) {
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
                    .header("Content-Type", "application/json")
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
                        .header("Content-Type", "application/json")
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

            let dev_id: String = match auth::token::validate_token(auth_header).await {
                Ok(value) => value,
                Err(_) => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::UNAUTHORIZED)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::to_string(&ResponseBody::error(
                                "Invalid auth token.".to_string(),
                            ))
                            .unwrap(),
                        ))
                        .unwrap();
                    return Ok(response);
                }
            };

            let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
            let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

            let update_info = match serde_json::from_str::<monitor::MonitorConfig>(&body_str) {
                Ok(req_json) => req_json,
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

            match persistence::insert_monitor_config(&update_info, &dev_id).await {
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
                        .header("Content-Type", "application/json")
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
        (&Method::GET, "/get-hardware-info") => {
            let status = monitor::HardwareInfo {
                cpu_info: monitor::HardwareCpuInfo {
                    vendor_id: "Intel".to_string(),
                    brand: "Intel(R) Core(TM) i7-7700HQ CPU @ 2.80GHz".to_string(),
                },

                disks_info: vec![
                    monitor::HardwareDiskInfo {
                        name: "C:".to_string(),
                    },
                    monitor::HardwareDiskInfo {
                        name: "D:".to_string(),
                    },
                ],
                last_check: time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            };

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&status).unwrap()))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-cpu-status") => {
            // read query params and convert to GetCpuStatusRequest
            let query_str = req.uri().query().unwrap();
            let query_params: Vec<&str> = query_str.split("&").collect();
            let start_time = query_params[0].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let end_time = query_params[1].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let req = monitor::GetCpuStatusRequest {
                start_time: start_time,
                end_time: end_time,
            };

            debug!("start_time: {}", req.start_time);
            debug!("end_time: {}", req.end_time);

            // TODO(isaidsari): read data frequency from config
            // TODO(isaidsari): convert from static data to real data

            let status = monitor::CpuStatusData {
                frames: vec![
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 1.8,
                                usage: 0.3,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.5,
                                usage: 0.1,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.5,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.4,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.1,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.1,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.99,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.99,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.7,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.6,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.22,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.25,
                            },
                        ],
                    },
                    monitor::CpuFrameStatus {
                        cores_usage: vec![
                            monitor::CpuCoreInfo {
                                freq: 2.8,
                                usage: 0.9,
                            },
                            monitor::CpuCoreInfo {
                                freq: 2.1,
                                usage: 0.99,
                            },
                        ],
                    },
                ],
            };

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&status).unwrap()))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-mem-status") => {
            // read query params and convert to GetCpuStatusRequest
            let query_str = req.uri().query().unwrap();
            let query_params: Vec<&str> = query_str.split("&").collect();
            let start_time = query_params[0].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let end_time = query_params[1].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let req = monitor::GetMemStatusRequest {
                start_time: start_time,
                end_time: end_time,
            };

            debug!("start_time: {}", req.start_time);
            debug!("end_time: {}", req.end_time);

            // TODO(isaidsari): read data frequency from config
            // TODO(isaidsari): convert from static data to real data

            let status = monitor::MemStatusData {
                frames: vec![
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 50,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 60,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 70,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 30,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 10,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 90,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 10,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 40,
                    },
                    monitor::MemFrameStatus {
                        total: 100,
                        available: 60,
                    },
                ],
            };

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&status).unwrap()))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-disk-status") => {
            // read query params and convert to GetCpuStatusRequest
            let query_str = req.uri().query().unwrap();
            let query_params: Vec<&str> = query_str.split("&").collect();
            let start_time = query_params[0].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let end_time = query_params[1].split("=").collect::<Vec<&str>>()[1]
                .parse::<i64>()
                .unwrap();
            let req = monitor::GetDiskStatusRequest {
                start_time: start_time,
                end_time: end_time,
            };

            debug!("start_time: {}", req.start_time);
            debug!("end_time: {}", req.end_time);

            // TODO(isaidsari): read data frequency from config
            // TODO(isaidsari): convert from static data to real data

            let status = monitor::DiskStatusData {
                frames: vec![
                    monitor::DiskFrameStatus {
                        disks_usage: vec![
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 10.0,
                            },
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 10.0,
                            },
                        ],
                    },
                    monitor::DiskFrameStatus {
                        disks_usage: vec![
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 30.0,
                            },
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 80.0,
                            },
                        ],
                    },
                    monitor::DiskFrameStatus {
                        disks_usage: vec![
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 80.0,
                            },
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 90.0,
                            },
                        ],
                    },
                    monitor::DiskFrameStatus {
                        disks_usage: vec![
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 10.0,
                            },
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 20.0,
                            },
                        ],
                    },
                    monitor::DiskFrameStatus {
                        disks_usage: vec![
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 100.0,
                            },
                            monitor::SingleDiskInfo {
                                total: 100.0,
                                available: 99.0,
                            },
                        ],
                    },
                ],
            };

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&status).unwrap()))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/get-status") => {
            /*let auth_header = req.headers().get("Authorization");

            let auth_header = match auth_header {
                Some(h) => h.to_str().unwrap(),
                None => {
                    let response = Response::builder()
                        .status(hyper::StatusCode::FORBIDDEN)
                        .header("Content-Type", "application/json")
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
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&ResponseBody::error(
                            "Invalid auth token.".to_string(),
                        ))
                        .unwrap(),
                    ))
                    .unwrap();
                return Ok(response);
            }
                        let status = match monitor::fetch_monitor_status().await {
                            Ok(status) => status,
                            Err(_) => {
                                let response = Response::builder()
                                    .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                                    .header("Content-Type", "application/json")
                                    .body(Body::from(
                                        serde_json::to_string(&ResponseBody::error(
                                            "Failed to update monitor config.".to_string(),
                                        ))
                                        .unwrap(),
                                    ))
                                    .unwrap();
                                return Ok(response);
                            }
                        };
            */

            let status = monitor::MonitorStatus {
                cpu_usage: monitor::CpuStatus {
                    vendor_id: "Intel".to_string(),
                    brand: "Intel(R) Core(TM) i7-7700HQ CPU @ 2.80GHz".to_string(),
                    cpu_usage: vec![
                        monitor::CoreInfo {
                            cpu_freq: 2.8,
                            cpu_usage: 0.5,
                        },
                        monitor::CoreInfo {
                            cpu_freq: 2.5,
                            cpu_usage: 0.3,
                        },
                    ],
                },
                mem_usage: monitor::MemStatus {
                    total: 100,
                    available: 50,
                },
                storage_usage: vec![
                    monitor::DiskStatus {
                        name: "C:".to_string(),
                        total: 100,
                        available: 50,
                    },
                    monitor::DiskStatus {
                        name: "D:".to_string(),
                        total: 100,
                        available: 50,
                    },
                ],
                last_check: time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            };

            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&status).unwrap()))
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
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::success(true)).unwrap(),
                ))
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
                        .header("Content-Type", "application/json")
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
                        .header("Content-Type", "application/json")
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
                .header("Content-Type", "application/json")
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
        env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .init();
    } else {
        env_logger::builder()
            .filter_level(log::LevelFilter::Error)
            .init();
    }
}

#[tokio::main]
async fn main() {
    init_logger();

    let socket_addr = match get_socket_addr() {
        Some(addr) => addr,
        None => {
            error!("Failed to get local IP address.");
            return;
        }
    };

    monitor::init().await;

    info!("Starting server at {}", socket_addr);

    if cfg!(debug_assertions) {
        let server_local = Server::bind(&SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))).serve(
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(req_handler)) }),
        );

        let server = Server::bind(&socket_addr).serve(make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(req_handler))
        }));

        tokio::select! {
            _ = server_local => {},
            _ = server => {},
        }
    } else {
        let server = Server::bind(&socket_addr).serve(make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(req_handler))
        }));

        if let Err(e) = server.await {
            error!("server error: {}", e);
        }
    }
}
