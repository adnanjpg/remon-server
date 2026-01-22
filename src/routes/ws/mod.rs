pub mod docker;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use std::sync::Arc;

/// Creates and returns the WebSocket routes
/// All WebSocket routes require authentication
pub fn create_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Docker container exec via WebSocket
        .route("/docker/containers/{id}/exec", get(docker::docker_exec))
        .layer(middleware::from_fn(
            crate::routes::middleware::auth_middleware,
        ))
}
