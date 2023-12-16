use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::monitor::{self};

pub async fn get_hardware_info(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    /*
    let auth_header = match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
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

    let status = monitor::HardwareInfo {
        cpu_info: vec![monitor::HardwareCpuInfo {
            cpu_id: "".to_string(),
            core_count: 3,
            vendor_id: "Intel".to_string(),
            brand: "Intel(R) Core(TM) i7-7700HQ CPU @ 2.80GHz".to_string(),
            last_check: chrono::Utc::now().timestamp(),
        }],

        disks_info: vec![
            monitor::HardwareDiskInfo {
                fs_type: "".to_string(),
                kind: "".to_string(),
                is_removable: false,
                mount_point: "".to_string(),
                total_space: 0.0,
                disk_id: "".to_string(),
                name: "C:".to_string(),
                last_check: chrono::Utc::now().timestamp(),
            },
            monitor::HardwareDiskInfo {
                fs_type: "".to_string(),
                kind: "".to_string(),
                is_removable: false,
                mount_point: "".to_string(),
                total_space: 0.0,
                disk_id: "".to_string(),
                name: "D:".to_string(),
                last_check: chrono::Utc::now().timestamp(),
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
