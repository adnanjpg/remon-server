use log::{error, info};

pub async fn signal() {
    tokio::select! {
        _ = ctrl_c() => info!("Received Ctrl+C, shutting down..."),
        _ = terminate() => info!("Received SIGTERM, shutting down..."),
    }
}

async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("Ctrl+C handler error: {}", e);
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
            error!("Failed to install SIGTERM handler: {}", e);
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}
