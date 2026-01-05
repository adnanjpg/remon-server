use bollard::models::{ContainerInspectResponse, ContainerSummary};
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RestartContainerOptionsBuilder, StartContainerOptions, StopContainerOptionsBuilder,
};
use bollard::Docker;
use log::{debug, error, info, warn};
use thiserror::Error;

use crate::config::Config;

#[derive(Debug, Error)]
pub enum DockerActionError {
    #[error("Docker is not available: {0}")]
    NotAvailable(String),
    #[error("Container not found: {0}")]
    ContainerNotFound(String),
    #[error("Docker API error: {0}")]
    ApiError(#[from] bollard::errors::Error),
    #[error("Config error: {0}")]
    ConfigError(String),
}

/// Get a Docker client instance
pub async fn get_docker_client() -> Result<Docker, DockerActionError> {
    let config = Config::new().map_err(|e| DockerActionError::ConfigError(e.to_string()))?;

    let docker = if config.docker.socket_path.is_empty() {
        // Use default connection based on platform
        Docker::connect_with_local_defaults()
    } else if config.docker.socket_path.starts_with("tcp://") {
        // TCP connection
        Docker::connect_with_http(
            &config.docker.socket_path,
            120,
            bollard::API_DEFAULT_VERSION,
        )
    } else {
        // Unix socket or named pipe
        #[cfg(unix)]
        {
            Docker::connect_with_unix(
                &config.docker.socket_path,
                120,
                bollard::API_DEFAULT_VERSION,
            )
        }
        #[cfg(windows)]
        {
            Docker::connect_with_named_pipe(
                &config.docker.socket_path,
                120,
                bollard::API_DEFAULT_VERSION,
            )
        }
    };

    docker.map_err(DockerActionError::ApiError)
}

/// Check if Docker is available
pub async fn is_docker_available() -> bool {
    match get_docker_client().await {
        Ok(docker) => match docker.ping().await {
            Ok(_) => true,
            Err(e) => {
                debug!("Docker ping failed: {}", e);
                false
            }
        },
        Err(e) => {
            debug!("Failed to create Docker client: {}", e);
            false
        }
    }
}

/// Get Docker version info
pub async fn get_docker_version() -> Result<String, DockerActionError> {
    let docker = get_docker_client().await?;
    let version = docker.version().await?;
    Ok(version.version.unwrap_or_else(|| "unknown".to_string()))
}

/// Start a container
pub async fn start_container(container_id: &str) -> Result<(), DockerActionError> {
    let docker = get_docker_client().await?;

    // Check if container exists
    docker
        .inspect_container(container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    docker
        .start_container(container_id, Some(StartContainerOptions::default()))
        .await?;

    info!("Started container: {}", container_id);
    Ok(())
}

/// Stop a container
pub async fn stop_container(container_id: &str) -> Result<(), DockerActionError> {
    let docker = get_docker_client().await?;

    // Check if container exists
    docker
        .inspect_container(container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    let options = StopContainerOptionsBuilder::new().t(10).build();
    docker.stop_container(container_id, Some(options)).await?;

    info!("Stopped container: {}", container_id);
    Ok(())
}

/// Restart a container
pub async fn restart_container(container_id: &str) -> Result<(), DockerActionError> {
    let docker = get_docker_client().await?;

    // Check if container exists
    docker
        .inspect_container(container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    let options = RestartContainerOptionsBuilder::new().t(10).build();
    docker
        .restart_container(container_id, Some(options))
        .await?;

    info!("Restarted container: {}", container_id);
    Ok(())
}

/// Get container logs
pub async fn get_container_logs(
    container_id: &str,
    tail: Option<usize>,
) -> Result<String, DockerActionError> {
    use futures_util::StreamExt;

    let docker = get_docker_client().await?;

    // Check if container exists
    docker
        .inspect_container(container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    let tail_str = tail
        .map(|t| t.to_string())
        .unwrap_or_else(|| "100".to_string());

    let options = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .tail(&tail_str)
        .build();

    let mut logs_stream = docker.logs(container_id, Some(options));
    let mut logs = String::new();

    while let Some(log_result) = logs_stream.next().await {
        match log_result {
            Ok(log_output) => {
                logs.push_str(&log_output.to_string());
            }
            Err(e) => {
                warn!("Error reading log: {}", e);
                break;
            }
        }
    }

    debug!(
        "Retrieved {} bytes of logs for container: {}",
        logs.len(),
        container_id
    );
    Ok(logs)
}

/// List all containers (running and stopped)
pub async fn list_containers() -> Result<Vec<ContainerSummary>, DockerActionError> {
    let docker = get_docker_client().await?;

    let options = ListContainersOptionsBuilder::new().all(true).build();

    let containers = docker.list_containers(Some(options)).await?;
    Ok(containers)
}

/// Get container details
pub async fn inspect_container(
    container_id: &str,
) -> Result<ContainerInspectResponse, DockerActionError> {
    let docker = get_docker_client().await?;

    let container = docker
        .inspect_container(container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    Ok(container)
}
