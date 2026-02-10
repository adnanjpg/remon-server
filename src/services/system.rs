use sysinfo::{System, Disks, Networks};

use crate::models::system::{
    DiskInfo, HardwareInfo, NetworkInterfaceInfo, SystemDescription, SystemInfo,
};
use crate::models::stats::{
    AllStats, CoreStats, CpuStats, DiskStats, LoadAverage, MemoryStats, NetworkStats,
};

/// Collect system description
pub fn get_description() -> SystemDescription {
    let _sys = System::new_all();

    SystemDescription {
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        os: System::name().unwrap_or_else(|| "unknown".into()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".into()),
        kernel: System::kernel_version().unwrap_or_else(|| "unknown".into()),
        uptime_secs: System::uptime(),
    }
}

/// Collect hardware information
pub fn get_hardware_info() -> HardwareInfo {
    let sys = System::new_all();
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

    // Disks
    let disk_infos: Vec<DiskInfo> = disks
        .iter()
        .map(|d| DiskInfo {
            device_name: d.name().to_string_lossy().to_string(),
            mount_point: d.mount_point().to_string_lossy().to_string(),
            fs_type: d.file_system().to_string_lossy().to_string(),
            total_bytes: d.total_space(),
            is_removable: d.is_removable(),
        })
        .collect();

    // Network interfaces
    let net_infos: Vec<NetworkInterfaceInfo> = networks
        .iter()
        .map(|(name, data)| NetworkInterfaceInfo {
            name: name.clone(),
            mac_address: Some(data.mac_address().to_string()),
            ip_addresses: vec![], // sysinfo doesn't provide IPs directly
            is_virtual: name.starts_with("veth") || name.starts_with("docker") || name.starts_with("br-"),
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

/// Collect combined system info
pub fn get_system_info() -> SystemInfo {
    SystemInfo {
        description: get_description(),
        hardware: get_hardware_info(),
    }
}

/// Collect CPU stats
pub fn get_cpu_stats(sys: &System) -> CpuStats {
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
        timestamp: chrono::Utc::now().timestamp(),
    }
}

/// Collect memory stats
pub fn get_memory_stats(sys: &System) -> MemoryStats {
    MemoryStats {
        total_bytes: sys.total_memory(),
        used_bytes: sys.used_memory(),
        available_bytes: sys.available_memory(),
        cached_bytes: sys.total_memory().saturating_sub(sys.used_memory()).saturating_sub(sys.available_memory()),
        swap_total_bytes: sys.total_swap(),
        swap_used_bytes: sys.used_swap(),
        timestamp: chrono::Utc::now().timestamp(),
    }
}

/// Collect disk stats
pub fn get_disk_stats(disks: &Disks) -> Vec<DiskStats> {
    let timestamp = chrono::Utc::now().timestamp();

    disks
        .iter()
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let used = total.saturating_sub(available);

            DiskStats {
                mount_point: d.mount_point().to_string_lossy().to_string(),
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
                read_bytes_per_sec: 0,  // Would need tracking over time
                write_bytes_per_sec: 0,
                timestamp,
            }
        })
        .collect()
}

/// Collect network stats
pub fn get_network_stats(networks: &Networks) -> Vec<NetworkStats> {
    let timestamp = chrono::Utc::now().timestamp();

    networks
        .iter()
        .filter(|(name, _)| !name.starts_with("lo")) // Filter loopback
        .map(|(name, data)| NetworkStats {
            interface: name.clone(),
            rx_bytes_per_sec: data.received(), // Cumulative, would need delta calc
            tx_bytes_per_sec: data.transmitted(),
            rx_packets_per_sec: data.packets_received(),
            tx_packets_per_sec: data.packets_transmitted(),
            timestamp,
        })
        .collect()
}

/// Collect all stats at once
pub fn get_all_stats(sys: &System, disks: &Disks, networks: &Networks) -> AllStats {
    AllStats {
        cpu: get_cpu_stats(sys),
        memory: get_memory_stats(sys),
        disks: get_disk_stats(disks),
        network: get_network_stats(networks),
    }
}
