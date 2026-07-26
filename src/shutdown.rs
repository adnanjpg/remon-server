use log::{error, info};
use tokio::sync::watch;

use crate::state::ExitIntent;

/// Resolve when it is time to stop serving.
///
/// Three ways in: the two signals a supervisor or a terminal sends, and an
/// in-process request from `/system/restart` or `/system/shutdown`. The last
/// one exists because a handler cannot end the process on its own — it has to
/// let the response go out first, and let the graceful drain run.
pub async fn signal(mut exit_intent: watch::Receiver<Option<ExitIntent>>) {
    tokio::select! {
        _ = ctrl_c() => info!("received ctrl+c, shutting down"),
        _ = terminate() => info!("received SIGTERM, shutting down"),
        intent = requested(&mut exit_intent) => {
            info!("shutdown requested via API ({intent:?})");
        }
    }
}

/// Wait for a handler to record why we are ending.
async fn requested(exit_intent: &mut watch::Receiver<Option<ExitIntent>>) -> Option<ExitIntent> {
    // `changed()` only errors when every sender is gone, which cannot happen
    // while AppState is alive — park rather than resolve, so a bug here never
    // looks like a shutdown request.
    loop {
        if exit_intent.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        let current = *exit_intent.borrow();
        if current.is_some() {
            return current;
        }
    }
}

async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("ctrl+c handler error: {}", e);
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            error!("failed to install SIGTERM handler: {}", e);
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}
