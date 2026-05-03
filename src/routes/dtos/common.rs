use serde::Serialize;

/// Generic empty success body. Prefer returning `StatusCode::NO_CONTENT` when
/// the response truly has no payload; use this when a JSON body is required.
#[derive(Debug, Serialize)]
pub struct SuccessResponse {
    pub success: bool,
}

impl SuccessResponse {
    pub const OK: Self = Self { success: true };
}
