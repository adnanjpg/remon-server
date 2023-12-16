use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::auth;

use super::ResponseBody;

pub async fn get_otp_qr(req: Request<Body>) -> Result<Response<Body>, Infallible> {
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
                        serde_json::to_string(&ResponseBody::Error("Invalid Value.".to_string()))
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
                    serde_json::to_string(&ResponseBody::Error("Invalid JSON.".to_string()))
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
                        "Failed to generate QR code.".to_string(),
                    ))
                    .unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
    }
}
