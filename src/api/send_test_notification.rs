use hyper::{Body, Request, Response};
use std::convert::Infallible;

use crate::notification_service;

use super::ResponseBody;

pub async fn send_test_notification(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    // Read the request body into a byte buffer
    let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    let body_json: serde_json::Value = serde_json::from_str(&body_str).unwrap();

    let device_ids = body_json["device_tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect::<Vec<&str>>();

    notification_service::send_notification_to_multi(&device_ids)
        .await
        .unwrap();

    let response = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&ResponseBody::Success(true)).unwrap(),
        ))
        .unwrap();

    Ok(response)
}
