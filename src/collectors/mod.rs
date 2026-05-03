mod stats;
mod processes;
#[cfg(feature = "docker")]
mod docker;

use std::sync::Arc;
use log::info;

use crate::state::AppState;

pub fn spawn_all(state: Arc<AppState>) {
    info!("Starting collectors...");

    // System stats collector
    tokio::spawn(stats::run(Arc::clone(&state)));

    // Process list collector
    tokio::spawn(processes::run(Arc::clone(&state)));

    // Docker stats collector
    #[cfg(feature = "docker")]
    tokio::spawn(docker::run(Arc::clone(&state)));

    info!("All collectors started");
}
