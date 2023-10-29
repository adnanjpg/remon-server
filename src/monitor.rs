use sysinfo::{CpuExt, System, SystemExt};

use serde::{de, Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    name: String,
    description: String,
}

// this is going to be improved later
#[derive(Debug, Serialize, Deserialize)]
pub struct MonitorConfig {
    cpu_threshold: f64,
    mem_threshold: f64,
    storage_threshold: f64,
}

const CONFIGURATION_PATH: &str = "./config/configuration.json";

pub fn get_default_server_desc() -> ServerDescription {
    let mut system = System::new_all();
    system.refresh_all();

    let cpu = system.cpus()[0].brand();
    let mem = (system.total_memory() as f64) / 1024.0 / 1024.0 / 1024.0;
    let name = system.name().unwrap_or("Unknown".to_string())
        + " • "
        + match system.global_cpu_info().vendor_id() {
            "GenuineIntel" => "Intel",
            _ => "Unknown",
        };
    let description = system.long_os_version().unwrap_or("Unknown".to_string())
        + " • "
        + cpu
        + " • "
        + &format!("{:.1}GB", &mem);

    ServerDescription { name, description }
}

pub fn get_default_monitor_config() -> MonitorConfig {
    MonitorConfig {
        cpu_threshold: 0.8,
        mem_threshold: 0.8,
        storage_threshold: 0.8,
    }
}

pub fn load_monitor_config() -> MonitorConfig {
    if !std::path::Path::new(CONFIGURATION_PATH).exists() {
        let default_config = get_default_monitor_config();
        save_monitor_config(&default_config);
        return default_config;
    }

    let config_str = std::fs::read_to_string(CONFIGURATION_PATH).unwrap();
    serde_json::from_str(&config_str).unwrap()
}

pub fn save_monitor_config(config: &MonitorConfig) {
    let config_str = serde_json::to_string(config).unwrap();
    std::fs::write(CONFIGURATION_PATH, config_str).unwrap();
}
