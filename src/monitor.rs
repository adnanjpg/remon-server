use sysinfo::{CpuExt, System, SystemExt};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    name: String,
    description: String,
}

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
    let description = system.long_os_version().unwrap_or("unknown".to_string())
        + " • "
        + cpu
        + " • "
        + &format!("{:.1}GB", &mem);

    ServerDescription { name, description }
}
