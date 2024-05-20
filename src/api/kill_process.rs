use hyper::{Body, Request, Response};
use log::debug;
use std::convert::Infallible;

use crate::{
    api::{authenticate, ResponseBody},
    monitor,
};

pub async fn kill_process(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    // extract pid from query params
    let query_str = match req.uri().query() {
        Some(q) => q,
        None => {
            let response = Response::builder()
                .status(hyper::StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("The query parameters are missing.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            return Ok(response);
        }
    };
    let query_params: Vec<&str> = query_str.split("&").collect();
    let pid = match query_params[0].split("=").collect::<Vec<&str>>().get(1) {
        Some(p) => match p.parse::<u32>() {
            Ok(pid) => pid,
            Err(_) => {
                let response = Response::builder()
                    .status(hyper::StatusCode::BAD_REQUEST)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&ResponseBody::Error("Query paramter parse error.".to_string()))
                            .unwrap(),
                    ))
                    .unwrap();
                return Ok(response);
            }
        },
        None => {
            let response = Response::builder()
                .status(hyper::StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error("Invalid query parameters.".to_string()))
                        .unwrap(),
                ))
                .unwrap();
            return Ok(response);
        }
        
    };

    match monitor::kill_process(pid).await {
        Ok(_) => {
            debug!("Process {} killed successfully", pid);
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
            debug!("Error killing process: {}", err);
            let response = Response::builder()
                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ResponseBody::Error(err.to_string()))
                        .unwrap(),
                ))
                .unwrap();
            Ok(response)
        }
    }   
}
