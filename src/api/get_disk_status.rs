use hyper::{Body, Request, Response};
use log::debug;
use std::convert::Infallible;

use crate::monitor::{self};

pub fn get_disk_status(req: Request<Body>) -> Result<Response<Body>, Infallible> {
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
                id: 1,
                last_check: chrono::Utc::now().timestamp(),
                disks_usage: vec![
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 10.0,
                    },
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 10.0,
                    },
                ],
            },
            monitor::DiskFrameStatus {
                id: 1,
                last_check: chrono::Utc::now().timestamp(),
                disks_usage: vec![
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 30.0,
                    },
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 80.0,
                    },
                ],
            },
            monitor::DiskFrameStatus {
                id: 1,
                last_check: chrono::Utc::now().timestamp(),
                disks_usage: vec![
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 80.0,
                    },
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 90.0,
                    },
                ],
            },
            monitor::DiskFrameStatus {
                id: 1,
                last_check: chrono::Utc::now().timestamp(),
                disks_usage: vec![
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 10.0,
                    },
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 20.0,
                    },
                ],
            },
            monitor::DiskFrameStatus {
                id: 1,
                last_check: chrono::Utc::now().timestamp(),
                disks_usage: vec![
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
                        available: 100.0,
                    },
                    monitor::SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: "".to_string(),
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
