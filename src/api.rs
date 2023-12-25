use hyper::{Body, Request, Response};

pub mod _404;
pub mod get_cpu_status;
pub mod get_desc;
pub mod get_disk_status;
pub mod get_hardware_info;
pub mod get_mem_status;
pub mod get_otp_qr;
pub mod healthcheck;
pub mod hello;
pub mod login;
pub mod send_test_notification;
pub mod teapot;
pub mod update_info;
pub mod validate_token_test;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Success(bool),
    Error(String),
    Token(String),
}

fn authenticate(req: &Request<Body>) -> Result<String, Response<Body>> {
    let auth_header = req.headers().get("Authorization");

    match auth_header {
        Some(h) => {
            return Ok(h.to_str().unwrap().to_owned());
        }
        None => {
            let response = Response::builder()
                .status(hyper::StatusCode::FORBIDDEN)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Missing auth token.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            return Err(response);
        }
    };
}
