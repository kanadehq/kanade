//! KLP peer authentication (SPEC §2.12.4).
//!
//! Windows: walk the OS handle chain — Pipe →
//! `GetNamedPipeClientProcessId` → `OpenProcess`
//! (with the minimum `PROCESS_QUERY_LIMITED_INFORMATION` right) →
//! `OpenProcessToken` → `GetTokenInformation` for the user SID and
//! console session id. The SID-derived identity is the
//! *authoritative* answer per SPEC §2.12.4; we never trust
//! user-supplied identity fields in the payload, even on a
//! single-user PC.

use anyhow::Result;

use tokio::net::windows::named_pipe::NamedPipeServer;

use super::PeerIdentity;

/// Resolve the peer's identity for a freshly-accepted KLP
/// connection. Synchronous Win32 calls; small enough to inline at
/// connect time rather than `spawn_blocking`.
pub fn resolve_peer(pipe: &NamedPipeServer) -> Result<PeerIdentity> {
    windows_impl::resolve(pipe)
}

mod windows_impl {
    use super::PeerIdentity;
    use anyhow::{Context, Result, bail};
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::NamedPipeServer;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, PSID, SidTypeUser, TOKEN_QUERY, TOKEN_USER,
        TokenSessionId, TokenUser,
    };
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::core::PWSTR;

    pub(super) fn resolve(pipe: &NamedPipeServer) -> Result<PeerIdentity> {
        let pipe_handle = HANDLE(pipe.as_raw_handle());

        let mut pid: u32 = 0;
        unsafe {
            GetNamedPipeClientProcessId(pipe_handle, &mut pid)
                .context("GetNamedPipeClientProcessId failed")?;
        }

        // PROCESS_QUERY_LIMITED_INFORMATION is the minimum right
        // needed for OpenProcessToken across an arbitrary peer
        // process. The agent runs as LocalSystem so this succeeds.
        let proc_handle = unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                .with_context(|| format!("OpenProcess(pid={pid}) failed"))?
        };

        let creds = read_token_info(proc_handle, pid);

        // Always close, even on error path. windows-rs doesn't
        // ship an RAII handle wrapper for HANDLE.
        unsafe {
            let _ = CloseHandle(proc_handle);
        }
        creds
    }

    fn read_token_info(proc_handle: HANDLE, pid: u32) -> Result<PeerIdentity> {
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(proc_handle, TOKEN_QUERY, &mut token)
                .context("OpenProcessToken failed")?;
        }

        let result = read_user_and_session(token, pid);

        unsafe {
            let _ = CloseHandle(token);
        }
        result
    }

    fn read_user_and_session(token: HANDLE, pid: u32) -> Result<PeerIdentity> {
        // TokenSessionId: fixed 4-byte u32, no two-call pattern.
        let mut session_id: u32 = 0;
        let mut returned: u32 = 0;
        unsafe {
            GetTokenInformation(
                token,
                TokenSessionId,
                Some(&mut session_id as *mut u32 as *mut c_void),
                std::mem::size_of::<u32>() as u32,
                &mut returned,
            )
            .context("GetTokenInformation(TokenSessionId) failed")?;
        }

        // TokenUser: TOKEN_USER struct + trailing SID bytes, so
        // variable-size. Standard Win32 two-call pattern — first
        // call returns ERROR_INSUFFICIENT_BUFFER but populates
        // `needed`; second call fills the buffer.
        let mut needed: u32 = 0;
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        }
        if needed == 0 {
            bail!("GetTokenInformation(TokenUser) returned zero size");
        }
        let mut buf = vec![0u8; needed as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut c_void),
                needed,
                &mut needed,
            )
            .context("GetTokenInformation(TokenUser) failed")?;
        }

        let (user, user_sid) = unsafe {
            // `buf` is a `Vec<u8>` with 1-byte alignment, but
            // `TOKEN_USER` wants pointer alignment (8 bytes on
            // x64). A `&*(buf.as_ptr() as *const TOKEN_USER)`
            // would be UB per the Rust reference even though the
            // OS allocator usually returns a sufficiently-aligned
            // backing buffer in practice. `read_unaligned` copies
            // the struct onto the stack without an alignment
            // assumption, so we get a well-aligned local.
            //
            // SAFETY: the embedded `PSID` still points into
            // `buf`, which outlives both lookups below.
            let token_user: TOKEN_USER = std::ptr::read_unaligned(buf.as_ptr().cast());
            let sid = token_user.User.Sid;
            // Both resolutions are best-effort with a `"<unknown>"`
            // fallback so a connection that only uses state/jobs still
            // succeeds even on a pathological token. The Phase E ack
            // handler rejects an `"<unknown>"` SID rather than writing a
            // colliding KV row.
            let user = lookup_friendly(sid).unwrap_or_else(|_| "<unknown>".into());
            let user_sid = stringify_sid(sid).unwrap_or_else(|_| "<unknown>".into());
            (user, user_sid)
        };

        Ok(PeerIdentity {
            user_key: user_sid,
            user_display: user,
            pid,
            session: Some(session_id),
        })
    }

    /// `ConvertSidToStringSidW` → canonical `S-1-5-21-…` string. The
    /// SID string is the stable per-user identity KLP keys ack state on
    /// (SPEC §2.12.4) — the friendly name can be renamed, the SID can't.
    ///
    /// # Safety
    ///
    /// `sid` must point to a valid SID owned by the caller, valid for
    /// the duration of the call. The caller in
    /// [`read_user_and_session`] holds the owning buffer (`buf`).
    unsafe fn stringify_sid(sid: PSID) -> Result<String> {
        unsafe {
            // ConvertSidToStringSidW LocalAlloc's the output string and
            // hands back a pointer we must LocalFree.
            let mut raw = PWSTR::null();
            ConvertSidToStringSidW(sid, &mut raw).context("ConvertSidToStringSidW failed")?;
            if raw.is_null() {
                bail!("ConvertSidToStringSidW returned a null string");
            }
            // Copy out before freeing; capture the result so the buffer
            // is released on both the ok and err paths.
            let owned = raw.to_string().context("SID string was not valid UTF-16");
            let _ = LocalFree(Some(HLOCAL(raw.as_ptr() as *mut c_void)));
            owned
        }
    }

    /// `LookupAccountSidW` → `DOMAIN\\user`. Two-call sizing
    /// pattern matches `GetTokenInformation` above.
    ///
    /// # Safety
    ///
    /// `sid` must point to a valid SID owned by the caller and
    /// remain valid for the duration of both `LookupAccountSidW`
    /// calls. The caller in [`read_user_and_session`] holds the
    /// owning buffer (`buf`) for the full lifetime of this call.
    unsafe fn lookup_friendly(sid: PSID) -> Result<String> {
        unsafe {
            let mut name_len: u32 = 0;
            let mut domain_len: u32 = 0;
            let mut sid_use = SidTypeUser;
            // First call: discover required sizes. Expected to
            // return Err(ERROR_INSUFFICIENT_BUFFER) but the size
            // fields are populated regardless.
            let _ = LookupAccountSidW(
                None,
                sid,
                None,
                &mut name_len,
                None,
                &mut domain_len,
                &mut sid_use,
            );
            if name_len == 0 || domain_len == 0 {
                bail!("LookupAccountSidW returned zero sizes");
            }
            let mut name = vec![0u16; name_len as usize];
            let mut domain = vec![0u16; domain_len as usize];
            LookupAccountSidW(
                None,
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut sid_use,
            )
            .context("LookupAccountSidW (resize) failed")?;
            // Win32 documents that, on success, the size fields
            // count characters EXCLUDING the trailing nul. The
            // defensive trim_end_matches('\0') guards against
            // quirky DCs / orphaned accounts where a stray nul
            // slips through — cheap insurance, doesn't affect
            // the well-behaved path.
            let user = String::from_utf16_lossy(&name[..name_len as usize])
                .trim_end_matches('\0')
                .to_string();
            let domain = String::from_utf16_lossy(&domain[..domain_len as usize])
                .trim_end_matches('\0')
                .to_string();
            Ok(format!("{domain}\\{user}"))
        }
    }
}
