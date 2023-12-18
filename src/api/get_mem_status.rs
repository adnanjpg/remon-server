use hyper::{Body, Request, Response};
use log::debug;
use serde_derive::Serialize;
use std::convert::Infallible;

use crate::{
    api::{authenticate, ResponseBody},
    monitor::{
        models::get_mem_status::{GetMemStatusRequest, MemFrameStatus},
        persistence::get_mem_status_between_dates,
    },
};

#[derive(Serialize)]
struct GetMemStatusResponse {
    frames: Vec<MemFrameStatus>,
}

pub async fn get_mem_status(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    // read query params and convert to GetMemStatusRequest
    let query_str = req.uri().query().unwrap();
    let query_params: Vec<&str> = query_str.split("&").collect();
    let start_time = query_params[0].split("=").collect::<Vec<&str>>()[1]
        .parse::<i64>()
        .unwrap();
    let end_time = query_params[1].split("=").collect::<Vec<&str>>()[1]
        .parse::<i64>()
        .unwrap();
    let req = GetMemStatusRequest {
        start_time,
        end_time,
    };

    let start_time = req.start_time;
    let end_time = req.end_time;

    debug!("start_time: {}", start_time);
    debug!("end_time: {}", end_time);

    // TODO(isaidsari): read data frequency from config

    let frames = match get_mem_status_between_dates(start_time, end_time).await {
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

    let res_model = GetMemStatusResponse { frames };

    let res_json = serde_json::to_string(&res_model).unwrap();

    let response = Response::builder()
        .status(hyper::StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(res_json))
        .unwrap();

    Ok(response)
}
