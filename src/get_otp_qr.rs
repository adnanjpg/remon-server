use ring::error::Unspecified;
use ring::rand::{SecureRandom, SystemRandom};

extern crate base32;
use base32::{encode, Alphabet};

use fast_qr::convert::ConvertError;
use fast_qr::qr::QRBuilder;
use totp_rs::{Algorithm, Secret, TOTP};

fn generate_totp_secret() -> Result<Vec<u8>, Unspecified> {
    let rng = SystemRandom::new();
    let mut secret = vec![0u8; 20]; // You can change the secret size if needed.

    rng.fill(&mut secret)?;

    Ok(secret)
}

fn generate_totp_secret_encoded() -> String {
    let shared_secret = generate_totp_secret().unwrap();

    // Encode the shared secret in base32
    let encoded_secret = encode(
        Alphabet::RFC4648 { padding: false },
        shared_secret.as_slice(),
    );

    encoded_secret
}

pub fn generate_otp_qr_code() -> String {
    let encoded_secret = generate_totp_secret_encoded();

    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        Secret::Encoded(encoded_secret).to_bytes().unwrap(),
    )
    .unwrap();
    let otp_base32 = totp.get_secret_base32();

    // TODO(adnanjpg): take from request
    let user_name = "adnan";
    let app_name = "remon";

    let digits = 6;
    let period = 30;

    // otpauth://totp/YourAppName:username?secret=sharedsecret&issuer=YourAppName
    let otpcode = format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        app_name, user_name, otp_base32, app_name, digits, period
    );

    otpcode
}

// takes a string as input
// returns Ok if successful, Err if not
// prints the QR code to the terminal
pub fn outputqr(input: &str) -> Result<(), ConvertError> {
    // QRBuilder::new can fail if content is too big for version,
    // please check before unwrapping.
    let qrcode = QRBuilder::new(input).build().unwrap();

    println!("{}", input);
    let val = qrcode.to_str(); // .print() exists
    println!("{}", val);

    Ok(())
}
