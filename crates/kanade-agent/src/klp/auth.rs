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
//!
//! Unix (Linux/macOS): not implemented in this PR. SPEC §2.12.1
//! puts the Linux socket at `/run/kanade/agent.sock` and
//! §2.12.4 specifies `SO_PEERCRED`; the full implementation
//! lands with the UDS listener in a follow-up PR.

use anyhow::Result;

#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::NamedPipeServer;

/// What the auth shim derives from the OS at connect time. The
/// dispatcher pairs this with the agent's configured `pc_id` to
/// build the [`kanade_shared::ipc::handshake::HandshakeSession`]
/// returned to the client.
#[derive(Debug, Clone)]
pub struct PeerCredentials {
    /// `DOMAIN\\user` on Windows (or `"<unknown>"` when
    /// `LookupAccountSidW` finds no friendly name — orphaned
    /// account, deleted user, …). `username` on Unix once that
    /// path lands.
    pub user: String,
    /// Console / RDP session id (Windows) or UID (Unix). Session
    /// 0 is the services session; user-interactive sessions start
    /// at 1.
    pub session_id: u32,
}

/// Resolve the peer's identity for a freshly-accepted KLP
/// connection. Synchronous Win32 calls; small enough to inline at
/// connect time rather than `spawn_blocking`.
#[cfg(target_os = "windows")]
pub fn resolve_peer(pipe: &NamedPipeServer) -> Result<PeerCredentials> {
    windows_impl::resolve(pipe)
}

/// Non-Windows placeholder so the rest of the crate compiles on
/// CI's Linux/macOS runners. Returns [`anyhow::Error`] — the
/// listener itself is `#[cfg(target_os = "windows")]` gated, so
/// this function is never actually called on those targets today.
#[cfg(not(target_os = "windows"))]
pub fn resolve_peer<T>(_pipe: &T) -> Result<PeerCredentials> {
    anyhow::bail!("KLP peer auth on non-Windows targets is not yet implemented");
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::PeerCredentials;
    use anyhow::{Context, Result, bail};
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::NamedPipeServer;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, PSID, SidTypeUser, TOKEN_QUERY, TOKEN_USER,
        TokenSessionId, TokenUser,
    };
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::core::PWSTR;

    pub(super) fn resolve(pipe: &NamedPipeServer) -> Result<PeerCredentials> {
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

        let creds = read_token_info(proc_handle);

        // Always close, even on error path. windows-rs doesn't
        // ship an RAII handle wrapper for HANDLE.
        unsafe {
            let _ = CloseHandle(proc_handle);
        }
        creds
    }

    fn read_token_info(proc_handle: HANDLE) -> Result<PeerCredentials> {
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(proc_handle, TOKEN_QUERY, &mut token)
                .context("OpenProcessToken failed")?;
        }

        let result = read_user_and_session(token);

        unsafe {
            let _ = CloseHandle(token);
        }
        result
    }

    fn read_user_and_session(token: HANDLE) -> Result<PeerCredentials> {
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

        let user = unsafe {
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            // SAFETY: the SID lives inside `buf`, which outlives
            // the lookup call below.
            lookup_friendly(token_user.User.Sid).unwrap_or_else(|_| "<unknown>".into())
        };

        Ok(PeerCredentials { user, session_id })
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
            // count characters EXCLUDING the trailing nul.
            let user = String::from_utf16_lossy(&name[..name_len as usize]);
            let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
            Ok(format!("{domain}\\{user}"))
        }
    }
}
