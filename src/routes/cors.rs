use anyhow::Result;
use axum::http::{HeaderName, HeaderValue, Method};
use log::{error, info, warn};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::CorsConfig;

/// Build CORS from config.
///
/// An empty allow-list with `allow_any_origin = false` is a valid, and the
/// tightest, policy: no `Access-Control-Allow-Origin` is ever emitted, so no
/// browser origin can call the API. Native clients (mobile, curl) are
/// unaffected — CORS is a browser mechanism, not an auth one. That has to
/// boot rather than abort, because it is the shipped default: a fresh install
/// knows no frontend origin yet, and a server that refuses to start is worse
/// than one that starts and turns browsers away with a logged warning.
pub fn build_cors_layer(cfg: &CorsConfig) -> Result<CorsLayer> {
    let methods = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ];
    let allowed_headers = [
        HeaderName::from_static("authorization"),
        HeaderName::from_static("content-type"),
    ];

    let base = CorsLayer::new()
        .allow_methods(methods)
        .allow_headers(allowed_headers);

    if cfg.allow_any_origin {
        info!("CORS: allowing any origin");
        return Ok(base.allow_origin(AllowOrigin::any()));
    }

    if cfg.allowed_origins.is_empty() {
        warn!(
            "CORS: no origins allowed — browser clients cannot reach this API. \
             Set cors.allowed_origins to your web UI's origin (or \
             REMON__CORS__ALLOW_ANY_ORIGIN=true for local development). \
             Native clients are unaffected."
        );
        return Ok(base.allow_origin(AllowOrigin::list([])));
    }

    let parsed: Vec<HeaderValue> = cfg
        .allowed_origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                error!("skipping invalid CORS origin '{}'", o);
                None
            }
        })
        .collect();

    if parsed.is_empty() {
        anyhow::bail!("cors.allowed_origins had no valid entries");
    }

    info!(
        "CORS: explicit allow-list ({} origin{})",
        parsed.len(),
        if parsed.len() == 1 { "" } else { "s" }
    );
    Ok(base.allow_origin(AllowOrigin::list(parsed)))
}
