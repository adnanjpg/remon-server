use axum::{extract::Query, http::StatusCode, Json};
use log::debug;
use serde::{Deserialize, Serialize};

use crate::{
    api::extractors::Claims,
    monitor::{
        models::{
            get_cpu_status::{CpuFrameStatus, GetCpuStatusRequest},
            get_disk_status::DiskFrameStatus,
            get_mem_status::{GetMemStatusRequest, MemFrameStatus},
            get_network_status::{GetNetworkStatusRequest, NetworkFrameStatus},
            system_info::SystemInfo,
            MonitorConfig, UpdateInfoRequest,
        },
        persistence::{
            fetch_latest_hardware_info, get_cpu_status_between_dates,
            get_disk_status_between_dates, get_latest_cpu_status, get_latest_disk_status,
            get_latest_mem_status, get_latest_network_status, get_mem_status_between_dates,
            get_network_status_between_dates, insert_or_update_monitor_config,
        },
        system_monitor,
    },
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Success(bool),
    Error(String),
}

// Response types
#[derive(Serialize)]
pub struct GetCpuStatusResponse {
    pub frames: Vec<CpuFrameStatus>,
}

#[derive(Serialize)]
pub struct GetMemStatusResponse {
    pub frames: Vec<MemFrameStatus>,
}

#[derive(Serialize)]
pub struct GetDiskStatusResponse {
    pub frames: Vec<DiskFrameStatus>,
}

#[derive(Serialize)]
pub struct GetNetworkStatusResponse {
    pub frames: Vec<NetworkFrameStatus>,
}

// Handlers
pub async fn get_desc() -> Result<Json<serde_json::Value>, StatusCode> {
    let desc = crate::monitor::get_default_server_desc();
    Ok(Json(serde_json::to_value(desc).unwrap()))
}

pub async fn get_hardware_info(
    _claims: Claims,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ResponseBody>)> {
    let info = fetch_latest_hardware_info().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    Ok(Json(serde_json::to_value(info).unwrap()))
}

pub async fn get_cpu_status(
    _claims: Claims,
    Query(params): Query<GetCpuStatusRequest>,
) -> Result<Json<GetCpuStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    // If no time range provided, get the latest frame directly
    if params.start_time.is_none() && params.end_time.is_none() {
        let frame = get_latest_cpu_status().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

        let frames = frame.map_or(vec![], |f| vec![f]);
        return Ok(Json(GetCpuStatusResponse { frames }));
    }

    // Otherwise use time range query
    let now = chrono::Utc::now().timestamp_millis();
    let start_time = params.start_time.unwrap_or(now - 10000);
    let end_time = params.end_time.unwrap_or(now);

    debug!("start_time: {}", start_time);
    debug!("end_time: {}", end_time);

    let frames = get_cpu_status_between_dates(start_time, end_time)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetCpuStatusResponse { frames }))
}

pub async fn get_mem_status(
    _claims: Claims,
    Query(params): Query<GetMemStatusRequest>,
) -> Result<Json<GetMemStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    // If no time range provided, get the latest frame directly
    if params.start_time.is_none() && params.end_time.is_none() {
        let frame = get_latest_mem_status().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

        let frames = frame.map_or(vec![], |f| vec![f]);
        return Ok(Json(GetMemStatusResponse { frames }));
    }

    // Otherwise use time range query
    let now = chrono::Utc::now().timestamp_millis();
    let start_time = params.start_time.unwrap_or(now - 10000);
    let end_time = params.end_time.unwrap_or(now);

    debug!("start_time: {}", start_time);
    debug!("end_time: {}", end_time);

    let frames = get_mem_status_between_dates(start_time, end_time)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetMemStatusResponse { frames }))
}

pub async fn get_disk_status(
    _claims: Claims,
    Query(params): Query<GetMemStatusRequest>,
) -> Result<Json<GetDiskStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    // If no time range provided, get the latest frame directly
    if params.start_time.is_none() && params.end_time.is_none() {
        let frame = get_latest_disk_status().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

        let frames = frame.map_or(vec![], |f| vec![f]);
        return Ok(Json(GetDiskStatusResponse { frames }));
    }

    // Otherwise use time range query
    let now = chrono::Utc::now().timestamp_millis();
    let start_time = params.start_time.unwrap_or(now - 10000);
    let end_time = params.end_time.unwrap_or(now);

    debug!("start_time: {}", start_time);
    debug!("end_time: {}", end_time);

    let frames = get_disk_status_between_dates(start_time, end_time)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetDiskStatusResponse { frames }))
}

pub async fn update_info(
    claims: Claims,
    Json(update_info): Json<UpdateInfoRequest>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    let mon_config = MonitorConfig {
        id: -1,
        device_id: "".to_string(),
        cpu_threshold: update_info.cpu_threshold,
        disk_threshold: update_info.disk_threshold,
        mem_threshold: update_info.mem_threshold,
        fcm_token: update_info.fcm_token,
        updated_at: chrono::Utc::now().timestamp_millis(),
    };

    insert_or_update_monitor_config(&mon_config, &claims.device_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(format!(
                    "Failed to update monitor config: {}",
                    e
                ))),
            )
        })?;

    Ok(Json(ResponseBody::Success(true)))
}

pub async fn validate_token_test(_claims: Claims) -> Json<ResponseBody> {
    Json(ResponseBody::Success(true))
}

/// GET /monitor/system - Get current system information (htop-like snapshot)
pub async fn get_system_info(_claims: Claims) -> Json<SystemInfo> {
    Json(system_monitor::get_system_info())
}

/// GET /get-network-status - Get network I/O statistics
pub async fn get_network_status(
    _claims: Claims,
    Query(params): Query<GetNetworkStatusRequest>,
) -> Result<Json<GetNetworkStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    // If no time range provided, get the latest frame directly
    if params.start_time.is_none() && params.end_time.is_none() {
        let frame = get_latest_network_status().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

        let frames = frame.map_or(vec![], |f| vec![f]);
        return Ok(Json(GetNetworkStatusResponse { frames }));
    }

    // Otherwise use time range query
    let now = chrono::Utc::now().timestamp_millis();
    let start_time = params.start_time.unwrap_or(now - 10000);
    let end_time = params.end_time.unwrap_or(now);

    debug!("start_time: {}", start_time);
    debug!("end_time: {}", end_time);

    let frames = get_network_status_between_dates(start_time, end_time)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetNetworkStatusResponse { frames }))
}
