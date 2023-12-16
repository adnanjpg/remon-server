use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::monitor::persistence::fetch_latest_hardware_info;

use super::ResponseBody;

pub async fn get_hardware_info(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let info = match fetch_latest_hardware_info().await {
        Ok(val) => val,
        Err(err) => {
            let bod = serde_json::to_string(&ResponseBody::Error(err.to_string())).unwrap();

            let response = Response::builder()
                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Body::from(bod))
                .unwrap();

            return Ok(response);
        }
    };

    let body = serde_json::to_string(&info).unwrap();

    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap();
    Ok(response)
}
