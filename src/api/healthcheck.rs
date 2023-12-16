use hyper::{Body, Request, Response};
use std::convert::Infallible;

pub fn healthcheck(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Body::from("Running smoothly!"))
        .unwrap();
    Ok(response)
}
