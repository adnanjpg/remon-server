use bollard::models::{ContainerInspectResponse, ContainerSummary, ImageSummary};
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, ListImagesOptions, LogsOptionsBuilder,
    PruneContainersOptions, PruneImagesOptions, RemoveContainerOptions, RemoveImageOptions,
    RestartContainerOptionsBuilder, StartContainerOptions, StatsOptionsBuilder,
    StopContainerOptionsBuilder,
};
use bollard::Docker;
use chrono::Utc;
use futures_util::StreamExt;
use log::{debug, info, warn};
use thiserror::Error;

use crate::config::Config;
use crate::monitor::models::docker::{
    ContainerInfo, ContainerInspectDetails, ContainerNetworkSettings, ContainerRealtimeStats,
    ContainerState, GetContainerLogsRequest, HealthDetails, HealthLogEntry, ImageInfo, PortBinding,
    PruneResult, RestartPolicy, VolumeMount,
};

#[derive(Debug, Error)]
pub enum DockerActionError {
    #[error("Docker is not available: {0}")]
    NotAvailable(String),
    #[error("Container not found: {0}")]
    ContainerNotFound(String),
    #[error("Image not found: {0}")]
    ImageNotFound(String),
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

/// Docker version info with backend detection
pub struct DockerVersionInfo {
    pub version: String,
    pub api_version: String,
    pub os: String,
    pub arch: String,
    pub backend: String,
}

/// Get Docker version info
pub async fn get_docker_version() -> Result<DockerVersionInfo, DockerActionError> {
    let docker = get_docker_client().await?;
    let version = docker.version().await?;

    let version_str = version.version.unwrap_or_else(|| "unknown".to_string());
    let api_version = version.api_version.unwrap_or_else(|| "unknown".to_string());
    let os = version.os.unwrap_or_else(|| "unknown".to_string());
    let arch = version.arch.unwrap_or_else(|| "unknown".to_string());

    // Detect backend: Podman usually has "podman" in api_version or os_type
    let backend = if api_version.to_lowercase().contains("podman")
        || version
            .components
            .as_ref()
            .map(|c| {
                c.iter()
                    .any(|comp| comp.name.to_lowercase().contains("podman"))
            })
            .unwrap_or(false)
    {
        "podman".to_string()
    } else {
        "docker".to_string()
    };

    Ok(DockerVersionInfo {
        version: version_str,
        api_version,
        os,
        arch,
        backend,
    })
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

/// Pause a container
pub async fn pause_container(container_id: &str) -> Result<(), DockerActionError> {
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

    docker.pause_container(container_id).await?;

    info!("Paused container: {}", container_id);
    Ok(())
}

/// Unpause a container
pub async fn unpause_container(container_id: &str) -> Result<(), DockerActionError> {
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

    docker.unpause_container(container_id).await?;

    info!("Unpaused container: {}", container_id);
    Ok(())
}

/// Delete a container
pub async fn delete_container(container_id: &str, force: bool) -> Result<(), DockerActionError> {
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

    let options = RemoveContainerOptions {
        force,
        v: true, // Remove volumes
        ..Default::default()
    };

    docker.remove_container(container_id, Some(options)).await?;

    info!("Deleted container: {} (force: {})", container_id, force);
    Ok(())
}

/// Prune stopped containers
pub async fn prune_containers() -> Result<PruneResult, DockerActionError> {
    let docker = get_docker_client().await?;

    let result = docker
        .prune_containers(Some(PruneContainersOptions::default()))
        .await?;

    let deleted_items = result.containers_deleted.unwrap_or_default();
    let deleted_count = deleted_items.len();
    let space_reclaimed = result.space_reclaimed.unwrap_or(0) as i64;

    info!(
        "Pruned {} containers, reclaimed {} bytes",
        deleted_count, space_reclaimed
    );

    Ok(PruneResult {
        deleted_count,
        deleted_items,
        space_reclaimed,
    })
}

/// Get container logs with optional time filtering
pub async fn get_container_logs(
    container_id: &str,
    params: &GetContainerLogsRequest,
) -> Result<String, DockerActionError> {
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

    let tail_str = params
        .tail
        .map(|t| t.to_string())
        .unwrap_or_else(|| "100".to_string());

    let mut builder = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail(&tail_str);

    // Convert ms epoch to seconds for bollard
    if let Some(start) = params.start_time {
        builder = builder.since((start / 1000) as i32);
    }
    if let Some(end) = params.end_time {
        builder = builder.until((end / 1000) as i32);
    }

    let options = builder.build();
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

/// Stream container logs (for SSE)
/// Returns a stream of log lines
pub async fn stream_container_logs(
    container_id: String,
    tail: Option<usize>,
) -> Result<
    impl futures_util::Stream<Item = Result<String, DockerActionError>>,
    DockerActionError,
> {
    let docker = get_docker_client().await?;

    docker
        .inspect_container(&container_id, Some(InspectContainerOptions::default()))
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ContainerNotFound(container_id.clone()),
            _ => DockerActionError::ApiError(e),
        })?;

    let tail_str = tail
        .map(|t| t.to_string())
        .unwrap_or_else(|| "50".to_string());

    let options = LogsOptionsBuilder::new()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail(&tail_str)
        .build();

    let logs_stream = docker.logs(&container_id, Some(options));

    let mapped_stream = logs_stream.map(move |result| match result {
        Ok(log_output) => Ok(log_output.to_string()),
        Err(e) => {
            warn!("Error reading log stream: {}", e);
            Err(DockerActionError::ApiError(e))
        }
    });

    Ok(mapped_stream)
}

/// Get real-time stats for a specific container
pub async fn get_realtime_stats(
    container_id: &str,
) -> Result<ContainerRealtimeStats, DockerActionError> {
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

    let options = StatsOptionsBuilder::new()
        .stream(false)
        .one_shot(true)
        .build();
    let mut stats_stream = docker.stats(container_id, Some(options));

    if let Some(stats_result) = stats_stream.next().await {
        let stats = stats_result?;

        // Calculate CPU percentage
        let cpu_percent = calculate_cpu_percent(&stats);

        // Memory stats
        let memory_usage = stats
            .memory_stats
            .as_ref()
            .and_then(|m| m.usage)
            .unwrap_or(0) as i64;
        let memory_limit = stats
            .memory_stats
            .as_ref()
            .and_then(|m| m.limit)
            .unwrap_or(0) as i64;
        let memory_percent = if memory_limit > 0 {
            (memory_usage as f64 / memory_limit as f64) * 100.0
        } else {
            0.0
        };

        // Network stats
        let (network_rx, network_tx) = stats
            .networks
            .as_ref()
            .map(|networks| {
                networks.values().fold((0i64, 0i64), |(rx, tx), net| {
                    let rx_bytes = net.rx_bytes.unwrap_or(0) as i64;
                    let tx_bytes = net.tx_bytes.unwrap_or(0) as i64;
                    (rx + rx_bytes, tx + tx_bytes)
                })
            })
            .unwrap_or((0, 0));

        // Block I/O stats
        let (block_read, block_write) = stats
            .blkio_stats
            .as_ref()
            .and_then(|blkio| blkio.io_service_bytes_recursive.as_ref())
            .map(|io_stats| {
                io_stats.iter().fold((0i64, 0i64), |(read, write), entry| {
                    match entry.op.as_deref() {
                        Some("read") | Some("Read") => {
                            (read + entry.value.unwrap_or(0) as i64, write)
                        }
                        Some("write") | Some("Write") => {
                            (read, write + entry.value.unwrap_or(0) as i64)
                        }
                        _ => (read, write),
                    }
                })
            })
            .unwrap_or((0, 0));

        // PIDs count
        let pids = stats
            .pids_stats
            .as_ref()
            .and_then(|p| p.current)
            .unwrap_or(0) as i64;

        Ok(ContainerRealtimeStats {
            container_id: container_id.to_string(),
            cpu_percent,
            memory_usage,
            memory_limit,
            memory_percent,
            network_rx_bytes: network_rx,
            network_tx_bytes: network_tx,
            block_read_bytes: block_read,
            block_write_bytes: block_write,
            pids,
            timestamp: Utc::now().timestamp_millis(),
        })
    } else {
        Err(DockerActionError::ApiError(
            bollard::errors::Error::IOError {
                err: std::io::Error::new(std::io::ErrorKind::Other, "No stats received"),
            },
        ))
    }
}

fn calculate_cpu_percent(stats: &bollard::models::ContainerStatsResponse) -> f64 {
    let cpu_stats = match &stats.cpu_stats {
        Some(s) => s,
        None => return 0.0,
    };
    let precpu_stats = match &stats.precpu_stats {
        Some(s) => s,
        None => return 0.0,
    };

    let cpu_usage = cpu_stats.cpu_usage.as_ref();
    let precpu_usage = precpu_stats.cpu_usage.as_ref();

    let cpu_delta = cpu_usage.and_then(|u| u.total_usage).unwrap_or(0) as f64
        - precpu_usage.and_then(|u| u.total_usage).unwrap_or(0) as f64;

    let system_delta = cpu_stats.system_cpu_usage.unwrap_or(0) as f64
        - precpu_stats.system_cpu_usage.unwrap_or(0) as f64;

    if system_delta > 0.0 && cpu_delta > 0.0 {
        let num_cpus = cpu_stats.online_cpus.unwrap_or(1) as f64;
        (cpu_delta / system_delta) * num_cpus * 100.0
    } else {
        0.0
    }
}

/// List all containers (running and stopped)
pub async fn list_containers() -> Result<Vec<ContainerSummary>, DockerActionError> {
    let docker = get_docker_client().await?;

    let options = ListContainersOptionsBuilder::new().all(true).build();

    let containers = docker.list_containers(Some(options)).await?;
    Ok(containers)
}

/// Get container details (raw bollard response)
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

/// Get full container inspect details with parsed information
pub async fn get_container_inspect_details(
    container_id: &str,
) -> Result<ContainerInspectDetails, DockerActionError> {
    let response = inspect_container(container_id).await?;

    // Extract basic info
    let name = response
        .name
        .as_ref()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let image = response
        .config
        .as_ref()
        .and_then(|c| c.image.as_ref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let created_at = response
        .created
        .as_ref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    let info = ContainerInfo {
        id: -1,
        container_id: container_id.to_string(),
        name,
        image,
        created_at,
        last_seen: chrono::Utc::now().timestamp_millis(),
    };

    // Extract state
    let state = extract_container_state(&response);

    // Extract ports
    let ports = extract_port_bindings(&response);

    // Extract volumes
    let volumes = extract_volume_mounts(&response);

    // Extract env
    let env = response
        .config
        .as_ref()
        .and_then(|c| c.env.as_ref())
        .map(|e| e.clone())
        .unwrap_or_default();

    // Extract cmd
    let cmd = response
        .config
        .as_ref()
        .and_then(|c| c.cmd.as_ref())
        .map(|c| c.clone())
        .unwrap_or_default();

    // Extract entrypoint
    let entrypoint = response
        .config
        .as_ref()
        .and_then(|c| c.entrypoint.as_ref())
        .map(|e| e.clone())
        .unwrap_or_default();

    // Extract networks
    let networks = extract_networks(&response);

    // Extract restart policy
    let restart_policy = response
        .host_config
        .as_ref()
        .and_then(|hc| hc.restart_policy.as_ref())
        .map(|rp| RestartPolicy {
            name: rp
                .name
                .as_ref()
                .map(|n| format!("{:?}", n).to_lowercase())
                .unwrap_or_else(|| "no".to_string()),
            maximum_retry_count: rp.maximum_retry_count.unwrap_or(0),
        });

    // Extract health
    let health = extract_health_details(&response);

    // Extract labels
    let labels = response
        .config
        .as_ref()
        .and_then(|c| c.labels.as_ref())
        .map(|l| l.clone())
        .unwrap_or_default();

    Ok(ContainerInspectDetails {
        info,
        state,
        ports,
        volumes,
        env,
        cmd,
        entrypoint,
        networks,
        restart_policy,
        health,
        labels,
    })
}

fn extract_container_state(response: &ContainerInspectResponse) -> ContainerState {
    let state = response.state.as_ref();

    let status = state
        .and_then(|s| s.status.as_ref())
        .map(|s| format!("{:?}", s).to_lowercase())
        .unwrap_or_else(|| "unknown".to_string());

    let running = state.and_then(|s| s.running).unwrap_or(false);
    let paused = state.and_then(|s| s.paused).unwrap_or(false);
    let restarting = state.and_then(|s| s.restarting).unwrap_or(false);

    let started_at = state
        .and_then(|s| s.started_at.as_ref())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis());

    let finished_at = state
        .and_then(|s| s.finished_at.as_ref())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis());

    let exit_code = state.and_then(|s| s.exit_code);

    ContainerState {
        status,
        running,
        paused,
        restarting,
        started_at,
        finished_at,
        exit_code,
    }
}

fn extract_port_bindings(response: &ContainerInspectResponse) -> Vec<PortBinding> {
    let mut ports = Vec::new();

    if let Some(host_config) = &response.host_config {
        if let Some(port_bindings) = &host_config.port_bindings {
            for (container_port_str, host_bindings) in port_bindings {
                // Parse container port (e.g., "80/tcp")
                let parts: Vec<&str> = container_port_str.split('/').collect();
                let container_port: u16 = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
                let protocol = parts.get(1).unwrap_or(&"tcp").to_string();

                if let Some(bindings) = host_bindings {
                    for binding in bindings {
                        let host_port = binding.host_port.as_ref().and_then(|p| p.parse().ok());
                        let host_ip = binding.host_ip.clone();

                        ports.push(PortBinding {
                            container_port,
                            host_port,
                            host_ip,
                            protocol: protocol.clone(),
                        });
                    }
                } else {
                    ports.push(PortBinding {
                        container_port,
                        host_port: None,
                        host_ip: None,
                        protocol,
                    });
                }
            }
        }
    }

    ports
}

fn extract_volume_mounts(response: &ContainerInspectResponse) -> Vec<VolumeMount> {
    response
        .mounts
        .as_ref()
        .map(|mounts| {
            mounts
                .iter()
                .map(|m| VolumeMount {
                    source: m.source.clone().unwrap_or_default(),
                    destination: m.destination.clone().unwrap_or_default(),
                    mode: m.mode.clone().unwrap_or_default(),
                    rw: m.rw.unwrap_or(false),
                    mount_type: m
                        .typ
                        .as_ref()
                        .map(|t| format!("{:?}", t).to_lowercase())
                        .unwrap_or_else(|| "bind".to_string()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_networks(response: &ContainerInspectResponse) -> Vec<ContainerNetworkSettings> {
    response
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref())
        .map(|networks| {
            networks
                .iter()
                .map(|(name, settings)| ContainerNetworkSettings {
                    network_name: name.clone(),
                    network_id: settings.network_id.clone().unwrap_or_default(),
                    ip_address: settings.ip_address.clone(),
                    gateway: settings.gateway.clone(),
                    mac_address: settings.mac_address.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_health_details(response: &ContainerInspectResponse) -> Option<HealthDetails> {
    response
        .state
        .as_ref()
        .and_then(|s| s.health.as_ref())
        .map(|h| {
            let log = h
                .log
                .as_ref()
                .map(|logs| {
                    logs.iter()
                        .map(|entry| HealthLogEntry {
                            start: entry.start.clone().unwrap_or_default(),
                            end: entry.end.clone().unwrap_or_default(),
                            exit_code: entry.exit_code.unwrap_or(0),
                            output: entry.output.clone().unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default();

            HealthDetails {
                status: h
                    .status
                    .as_ref()
                    .map(|s| format!("{:?}", s).to_lowercase())
                    .unwrap_or_else(|| "none".to_string()),
                failing_streak: h.failing_streak.unwrap_or(0),
                log,
            }
        })
}

// ============ Image operations ============

/// List all images
pub async fn list_images() -> Result<Vec<ImageSummary>, DockerActionError> {
    let docker = get_docker_client().await?;

    let options = ListImagesOptions {
        all: true,
        ..Default::default()
    };

    let images = docker.list_images(Some(options)).await?;
    Ok(images)
}

/// Get image info in our format
pub async fn get_images() -> Result<Vec<ImageInfo>, DockerActionError> {
    let images = list_images().await?;

    Ok(images
        .into_iter()
        .map(|img| ImageInfo {
            id: img.id.clone(),
            repo_tags: img.repo_tags.clone(),
            repo_digests: img.repo_digests.clone(),
            created: img.created,
            size: img.size,
            labels: img.labels.clone(),
        })
        .collect())
}

/// Delete an image
pub async fn delete_image(image_id: &str, force: bool) -> Result<(), DockerActionError> {
    let docker = get_docker_client().await?;

    let options = RemoveImageOptions {
        force,
        noprune: false,
        ..Default::default()
    };

    docker
        .remove_image(image_id, Some(options), None)
        .await
        .map_err(|e| match e {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => DockerActionError::ImageNotFound(image_id.to_string()),
            _ => DockerActionError::ApiError(e),
        })?;

    info!("Deleted image: {} (force: {})", image_id, force);
    Ok(())
}

/// Prune unused images
pub async fn prune_images() -> Result<PruneResult, DockerActionError> {
    let docker = get_docker_client().await?;

    let result = docker
        .prune_images(Some(PruneImagesOptions::default()))
        .await?;

    let deleted_items: Vec<String> = result
        .images_deleted
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.untagged.or(item.deleted))
        .collect();
    let deleted_count = deleted_items.len();
    let space_reclaimed = result.space_reclaimed.unwrap_or(0) as i64;

    info!(
        "Pruned {} images, reclaimed {} bytes",
        deleted_count, space_reclaimed
    );

    Ok(PruneResult {
        deleted_count,
        deleted_items,
        space_reclaimed,
    })
}
