use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Unified error type for the application.
///
/// Variants are added when at least one handler actually produces them.
/// `DockerError` and `DatabaseError` look unused at first glance — no
/// handler writes them by hand — but they are reachable through the
/// `From<sqlx::Error>` / `From<bollard::errors::Error>` impls below
/// (the `?` operator hits those automatically).
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    // ===== Auth =====
    #[error("Authentication required")]
    Unauthorized,

    #[error("Invalid or expired token")]
    InvalidToken,

    #[error("Device not registered")]
    DeviceNotFound,

    #[error("Device is deactivated")]
    DeviceInactive,

    #[error("Pairing code expired or invalid")]
    PairingExpired,

    // ===== Resource =====
    #[error("{0} not found")]
    NotFound(String),

    #[error("Resource already exists")]
    AlreadyExists,

    #[error("{0}")]
    Conflict(String),

    // ===== Validation =====
    #[error("{0}")]
    BadRequest(String),

    // ===== Operation =====
    #[error("Failed to kill process: {0}")]
    ProcessKillFailed(String),

    #[cfg(feature = "docker")]
    #[error("Docker not available: {0}")]
    DockerUnavailable(String),

    /// Reachable only via `From<bollard::errors::Error>` / `From<DockerError>`.
    /// Don't construct it directly in handlers — surface a more specific
    /// variant (`NotFound`, `DockerUnavailable`) when you have one.
    #[cfg(feature = "docker")]
    #[error("Docker error: {0}")]
    DockerError(String),

    // ===== Storage =====
    /// Reachable only via `From<sqlx::Error>`. Internal-detail messages are
    /// logged server-side and replaced with a generic 500 body before
    /// reaching the client.
    #[error("Database error")]
    DatabaseError(String),

    // ===== Platform =====
    #[error("Not supported on this platform")]
    NotSupported,

    #[error("Forbidden: {0}")]
    Forbidden(String),

    // ===== Generic =====
    #[error("Internal error")]
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
        let (status, code, public_message) = match &self {
            // 400 Bad Request
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),

            // 401 Unauthorized
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Authentication required".to_string(),
            ),
            AppError::InvalidToken => (
                StatusCode::UNAUTHORIZED,
                "INVALID_TOKEN",
                "Invalid or expired token".to_string(),
            ),
            AppError::DeviceNotFound => (
                StatusCode::UNAUTHORIZED,
                "DEVICE_NOT_FOUND",
                "Device not registered".to_string(),
            ),
            AppError::DeviceInactive => (
                StatusCode::UNAUTHORIZED,
                "DEVICE_INACTIVE",
                "Device is deactivated".to_string(),
            ),

            // 404 Not Found
            AppError::NotFound(what) => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                format!("{} not found", what),
            ),

            // 409 Conflict
            AppError::AlreadyExists => (
                StatusCode::CONFLICT,
                "ALREADY_EXISTS",
                "Resource already exists".to_string(),
            ),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg.clone()),

            // 410 Gone
            AppError::PairingExpired => (
                StatusCode::GONE,
                "PAIRING_EXPIRED",
                "Pairing code expired or invalid".to_string(),
            ),

            // 422 Unprocessable Entity
            AppError::ProcessKillFailed(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "PROCESS_KILL_FAILED",
                msg.clone(),
            ),
            #[cfg(feature = "docker")]
            AppError::DockerError(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "DOCKER_ERROR",
                msg.clone(),
            ),

            // 403 Forbidden — caller authenticated but lacks privilege
            // (e.g. systemd service management without root / PolicyKit).
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, "FORBIDDEN", msg.clone()),

            // 501 Not Implemented — operation cannot run on this platform
            // (e.g. systemd timers on Windows, journalctl logs on macOS).
            AppError::NotSupported => (
                StatusCode::NOT_IMPLEMENTED,
                "NOT_SUPPORTED",
                "Not supported on this platform".to_string(),
            ),

            // 503 Service Unavailable
            #[cfg(feature = "docker")]
            AppError::DockerUnavailable(msg) => {
                log::warn!("Docker unavailable: {}", msg);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "DOCKER_UNAVAILABLE",
                    "Docker is not available".to_string(),
                )
            }

            // 500 Internal Server Error — never leak internal detail to client
            AppError::DatabaseError(msg) => {
                log::error!("database error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "DATABASE_ERROR",
                    "Internal server error".to_string(),
                )
            }
            AppError::Internal(msg) => {
                log::error!("internal error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "Internal server error".to_string(),
                )
            }
        };

        let body = ErrorResponse {
            error: ErrorBody {
                code,
                message: public_message,
            },
        };

        (status, Json(body)).into_response()
    }
}

// ===== Convenience conversions =====

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => AppError::NotFound("Resource".to_string()),
            other => AppError::DatabaseError(other.to_string()),
        }
    }
}

#[cfg(feature = "docker")]
impl From<bollard::errors::Error> for AppError {
    fn from(err: bollard::errors::Error) -> Self {
        match err {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message,
            } => AppError::NotFound(message),
            bollard::errors::Error::DockerResponseServerError {
                status_code: 409,
                message,
            } => AppError::DockerError(message),
            other => AppError::DockerError(other.to_string()),
        }
    }
}

impl From<jsonwebtoken::errors::Error> for AppError {
    fn from(_: jsonwebtoken::errors::Error) -> Self {
        AppError::InvalidToken
    }
}

#[cfg(feature = "docker")]
impl From<crate::services::docker::DockerError> for AppError {
    fn from(err: crate::services::docker::DockerError) -> Self {
        use crate::services::docker::DockerError as DE;
        match err {
            DE::NotAvailable(msg) => AppError::DockerUnavailable(msg),
            DE::ContainerNotFound(msg) => AppError::NotFound(format!("Container {}", msg)),
            DE::ImageNotFound(msg) => AppError::NotFound(format!("Image {}", msg)),
            DE::ApiError(msg) => AppError::DockerError(msg),
        }
    }
}

impl From<crate::platform::services::ServiceError> for AppError {
    fn from(err: crate::platform::services::ServiceError) -> Self {
        use crate::platform::services::ServiceError as SE;
        match err {
            SE::NotFound(name) => AppError::NotFound(format!("Service '{}'", name)),
            SE::NotSupported => AppError::NotSupported,
            SE::PermissionDenied => {
                AppError::Forbidden("Service management requires elevated privileges".to_string())
            }
            SE::BackendError(msg) => AppError::Internal(msg),
        }
    }
}

/// Result type alias for handlers
pub type AppResult<T> = Result<T, AppError>;
