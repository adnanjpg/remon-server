use serde::{Deserialize, Serialize};

/// Basic system description
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDescription {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub uptime_secs: u64,
}

/// Hardware information (rarely changes)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub total_memory_bytes: u64,
    pub disks: Vec<DiskInfo>,
    pub network_interfaces: Vec<NetworkInterfaceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub device_name: String,
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub is_removable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub mac_address: Option<String>,
    pub ip_addresses: Vec<String>,
    pub is_virtual: bool,
}

/// Combined system info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub description: SystemDescription,
    pub hardware: HardwareInfo,
}
