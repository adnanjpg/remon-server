//! Authentication middleware

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

use crate::auth::service::AuthService;
use crate::error::AppError;
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

/// Validates the JWT access token, checks that its jti is still in the
/// `sessions` table (not revoked via logout or refresh-rotation), and
/// injects `Claims` into request extensions.
///
/// Only `Authorization: Bearer <jwt>` is read here. A token in the URL
/// ends up in proxy access logs, so it is accepted on WebSocket routes alone;
/// see [`auth_middleware_with_query`].
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    authenticate(&state, req, next, false).await
}

/// [`auth_middleware`] that also takes `?access_token=<jwt>`, for
/// `new WebSocket(...)`, which cannot send headers. The trace layer in
/// `main.rs` redacts it from logged URIs.
pub async fn auth_middleware_with_query(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    authenticate(&state, req, next, true).await
}

async fn authenticate(
    state: &AppState,
    mut req: Request,
    next: Next,
    allow_query: bool,
) -> Result<Response, AppError> {
    let token = extract_token(&req, allow_query).ok_or(AppError::Unauthorized)?;

    let auth_service = AuthService::new(state.auth_config.clone());
    let claims = auth_service
        .validate_access_token(&token)
        .map_err(|_| AppError::InvalidToken)?;

    // Revocation check: positive in-memory cache first, DB on a miss. Only
    // DB-confirmed jtis ever enter the cache; logout/rotation/revoke evict
    // (see SessionCache for the staleness bound when they don't).
    if !state.session_cache.check(&claims.jti) {
        let device_repo = DeviceRepository::new(state.db.clone());
        if !device_repo.session_exists(&claims.jti).await? {
            return Err(AppError::InvalidToken);
        }
        state.session_cache.insert(&claims.jti);
    }

    req.extensions_mut().insert(Claims {
        device_id: claims.sub,
        jti: claims.jti,
    });

    Ok(next.run(req).await)
}

fn extract_token(req: &Request, allow_query: bool) -> Option<String> {
    if let Some(token) = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
    {
        return Some(token.to_string());
    }

    if !allow_query {
        return None;
    }
    req.uri().query().and_then(|q| {
        q.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == "access_token").then(|| v.to_string())
        })
    })
}

/// RFC 7235 auth schemes are case-insensitive (`Bearer` / `bearer` / `BEARER`
/// are all valid). The strict `strip_prefix("Bearer ")` we had before locked
/// out callers that happened to lowercase the scheme.
fn parse_bearer(header: &str) -> Option<&str> {
    let (scheme, rest) = header.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("Bearer")
        .then_some(rest.trim_start())
}
