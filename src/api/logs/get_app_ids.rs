use hyper::{Body, Request, Response};
use log::debug;
use serde_derive::Serialize;
use std::convert::Infallible;

#[derive(Serialize)]
struct GepAppIdsResponse {
    frames: Vec<CpuFrameStatus>,
}

pub async fn get_app_ids(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    let frames = match get_app_ids_between_dates(start_time, end_time).await {
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

    let res_json = serde_json::to_string(&res_model).unwrap();

    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(res_json))
        .unwrap();

    Ok(response)
}
