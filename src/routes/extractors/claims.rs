use axum::{extract::FromRequestParts, http::request::Parts};

use crate::error::AppError;

/// Claims injected into request extensions by `auth_middleware` after a
/// successful access-token validation + jti revocation check.
#[derive(Clone, Debug)]
pub struct Claims {
    pub device_id: String,
    pub jti: String,
}

impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Claims>()
            .cloned()
            .ok_or(AppError::Unauthorized)
    }
}
