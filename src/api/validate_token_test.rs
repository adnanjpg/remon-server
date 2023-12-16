use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::auth;

use super::response_body::ResponseBody;

pub async fn validate_token_test(req: Request<Body>) -> Result<Response<Body>, Infallible> {
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

    match auth::token::validate_token(auth_header).await {
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
                .status(hyper::StatusCode::UNAUTHORIZED)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Invalid auth token.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
    }
}
