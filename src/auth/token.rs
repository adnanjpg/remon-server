//! Startup-time JWT secret validation.
//!
//! Token issuance and verification live in `auth::service`. This module exists
//! only to fail-fast at startup when the configured secret is weak or default.

use crate::config::Config;

const MIN_JWT_SECRET_LENGTH: usize = 32;
const DEFAULT_SECRET: &str = "change-me";

/// Validate JWT secret strength at startup.
/// In debug builds: print a warning and continue. In release builds: hard error.
pub fn validate() -> Result<(), String> {
    let config = Config::new().map_err(|e| format!("Failed to load config: {}", e))?;
    let secret = &config.auth.jwt_secret;

    if secret == DEFAULT_SECRET || secret.len() < MIN_JWT_SECRET_LENGTH {
        if cfg!(debug_assertions) {
            eprintln!("⚠️  WARNING: Using weak/default JWT secret in development!");
            eprintln!(
                "⚠️  Current secret length: {} chars (minimum: {})",
                secret.len(),
                MIN_JWT_SECRET_LENGTH
            );
            eprintln!("⚠️  Set REMON__AUTH__JWT_SECRET or update config/development.toml");
            eprintln!("⚠️  Example: REMON__AUTH__JWT_SECRET=\"$(openssl rand -base64 32)\"");
            Ok(())
        } else {
            Err(format!(
                "FATAL SECURITY ERROR: JWT_SECRET not set or too weak for production.\n\
                 Current secret length: {} chars (minimum required: {})\n\
                 Set REMON__AUTH__JWT_SECRET environment variable with a strong secret.\n\
                 Generate one with: openssl rand -base64 48",
                secret.len(),
                MIN_JWT_SECRET_LENGTH
            ))
        }
    } else {
        Ok(())
    }
}
