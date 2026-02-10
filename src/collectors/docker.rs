use std::sync::Arc;
use std::time::Duration;
use log::{debug, info};

use crate::state::AppState;

/// Docker stats collector
/// Collects container stats and broadcasts them
pub async fn run(_state: Arc<AppState>) {
    info!("Docker collector started");

    // Default interval: 3 seconds
    let interval_ms = 3000;

    loop {
        // TODO: Implement Docker stats collection when Docker service is ready
        debug!("Docker stats collection not yet implemented");

        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}
