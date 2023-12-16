use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::monitor::{self};

pub fn get_desc(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let desc = monitor::get_default_server_desc();

    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&desc).unwrap()))
        .unwrap();
    Ok(response)
}
