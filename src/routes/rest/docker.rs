#![cfg(feature = "docker")]

use axum::{
    extract::{Path, Query},
    Json,
};
use bollard::models::ContainerInspectResponse as BollardInspectResponse;
use log::error;

use crate::error::AppResult;
use crate::routes::dtos::docker::{
    ContainerInfo, ContainerInspectInfo, ContainerStateInfo, DockerActionResponse,
    DockerStatusResponse, ForceDeleteRequest, GetContainerLogsRequest, GetContainerLogsResponse,
    ImageInfo, ListContainersResponse, ListImagesResponse, NetworkSummary, PortMapping,
};
use crate::routes::extractors::Claims;
use crate::services::docker::{self, PruneResult};

// ===== Status =====

/// GET /docker/status — check if Docker is available.
pub async fn get_docker_status(_claims: Claims) -> Json<DockerStatusResponse> {
    let available = docker::is_docker_available().await;

    if !available {
        return Json(DockerStatusResponse {
            available: false,
            version: None,
            backend: None,
            api_version: None,
            os: None,
            arch: None,
        });
    }

    match docker::get_docker_version().await {
        Ok(info) => Json(DockerStatusResponse {
            available: true,
            version: Some(info.version),
            backend: Some(info.backend),
            api_version: Some(info.api_version),
            os: Some(info.os),
            arch: Some(info.arch),
        }),
        Err(_) => Json(DockerStatusResponse {
            available: true,
            version: None,
            backend: None,
            api_version: None,
            os: None,
            arch: None,
        }),
    }
}

// ===== Containers =====

/// GET /docker/containers — list all containers.
pub async fn list_containers(_claims: Claims) -> AppResult<Json<ListContainersResponse>> {
    let containers = docker::list_containers().await?;

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

/// POST /docker/containers/{id}/start
pub async fn start_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::start_container(&container_id).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} started", container_id),
    }))
}

/// POST /docker/containers/{id}/stop
pub async fn stop_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::stop_container(&container_id).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} stopped", container_id),
    }))
}

/// POST /docker/containers/{id}/restart
pub async fn restart_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::restart_container(&container_id).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} restarted", container_id),
    }))
}

/// POST /docker/containers/{id}/pause
pub async fn pause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::pause_container(&container_id).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} paused", container_id),
    }))
}

/// POST /docker/containers/{id}/unpause
pub async fn unpause_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::unpause_container(&container_id).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} unpaused", container_id),
    }))
}

/// DELETE /docker/containers/{id}
pub async fn delete_container(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::delete_container(&container_id, params.force).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Container {} deleted", container_id),
    }))
}

/// GET /docker/containers/{id}/inspect — return a whitelisted, operational
/// view of the container. Sensitive fields (env, mounts, secrets, host config,
/// network IPs and driver options) are intentionally NOT included.
pub async fn inspect_container(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<ContainerInspectInfo>> {
    let raw = docker::get_container_inspect(&container_id).await?;
    Ok(Json(map_inspect(raw)))
}

/// GET /docker/containers/{id}/logs
pub async fn get_logs(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<GetContainerLogsRequest>,
) -> AppResult<Json<GetContainerLogsResponse>> {
    let logs = docker::get_container_logs(&container_id, params.tail, params.since).await?;
    Ok(Json(GetContainerLogsResponse { logs }))
}

/// GET /docker/containers/{id}/stats — real-time stats snapshot.
///
/// NOTE: returns the raw bollard stats payload. Tracked under follow-up
/// task to add an explicit DTO; for now relayed as-is to unblock mobile.
pub async fn get_container_stats(
    _claims: Claims,
    Path(container_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let stats = docker::get_container_stats(&container_id).await?;
    Ok(Json(stats))
}

/// POST /docker/containers/prune
pub async fn prune_containers(_claims: Claims) -> AppResult<Json<PruneResult>> {
    let result = docker::prune_containers().await.map_err(|e| {
        error!("Failed to prune containers: {}", e);
        e
    })?;
    Ok(Json(result))
}

// ===== Images =====

/// GET /docker/images
pub async fn list_images(_claims: Claims) -> AppResult<Json<ListImagesResponse>> {
    let images = docker::list_images().await?;

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

/// DELETE /docker/images/{id}
pub async fn delete_image(
    _claims: Claims,
    Path(image_id): Path<String>,
    Query(params): Query<ForceDeleteRequest>,
) -> AppResult<Json<DockerActionResponse>> {
    docker::delete_image(&image_id, params.force).await?;
    Ok(Json(DockerActionResponse {
        success: true,
        message: format!("Image {} deleted", image_id),
    }))
}

/// POST /docker/images/prune
pub async fn prune_images(_claims: Claims) -> AppResult<Json<PruneResult>> {
    let result = docker::prune_images().await.map_err(|e| {
        error!("Failed to prune images: {}", e);
        e
    })?;
    Ok(Json(result))
}

// ===== Inspect mapper =====

/// Map a raw bollard inspect response onto our public DTO. This is the only
/// place that touches the bollard schema; if bollard adds new fields, they
/// stay private until explicitly opted into here.
fn map_inspect(raw: BollardInspectResponse) -> ContainerInspectInfo {
    let state = raw.state.map(|s| ContainerStateInfo {
        status: s.status.map(|st| st.to_string()),
        running: s.running,
        paused: s.paused,
        restarting: s.restarting,
        started_at: s.started_at,
        finished_at: s.finished_at,
        exit_code: s.exit_code,
        health: s.health.and_then(|h| h.status).map(|h| h.to_string()),
    });

    let (ports, networks) = raw
        .network_settings
        .as_ref()
        .map(|ns| {
            let ports = ns
                .ports
                .as_ref()
                .map(|p| extract_ports(p))
                .unwrap_or_default();
            let networks = ns
                .networks
                .as_ref()
                .map(|n| {
                    n.iter()
                        .map(|(name, ep)| NetworkSummary {
                            name: name.clone(),
                            network_id: ep.network_id.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            (ports, networks)
        })
        .unwrap_or_default();

    ContainerInspectInfo {
        id: raw.id,
        name: raw
            .name
            .map(|n| n.trim_start_matches('/').to_string()),
        image: raw.image,
        created: raw.created,
        state,
        ports,
        networks,
        restart_count: raw.restart_count,
    }
}

/// Convert bollard PortMap (`HashMap<"80/tcp", Vec<PortBinding>>`) into a
/// flat list of (container_port, protocol, host_port) without the host IP.
fn extract_ports(port_map: &bollard::models::PortMap) -> Vec<PortMapping> {
    let mut out = Vec::new();
    for (key, bindings_opt) in port_map {
        let (port_str, protocol) = match key.split_once('/') {
            Some((p, proto)) => (p, proto.to_string()),
            None => (key.as_str(), "tcp".to_string()),
        };
        let container_port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        match bindings_opt {
            Some(bindings) if !bindings.is_empty() => {
                for b in bindings {
                    out.push(PortMapping {
                        container_port,
                        protocol: protocol.clone(),
                        host_port: b.host_port.as_deref().and_then(|s| s.parse().ok()),
                    });
                }
            }
            _ => {
                // Exposed but not bound: still report so client knows the port exists.
                out.push(PortMapping {
                    container_port,
                    protocol: protocol.clone(),
                    host_port: None,
                });
            }
        }
    }
    out
}
