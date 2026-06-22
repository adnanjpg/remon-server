pub mod dtos;
pub mod extractors;
pub mod rest;
pub mod sse;
pub mod ws;

mod cors;

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, http::StatusCode};
use tower_http::LatencyUnit;
use tower_http::compression::{
    CompressionLayer,
    predicate::{DefaultPredicate, NotForContentType, Predicate},
};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::request_log;
use crate::state::AppState;

/// Assemble the full application router: REST (with request timeouts) merged
/// with the long-lived SSE/WS streams, wrapped in the shared tower-http stack.
pub fn build_app(app_state: Arc<AppState>, config: &Config) -> anyhow::Result<Router> {
    let cors_layer = cors::build_cors_layer(&config.cors)?;

    // REST gets request timeouts; SSE/WS streams stay long-lived.
    let rest_router = rest::create_routes(app_state.clone()).layer(TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_secs(30),
    ));
    let compression = CompressionLayer::new()
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("text/event-stream")));

    let app = Router::new()
        .merge(rest_router)
        .nest("/sse", sse::create_routes(app_state.clone()))
        .nest("/ws", ws::create_routes(app_state.clone()))
        .with_state(app_state)
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(compression)
        // SSE/WS browser clients authenticate via query string, so redact
        // access_token before the URI reaches stdout or DB-backed logs.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let uri = request_log::redact_access_token(&req.uri().to_string());
                    tracing::info_span!(
                        "request",
                        method = %req.method(),
                        uri = %uri,
                    )
                })
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::INFO)
                        .latency_unit(LatencyUnit::Millis),
                ),
        )
        // Outermost so responses produced by inner layers — notably the
        // 413 from the body limit above — still carry CORS headers; without
        // this a browser sees an opaque CORS error instead of the real status.
        .layer(cors_layer);

    Ok(app)
}
