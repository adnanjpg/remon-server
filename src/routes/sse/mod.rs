pub mod docker;
pub mod system;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use std::sync::Arc;

/// Creates and returns the SSE (Server-Sent Events) routes
/// All SSE routes require authentication
pub fn create_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Docker log streaming
        .route(
            "/docker/containers/{id}/logs/stream",
            get(docker::stream_logs),
        )
        .layer(middleware::from_fn(
            crate::routes::middleware::auth_middleware,
        ))
}
