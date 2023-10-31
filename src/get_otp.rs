use ring::rand::{SecureRandom, SystemRandom};

use base32::{encode, Alphabet};

use fast_qr::qr::QRBuilder;
use fast_qr::{convert::ConvertError, qr::QRCodeError};
use totp_rs::{Algorithm, Secret, TotpUrlError, TOTP};

use serde::{Deserialize, Serialize};

const OTP_APP_NAME: &str = "remon";
const OTP_TIME_STEP: u64 = 30;
const OTP_DIGITS: usize = 6;
const OTP_SKEW: u8 = 1;
const OTP_ALGORITHM: Algorithm = Algorithm::SHA1;

pub const TOTP_KEY: &str = "JJFECVSHKJBUYSS2JVDFKUZSIRFFURSVINKTES2JKE";

#[derive(Serialize, Deserialize)] // Derive Deserialize and Serialize for your struct
pub struct ValidateOtpData {
    pub device_id: String,
    pub token: String,
}

fn generate_totp_secret() -> String {
    // let rng = SystemRandom::new();
    // let mut secret = vec![0u8; 20];

    // rng.fill(&mut secret).unwrap();

    // // Encode the shared secret in base32
    // let encoded_secret = encode(Alphabet::RFC4648 { padding: false }, secret.as_slice());

    // let totp = generate_totp_obj(&encoded_secret).unwrap();
    // let otp_base32 = totp.get_secret_base32();

    // otp_base32

    TOTP_KEY.to_owned()
}

pub fn generate_otp_qr_url(user_name: &str) -> String {
    let encoded_secret = generate_totp_secret();

    // otpauth://totp/YourAppName:username?secret=sharedsecret&issuer=YourAppName&algorithm=SHA1&digits=6&period=30
    let otpcode = format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        OTP_APP_NAME, user_name, encoded_secret, OTP_APP_NAME, OTP_DIGITS, OTP_TIME_STEP
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
    let totp = TOTP::new(
        OTP_ALGORITHM,
        OTP_DIGITS,
        OTP_SKEW,
        OTP_TIME_STEP,
        Secret::Encoded(secret.to_owned()).to_bytes().unwrap(),
    );

    return totp;
}

pub fn generate_totp(secret: &str) -> String {
    let totp = generate_totp_obj(secret).unwrap();

    let result = totp.generate_current().unwrap();

    result
}

pub fn check_totp_match(key: &str, secret: &str) -> bool {
    if key.len() != OTP_DIGITS {
        return false;
    }

    let totp = generate_totp_obj(secret).unwrap();

    let result = totp.check_current(key).unwrap();

    result
}
