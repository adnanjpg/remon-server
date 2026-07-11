#![cfg(feature = "docker")]

//! Container-stats collector.
//!
//! Each tick: list running containers, pull a one-shot stats snapshot per
//! container, and append a `metrics_docker` row. CPU% is computed from the
//! delta between consecutive ticks — a one-shot snapshot carries no usable
//! `precpu_stats`, so a container's first appearance reads 0% and every tick
//! after is the true inter-tick average. Rollup and retention are automatic
//! once rows land; this task only produces raw samples.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use log::{debug, info, warn};
use serde_json::Value;

use crate::services::docker;
use crate::state::AppState;
use crate::storage::repositories::{DockerMetricsRepository, DockerStatsRow};

/// Sub-second cadence collides with second-resolution metric PKs.
const MIN_INTERVAL_MS: u64 = 1000;

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move { run(state).await });
}

async fn run(state: Arc<AppState>) {
    // Docker being absent is a supported configuration, not an error loop.
    if !docker::is_docker_available().await {
        info!("docker collector: daemon not reachable, container stats off");
        return;
    }
    info!("docker collector started");

    let repo = DockerMetricsRepository::new(state.db.clone());

    // container name -> (cpu total_usage, system_cpu_usage) from the prior
    // tick, for the CPU% delta. Rebuilt each tick from live containers, so it
    // never retains a vanished container.
    let mut prev: HashMap<String, (u64, u64)> = HashMap::new();

    let mut current_ms = interval_ms(&state);
    let mut ticker = new_ticker(current_ms);

    loop {
        ticker.tick().await;

        match collect_once(&mut prev).await {
            Ok(rows) if !rows.is_empty() => {
                let now = Utc::now().timestamp();
                if let Err(e) = repo.insert_tick(now, &rows).await {
                    warn!("docker collector: persist failed: {:?}", e);
                } else {
                    debug!(
                        "docker collector: stored {} container reading(s)",
                        rows.len()
                    );
                }
            }
            Ok(_) => {}
            Err(e) => warn!("docker collector: list failed: {}", e),
        }

        let want_ms = interval_ms(&state);
        if want_ms != current_ms {
            current_ms = want_ms;
            ticker = new_ticker(current_ms);
        }
    }
}

fn interval_ms(state: &AppState) -> u64 {
    state
        .collector_docker_interval_ms
        .load(Ordering::Relaxed)
        .max(MIN_INTERVAL_MS)
}

fn new_ticker(ms: u64) -> tokio::time::Interval {
    let mut t = tokio::time::interval(Duration::from_millis(ms));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    t
}

/// One pass over running containers. `prev` is replaced with this tick's CPU
/// samples so the next tick can delta against it. Per-container failures are
/// logged and skipped — one gone container must not hide the rest.
async fn collect_once(
    prev: &mut HashMap<String, (u64, u64)>,
) -> Result<Vec<DockerStatsRow>, String> {
    let containers = docker::list_containers().await.map_err(|e| e.to_string())?;

    let mut rows = Vec::new();
    let mut next_prev = HashMap::new();

    for c in containers {
        // Stats only make sense for a running container. `state` is a typed
        // enum that Displays as the lowercase status string ("running", …).
        if c.state.as_ref().map(|s| s.to_string()).as_deref() != Some("running") {
            continue;
        }
        let Some(id) = c.id.as_deref() else { continue };
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.chars().take(12).collect());

        let stats = match docker::get_container_stats(id).await {
            Ok(v) => v,
            Err(e) => {
                debug!("docker collector: stats failed for {}: {}", name, e);
                continue;
            }
        };

        let (total, system) = cpu_samples(&stats);
        let cpu_percent = match prev.get(&name) {
            Some(&(pt, ps)) => cpu_percent(&stats, total, system, pt, ps),
            None => 0.0,
        };
        next_prev.insert(name.clone(), (total, system));

        let (mem_used, mem_limit) = memory(&stats);
        let (rx, tx) = network(&stats);
        let (blk_r, blk_w) = blkio(&stats);

        rows.push(DockerStatsRow {
            container_id: name,
            cpu_percent,
            memory_used_bytes: mem_used,
            memory_limit_bytes: mem_limit,
            network_rx_bytes: rx,
            network_tx_bytes: tx,
            block_read_bytes: blk_r,
            block_write_bytes: blk_w,
            pids: stats["pids_stats"]["current"].as_i64().unwrap_or(0),
        });
    }

    *prev = next_prev;
    Ok(rows)
}

fn cpu_samples(v: &Value) -> (u64, u64) {
    let total = v["cpu_stats"]["cpu_usage"]["total_usage"]
        .as_u64()
        .unwrap_or(0);
    let system = v["cpu_stats"]["system_cpu_usage"].as_u64().unwrap_or(0);
    (total, system)
}

/// Standard Docker CPU% from inter-tick deltas, scaled by online CPUs.
fn cpu_percent(v: &Value, total: u64, system: u64, prev_total: u64, prev_system: u64) -> f64 {
    let cpu_delta = total.saturating_sub(prev_total) as f64;
    let sys_delta = system.saturating_sub(prev_system) as f64;
    if sys_delta <= 0.0 || cpu_delta <= 0.0 {
        return 0.0;
    }
    let online = v["cpu_stats"]["online_cpus"]
        .as_u64()
        .or_else(|| {
            v["cpu_stats"]["cpu_usage"]["percpu_usage"]
                .as_array()
                .map(|a| a.len() as u64)
        })
        .unwrap_or(1)
        .max(1);
    (cpu_delta / sys_delta) * online as f64 * 100.0
}

/// Used memory to match what `docker stats` shows: usage minus reclaimable
/// page cache (`inactive_file` on cgroup v2, `cache` on v1).
fn memory(v: &Value) -> (i64, i64) {
    let usage = v["memory_stats"]["usage"].as_i64().unwrap_or(0);
    let cache = v["memory_stats"]["stats"]["inactive_file"]
        .as_i64()
        .or_else(|| v["memory_stats"]["stats"]["cache"].as_i64())
        .unwrap_or(0);
    let limit = v["memory_stats"]["limit"].as_i64().unwrap_or(0);
    ((usage - cache).max(0), limit)
}

fn network(v: &Value) -> (i64, i64) {
    let (mut rx, mut tx) = (0i64, 0i64);
    if let Some(nets) = v["networks"].as_object() {
        for iface in nets.values() {
            rx += iface["rx_bytes"].as_i64().unwrap_or(0);
            tx += iface["tx_bytes"].as_i64().unwrap_or(0);
        }
    }
    (rx, tx)
}

fn blkio(v: &Value) -> (i64, i64) {
    let (mut read, mut write) = (0i64, 0i64);
    if let Some(entries) = v["blkio_stats"]["io_service_bytes_recursive"].as_array() {
        for e in entries {
            let val = e["value"].as_i64().unwrap_or(0);
            match e["op"].as_str().map(str::to_ascii_lowercase).as_deref() {
                Some("read") => read += val,
                Some("write") => write += val,
                _ => {}
            }
        }
    }
    (read, write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cpu_percent_from_deltas() {
        // 2 cpus; container burned 50% of one over the interval.
        let v = json!({
            "cpu_stats": {
                "cpu_usage": { "total_usage": 1_500u64 },
                "system_cpu_usage": 20_000u64,
                "online_cpus": 2
            }
        });
        // prev totals: container 1000, system 19000.
        // cpu_delta=500, sys_delta=1000 → 0.5 * 2 * 100 = 100%.
        assert_eq!(cpu_percent(&v, 1_500, 20_000, 1_000, 19_000), 100.0);
    }

    #[test]
    fn cpu_percent_first_tick_zero() {
        // No system delta available → 0, never a divide-by-zero.
        let v = json!({"cpu_stats": {"online_cpus": 4}});
        assert_eq!(cpu_percent(&v, 500, 1000, 500, 1000), 0.0);
    }

    #[test]
    fn memory_subtracts_cache() {
        let v = json!({
            "memory_stats": {
                "usage": 200_000_000i64,
                "limit": 1_000_000_000i64,
                "stats": { "inactive_file": 50_000_000i64 }
            }
        });
        assert_eq!(memory(&v), (150_000_000, 1_000_000_000));
    }

    #[test]
    fn network_sums_interfaces() {
        let v = json!({
            "networks": {
                "eth0": { "rx_bytes": 100, "tx_bytes": 200 },
                "eth1": { "rx_bytes": 5,   "tx_bytes": 7 }
            }
        });
        assert_eq!(network(&v), (105, 207));
    }

    #[test]
    fn blkio_sums_read_write_case_insensitive() {
        let v = json!({
            "blkio_stats": { "io_service_bytes_recursive": [
                { "op": "Read",  "value": 1000 },
                { "op": "write", "value": 2000 },
                { "op": "Async", "value": 9999 }
            ]}
        });
        assert_eq!(blkio(&v), (1000, 2000));
    }

    #[test]
    fn missing_fields_default_to_zero() {
        let v = json!({});
        assert_eq!(memory(&v), (0, 0));
        assert_eq!(network(&v), (0, 0));
        assert_eq!(blkio(&v), (0, 0));
        assert_eq!(cpu_samples(&v), (0, 0));
    }
}
