use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::auth;

use super::{authenticate, ResponseBody};

pub async fn validate_token_test(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let auth_header = match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    match auth::token::validate_token(&auth_header).await {
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
