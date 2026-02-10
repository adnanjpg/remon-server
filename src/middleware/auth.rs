//! Authentication middleware

use axum::{extract::Request, http::StatusCode, middleware::Next, response::Response};

use crate::auth::service::AuthService;
use crate::config::Config;
use crate::routes::extractors::Claims;

/// Auth middleware that validates JWT access tokens
/// Supports both new access tokens and legacy tokens for backward compatibility
pub async fn auth_middleware(mut req: Request, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Extract Bearer token
    let token = if auth_header.starts_with("Bearer ") {
        &auth_header[7..]
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Load config for auth settings
    let config = Config::new().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let auth_service = AuthService::new(config.auth);

    // Try new access token validation first
    let device_id = match auth_service.validate_access_token(token) {
        Ok(claims) => {
            // Token is valid - device was verified at login time
            // Note: We skip device re-validation here for performance
            // Device status is checked on login and token refresh
            claims.sub
        }
        Err(_) => {
            // Fallback to legacy token validation for backward compatibility
            crate::auth::token::validate_token(auth_header)
                .await
                .map_err(|_| StatusCode::UNAUTHORIZED)?
        }
    };

    req.extensions_mut().insert(Claims { device_id });

    Ok(next.run(req).await)
}
