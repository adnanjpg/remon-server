use anyhow::Result;
use axum::http::{HeaderName, HeaderValue, Method};
use log::{error, info};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::CorsConfig;

/// Build CORS from config, failing fast when production allow-listing is empty.
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
        anyhow::bail!(
            "cors.allow_any_origin = false but cors.allowed_origins is empty. \
             Set REMON__CORS__ALLOW_ANY_ORIGIN=true (dev) or populate allowed_origins."
        );
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
