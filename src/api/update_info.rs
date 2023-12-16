use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::{
    auth,
    monitor::{self, persistence},
};

use super::response_body::ResponseBody;

pub async fn update_info(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let auth_header = req.headers().get("Authorization");

    let auth_header = match auth_header {
        Some(h) => h.to_str().unwrap(),
        None => {
            let response = Response::builder()
                .status(hyper::StatusCode::FORBIDDEN)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Missing auth token.".to_string()))
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
                    serde_json::to_string(&ResponseBody::Error("Invalid auth token.".to_string()))
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
                    serde_json::to_string(&ResponseBody::Error("Invalid JSON.".to_string()))
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
                    serde_json::to_string(&ResponseBody::Success(true)).unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
        Err(_) => {
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
