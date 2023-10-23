use totp_rs::{Algorithm, Secret, TOTP};

pub fn generate_totp(secret: String, time_step: u64) -> String {
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        time_step,
        Secret::Encoded(secret).to_bytes().unwrap(),
    )
    .unwrap();

    let result = totp.generate_current().unwrap();

    result
}
