use std::sync::Arc;
use std::time::Duration;
use sysinfo::System;
use log::debug;

use crate::services::process;
use crate::state::AppState;

pub async fn run(state: Arc<AppState>) {
    let mut sys = System::new_all();

    // Default interval: 5 seconds
    let interval_ms = 5000;

    loop {
        // Refresh process list
        sys.refresh_all();

        // Collect processes
        let process_list = process::get_processes(&sys);

        debug!("Processes collected: {} total", process_list.total_count);

        // Broadcast to subscribers
        let _ = state.processes_tx.send(process_list);

        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}
