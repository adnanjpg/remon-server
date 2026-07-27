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

    let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // An interval's first tick is immediate, and the boot sweep above already
    // did that pass.
    ticker.tick().await;
    let mut shutdown = state.shutdown.subscribe();

    while crate::shutdown::tick_or_stop(&mut ticker, &mut shutdown).await {
        sweep(&repo).await;
    }
}

async fn sweep(repo: &DeviceRepository) {
    match repo.cleanup_expired_sessions().await {
        Ok(0) => {}
        Ok(n) => debug!("session cleanup: removed {} expired session(s)", n),
        Err(e) => warn!("session cleanup failed: {:?}", e),
    }
}
