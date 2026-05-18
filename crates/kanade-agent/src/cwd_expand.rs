//! v0.21.2: agent-side expansion of `Execute.cwd`.
//!
//! Two transformations applied in order, both rooted in the same
//! Win32 token so they respect `run_as`:
//!
//! 1. **Tilde**. A leading `~`, `~\`, or `~/` becomes the token's
//!    user profile directory via `GetUserProfileDirectoryW`. For
//!    `run_as: user` / `system_gui` that's the logged-in user's
//!    `C:\Users\<name>`; for `run_as: system` it's LocalSystem's
//!    `C:\Windows\System32\config\systemprofile` (rarely useful;
//!    operators usually shouldn't write `~` with `system`).
//! 2. **`%FOO%` env-var expansion** via `ExpandEnvironmentStringsForUserW`,
//!    which looks up variables in the token's per-user environment
//!    block. So `%USERPROFILE%`, `%APPDATA%`, `%PROGRAMDATA%`, etc.
//!    all resolve to the right values for the spawning identity.
//!
//! PowerShell's `$env:FOO` syntax is intentionally **not** supported
//! — it would require shipping a PowerShell parser. `%FOO%` is the
//! Windows-native form and covers the same use cases.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::HANDLE;

/// Expand `~` + `%FOO%` against the given token. Returns the raw
/// string unchanged when neither prefix applies.
pub fn expand(raw: &str, token: HANDLE) -> Result<String> {
    let after_tilde = expand_tilde(raw, token)?;
    expand_env(&after_tilde, token)
}

/// Pure tilde-prefix handling (substring math). Public so the unit
/// test can call it with a pre-resolved `profile` instead of going
/// through Win32.
fn split_tilde(raw: &str) -> Option<&str> {
    if let Some(rest) = raw.strip_prefix("~\\") {
        Some(rest)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Some(rest)
    } else if raw == "~" {
        Some("")
    } else {
        None
    }
}

fn expand_tilde(raw: &str, token: HANDLE) -> Result<String> {
    let Some(rest) = split_tilde(raw) else {
        return Ok(raw.to_string());
    };
    let profile = get_user_profile_dir(token)?;
    if rest.is_empty() {
        Ok(profile)
    } else {
        let sep = if profile.ends_with(['\\', '/']) {
            ""
        } else {
            "\\"
        };
        Ok(format!("{profile}{sep}{rest}"))
    }
}

fn get_user_profile_dir(token: HANDLE) -> Result<String> {
    // Declare the binding ourselves so we don't need to figure out
    // which `Win32_*` feature ships GetUserProfileDirectoryW — it
    // moves around between windows-rs versions. Link directly
    // against userenv.dll, the OS-stable name.
    #[link(name = "userenv")]
    unsafe extern "system" {
        fn GetUserProfileDirectoryW(token: HANDLE, buf: *mut u16, len: *mut u32) -> i32;
    }
    unsafe {
        let mut len: u32 = 0;
        // First call probes required buffer size; expected to return
        // FALSE with ERROR_INSUFFICIENT_BUFFER and the right `len`.
        let _ = GetUserProfileDirectoryW(token, std::ptr::null_mut(), &mut len);
        if len == 0 {
            return Err(anyhow!("GetUserProfileDirectoryW size probe returned 0"));
        }
        let mut buf: Vec<u16> = vec![0u16; len as usize];
        let ok = GetUserProfileDirectoryW(token, buf.as_mut_ptr(), &mut len);
        if ok == 0 {
            return Err(anyhow!(
                "GetUserProfileDirectoryW failed: Win32 err {:?}",
                std::io::Error::last_os_error()
            ));
        }
        // Trim trailing NUL — len includes it.
        let trimmed = &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())];
        String::from_utf16(trimmed).context("decode profile path")
    }
}

fn expand_env(raw: &str, token: HANDLE) -> Result<String> {
    if !raw.contains('%') {
        // Fast-path: nothing to expand. Saves the Win32 call for the
        // common case where operators write absolute paths.
        return Ok(raw.to_string());
    }
    #[link(name = "userenv")]
    unsafe extern "system" {
        fn ExpandEnvironmentStringsForUserW(
            token: HANDLE,
            src: *const u16,
            dst: *mut u16,
            count: u32,
        ) -> i32;
    }
    let mut src: Vec<u16> = raw.encode_utf16().collect();
    src.push(0);
    // Start with a sensible buffer; if too small, retry once with
    // the exact size the API tells us. Capped at 32K to defang a
    // pathological env that's all expansions.
    let mut buf: Vec<u16> = vec![0u16; 512];
    unsafe {
        let ok = ExpandEnvironmentStringsForUserW(
            token,
            src.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        );
        if ok == 0 {
            return Err(anyhow!(
                "ExpandEnvironmentStringsForUserW failed: Win32 err {:?}",
                std::io::Error::last_os_error()
            ));
        }
        // The return value isn't a size on success; instead a NUL
        // terminator in `buf` marks end-of-string. If the buffer was
        // too small the API writes a truncated result without the
        // NUL — we double the buffer once and retry.
        if !buf.contains(&0) {
            let mut bigger: Vec<u16> = vec![0u16; 32 * 1024];
            let ok2 = ExpandEnvironmentStringsForUserW(
                token,
                src.as_ptr(),
                bigger.as_mut_ptr(),
                bigger.len() as u32,
            );
            if ok2 == 0 {
                return Err(anyhow!(
                    "ExpandEnvironmentStringsForUserW (retry) failed: Win32 err {:?}",
                    std::io::Error::last_os_error()
                ));
            }
            buf = bigger;
        }
        let trimmed = &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())];
        String::from_utf16(trimmed).context("decode expanded path")
    }
}

/// Open the agent's own process token. Used by the `run_as: system`
/// (tokio::process) path — no need for WTSQueryUserToken there since
/// the child inherits the agent's identity.
pub fn open_self_token() -> Result<SelfToken> {
    use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut tok)
            .map_err(|e| anyhow!("OpenProcessToken (self) failed: {e:?}"))?;
        Ok(SelfToken(tok))
    }
}

/// RAII wrapper for the agent's own process token.
pub struct SelfToken(HANDLE);
impl SelfToken {
    pub fn handle(&self) -> HANDLE {
        self.0
    }
}
impl Drop for SelfToken {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::Foundation::CloseHandle;
            if !self.0.is_invalid() {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stand-alone version of expand_tilde that doesn't touch Win32 —
    // mirror of the prefix logic for boundary testing.
    fn join_with_profile(raw: &str, profile: &str) -> Option<String> {
        let rest = split_tilde(raw)?;
        if rest.is_empty() {
            return Some(profile.to_string());
        }
        let sep = if profile.ends_with(['\\', '/']) {
            ""
        } else {
            "\\"
        };
        Some(format!("{profile}{sep}{rest}"))
    }

    #[test]
    fn tilde_alone_resolves_to_profile() {
        assert_eq!(
            join_with_profile("~", r"C:\Users\op").as_deref(),
            Some(r"C:\Users\op"),
        );
    }

    #[test]
    fn tilde_backslash_subpath() {
        assert_eq!(
            join_with_profile(r"~\src\zandaka", r"C:\Users\op").as_deref(),
            Some(r"C:\Users\op\src\zandaka"),
        );
    }

    #[test]
    fn tilde_forward_slash_subpath() {
        assert_eq!(
            join_with_profile("~/src/zandaka", r"C:\Users\op").as_deref(),
            Some(r"C:\Users\op\src/zandaka"),
        );
    }

    #[test]
    fn no_tilde_passes_through_unchanged() {
        assert!(split_tilde(r"C:\Users\op").is_none());
        assert!(split_tilde("%USERPROFILE%").is_none());
        assert!(split_tilde("./relative").is_none());
    }

    #[test]
    fn tilde_mid_path_is_not_split() {
        // Only the leading prefix is special; `foo~bar` is just a name.
        assert!(split_tilde("foo~bar").is_none());
        assert!(split_tilde(r"C:\Users\~name").is_none());
    }

    #[test]
    fn profile_with_trailing_separator_doesnt_double() {
        assert_eq!(
            join_with_profile(r"~\sub", r"C:\Users\op\").as_deref(),
            Some(r"C:\Users\op\sub"),
        );
    }
}
