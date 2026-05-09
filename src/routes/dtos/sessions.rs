//! Session/device management DTOs. Backs the multi-device view in the
//! web client's settings page — operators can see every browser/app
//! paired to this server, rename them, and revoke access.

use serde::{Deserialize, Serialize};

use crate::models::auth::Device;

/// One paired device, augmented with two pieces of context the client
/// needs to render the row meaningfully:
/// - `is_current`: was this token issued to the device making the
///   request? Lets the UI tag "this device" and warn before self-revoke.
/// - `active_sessions`: how many non-expired JWT sessions reference
///   this device. Zero means paired but logged out.
#[derive(Debug, Serialize)]
pub struct SessionInfo {
    #[serde(flatten)]
    pub device: Device,
    pub is_current: bool,
    pub active_sessions: i64,
}

#[derive(Debug, Serialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Deserialize)]
pub struct RenameSessionRequest {
    pub name: String,
}
