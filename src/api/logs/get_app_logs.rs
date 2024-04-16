use hyper::{Body, Request, Response};
use serde_derive::Serialize;
use std::{collections::HashMap, convert::Infallible};

use crate::{
    api::{authenticate, ResponseBody},
    logs::{
        self,
        models::get_app_logs::{AppLogRecord, GetAppLogsRequest},
        persistence::AppLog,
    },
};

#[derive(Serialize)]
struct GepAppLogsResponse {
    total: i64,
    page: i64,
    page_size: i64,
    logs: Vec<AppLog>,
}

pub async fn get_app_logs(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match authenticate(&req) {
        Ok(val) => val,
        Err(err) => {
            return Ok(err);
        }
    };

    let err_res_builder = |err: String| {
        Response::builder()
            .status(hyper::StatusCode::BAD_REQUEST)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_string(&ResponseBody::Error(err)).unwrap(),
            ))
            .unwrap()
    };

    let req = match req.uri().query() {
        None => return Ok(err_res_builder("No query params provided".to_string())),
        Some(query_str) => {
            let query_params: Vec<&str> = query_str.split("&").collect();

            // convert query params to GetAppLogsRequest
            // first, split '=' from query params
            // and get them as key-value pair (HashMap)
            let spl: HashMap<String, String> = query_params
                .iter()
                .map(|x| x.split("=").collect::<Vec<&str>>())
                .map(|x| (x[0].to_string(), x[1].to_string()))
                .collect();

            // then, convert HashMap to GetAppLogsRequest
            let req = GetAppLogsRequest {
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
                include_app_ids: match spl.get("include_app_ids") {
                    Some(val) => {
                        let par = val.split(",").map(|x| x.to_string()).collect();

                        Some(par)
                    }
                    None => None,
                },
                order_by: match spl.get("order_by") {
                    Some(val) => match val.as_str() {
                        "Time" => Some(logs::models::get_app_logs::OrderBy::Time),
                        "AppId" => Some(logs::models::get_app_logs::OrderBy::AppId),
                        "Log" => Some(logs::models::get_app_logs::OrderBy::Log),
                        _ => None,
                    },
                    None => None,
                },
                order_by_direction: match spl.get("order_by_direction") {
                    Some(val) => match val.as_str() {
                        "Asc" => Some(logs::models::get_app_logs::OrderByDirection::Asc),
                        "Desc" => Some(logs::models::get_app_logs::OrderByDirection::Desc),
                        _ => None,
                    },
                    None => None,
                },
                filter_by_word: match spl.get("filter_by_word") {
                    Some(val) => Some(val.to_string()),
                    None => None,
                },
                levels: match spl.get("levels") {
                    Some(val) => {
                        let par = val
                            .split(",")
                            .map(|x| logs::persistence::LogLevel::from_string(&x.to_string()))
                            .collect();

                        Some(par)
                    }
                    None => None,
                },
                pagination_info: logs::models::get_app_logs::PaginationInfo {
                    page: match spl.get("page") {
                        Some(val) => {
                            let par = val.parse::<i64>();

                            match par {
                                Ok(val) => val,
                                Err(_) => 1,
                            }
                        }
                        None => 1,
                    },
                    page_size: match spl.get("page_size") {
                        Some(val) => {
                            let par = val.parse::<i64>();

                            match par {
                                Ok(val) => val,
                                Err(_) => 10,
                            }
                        }
                        None => 10,
                    },
                },
            };

            req
        }
    };

    let start_time = req.start_time;
    let end_time = req.end_time;
    let pag_info = req.pagination_info;

    let app_logs = match logs::persistence::get_app_logs(
        start_time,
        end_time,
        req.include_app_ids,
        req.order_by,
        req.order_by_direction,
        req.filter_by_word,
        req.levels,
        &pag_info,
    )
    .await
    {
        Ok(val) => val,
        Err(err) => {
            let bod = serde_json::to_string(&ResponseBody::Error(err.to_string())).unwrap();

            let response = err_res_builder(bod);

            return Ok(response);
        }
    };

    let req_len = &pag_info.page_size;
    let page = &pag_info.page;
    let len = app_logs.len() as i64;
    let res_model = GepAppLogsResponse {
        total: page * req_len + len,
        page: page + 1,
        page_size: len,
        logs: app_logs,
    };

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
