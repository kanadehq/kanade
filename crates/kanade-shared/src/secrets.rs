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

/// Like [`read_hklm_value`], but separating **absent** from **unreadable**.
///
/// `read_hklm_value` collapses the two into `None`, which is fine for a value
/// consulted once at startup: either way there is nothing to use. It is not
/// fine for one re-read on a schedule, where "absent" means *adopt an empty
/// value* — a transient failure would then replace whatever is loaded with
/// nothing, on every poll, forever. The command keyring is read that way
/// (#1165), and there "adopt nothing" silently turns verification off.
///
/// `Ok(None)` is reserved for the cases that genuinely mean absent: the subkey
/// or value does not exist, the value is empty, or this is not Windows and
/// there is no registry to consult.
#[cfg(windows)]
pub fn try_read_hklm_value(subkey: &str, value: &str) -> Result<Option<String>, String> {
    use std::io::ErrorKind;
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = match hklm.open_subkey(subkey) {
        Ok(k) => k,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("open HKLM\\{subkey}: {e}")),
    };
    match key.get_value::<String, _>(value) {
        Ok(s) if s.is_empty() => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read HKLM\\{subkey}\\{value}: {e}")),
    }
}

#[cfg(not(windows))]
pub fn try_read_hklm_value(_subkey: &str, _value: &str) -> Result<Option<String>, String> {
    // Genuinely absent rather than a failure: there is no registry here, so
    // "nothing is provisioned" is the truthful answer and an agent on this
    // platform simply holds no keys.
    Ok(None)
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
