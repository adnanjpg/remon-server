use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// Like `axum::extract::Query`, but a missing or malformed query parameter is
/// surfaced as `AppError::BadRequest` so the client gets the standard
/// `{"error":{code,message}}` envelope instead of axum's default plain-text
/// 400 ("Failed to deserialize query string: ...").
pub struct ValidatedQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().unwrap_or("");
        let value = serde_urlencoded::from_str::<T>(query)
            .map_err(|e| AppError::BadRequest(format!("invalid query parameters: {}", e)))?;
        Ok(ValidatedQuery(value))
    }
}
