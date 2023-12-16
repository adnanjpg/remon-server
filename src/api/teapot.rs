use hyper::{Body, Request, Response};
use std::convert::Infallible;

pub fn teapot(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let response = Response::builder()
        .status(hyper::StatusCode::IM_A_TEAPOT)
        .header("Content-Type", "text/plain")
        .body(Body::from("I'm a teapot!"))
        .unwrap();

    Ok(response)
}
