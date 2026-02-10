use std::sync::Arc;
use std::time::Duration;
use sysinfo::{System, Disks, Networks};
use log::debug;

use crate::models::stats::StatsEvent;
use crate::services::system;
use crate::state::AppState;

pub async fn run(state: Arc<AppState>) {
    let mut sys = System::new_all();
    let mut disks = Disks::new_with_refreshed_list();
    let mut networks = Networks::new_with_refreshed_list();

    // Default interval: 2 seconds
    let interval_ms = 2000;

    loop {
        // Refresh system info
        sys.refresh_all();
        disks.refresh(true);
        networks.refresh(true);

        // Collect stats
        let cpu_stats = system::get_cpu_stats(&sys);
        let memory_stats = system::get_memory_stats(&sys);
        let disk_stats = system::get_disk_stats(&disks);
        let network_stats = system::get_network_stats(&networks);

        // Broadcast individual events
        let _ = state.stats_tx.send(StatsEvent::Cpu(cpu_stats.clone()));
        let _ = state.stats_tx.send(StatsEvent::Memory(memory_stats.clone()));
        let _ = state.stats_tx.send(StatsEvent::Disk(disk_stats.clone()));
        let _ = state.stats_tx.send(StatsEvent::Network(network_stats.clone()));

        debug!("Stats collected: CPU {:.1}%, Memory {:.1}%",
            cpu_stats.usage_percent,
            (memory_stats.used_bytes as f64 / memory_stats.total_bytes as f64) * 100.0
        );

        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}
