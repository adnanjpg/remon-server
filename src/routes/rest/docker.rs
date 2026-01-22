use axum::{
    extract::{Path, Query},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use log::error;

use crate::{
    monitor::{
        docker_actions::{self, get_docker_version, is_docker_available, DockerActionError},
        docker_monitor::get_container_state_from_summary,
        models::docker::{
            ContainerDetails, ContainerInfo, ContainerInspectResponse, ContainerRealtimeStats,
            DockerActionResponse, DockerStatusResponse, ForceDeleteRequest,
            GetContainerLogsRequest, GetDockerStatsRequest, GetDockerStatsResponse,
            ListContainersResponse, ListImagesResponse, PruneResult,
        },
        persistence::{fetch_all_containers, get_docker_stats_between_dates},
    },
    routes::{dtos::common::ResponseBody, extractors::Claims},
};

/// GET /docker/status - Check if Docker is available
pub async fn get_docker_status(
    _claims: Claims,
) -> Result<Json<DockerStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    let available = is_docker_available().await;

    if available {
        match get_docker_version().await {
            Ok(info) => Ok(Json(DockerStatusResponse {
                available: true,
                version: Some(info.version),
                backend: Some(info.backend),
                api_version: Some(info.api_version),
                os: Some(info.os),
                arch: Some(info.arch),
            })),
            Err(_) => Ok(Json(DockerStatusResponse {
                available: true,
                version: None,
                backend: None,
                api_version: None,
                os: None,
                arch: None,
            })),
        }
    } else {
        Ok(Json(DockerStatusResponse {
            available: false,
            version: None,
            backend: None,
            api_version: None,
            os: None,
            arch: None,
        }))
    }
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
) -> Result<
    Json<crate::monitor::models::docker::GetContainerLogsResponse>,
    (StatusCode, Json<ResponseBody>),
> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let logs = docker_actions::get_container_logs(&container_id, &params)
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

    Ok(Json(
        crate::monitor::models::docker::GetContainerLogsResponse { logs },
    ))
}

/// GET /docker/containers/{id}/inspect - Get full container inspect details
pub async fn inspect_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<ContainerInspectResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let container = docker_actions::get_container_inspect_details(&container_id)
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

    Ok(Json(ContainerInspectResponse { container }))
}

/// GET /docker/containers/{id}/stats - Get real-time container stats
pub async fn get_container_stats(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<ContainerRealtimeStats>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let stats = docker_actions::get_realtime_stats(&container_id)
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

    Ok(Json(stats))
}

/// POST /docker/containers/{id}/pause - Pause a container
pub async fn pause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::pause_container(&container_id)
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
        message: format!("Container {} paused", container_id),
    }))
}

/// POST /docker/containers/{id}/unpause - Unpause a container
pub async fn unpause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::unpause_container(&container_id)
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
        message: format!("Container {} unpaused", container_id),
    }))
}

/// DELETE /docker/containers/{id} - Delete a container
pub async fn delete_container(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::delete_container(&container_id, params.force)
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
        message: format!("Container {} deleted", container_id),
    }))
}

/// POST /docker/containers/prune - Prune stopped containers
pub async fn prune_containers(
    _claims: Claims,
) -> Result<Json<PruneResult>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let result = docker_actions::prune_containers().await.map_err(|e| {
        error!("Failed to prune containers: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    Ok(Json(result))
}

/// GET /docker/images - List all images
pub async fn list_images(
    _claims: Claims,
) -> Result<Json<ListImagesResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let images = docker_actions::get_images().await.map_err(|e| {
        error!("Failed to list images: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    Ok(Json(ListImagesResponse { images }))
}

/// DELETE /docker/images/{id} - Delete an image
pub async fn delete_image(
    _claims: Claims,
    Path(image_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    docker_actions::delete_image(&image_id, params.force)
        .await
        .map_err(|e| match e {
            DockerActionError::ImageNotFound(_) => (
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
        message: format!("Image {} deleted", image_id),
    }))
}

/// POST /docker/images/prune - Prune unused images
pub async fn prune_images(
    _claims: Claims,
) -> Result<Json<PruneResult>, (StatusCode, Json<ResponseBody>)> {
    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let result = docker_actions::prune_images().await.map_err(|e| {
        error!("Failed to prune images: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(e.to_string())),
        )
    })?;

    Ok(Json(result))
}
