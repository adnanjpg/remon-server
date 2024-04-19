use hyper::{Body, Request, Response};
use serde_derive::Serialize;
use std::{collections::HashMap, convert::Infallible};

use crate::{
    api::{authenticate, ResponseBody},
    logs::{self, models::get_app_ids::GetAppIdsRequest},
};

#[derive(Serialize)]
struct GepAppIdsResponse {
    app_ids: Vec<String>,
}

pub async fn get_app_ids(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    let req = match req.uri().query() {
        None => GetAppIdsRequest {
            start_time: None,
            end_time: None,
        },
        Some(query_str) => {
            let query_params: Vec<&str> = query_str.split("&").collect();

            // convert query params to GetAppIdsRequest
            // first, split '=' from query params
            // and get them as key-value pair (HashMap)
            let spl: HashMap<String, String> = query_params
                .iter()
                .map(|x| x.split("=").collect::<Vec<&str>>())
                .map(|x| (x[0].to_string(), x[1].to_string()))
                .collect();

            // then, convert HashMap to GetAppIdsRequest
            let req = GetAppIdsRequest {
                start_time: match spl.get("start_time") {
                    Some(val) => {
                        let par = val.parse::<i64>();

                        match par {
                            Ok(val) => Some(val),
                            Err(_) => None,
                        }
                    }
                    None => None,
                },
                end_time: match spl.get("end_time") {
                    Some(val) => {
                        let par = val.parse::<i64>();

                        match par {
                            Ok(val) => Some(val),
                            Err(_) => None,
                        }
                    }
                    None => None,
                },
            };

            req
        }
    };

    let start_time = req.start_time;
    let end_time = req.end_time;

    let err_res_builder = |err: String| {
        Response::builder()
            .status(hyper::StatusCode::BAD_REQUEST)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&ResponseBody::Error(err)).unwrap(),
            ))
            .unwrap()
    };

    let app_ids = match logs::persistence::get_app_ids(start_time, end_time).await {
        Ok(val) => val,
        Err(err) => {
            let bod = serde_json::to_string(&ResponseBody::Error(err.to_string())).unwrap();

            let response = err_res_builder(bod);

            return Ok(response);
        }
    };

    let res_model = GepAppIdsResponse { app_ids };

    let res_json = serde_json::to_string(&res_model);

    let response = match res_json {
        Ok(res_json) => Response::builder()
            .status(hyper::StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(res_json))
            .unwrap(),
        Err(err) => err_res_builder(err.to_string()),
    };

    Ok(response)
}
