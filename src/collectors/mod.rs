#[cfg(feature = "docker")]
mod docker;
pub(crate) mod processes;
pub mod smart;
pub(crate) mod stats;

use log::info;
use std::sync::Arc;

use crate::state::AppState;

pub fn spawn_all(state: Arc<AppState>) {
    // System stats collector
    tokio::spawn(stats::run(Arc::clone(&state)));

    // Container stats (Docker/Podman); exits quietly if the daemon is absent.
    #[cfg(feature = "docker")]
    docker::spawn(Arc::clone(&state));

    // Continuous process collector: keeps the process cache warm and feeds
    // the rolling per-process history behind the assistant's time context.
    // Config-gated because a full process refresh per tick is real work on
    // hosts with very large process tables; off, GET /processes falls back
    // to on-demand refresh with TTL caching.
    if state.assistant_config.process_history {
        tokio::spawn(processes::run(Arc::clone(&state)));
    }

    info!("collectors started");
}
