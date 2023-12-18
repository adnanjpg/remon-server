use hyper::{Body, Request, Response};
use log::error;
use std::convert::Infallible;

use crate::{
    auth,
    monitor::{self, persistence, MonitorConfig},
};

use super::{authenticate, ResponseBody};

pub async fn update_info(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let auth_header = match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    let dev_id: String = match auth::token::validate_token(&auth_header).await {
        Ok(value) => value,
        Err(_) => {
            let response = Response::builder()
                .status(hyper::StatusCode::UNAUTHORIZED)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Invalid auth token.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            return Ok(response);
        }
    };

    let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

    let update_info = match serde_json::from_str::<monitor::UpdateInfoRequest>(&body_str) {
        Ok(req_json) => req_json,
        Err(_) => {
            let response = Response::builder()
                .status(hyper::StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Invalid JSON.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            return Ok(response);
        }
    };

    let mon_config = MonitorConfig {
        id: -1,
        device_id: "".to_string(),
        cpu_threshold: update_info.cpu_threshold,
        disk_threshold: update_info.disk_threshold,
        mem_threshold: update_info.mem_threshold,
        updated_at: chrono::Utc::now().timestamp_millis(),
    };

    match persistence::insert_or_update_monitor_config(&mon_config, &dev_id).await {
        Ok(_) => {
            let response = Response::builder()
                .status(hyper::StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Success(true)).unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
        Err(err) => {
            error!("{}", err);

            let response = Response::builder()
                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error(
                        "Failed to update monitor config.".to_string(),
                    ))
                    .unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
    }
}
