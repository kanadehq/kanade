//! TOTP MFA helper (#1192). Wraps `totp-rs` so the crate surface lives in one
//! place; the enrollment / login handlers deal only in base32 secrets + codes.
//!
//! Standard authenticator-app parameters: SHA1, 6 digits, 30 s step, ±1 step
//! tolerance for clock drift. The secret is bound to the username as the
//! otpauth `account_name` so a QR scanned in an authenticator is labelled
//! `kanade:<user>`.

use totp_rs::{Algorithm, Secret, TOTP};

const ISSUER: &str = "kanade";
const DIGITS: usize = 6;
const SKEW: u8 = 1;
const STEP: u64 = 30;

/// A freshly generated base32 secret — what we store in `users.totp_secret`.
pub fn generate_secret_base32() -> String {
    Secret::generate_secret().to_encoded().to_string()
}

fn totp(secret_base32: &str, account: &str) -> Result<TOTP, String> {
    let bytes = Secret::Encoded(secret_base32.to_string())
        .to_bytes()
        .map_err(|e| format!("decode totp secret: {e}"))?;
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        SKEW,
        STEP,
        bytes,
        Some(ISSUER.to_string()),
        account.to_string(),
    )
    .map_err(|e| format!("build totp: {e}"))
}

/// `otpauth://` URL for enrollment; the SPA renders it as a QR code.
pub fn otpauth_url(secret_base32: &str, account: &str) -> Result<String, String> {
    Ok(totp(secret_base32, account)?.get_url())
}

/// Verify a submitted code against the stored secret (±1 step window).
pub fn verify(secret_base32: &str, account: &str, code: &str) -> Result<bool, String> {
    totp(secret_base32, account)?
        .check_current(code)
        .map_err(|e| format!("totp check: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_roundtrips_and_current_code_verifies() {
        let secret = generate_secret_base32();
        assert!(!secret.is_empty());
        // A URL builds (secret decodes cleanly).
        let url = otpauth_url(&secret, "alice").unwrap();
        assert!(url.starts_with("otpauth://totp/"));
        assert!(url.contains("kanade"));
        // The code the same secret produces right now must verify.
        let bytes = Secret::Encoded(secret.clone()).to_bytes().unwrap();
        let t = TOTP::new(
            Algorithm::SHA1,
            DIGITS,
            SKEW,
            STEP,
            bytes,
            Some(ISSUER.to_string()),
            "alice".to_string(),
        )
        .unwrap();
        let code = t.generate_current().unwrap();
        assert!(verify(&secret, "alice", &code).unwrap());
        // A wrong code does not.
        assert!(!verify(&secret, "alice", "000000").unwrap());
    }
}
