use log::{error, info};

pub async fn signal() {
    tokio::select! {
        _ = ctrl_c() => info!("received ctrl+c, shutting down"),
        _ = terminate() => info!("received SIGTERM, shutting down"),
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
