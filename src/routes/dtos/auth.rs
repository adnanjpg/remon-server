use serde::Deserialize;

#[derive(Deserialize)]
pub struct GetOtpQrRequest {
    pub device_id: String,
}