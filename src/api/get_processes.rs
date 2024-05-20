use hyper::{Body, Request, Response};
use log::debug;
use serde::Serialize;
use std::convert::Infallible;

use crate::{
    api::authenticate,
    monitor::{get_process_list, models::ProcessInfo},
};

#[derive(Serialize)]
struct ResponseData {
    processes: Vec<ProcessInfo>,
}

pub async fn get_processes(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    let start = std::time::Instant::now();
    let processes = get_process_list().await;
    debug!(
        "get_processes[{}] took: {:?}",
        processes.len(),
        start.elapsed()
    );

    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&ResponseData { processes }).unwrap(),
        ))
        .unwrap();

    Ok(response)
}
