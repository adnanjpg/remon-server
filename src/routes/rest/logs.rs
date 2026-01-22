use axum::{extract::Query, http::StatusCode, Json};

use crate::{
    logs::{
        models::get_app_logs::{OrderBy, OrderByDirection, PaginationInfo},
        persistence::{get_app_ids, get_app_logs, LogLevel},
    },
    routes::{
        dtos::{
            common::ResponseBody,
            logs::{GetAppIdsQueryParams, GetAppIdsResponse, GetAppLogsQueryParams, GetAppLogsResponse},
        },
        extractors::Claims,
    },
};

pub async fn get_app_ids_handler(
    _claims: Claims,
    Query(params): Query<GetAppIdsQueryParams>,
) -> Result<Json<GetAppIdsResponse>, (StatusCode, Json<ResponseBody>)> {
    let ids = get_app_ids(params.start_time, params.end_time)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetAppIdsResponse { ids }))
}

pub async fn get_app_logs_handler(
    _claims: Claims,
    Query(params): Query<GetAppLogsQueryParams>,
) -> Result<Json<GetAppLogsResponse>, (StatusCode, Json<ResponseBody>)> {
    let include_app_ids = params
        .include_app_ids
        .map(|s| s.split(',').map(|x| x.to_string()).collect());

    let order_by = params.order_by.and_then(|s| match s.as_str() {
        "Time" => Some(OrderBy::Time),
        "AppId" => Some(OrderBy::AppId),
        "Log" => Some(OrderBy::Log),
        _ => None,
    });

    let order_by_direction = params
        .order_by_direction
        .and_then(|s| match s.as_str() {
            "Asc" => Some(OrderByDirection::Asc),
            "Desc" => Some(OrderByDirection::Desc),
            _ => None,
        });

    let levels = params
        .levels
        .map(|s| s.split(',').map(|x| LogLevel::from_string(&x.to_string())).collect());

    let pagination_info = PaginationInfo {
        page: params.page,
        page_size: params.page_size,
    };

    let app_logs = get_app_logs(
        params.start_time,
        params.end_time,
        include_app_ids,
        order_by,
        order_by_direction,
        params.filter_by_word,
        levels,
        &pagination_info,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    let len = app_logs.len() as i64;
    let res_model = GetAppLogsResponse {
        total: pagination_info.page * pagination_info.page_size + len,
        page: pagination_info.page,
        page_size: len,
        logs: app_logs,
    };

    Ok(Json(res_model))
}
