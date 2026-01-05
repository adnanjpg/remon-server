use axum::{
    extract::{Path, Query},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use log::error;
use serde::{Deserialize, Serialize};

use crate::{
    api::extractors::Claims,
    monitor::{
        docker_actions::{
            self, get_docker_version, is_docker_available, DockerActionError,
        },
        docker_monitor::{get_container_state_from_inspect, get_container_state_from_summary},
        models::docker::{
            ContainerDetails, ContainerInfo, DockerActionResponse, DockerStatusResponse,
            GetContainerLogsRequest, GetDockerStatsRequest, GetDockerStatsResponse,
            ListContainersResponse,
        },
        persistence::{fetch_all_containers, fetch_container_by_id, get_docker_stats_between_dates},
    },
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Success(bool),
    Error(String),
}

/// GET /docker/status - Check if Docker is available
pub async fn get_docker_status(
    _claims: Claims,
) -> Result<Json<DockerStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    let available = is_docker_available().await;
    let version = if available {
        get_docker_version().await.ok()
    } else {
        None
    };

    Ok(Json(DockerStatusResponse { available, version }))
}

/// GET /docker/containers - List all containers
pub async fn list_containers(
    _claims: Claims,
) -> Result<Json<ListContainersResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let containers = docker_actions::list_containers().await.map_err(|e| {
        error!("Failed to list containers: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    // Get stored container info from DB
    let db_containers = fetch_all_containers().await.unwrap_or_default();

    let container_details: Vec<ContainerDetails> = containers
        .iter()
        .map(|c| {
            let container_id = c.id.as_ref().map(|s| s.as_str()).unwrap_or("");
            let name = c
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let image = c
                .image
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let created_at = c.created.unwrap_or(0);

            // Try to find stored info
            let db_info = db_containers
                .iter()
                .find(|db| db.container_id == container_id);

            let info = ContainerInfo {
                id: db_info.map(|i| i.id).unwrap_or(-1),
                container_id: container_id.to_string(),
                name,
                image,
                created_at,
                last_seen: Utc::now().timestamp_millis(),
            };

            let state = get_container_state_from_summary(c);

            ContainerDetails { info, state }
        })
        .collect();

    Ok(Json(ListContainersResponse {
        containers: container_details,
    }))
}

/// GET /docker/containers/:id - Get container details
pub async fn get_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<ContainerDetails>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let inspect_response = docker_actions::inspect_container(&container_id)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    // Get stored info from DB
    let db_info = fetch_container_by_id(&container_id).await.ok().flatten();

    let name = inspect_response
        .name
        .as_ref()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let image = inspect_response
        .config
        .as_ref()
        .and_then(|c| c.image.as_ref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let created_at = inspect_response
        .created
        .as_ref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    let info = ContainerInfo {
        id: db_info.map(|i| i.id).unwrap_or(-1),
        container_id: container_id.clone(),
        name,
        image,
        created_at,
        last_seen: Utc::now().timestamp_millis(),
    };

    let state = get_container_state_from_inspect(&inspect_response);

    Ok(Json(ContainerDetails { info, state }))
}

/// GET /docker/stats - Get Docker stats with date range
pub async fn get_stats(
    _claims: Claims,
    Query(params): Query<GetDockerStatsRequest>,
) -> Result<Json<GetDockerStatsResponse>, (StatusCode, Json<ResponseBody>)> {
    let frames = get_docker_stats_between_dates(params.start_time, params.end_time)
        .await
        .map_err(|e| {
            error!("Failed to get Docker stats: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    Ok(Json(GetDockerStatsResponse { frames }))
}

/// POST /docker/containers/:id/start - Start a container
pub async fn start_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::start_container(&container_id)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} started", container_id),
    }))
}

/// POST /docker/containers/:id/stop - Stop a container
pub async fn stop_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::stop_container(&container_id)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} stopped", container_id),
    }))
}

/// POST /docker/containers/:id/restart - Restart a container
pub async fn restart_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::restart_container(&container_id)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} restarted", container_id),
    }))
}

/// GET /docker/containers/:id/logs - Get container logs
pub async fn get_logs(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<GetContainerLogsRequest>,
) -> Result<Json<crate::monitor::models::docker::GetContainerLogsResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let logs = docker_actions::get_container_logs(&container_id, params.tail)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    Ok(Json(crate::monitor::models::docker::GetContainerLogsResponse { logs }))
}
