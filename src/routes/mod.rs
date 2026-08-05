pub mod dtos;
pub mod extractors;
pub mod rest;
pub mod sse;
pub mod ws;

mod cors;

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, http::StatusCode, response::IntoResponse};
use tower_http::LatencyUnit;
use tower_http::catch_panic::CatchPanicLayer;
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

/// Turn a panicking handler into a 500 for that request rather than a dead daemon.
fn panic_to_500(err: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    let payload = err
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    crate::error::AppError::Internal(format!("handler panicked: {payload}")).into_response()
}

/// Assemble the full application router: REST (with request timeouts) merged
/// with the long-lived SSE/WS streams, wrapped in the shared tower-http stack.
pub fn build_app(app_state: Arc<AppState>, config: &Config) -> anyhow::Result<Router> {
    let cors_layer = cors::build_cors_layer(&config.cors)?;

    // REST gets a 30s request timeout; SSE/WS streams stay long-lived.
    let rest_router = rest::create_routes(app_state.clone()).layer(TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_secs(30),
    ));

    // The assistant runs a multi-step tool-use loop that legitimately exceeds
    // 30s (several provider round trips, each up to its own 60s cap), so it
    // gets its own longer timeout. Auth-gated the same way — auth_middleware
    // is applied here since it lives outside create_routes' protected group.
    let assistant_router = rest::create_assistant_routes()
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middleware::auth_middleware,
        ))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(150),
        ));
    let compression = CompressionLayer::new()
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("text/event-stream")));

    let app = Router::new()
        .merge(rest_router)
        .merge(assistant_router)
        .nest("/sse", sse::create_routes(app_state.clone()))
        .nest("/ws", ws::create_routes(app_state.clone()))
        .with_state(app_state)
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(compression)
        // Inside trace and CORS so its 500 is logged and a browser can read it.
        .layer(CatchPanicLayer::custom(panic_to_500))
        // SSE/WS browser clients authenticate via query string and
        // heartbeat pings carry their capability slug in the path, so
        // scrub both before the URI reaches stdout or DB-backed logs.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let uri = request_log::redact_uri(&req.uri().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get};
    use tower::ServiceExt;

    async fn boom() -> axum::response::Response {
        panic!("boom")
    }

    #[tokio::test]
    async fn a_panicking_handler_answers_500_instead_of_taking_the_process() {
        let app = Router::new()
            .route("/boom", get(boom))
            .layer(CatchPanicLayer::custom(panic_to_500));

        let resp = app
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
