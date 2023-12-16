use hyper::{Body, Request, Response};
use std::convert::Infallible;

use super::response_body::ResponseBody;

pub fn _404(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let response = Response::builder()
        .status(hyper::StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&ResponseBody::Error(
                "The requested resource was not found.".to_string(),
            ))
            .unwrap(),
        ))
        .unwrap();

    Ok(response)
}
