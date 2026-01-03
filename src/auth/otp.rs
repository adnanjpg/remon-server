use base32::{encode, Alphabet};

use fast_qr::qr::QRBuilder;
use fast_qr::qr::QRCodeError;
use totp_rs::{Algorithm, Secret, TotpUrlError, TOTP};

use serde::{Deserialize, Serialize};
use blake3;

const OTP_APP_NAME: &str = "remon";
const OTP_TIME_STEP: u64 = 30;
const OTP_DIGITS: usize = 6;
const OTP_SKEW: u8 = 1;
const OTP_ALGORITHM: Algorithm = Algorithm::SHA1;

#[derive(Serialize, Deserialize)] // Derive Deserialize and Serialize for your struct
pub struct ValidateOtpData {
    pub device_id: String,
    pub token: String,
}

fn generate_totp_secret(device_id: &str) -> String {
    // Hash the device_id to get a consistent, fixed-size secret
    // Using first 20 bytes (160 bits) which is standard for TOTP
    let hash = blake3::hash(device_id.as_bytes());
    let secret_bytes = &hash.as_bytes()[..20]; // Take first 20 bytes

    // Encode in base32
    let encoded_secret = encode(Alphabet::Rfc4648 { padding: false }, secret_bytes);

    encoded_secret
}

pub fn generate_otp_qr_url(device_id: &str) -> String {
    let encoded_secret = generate_totp_secret(device_id);

    // otpauth://totp/YourAppName:username?secret=sharedsecret&issuer=YourAppName&algorithm=SHA1&digits=6&period=30
    let otpcode = format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        OTP_APP_NAME, device_id, encoded_secret, OTP_APP_NAME, OTP_DIGITS, OTP_TIME_STEP
    );

    otpcode
}

// takes a string as input
// returns Ok if successful, Err if not
pub fn outputqr(input: &str) -> Result<String, QRCodeError> {
    // QRBuilder::new can fail if content is too big for version,
    // please check before unwrapping.
    let qrcode = QRBuilder::new(input).build()?;

    Ok(qrcode.to_str())
}

fn generate_totp_obj(secret: &str) -> Result<TOTP, TotpUrlError> {
    let secret_bytes = Secret::Encoded(secret.to_owned())
        .to_bytes()
        .map_err(|_| TotpUrlError::Secret("Invalid secret".to_string()))?;

    let totp = TOTP::new(
        OTP_ALGORITHM,
        OTP_DIGITS,
        OTP_SKEW,
        OTP_TIME_STEP,
        secret_bytes,
    )?;

    Ok(totp)
}

pub fn check_totp_match(key: &str, secret: &str) -> bool {
    if key.len() != OTP_DIGITS {
        return false;
    }

    let totp = match generate_totp_obj(secret) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let result = match totp.check_current(key) {
        Ok(r) => r,
        Err(_) => return false,
    };

    result
}
pub fn check_totp_match_dev_id(key: &str, device_id: &str) -> bool {
    let secret = generate_totp_secret(device_id);

    check_totp_match(key, &secret)
}
