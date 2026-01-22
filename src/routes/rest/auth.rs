use axum::{http::StatusCode, Json};

use crate::routes::dtos::{auth::GetOtpQrRequest, common::ResponseBody};

pub async fn get_otp_qr(
    Json(payload): Json<GetOtpQrRequest>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    let url = crate::auth::otp::generate_otp_qr_url(&payload.device_id);

    match crate::auth::otp::outputqr(&url) {
        Ok(qr) => {
            println!("{}\r\n{}", url, qr);
            Ok(Json(ResponseBody::Success(true)))
        }
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(
                "Failed to generate QR code.".to_string(),
            )),
        )),
    }
}

pub async fn login(
    Json(login_req): Json<crate::auth::token::LoginRequest>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    if crate::auth::otp::check_totp_match_dev_id(&login_req.otp, &login_req.device_id) {
        let token = crate::auth::token::generate_token(&login_req.device_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ResponseBody::Error("Failed to generate token.".to_string())),
                )
            })?;

        Ok(Json(ResponseBody::Token(token)))
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ResponseBody::Error("Invalid OTP code.".to_string())),
        ))
    }
}
