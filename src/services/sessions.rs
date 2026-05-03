use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};

use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

/// Interval between expired-session sweeps. Expired rows are already
/// rejected by the `session_exists` WHERE clause (expires_at > unixepoch()),
/// so this is purely a hygiene / table-size concern — hourly is plenty.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        run(state).await;
    });
}

async fn run(state: Arc<AppState>) {
    let repo = DeviceRepository::new(state.db.clone());

    // Eagerly clean up on boot before the first hour elapses.
    sweep(&repo).await;

    loop {
        tokio::time::sleep(CLEANUP_INTERVAL).await;
        sweep(&repo).await;
    }
}

async fn sweep(repo: &DeviceRepository) {
    match repo.cleanup_expired_sessions().await {
        Ok(0) => {}
        Ok(n) => debug!("Session cleanup: removed {} expired session(s)", n),
        Err(e) => warn!("Session cleanup failed: {:?}", e),
    }
}
