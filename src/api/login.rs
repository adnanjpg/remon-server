use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::auth;

use super::response_body::ResponseBody;

pub async fn login(req: Request<Body>) -> Result<Response<Body>, Infallible> {
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
                    serde_json::to_string(&ResponseBody::Error("Invalid JSON.".to_string()))
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
                serde_json::to_string(&ResponseBody::Token(token)).unwrap(),
            ))
            .unwrap();
        Ok(response)
    } else {
        let response = Response::builder()
            .status(hyper::StatusCode::UNAUTHORIZED)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&ResponseBody::Error("Invalid OTP code.".to_string()))
                    .unwrap(),
            ))
            .unwrap();
        Ok(response)
    }
}
