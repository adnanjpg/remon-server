pub mod docker;

use axum::{Router, middleware, routing::get};

/// Creates and returns the WebSocket routes
/// All WebSocket routes require authentication
pub fn create_routes() -> Router {
    Router::new()
        // Docker container exec via WebSocket
        .route("/docker/containers/{id}/exec", get(docker::docker_exec))
        .layer(middleware::from_fn(crate::api::middleware::auth_middleware))
}
