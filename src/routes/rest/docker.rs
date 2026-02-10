use axum::{
    extract::{Path, Query},
    http::StatusCode,
    Json,
};
use log::error;
use serde::{Deserialize, Serialize};

use crate::{
    routes::{dtos::common::ResponseBody, extractors::Claims},
    services::docker::{self, DockerError, PruneResult},
};

// ==================== Response Types ====================

#[derive(Debug, Serialize)]
pub struct DockerStatusResponse {
    pub available: bool,
    pub version: Option<String>,
    pub backend: Option<String>,
    pub api_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub names: Vec<String>,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: i64,
}

#[derive(Debug, Serialize)]
pub struct ListContainersResponse {
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Serialize)]
pub struct ImageInfo {
    pub id: String,
    pub tags: Vec<String>,
    pub size: i64,
    pub created: i64,
}

#[derive(Debug, Serialize)]
pub struct ListImagesResponse {
    pub images: Vec<ImageInfo>,
}

#[derive(Debug, Serialize)]
pub struct DockerActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ContainerInspectResponse {
    pub container: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct GetContainerLogsResponse {
    pub logs: Vec<String>,
}

// ==================== Request Types ====================

#[derive(Debug, Deserialize)]
pub struct GetContainerLogsRequest {
    pub tail: Option<usize>,
    pub since: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ForceDeleteRequest {
    #[serde(default)]
    pub force: bool,
}

// ==================== Error Handling ====================

impl From<DockerError> for (StatusCode, Json<ResponseBody>) {
    fn from(err: DockerError) -> Self {
        match err {
            DockerError::NotAvailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ResponseBody::Error(format!("Docker not available: {}", msg))),
            ),
            DockerError::ContainerNotFound(msg) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(format!("Container not found: {}", msg))),
            ),
            DockerError::ImageNotFound(msg) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(format!("Image not found: {}", msg))),
            ),
            DockerError::ApiError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(format!("Docker API error: {}", msg))),
            ),
        }
    }
}

// ==================== Endpoints ====================

/// GET /docker/status - Check if Docker is available
pub async fn get_docker_status(
    _claims: Claims,
) -> Result<Json<DockerStatusResponse>, (StatusCode, Json<ResponseBody>)> {
    let available = docker::is_docker_available().await;

    if available {
        match docker::get_docker_version().await {
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
    let containers = docker::list_containers().await.map_err(|e| {
        error!("Failed to list containers: {}", e);
        e
    })?;

    let container_info: Vec<ContainerInfo> = containers
        .iter()
        .map(|c| ContainerInfo {
            id: c.id.clone().unwrap_or_default(),
            names: c.names.clone().unwrap_or_default(),
            image: c.image.clone().unwrap_or_default(),
            state: c.state.as_ref().map(|s| s.to_string()).unwrap_or_default(),
            status: c.status.clone().unwrap_or_default(),
            created: c.created.unwrap_or(0),
        })
        .collect();

    Ok(Json(ListContainersResponse {
        containers: container_info,
    }))
}

/// POST /docker/containers/:id/start - Start a container
pub async fn start_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    docker::start_container(&container_id).await?;

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
    docker::stop_container(&container_id).await?;

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
    docker::restart_container(&container_id).await?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} restarted", container_id),
    }))
}

/// POST /docker/containers/:id/pause - Pause a container
pub async fn pause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    docker::pause_container(&container_id).await?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} paused", container_id),
    }))
}

/// POST /docker/containers/:id/unpause - Unpause a container
pub async fn unpause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    docker::unpause_container(&container_id).await?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} unpaused", container_id),
    }))
}

/// DELETE /docker/containers/:id - Delete a container
pub async fn delete_container(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    docker::delete_container(&container_id, params.force).await?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} deleted", container_id),
    }))
}

/// GET /docker/containers/:id/inspect - Get container inspect details
pub async fn inspect_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<ContainerInspectResponse>, (StatusCode, Json<ResponseBody>)> {
    let container = docker::get_container_inspect(&container_id).await?;
    let container_json = serde_json::to_value(container).unwrap_or(serde_json::Value::Null);

    Ok(Json(ContainerInspectResponse {
        container: container_json,
    }))
}

/// GET /docker/containers/:id/logs - Get container logs
pub async fn get_logs(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<GetContainerLogsRequest>,
) -> Result<Json<GetContainerLogsResponse>, (StatusCode, Json<ResponseBody>)> {
    let logs = docker::get_container_logs(&container_id, params.tail, params.since).await?;

    Ok(Json(GetContainerLogsResponse { logs }))
}

/// GET /docker/containers/:id/stats - Get real-time container stats
pub async fn get_container_stats(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ResponseBody>)> {
    let stats = docker::get_container_stats(&container_id).await?;
    Ok(Json(stats))
}

/// POST /docker/containers/prune - Prune stopped containers
pub async fn prune_containers(
    _claims: Claims,
) -> Result<Json<PruneResult>, (StatusCode, Json<ResponseBody>)> {
    let result = docker::prune_containers().await.map_err(|e| {
        error!("Failed to prune containers: {}", e);
        e
    })?;

    Ok(Json(result))
}

/// GET /docker/images - List all images
pub async fn list_images(
    _claims: Claims,
) -> Result<Json<ListImagesResponse>, (StatusCode, Json<ResponseBody>)> {
    let images = docker::list_images().await.map_err(|e| {
        error!("Failed to list images: {}", e);
        e
    })?;

    let image_info: Vec<ImageInfo> = images
        .iter()
        .map(|img| ImageInfo {
            id: img.id.clone(),
            tags: img.repo_tags.clone(),
            size: img.size,
            created: img.created,
        })
        .collect();

    Ok(Json(ListImagesResponse { images: image_info }))
}

/// DELETE /docker/images/:id - Delete an image
pub async fn delete_image(
    _claims: Claims,
    Path(image_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> Result<Json<DockerActionResponse>, (StatusCode, Json<ResponseBody>)> {
    docker::delete_image(&image_id, params.force).await?;

    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Image {} deleted", image_id),
    }))
}

/// POST /docker/images/prune - Prune unused images
pub async fn prune_images(
    _claims: Claims,
) -> Result<Json<PruneResult>, (StatusCode, Json<ResponseBody>)> {
    let result = docker::prune_images().await.map_err(|e| {
        error!("Failed to prune images: {}", e);
        e
    })?;

    Ok(Json(result))
}
