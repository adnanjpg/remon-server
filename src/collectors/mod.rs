#[cfg(feature = "docker")]
mod docker;
pub mod smart;
mod stats;

use log::info;
use std::sync::Arc;

use crate::state::AppState;

pub fn spawn_all(state: Arc<AppState>) {
    // System stats collector
    tokio::spawn(stats::run(Arc::clone(&state)));

    // Container stats (Docker/Podman); exits quietly if the daemon is absent.
    #[cfg(feature = "docker")]
    docker::spawn(Arc::clone(&state));

    // Process inventory is refreshed on demand by GET /processes. Keeping a
    // continuous process collector running is wasteful on hosts with very
    // large process tables and there is no process SSE route today.
    info!("collectors started");
}
