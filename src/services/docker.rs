use bollard::{
    models::ContainerInspectResponse as BollardInspectResponse,
    query_parameters::{ListContainersOptions, LogsOptions, RemoveContainerOptions, RemoveImageOptions, StatsOptions},
    secret::{ContainerSummary, ImageSummary},
    Docker,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// Docker service error type
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("Docker not available: {0}")]
    NotAvailable(String),
    #[error("Container not found: {0}")]
    ContainerNotFound(String),
    #[error("Image not found: {0}")]
    ImageNotFound(String),
    #[error("Docker API error: {0}")]
    ApiError(String),
}

impl From<bollard::errors::Error> for DockerError {
    fn from(err: bollard::errors::Error) -> Self {
        match err {
            bollard::errors::Error::DockerResponseServerError { status_code, message }
                if status_code == 404 =>
            {
                DockerError::ContainerNotFound(message)
            }
            _ => DockerError::ApiError(err.to_string()),
        }
    }
}

/// Docker version info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerVersionInfo {
    pub version: String,
    pub api_version: String,
    pub os: String,
    pub arch: String,
    pub backend: String,
}

/// Check if Docker is available
pub async fn is_docker_available() -> bool {
    match Docker::connect_with_local_defaults() {
        Ok(docker) => docker.ping().await.is_ok(),
        Err(_) => false,
    }
}

/// Get Docker version information
pub async fn get_docker_version() -> Result<DockerVersionInfo, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let version = docker.version().await?;

    Ok(DockerVersionInfo {
        version: version.version.unwrap_or_default(),
        api_version: version.api_version.unwrap_or_default(),
        os: version.os.unwrap_or_default(),
        arch: version.arch.unwrap_or_default(),
        backend: "Docker".to_string(),
    })
}

/// List all containers
pub async fn list_containers() -> Result<Vec<ContainerSummary>, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(ListContainersOptions {
        all: true,
        ..Default::default()
    });

    let containers = docker.list_containers(options).await?;
    Ok(containers)
}

/// Start a container
pub async fn start_container(container_id: &str) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    docker
        .start_container(container_id, None)
        .await?;

    Ok(())
}

/// Stop a container
pub async fn stop_container(container_id: &str) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    docker.stop_container(container_id, None).await?;
    Ok(())
}

/// Restart a container
pub async fn restart_container(container_id: &str) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    docker.restart_container(container_id, None).await?;
    Ok(())
}

/// Pause a container
pub async fn pause_container(container_id: &str) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    docker.pause_container(container_id).await?;
    Ok(())
}

/// Unpause a container
pub async fn unpause_container(container_id: &str) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    docker.unpause_container(container_id).await?;
    Ok(())
}

/// Delete a container
pub async fn delete_container(container_id: &str, force: bool) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(RemoveContainerOptions {
        force,
        ..Default::default()
    });

    docker.remove_container(container_id, options).await?;
    Ok(())
}

/// Get container inspect details
pub async fn get_container_inspect(
    container_id: &str,
) -> Result<BollardInspectResponse, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let container = docker
        .inspect_container(container_id, None)
        .await?;

    Ok(container)
}

/// Get container logs
pub async fn get_container_logs(
    container_id: &str,
    tail: Option<usize>,
    since: Option<i64>,
) -> Result<Vec<String>, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(LogsOptions {
        stdout: true,
        stderr: true,
        tail: tail.unwrap_or(100).to_string(),
        since: since.unwrap_or(0) as i32,
        ..Default::default()
    });

    let logs: Vec<String> = docker
        .logs(container_id, options)
        .map(|chunk| {
            chunk
                .map(|output| output.to_string())
                .unwrap_or_else(|e| format!("Error: {}", e))
        })
        .collect()
        .await;

    Ok(logs)
}

/// Stream container logs
pub async fn stream_container_logs(
    container_id: String,
    tail: Option<usize>,
) -> Result<Pin<Box<dyn Stream<Item = Result<String, DockerError>> + Send>>, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        tail: tail.unwrap_or(100).to_string(),
        ..Default::default()
    });

    let stream = docker
        .logs(&container_id, options)
        .map(|result| {
            result
                .map(|output| output.to_string())
                .map_err(|e| DockerError::ApiError(e.to_string()))
        });

    Ok(Box::pin(stream))
}

/// Get real-time container stats (single snapshot)
pub async fn get_container_stats(container_id: &str) -> Result<serde_json::Value, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(StatsOptions {
        stream: false,
        one_shot: true,
    });

    let mut stats_stream = docker.stats(container_id, options);

    if let Some(stats_result) = stats_stream.next().await {
        let stats = stats_result?;
        Ok(serde_json::to_value(stats).unwrap_or(serde_json::Value::Null))
    } else {
        Err(DockerError::ApiError("No stats available".to_string()))
    }
}

/// Prune stopped containers
#[derive(Debug, Serialize)]
pub struct PruneResult {
    pub containers_deleted: Vec<String>,
    pub space_reclaimed: u64,
}

pub async fn prune_containers() -> Result<PruneResult, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let result = docker.prune_containers(None).await?;

    Ok(PruneResult {
        containers_deleted: result.containers_deleted.unwrap_or_default(),
        space_reclaimed: result.space_reclaimed.unwrap_or(0) as u64,
    })
}

/// List all images
pub async fn list_images() -> Result<Vec<ImageSummary>, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    use bollard::query_parameters::ListImagesOptions as LOpts;
    let images = docker.list_images(Some(LOpts::default())).await?;
    Ok(images)
}

/// Delete an image
pub async fn delete_image(image_id: &str, force: bool) -> Result<(), DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    let options = Some(RemoveImageOptions {
        force,
        ..Default::default()
    });

    docker.remove_image(image_id, options, None).await?;
    Ok(())
}

/// Prune unused images
pub async fn prune_images() -> Result<PruneResult, DockerError> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| DockerError::NotAvailable(e.to_string()))?;

    use bollard::query_parameters::PruneImagesOptions as POpts;
    let result = docker.prune_images(Some(POpts::default())).await?;

    Ok(PruneResult {
        containers_deleted: result.images_deleted.unwrap_or_default().iter()
            .filter_map(|item| item.deleted.clone())
            .collect(),
        space_reclaimed: result.space_reclaimed.unwrap_or(0) as u64,
    })
}
