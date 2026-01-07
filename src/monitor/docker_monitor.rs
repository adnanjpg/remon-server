use super::{
    docker_actions::{get_docker_client, is_docker_available, list_containers},
    models::docker::{ContainerInfo, ContainerState, ContainerStats, DockerStatsFrame},
    persistence::{insert_docker_stats_frame, upsert_container_info},
};

use bollard::models::{ContainerInspectResponse, ContainerSummary, ContainerSummaryStateEnum};
use bollard::query_parameters::StatsOptionsBuilder;
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use log::{debug, error, info, warn};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time;

use crate::config::Config;

pub struct DockerMonitor {
    should_exit: Arc<Mutex<bool>>,
    check_interval: Duration,
    enabled: bool,
}

impl DockerMonitor {
    pub async fn new() -> Self {
        let config = Config::new().unwrap_or_else(|e| {
            error!("Failed to load config for DockerMonitor: {}", e);
            panic!("Failed to load config");
        });

        let should_exit = Arc::new(Mutex::new(false));
        let check_interval = Duration::from_millis(config.docker.check_interval_ms);
        let enabled = config.docker.enabled;

        Self {
            should_exit,
            check_interval,
            enabled,
        }
    }

    pub async fn start_monitoring(&self) {
        if !self.enabled {
            info!("Docker monitoring is disabled in config");
            return;
        }

        // Check if Docker is available
        if !is_docker_available().await {
            warn!("Docker is not available on this system. Docker monitoring will be skipped.");
            return;
        }

        info!(
            "Starting Docker monitoring with interval: {:?}",
            self.check_interval
        );

        let should_exit_clone = Arc::clone(&self.should_exit);
        let check_interval = self.check_interval;

        tokio::spawn(async move {
            while !*should_exit_clone.lock().unwrap() {
                let start_time = std::time::Instant::now();

                if let Err(e) = collect_docker_stats().await {
                    error!("Failed to collect Docker stats: {}", e);
                }

                let elapsed = start_time.elapsed();
                debug!("Docker stats collection took: {:?}", elapsed);

                let sleep_duration = check_interval.saturating_sub(elapsed);
                time::sleep(sleep_duration).await;
            }
        });
    }

    #[allow(dead_code)]
    pub fn stop_monitoring(&self) {
        *self.should_exit.lock().unwrap() = true;
    }
}

async fn collect_docker_stats() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let docker = get_docker_client().await?;
    let containers = list_containers().await?;

    let last_check = Utc::now().timestamp_millis();
    let mut container_stats: Vec<ContainerStats> = Vec::new();

    for container in &containers {
        let container_id = container.id.as_ref().map(|s| s.as_str()).unwrap_or("");
        if container_id.is_empty() {
            continue;
        }

        // Extract container name (remove leading /)
        let name = container
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Extract image name
        let image = container
            .image
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Extract created timestamp
        let created_at = Utc
            .timestamp_millis_opt(container.created.unwrap_or(0))
            .unwrap()
            .timestamp_micros(); // container.created.unwrap_or(0);
                                 // it returns ms epoch, we use us ?

        // Upsert container info
        let container_info = ContainerInfo {
            id: -1,
            container_id: container_id.to_string(),
            name,
            image,
            created_at,
            last_seen: last_check,
        };

        if let Err(e) = upsert_container_info(&container_info).await {
            error!(
                "Failed to upsert container info for {}: {}",
                container_id, e
            );
        }

        // Get container stats (only for running containers)
        let is_running = container
            .state
            .as_ref()
            .map(|s| matches!(s, ContainerSummaryStateEnum::RUNNING))
            .unwrap_or(false);

        if !is_running {
            // For non-running containers, add zero stats
            container_stats.push(ContainerStats {
                id: -1,
                frame_id: -1,
                container_id: container_id.to_string(),
                cpu_percent: 0.0,
                memory_usage: 0,
                memory_limit: 0,
                network_rx_bytes: 0,
                network_tx_bytes: 0,
                block_read_bytes: 0,
                block_write_bytes: 0,
                pids: 0,
            });
            continue;
        }

        // Get real-time stats for running containers
        match get_container_resource_stats(&docker, container_id).await {
            Ok(stats) => {
                container_stats.push(stats);
            }
            Err(e) => {
                warn!("Failed to get stats for container {}: {}", container_id, e);
                // Add zero stats on error
                container_stats.push(ContainerStats {
                    id: -1,
                    frame_id: -1,
                    container_id: container_id.to_string(),
                    cpu_percent: 0.0,
                    memory_usage: 0,
                    memory_limit: 0,
                    network_rx_bytes: 0,
                    network_tx_bytes: 0,
                    block_read_bytes: 0,
                    block_write_bytes: 0,
                    pids: 0,
                });
            }
        }
    }

    // Save frame to DB
    let frame = DockerStatsFrame {
        id: -1,
        last_check,
        container_stats,
    };

    if let Err(e) = insert_docker_stats_frame(&frame).await {
        error!("Failed to insert Docker stats frame: {}", e);
    }

    debug!("Collected stats for {} containers", containers.len());
    Ok(())
}

async fn get_container_resource_stats(
    docker: &bollard::Docker,
    container_id: &str,
) -> Result<ContainerStats, Box<dyn std::error::Error + Send + Sync>> {
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

        // Network stats (aggregate all interfaces)
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

        Ok(ContainerStats {
            id: -1,
            frame_id: -1,
            container_id: container_id.to_string(),
            cpu_percent,
            memory_usage,
            memory_limit,
            network_rx_bytes: network_rx,
            network_tx_bytes: network_tx,
            block_read_bytes: block_read,
            block_write_bytes: block_write,
            pids,
        })
    } else {
        Err("No stats received".into())
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

/// Get container state from bollard container summary
pub fn get_container_state_from_summary(container: &ContainerSummary) -> ContainerState {
    let status = container
        .state
        .as_ref()
        .map(|s| format!("{:?}", s).to_lowercase())
        .unwrap_or_else(|| "unknown".to_string());

    let is_running = container
        .state
        .as_ref()
        .map(|s| matches!(s, ContainerSummaryStateEnum::RUNNING))
        .unwrap_or(false);
    let is_paused = container
        .state
        .as_ref()
        .map(|s| matches!(s, ContainerSummaryStateEnum::PAUSED))
        .unwrap_or(false);
    let is_restarting = container
        .state
        .as_ref()
        .map(|s| matches!(s, ContainerSummaryStateEnum::RESTARTING))
        .unwrap_or(false);

    ContainerState {
        status,
        running: is_running,
        paused: is_paused,
        restarting: is_restarting,
        started_at: None,
        finished_at: None,
        exit_code: None,
    }
}

/// Get container state from bollard inspect response
pub fn get_container_state_from_inspect(response: &ContainerInspectResponse) -> ContainerState {
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
