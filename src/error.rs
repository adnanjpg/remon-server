use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

/// Unified error type for the application
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    // Auth errors
    #[error("Authentication required")]
    Unauthorized,

    #[error("Invalid or expired token")]
    InvalidToken,

    #[error("Device not registered")]
    DeviceNotFound,

    #[error("Pairing code expired or invalid")]
    PairingExpired,

    #[error("TOTP verification required for this operation")]
    TotpRequired,

    #[error("Invalid TOTP code")]
    TotpInvalid,

    #[error("Too many requests")]
    RateLimited,

    // Resource errors
    #[error("{0} not found")]
    NotFound(String),

    #[error("Resource already exists")]
    AlreadyExists,

    // Operation errors
    #[error("Failed to kill process: {0}")]
    ProcessKillFailed(String),

    #[error("Docker error: {0}")]
    DockerError(String),

    // Storage errors
    #[error("Database error: {0}")]
    DatabaseError(String),

    // Validation errors
    #[error("Validation error: {0}")]
    ValidationError(String),

    // External service errors
    #[error("Notifier '{notifier}' failed: {message}")]
    NotifierError { notifier: String, message: String },

    // Generic internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            // 401 Unauthorized
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            AppError::InvalidToken => (StatusCode::UNAUTHORIZED, "INVALID_TOKEN"),
            AppError::TotpRequired => (StatusCode::UNAUTHORIZED, "TOTP_REQUIRED"),
            AppError::TotpInvalid => (StatusCode::UNAUTHORIZED, "TOTP_INVALID"),

            // 403 Forbidden
            AppError::DeviceNotFound => (StatusCode::FORBIDDEN, "DEVICE_NOT_FOUND"),

            // 404 Not Found
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),

            // 409 Conflict
            AppError::AlreadyExists => (StatusCode::CONFLICT, "ALREADY_EXISTS"),

            // 410 Gone
            AppError::PairingExpired => (StatusCode::GONE, "PAIRING_EXPIRED"),

            // 422 Unprocessable Entity
            AppError::ProcessKillFailed(_) => (StatusCode::UNPROCESSABLE_ENTITY, "PROCESS_KILL_FAILED"),
            AppError::DockerError(_) => (StatusCode::UNPROCESSABLE_ENTITY, "DOCKER_ERROR"),
            AppError::ValidationError(_) => (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),

            // 429 Too Many Requests
            AppError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),

            // 502 Bad Gateway
            AppError::NotifierError { .. } => (StatusCode::BAD_GATEWAY, "NOTIFIER_ERROR"),

            // 500 Internal Server Error
            AppError::DatabaseError(msg) => {
                log::error!("Database error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "DATABASE_ERROR")
            }
            AppError::Internal(msg) => {
                log::error!("Internal error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR")
            }
        };

        let body = ErrorResponse {
            error: ErrorBody {
                code,
                message: self.to_string(),
            },
        };

        (status, Json(body)).into_response()
    }
}

// Convenience conversions
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::DatabaseError(e.to_string())
    }
}

impl From<bollard::errors::Error> for AppError {
    fn from(e: bollard::errors::Error) -> Self {
        AppError::DockerError(e.to_string())
    }
}

impl From<jsonwebtoken::errors::Error> for AppError {
    fn from(_: jsonwebtoken::errors::Error) -> Self {
        AppError::InvalidToken
    }
}

/// Result type alias for handlers
pub type AppResult<T> = Result<T, AppError>;
