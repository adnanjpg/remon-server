use hyper::{Body, Request, Response};
use std::convert::Infallible;

pub fn hello(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Body::from("Hello World!"))
        .unwrap();

    Ok(response)
}
