use sysinfo::{Components, Disks, Networks, System};

use crate::models::stats::{
    AllStats, ComponentInfo, ComponentsSnapshot, CoreStats, CpuStats, DiskStats, LoadAverage,
    MemoryStats, NetworkStats,
};
use crate::models::system::{DiskInfo, HardwareInfo, NetworkInterfaceInfo, SystemDescription};

/// Collect system description.
pub fn get_description() -> SystemDescription {
    SystemDescription {
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        os: System::name().unwrap_or_else(|| "unknown".into()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".into()),
        kernel: System::kernel_version().unwrap_or_else(|| "unknown".into()),
        uptime_secs: System::uptime()
    }
}

/// Collect hardware information.
pub fn get_hardware_info() -> HardwareInfo {
    let mut sys = System::new();
    sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
    sys.refresh_memory();
    let disks = Disks::new_with_refreshed_list();
    let networks = Networks::new_with_refreshed_list();

    // CPU info
    let cpu_model = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "unknown".into());

    let cpu_cores = System::physical_core_count().unwrap_or(0) as u32;
    let cpu_threads = sys.cpus().len() as u32;

    // Memory
    let total_memory_bytes = sys.total_memory();

    let disk_infos: Vec<DiskInfo> = disks
        .iter()
        .filter(|d| !is_hidden_mount(&d.mount_point().to_string_lossy()))
        .map(|d| DiskInfo {
            device_name: d.name().to_string_lossy().to_string(),
            mount_point: d.mount_point().to_string_lossy().to_string(),
            fs_type: d.file_system().to_string_lossy().to_string(),
            total_bytes: d.total_space(),
            is_removable: d.is_removable(),
        })
        .collect();

    let net_infos: Vec<NetworkInterfaceInfo> = networks
        .iter()
        .filter(|(name, _)| !is_loopback(name) && !is_virtual_interface(name))
        .map(|(name, data)| NetworkInterfaceInfo {
            name: name.clone(),
            mac_address: Some(data.mac_address().to_string()),
            ip_addresses: vec![], // sysinfo doesn't provide IPs directly
            is_virtual: name.starts_with("veth")
                || name.starts_with("docker")
                || name.starts_with("br-"),
        })
        .collect();

    HardwareInfo {
        cpu_model,
        cpu_cores,
        cpu_threads,
        total_memory_bytes,
        disks: disk_infos,
        network_interfaces: net_infos,
    }
}

pub fn get_cpu_stats(sys: &System, timestamp: i64) -> CpuStats {
    let load_avg = System::load_average();

    let per_core: Vec<CoreStats> = sys
        .cpus()
        .iter()
        .enumerate()
        .map(|(i, cpu)| CoreStats {
            core_index: i as u32,
            usage_percent: cpu.cpu_usage() as f64,
            freq_mhz: cpu.frequency(),
        })
        .collect();

    let total_usage = if per_core.is_empty() {
        0.0
    } else {
        per_core.iter().map(|c| c.usage_percent).sum::<f64>() / per_core.len() as f64
    };

    CpuStats {
        usage_percent: total_usage,
        per_core,
        load_avg: LoadAverage {
            one: load_avg.one,
            five: load_avg.five,
            fifteen: load_avg.fifteen,
        },
        timestamp,
        steal_percent: None,
        iowait_percent: None,
        guest_percent: None,
        context_switches_per_sec: None,
        process_forks_per_sec: None,
    }
}

/// Collect memory stats.
///
/// `cached_bytes` semantics:
/// - On Linux we read `/proc/meminfo` and sum `Cached + Buffers + SReclaimable`,
///   matching what `free`/`htop` call "cached" / "buff/cache".
/// - On other platforms sysinfo doesn't expose a cached counter and we
///   return 0 rather than guessing with arithmetic that would be wrong
///   on a NUMA system.
pub fn get_memory_stats(sys: &System, timestamp: i64) -> MemoryStats {
    MemoryStats {
        total_bytes: sys.total_memory(),
        used_bytes: sys.used_memory(),
        available_bytes: sys.available_memory(),
        cached_bytes: read_cached_bytes(),
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
        timestamp,
        // Linux-only extras filled in by `enrich_linux`.
        page_faults_minor_per_sec: None,
        page_faults_major_per_sec: None,
        swap_in_pages_per_sec: None,
        swap_out_pages_per_sec: None,
    }
}

#[cfg(target_os = "linux")]
fn read_cached_bytes() -> u64 {
    let content = match std::fs::read_to_string("/proc/meminfo") {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let mut cached_kib = 0u64;
    let mut buffers_kib = 0u64;
    let mut sreclaim_kib = 0u64;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match key {
            "Cached:" => cached_kib = value,
            "Buffers:" => buffers_kib = value,
            "SReclaimable:" => sreclaim_kib = value,
            _ => {}
        }
    }
    (cached_kib + buffers_kib + sreclaim_kib).saturating_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn read_cached_bytes() -> u64 {
    0
}

/// Collect disk stats.
///
/// `read_bytes_per_sec` and `write_bytes_per_sec` are derived from
/// `Disk::usage()`, which on sysinfo 0.38 returns "bytes since last refresh".
/// We divide that by `interval_secs` to land on a real per-second rate.
/// First-tick `interval_secs` is near-zero, so we floor at 0 to avoid
/// nonsensical infinities. Container-overlay mounts are filtered out.
pub fn get_disk_stats(disks: &Disks, interval_secs: f64, timestamp: i64) -> Vec<DiskStats> {
    let safe_div = if interval_secs > 0.0 {
        interval_secs
    } else {
        1.0
    };

    disks
        .iter()
        .filter(|d| !is_hidden_mount(&d.mount_point().to_string_lossy()))
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let used = total.saturating_sub(available);
            let usage = d.usage();

            let read_per_sec = if interval_secs > 0.0 {
                (usage.read_bytes as f64 / safe_div) as u64
            } else {
                0
            };
            let write_per_sec = if interval_secs > 0.0 {
                (usage.written_bytes as f64 / safe_div) as u64
            } else {
                0
            };

            DiskStats {
                mount_point: d.mount_point().to_string_lossy().to_string(),
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
                read_bytes_per_sec: read_per_sec,
                write_bytes_per_sec: write_per_sec,
                timestamp,
                inode_used_percent: None, // patched by the collector on Linux
            }
        })
        .collect()
}

/// Collect network stats.
///
/// sysinfo's `received()` / `transmitted()` / `packets_*()` return "delta
/// since last refresh" (NOT a rate, NOT cumulative — the docs are easy to
/// misread). We divide by `interval_secs` to convert into a true per-second
/// rate. Loopback and virtual/bridge interfaces are filtered out — see
/// `is_loopback` and `is_virtual_interface`.
pub fn get_network_stats(
    networks: &Networks,
    interval_secs: f64,
    timestamp: i64,
) -> Vec<NetworkStats> {
    let safe_div = if interval_secs > 0.0 {
        interval_secs
    } else {
        1.0
    };

    networks
        .iter()
        .filter(|(name, _)| !is_loopback(name) && !is_virtual_interface(name))
        .map(|(name, data)| {
            let (rxb, txb, rxp, txp) = if interval_secs > 0.0 {
                (
                    (data.received() as f64 / safe_div) as u64,
                    (data.transmitted() as f64 / safe_div) as u64,
                    (data.packets_received() as f64 / safe_div) as u64,
                    (data.packets_transmitted() as f64 / safe_div) as u64,
                )
            } else {
                (0, 0, 0, 0)
            };
            // sysinfo's errors_on_received() / errors_on_transmitted() are
            // also "since last refresh"; same /interval normalization as
            // the byte counters.
            let (errs_in, errs_out) = if interval_secs > 0.0 {
                (
                    (data.errors_on_received() as f64 / safe_div) as u64,
                    (data.errors_on_transmitted() as f64 / safe_div) as u64,
                )
            } else {
                (0, 0)
            };
            NetworkStats {
                interface: name.clone(),
                rx_bytes_per_sec: rxb,
                tx_bytes_per_sec: txb,
                rx_packets_per_sec: rxp,
                tx_packets_per_sec: txp,
                errors_in_per_sec: errs_in,
                errors_out_per_sec: errs_out,
                rx_bytes_total: data.total_received(),
                tx_bytes_total: data.total_transmitted(),
                timestamp,
            }
        })
        .collect()
}

/// Snapshot the available hardware sensors. The caller is expected to keep
/// a `Components` handle around and refresh it once per tick — repeatedly
/// constructing one is more expensive than reading temperatures.
pub fn get_components(components: &Components, timestamp: i64) -> ComponentsSnapshot {
    let list: Vec<ComponentInfo> = components
        .iter()
        .map(|c| ComponentInfo {
            label: c.label().to_string(),
            temperature_c: c.temperature().map(|v| v as f64),
            max_c: c.max().map(|v| v as f64),
            critical_c: c.critical().map(|v| v as f64),
        })
        .collect();
    ComponentsSnapshot {
        components: list,
        timestamp,
    }
}

fn is_loopback(name: &str) -> bool {
    if name == "lo" {
        return true;
    }
    if let Some(rest) = name.strip_prefix("lo") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    name.to_ascii_lowercase()
        .starts_with("loopback pseudo-interface")
}

const HIDDEN_MOUNT_PREFIXES: &[&str] = &["/var/lib/docker/", "/var/lib/containers/"];

fn is_hidden_mount(mount: &str) -> bool {
    HIDDEN_MOUNT_PREFIXES.iter().any(|p| mount.starts_with(p))
}

/// Drops container/hypervisor bridges, packet-capture shadow adapters,
/// and platform pseudo-tunnels. VPN tunnels (`tun*`, `wg*`, `utun*`,
/// `tailscale*`) are deliberately kept.
fn is_virtual_interface(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    const CONTAINER_PREFIXES: &[&str] = &[
        "veth", "docker", "br-", "cni", "cilium", "flannel", "calico", "kube",
    ];
    if CONTAINER_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }

    const HV_PREFIXES: &[&str] = &["vethernet", "vmnet", "vboxnet", "virbr", "tap"];
    if HV_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    if lower.contains("vmware virtual") || lower.contains("virtualbox host-only") {
        return true;
    }

    if lower.contains("npcap") {
        return true;
    }

    if lower.contains("wan miniport")
        || lower.contains("teredo tunneling")
        || lower.contains("isatap")
        || lower.contains("wfp lightweight")
    {
        return true;
    }

    // macOS aux: prefix + pure digits.
    for prefix in ["awdl", "llw", "gif", "stf"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    if let Some(rest) = lower.strip_prefix("bridge") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }

    false
}

/// Collect all stats at once. Currently unused — collector code samples a
/// single tick timestamp and calls the per-resource fns directly so the
/// Linux enrichment block can patch them in-place.
#[allow(dead_code)]
pub fn get_all_stats(
    sys: &System,
    disks: &Disks,
    networks: &Networks,
    interval_secs: f64,
) -> AllStats {
    let timestamp = chrono::Utc::now().timestamp();
    AllStats {
        cpu: get_cpu_stats(sys, timestamp),
        memory: get_memory_stats(sys, timestamp),
        pressure: None,
        components: None,
        disks: get_disk_stats(disks, interval_secs, timestamp),
        network: get_network_stats(networks, interval_secs, timestamp),
    }
}
