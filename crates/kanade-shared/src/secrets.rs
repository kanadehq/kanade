//! Registry-backed secret store for production credentials.
//!
//! Windows services run as LocalSystem and inherit Machine-scope env
//! vars, but those vars are readable by any logged-in user. Storing
//! the credential under HKLM with a hardened ACL (SYSTEM +
//! Administrators only) keeps it out of low-privilege reach.
//!
//! Layout in use across kanade:
//!
//! ```text
//! HKLM\SOFTWARE\kanade\
//!   agent\
//!     NatsToken      — shared NATS bearer token (agent + backend + CLI)
//!   backend\
//!     StaticToken    — KANADE_AUTH_STATIC_TOKEN counterpart
//!     JwtSecret      — KANADE_JWT_SECRET counterpart
//!     MailPassword   — KANADE_MAIL_PASSWORD counterpart (SMTP AUTH)
//! ```
//!
//! `deploy-agent.ps1` / `deploy-backend.ps1` provision these keys and
//! apply the ACL. Non-Windows builds get an empty stub so the
//! workspace still cross-compiles for the CLI's Linux / macOS release
//! artifacts.

/// Read a `REG_SZ` value from `HKLM\<subkey>` and return it when
/// non-empty. Returns `None` for missing keys, missing values, empty
/// strings, or non-Windows targets.
#[cfg(windows)]
pub fn read_hklm_value(subkey: &str, value: &str) -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(subkey).ok()?;
    let s: String = key.get_value(value).ok()?;
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(not(windows))]
pub fn read_hklm_value(_subkey: &str, _value: &str) -> Option<String> {
    None
}
