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

/// Write a `REG_SZ` value into an **existing** `HKLM\<subkey>`.
///
/// Deliberately opens rather than creates. Registry ACLs are per-key, and the
/// deploy scripts are what harden `HKLM\SOFTWARE\kanade\*` to SYSTEM +
/// Administrators. Creating a missing key here would produce an unhardened one
/// and leave whatever secret is being written readable by any logged-in user —
/// the exact thing this module exists to prevent. A missing key means the host
/// was never deployed properly, which is worth failing loudly over.
#[cfg(windows)]
pub fn write_hklm_value(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_WRITE};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(subkey, KEY_WRITE)
        .map_err(|e| match e.kind() {
            // The two failures need opposite responses, so they must not share
            // a message. Asserting "not present, deploy this host" at an
            // operator whose real problem is an unelevated shell sends them
            // re-running a deploy that was already fine.
            std::io::ErrorKind::NotFound => format!(
                "HKLM\\{subkey} is not present — refusing to create it. A key created here \
                 would not carry the SYSTEM+Administrators ACL the deploy scripts apply, \
                 leaving the value readable by any logged-in user. Deploy this host first."
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "HKLM\\{subkey} exists but could not be opened for writing: {e}. It is \
                 restricted to SYSTEM + Administrators — run this elevated."
            ),
            _ => format!("HKLM\\{subkey} could not be opened for writing: {e}"),
        })?;
    key.set_value(value, &data.to_string())
        .map_err(|e| format!("writing HKLM\\{subkey}\\{value}: {e}"))
}

#[cfg(not(windows))]
pub fn write_hklm_value(_subkey: &str, _value: &str, _data: &str) -> Result<(), String> {
    Err("registry secrets are Windows-only".to_string())
}
