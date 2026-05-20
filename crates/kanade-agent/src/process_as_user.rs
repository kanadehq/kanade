//! Windows-only spawn path for `run_as: user` / `run_as: system_gui`
//! (v0.21). The default `system` path stays on tokio::process — only
//! when the operator opts in does the agent walk the WTS token dance
//! below.
//!
//! Flow:
//!
//! 1. Pick the active console session via `WTSGetActiveConsoleSessionId`.
//!    No console user → fail fast with a clear error.
//! 2. Acquire a primary token:
//!    * `RunAs::User` → `WTSQueryUserToken(session)` — the user's
//!      filtered (UAC-LIM when they're admin) primary token.
//!    * `RunAs::SystemGui` → `OpenProcessToken(GetCurrentProcess())`
//!      (= the agent's LocalSystem token) → `DuplicateTokenEx` →
//!      `SetTokenInformation(TokenSessionId, ...)` so the spawned
//!      process lives in the user's session while keeping SYSTEM
//!      privileges. PsExec `-i -s` pattern.
//! 3. Create stdout/stderr pipes with the child-end inheritable.
//! 4. Build a user-environment block via `CreateEnvironmentBlock`.
//! 5. `CreateProcessAsUserW` with the token + STARTUPINFO carrying
//!    the pipe handles.
//! 6. Close the parent's copy of the child-end handles so EOF
//!    propagates when the child exits.
//! 7. Read pipes on dedicated blocking threads (Win32 anonymous
//!    pipes don't support overlapped I/O).
//! 8. Wait via `WaitForSingleObject(INFINITE)` on yet another
//!    blocking thread; the outer async layer races it against the
//!    kill receiver + timeout, calling `TerminateProcess` on either.
//!
//! Every `unsafe` is concentrated here so the rest of the agent
//! stays plain Rust.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use kanade_shared::wire::{Command, RunAs, Shell};
use tokio::sync::oneshot;
use tracing::{info, warn};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;

use crate::process::ExecOutcome;

pub async fn run_command_in_user_session(
    cmd: &Command,
    run_as: RunAs,
    timeout: Duration,
    mut kill: oneshot::Receiver<()>,
) -> Result<ExecOutcome> {
    debug_assert!(matches!(run_as, RunAs::User | RunAs::SystemGui));

    let cmd_line = build_command_line(cmd);
    let cwd = cmd.cwd.clone();
    // 1) Spawn on a blocking thread (Win32 dance is sync).
    let SpawnHandles {
        process,
        stdout_read,
        stderr_read,
    } = tokio::task::spawn_blocking(move || spawn_native(&cmd_line, run_as, cwd.as_deref()))
        .await
        .map_err(|e| anyhow!("spawn-blocking join: {e}"))??;
    let process = Arc::new(process);

    // 2) Pipe drain on dedicated threads (anonymous pipes are
    //    blocking-only on Windows).
    let stdout_task = tokio::task::spawn_blocking(move || read_to_string(stdout_read));
    let stderr_task = tokio::task::spawn_blocking(move || read_to_string(stderr_read));

    // 3) Wait for completion. Block on WaitForSingleObject in a
    //    dedicated thread; the outer select! races it against kill
    //    + timeout and calls TerminateProcess on either path.
    let process_for_wait = process.clone();
    let mut wait = tokio::task::spawn_blocking(move || wait_native(process_for_wait.raw()));

    let wait_outcome: WaitOutcome = tokio::select! {
        biased;
        _ = &mut kill => {
            info!(target: "kanade_agent::process_as_user", "kill arm fired — TerminateProcess");
            terminate(process.raw());
            // wait should return imminently; if not we still record Killed.
            let _ = (&mut wait).await;
            WaitOutcome::Killed
        }
        _ = tokio::time::sleep(timeout) => {
            info!(target: "kanade_agent::process_as_user", "timeout arm fired — TerminateProcess");
            terminate(process.raw());
            let _ = (&mut wait).await;
            WaitOutcome::Timeout
        }
        res = &mut wait => {
            res.map_err(|e| anyhow!("wait spawn-blocking join: {e}"))??
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|e| anyhow!("stdout join: {e}"))?
        .unwrap_or_default();
    let stderr = stderr_task
        .await
        .map_err(|e| anyhow!("stderr join: {e}"))?
        .unwrap_or_default();

    Ok(match wait_outcome {
        WaitOutcome::Completed(code) => ExecOutcome::Completed {
            exit_code: code,
            stdout,
            stderr,
        },
        WaitOutcome::Killed => ExecOutcome::Killed { stdout, stderr },
        WaitOutcome::Timeout => ExecOutcome::Timeout { stdout, stderr },
    })
}

// ── Internal types ────────────────────────────────────────────────

struct SpawnHandles {
    process: SafeHandle,
    stdout_read: OwnedHandle,
    stderr_read: OwnedHandle,
}

enum WaitOutcome {
    Completed(i32),
    Killed,
    Timeout,
}

/// RAII HANDLE that CloseHandle's on drop. Use only for Win32
/// process / token handles — pipe handles go through `OwnedHandle`
/// (std-compat) so we can write to them as files.
struct SafeHandle(HANDLE);
impl SafeHandle {
    fn new(h: HANDLE) -> Self {
        Self(h)
    }
    fn raw(&self) -> HANDLE {
        self.0
    }
}
impl Drop for SafeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
            self.0 = INVALID_HANDLE_VALUE;
        }
    }
}
// SAFETY: HANDLE is a kernel-table index, safe to use across threads.
// SafeHandle wraps it with an exclusive owner via Arc / Drop, so
// double-close is impossible.
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

// ── Synchronous Win32 building blocks ─────────────────────────────

fn spawn_native(cmd_line: &[u16], run_as: RunAs, cwd: Option<&str>) -> Result<SpawnHandles> {
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            bail!("no active console session — run_as: user / system_gui needs a logged-in user");
        }

        let token = acquire_token(run_as, session)?;

        let (stdout_read, stdout_write) = make_inheritable_pipe()?;
        let (stderr_read, stderr_write) = make_inheritable_pipe()?;

        let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env_block, Some(token.raw()), false).is_ok();
        if !env_ok {
            warn!(
                target: "kanade_agent::process_as_user",
                "CreateEnvironmentBlock failed; child inherits the agent's env",
            );
        }
        let env_guard = EnvBlockGuard(if env_ok {
            env_block
        } else {
            std::ptr::null_mut()
        });

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = HANDLE(stdout_write.as_raw_handle_value());
        si.hStdError = HANDLE(stderr_write.as_raw_handle_value());
        si.hStdInput = HANDLE::default();

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut cmd_buf: Vec<u16> = cmd_line.to_vec();
        let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW;

        // CreateProcessAsUserW's lpCurrentDirectory wants a
        // NUL-terminated wide string or NULL (= inherit parent's cwd).
        // v0.21.2: expand `~` / `%FOO%` against the user's token so
        // operators can write `~\src\foo` and get `C:\Users\<user>\
        // src\foo`. Failure to expand falls back to the raw string
        // with a warning — better than refusing to spawn.
        let cwd_expanded: Option<String> = cwd.filter(|s| !s.is_empty()).map(|s| {
            match crate::cwd_expand::expand(s, token.raw()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        target: "kanade_agent::process_as_user",
                        error = %e,
                        raw_cwd = %s,
                        "cwd expansion failed; using raw value",
                    );
                    s.to_string()
                }
            }
        });
        let cwd_wide: Option<Vec<u16>> = cwd_expanded.as_deref().map(|s| {
            let mut v: Vec<u16> = s.encode_utf16().collect();
            v.push(0);
            v
        });
        let cwd_pwstr = match &cwd_wide {
            Some(v) => PWSTR(v.as_ptr() as *mut _),
            None => PWSTR::null(),
        };

        let result = CreateProcessAsUserW(
            Some(token.raw()),
            PWSTR::null(),
            Some(PWSTR(cmd_buf.as_mut_ptr())),
            None,
            None,
            true,
            flags,
            Some(env_guard.0 as *const _ as _),
            cwd_pwstr,
            &si,
            &mut pi,
        );
        if let Err(e) = result {
            bail!(
                "CreateProcessAsUserW failed: {e:?} (Win32 err {:?})",
                GetLastError(),
            );
        }
        let _ = CloseHandle(pi.hThread);

        // Drop parent's copy of child-end handles so the child's exit
        // closes the pipe and our ReadFile returns 0 bytes (EOF).
        drop(stdout_write);
        drop(stderr_write);

        Ok(SpawnHandles {
            process: SafeHandle::new(pi.hProcess),
            stdout_read,
            stderr_read,
        })
    }
}

unsafe fn acquire_token(run_as: RunAs, session: u32) -> Result<SafeHandle> {
    unsafe {
        match run_as {
            RunAs::User => {
                let mut tok = HANDLE::default();
                WTSQueryUserToken(session, &mut tok).map_err(|e| {
                    anyhow!(
                        "WTSQueryUserToken(session={session}) failed: {e:?} — \
                     run_as: user usually needs the agent running as LocalSystem"
                    )
                })?;
                Ok(SafeHandle::new(tok))
            }
            RunAs::SystemGui => {
                let mut self_tok = HANDLE::default();
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE
                        | TOKEN_ASSIGN_PRIMARY
                        | TOKEN_QUERY
                        | TOKEN_ADJUST_DEFAULT
                        | TOKEN_ADJUST_SESSIONID
                        | TOKEN_ADJUST_PRIVILEGES,
                    &mut self_tok,
                )
                .map_err(|e| anyhow!("OpenProcessToken (self) failed: {e:?}"))?;
                let self_tok = SafeHandle::new(self_tok);

                let mut dup = HANDLE::default();
                DuplicateTokenEx(
                    self_tok.raw(),
                    TOKEN_ASSIGN_PRIMARY
                        | TOKEN_DUPLICATE
                        | TOKEN_QUERY
                        | TOKEN_ADJUST_DEFAULT
                        | TOKEN_ADJUST_SESSIONID
                        | TOKEN_ADJUST_PRIVILEGES,
                    None,
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut dup,
                )
                .map_err(|e| anyhow!("DuplicateTokenEx failed: {e:?}"))?;
                let dup = SafeHandle::new(dup);

                let session_arg = session;
                SetTokenInformation(
                    dup.raw(),
                    TokenSessionId,
                    &session_arg as *const _ as _,
                    std::mem::size_of::<u32>() as u32,
                )
                .map_err(|e| {
                    anyhow!(
                        "SetTokenInformation(TokenSessionId={session_arg}) failed: {e:?} — \
                     run_as: system_gui needs LocalSystem privileges (SE_TCB_NAME), \
                     which the agent only has when running as the prod KanadeAgent service"
                    )
                })?;
                Ok(dup)
            }
            RunAs::System => unreachable!("System variant should never reach this module"),
        }
    }
}

fn make_inheritable_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    unsafe {
        let mut sa: SECURITY_ATTRIBUTES = std::mem::zeroed();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = true.into();
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        CreatePipe(&mut read, &mut write, Some(&sa), 0)
            .map_err(|e| anyhow!("CreatePipe failed: {e:?}"))?;
        Ok((
            OwnedHandle::from_raw_handle(read.0 as _),
            OwnedHandle::from_raw_handle(write.0 as _),
        ))
    }
}

trait HandleAsRaw {
    fn as_raw_handle_value(&self) -> *mut core::ffi::c_void;
}
impl HandleAsRaw for OwnedHandle {
    fn as_raw_handle_value(&self) -> *mut core::ffi::c_void {
        use std::os::windows::io::AsRawHandle;
        self.as_raw_handle()
    }
}

fn read_to_string(handle: OwnedHandle) -> Option<String> {
    let mut buf = Vec::<u8>::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let raw = handle.as_raw_handle_value();
    loop {
        let mut read: u32 = 0;
        let ok = unsafe { ReadFile(HANDLE(raw), Some(&mut chunk), Some(&mut read), None) };
        if ok.is_err() {
            // ERROR_BROKEN_PIPE on EOF is normal; anything else, stop.
            break;
        }
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn wait_native(process: HANDLE) -> Result<WaitOutcome> {
    unsafe {
        let r = WaitForSingleObject(process, INFINITE);
        if r == WAIT_OBJECT_0 {
            let mut code: u32 = 0;
            GetExitCodeProcess(process, &mut code)
                .map_err(|e| anyhow!("GetExitCodeProcess failed: {e:?}"))?;
            Ok(WaitOutcome::Completed(code as i32))
        } else {
            Err(anyhow!(
                "WaitForSingleObject returned {r:?} (Win32 err {:?})",
                GetLastError()
            ))
        }
    }
}

fn terminate(process: HANDLE) {
    unsafe {
        if let Err(e) = TerminateProcess(process, 1) {
            warn!(
                target: "kanade_agent::process_as_user",
                "TerminateProcess failed: {e:?}",
            );
        }
    }
}

struct EnvBlockGuard(*mut core::ffi::c_void);
impl Drop for EnvBlockGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }
}

fn build_command_line(cmd: &Command) -> Vec<u16> {
    // #43: mirror the System-path UTF-8 prelude (process.rs) so a
    // `run_as: user` PowerShell job sees the same console encoding
    // contract as `run_as: system`. Without this, ja-JP / DE / KR
    // / CN users hitting the user-session branch would still get
    // CP932 / OEM bytes on stdout — and although our reader is
    // already lossy here (the `read_to_string` helper does
    // `from_utf8_lossy` since v0.21), the JSON inventory output
    // becomes mojibake instead of clean text. Shared helper from
    // `crate::process` so both spawn paths use the same prelude
    // shape (CodeRabbit #83 nitpick: avoid duplicated literal).
    let ps_script;
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => {
            ps_script = crate::process::with_powershell_utf8_prelude(&cmd.script);
            (
                "powershell.exe",
                vec!["-NoProfile", "-NonInteractive", "-Command", &ps_script],
            )
        }
        Shell::Cmd => ("cmd.exe", vec!["/C", &cmd.script]),
    };
    let mut full = OsString::from(program);
    for a in args {
        full.push(" ");
        full.push("\"");
        let escaped = a.replace('"', "\\\"");
        full.push(&escaped);
        full.push("\"");
    }
    let mut wide: Vec<u16> = full.encode_wide().collect();
    wide.push(0);
    wide
}
