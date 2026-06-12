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
    CloseHandle, ERROR_BROKEN_PIPE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
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
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DETACHED_PROCESS, GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::PWSTR;

use crate::job_object::JobObject;
use crate::live_tail::LiveTail;
use crate::process::ExecOutcome;

pub async fn run_command_in_user_session(
    cmd: &Command,
    run_as: RunAs,
    timeout: Duration,
    mut kill: oneshot::Receiver<()>,
    live: Option<Arc<LiveTail>>,
) -> Result<ExecOutcome> {
    debug_assert!(matches!(run_as, RunAs::User | RunAs::SystemGui));

    let (cmd_line, _launch) = build_command_line(cmd)?;
    let cwd = cmd.cwd.clone();
    // 1) Spawn on a blocking thread (Win32 dance is sync).
    //    `_launch` (if any) stays in scope here until this fn
    //    returns; PowerShell parses both staged files at spawn
    //    time so they only need to live across `spawn_native`.
    let SpawnHandles {
        process,
        job,
        stdout_read,
        stderr_read,
    } = tokio::task::spawn_blocking(move || spawn_native(&cmd_line, run_as, cwd.as_deref()))
        .await
        .map_err(|e| anyhow!("spawn-blocking join: {e}"))??;
    let process = Arc::new(process);

    // 2) Pipe drain on dedicated threads (anonymous pipes are
    //    blocking-only on Windows). Each chunk is mirrored to the
    //    live-tail ring (when present) as it's read, so the
    //    `job.tail.<pc_id>` handler can serve a running user-session
    //    job's output just like the default `system` path.
    let live_out = live.clone();
    let stdout_task =
        tokio::task::spawn_blocking(move || read_to_string(stdout_read, live_out, Stream::Stdout));
    let live_err = live.clone();
    let stderr_task =
        tokio::task::spawn_blocking(move || read_to_string(stderr_read, live_err, Stream::Stderr));

    // 3) Wait for completion. Block on WaitForSingleObject in a
    //    dedicated thread; the outer select! races it against kill
    //    + timeout and calls TerminateProcess on either path.
    let process_for_wait = process.clone();
    let mut wait = tokio::task::spawn_blocking(move || wait_native(process_for_wait.raw()));

    let wait_outcome: WaitOutcome = tokio::select! {
        biased;
        _ = &mut kill => {
            info!(target: "kanade_agent::process_as_user", "kill arm fired — terminating job tree");
            terminate_tree(&job, process.raw());
            // wait should return imminently; if not we still record Killed.
            let _ = (&mut wait).await;
            WaitOutcome::Killed
        }
        _ = tokio::time::sleep(timeout) => {
            info!(target: "kanade_agent::process_as_user", "timeout arm fired — terminating job tree");
            terminate_tree(&job, process.raw());
            let _ = (&mut wait).await;
            WaitOutcome::Timeout
        }
        res = &mut wait => {
            res.map_err(|e| anyhow!("wait spawn-blocking join: {e}"))??
        }
    };

    // Mirror the System path (`process.rs`): keep whatever bytes we
    // read and surface a non-EOF capture failure via warn + a stderr
    // marker, instead of dropping it silently.
    let (stdout, stdout_err) = stdout_task.await.map_err(|e| anyhow!("stdout join: {e}"))?;
    if let Some(e) = stdout_err {
        warn!(error = %e, "stdout capture failed (kept partial)");
    }
    let (mut stderr, stderr_err) = stderr_task.await.map_err(|e| anyhow!("stderr join: {e}"))?;
    if let Some(e) = stderr_err {
        warn!(error = %e, "stderr capture failed (kept partial)");
        stderr.push_str(&format!("\n[agent: stderr capture failed: {e}]\n"));
    }

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
    /// The Job the host + its descendants were assigned to. `None`
    /// when Job creation/assignment failed — the caller then falls
    /// back to single-process `TerminateProcess`.
    job: Option<JobObject>,
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
        // CREATE_SUSPENDED so we can assign the process to a Job
        // Object BEFORE it runs a single instruction — that makes the
        // Job capture race-free (no descendant can be spawned, and
        // thus escape the Job, until we ResumeThread below).
        let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED;

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

        // Assign the (still-suspended) process to a Job Object so a
        // later kill/timeout can terminate the whole tree at once. A
        // failure here degrades to single-process TerminateProcess
        // (job = None) rather than refusing to spawn — better a
        // narrower kill than no run at all.
        let job = match JobObject::assign_handle(pi.hProcess) {
            Ok(j) => Some(j),
            Err(e) => {
                warn!(
                    target: "kanade_agent::process_as_user",
                    error = %e,
                    "job object assign failed; kill falls back to single-process terminate",
                );
                None
            }
        };

        // Release the suspended main thread now that the Job is in
        // place. ResumeThread returns (DWORD)-1 on failure; if that
        // ever happens the child would be wedged suspended forever, so
        // tear down whatever we created and bail rather than leak a
        // frozen process + its pipes.
        if ResumeThread(pi.hThread) == u32::MAX {
            let err = GetLastError();
            let _ = CloseHandle(pi.hThread);
            if let Some(j) = &job {
                j.terminate();
            } else {
                let _ = TerminateProcess(pi.hProcess, 1);
            }
            let _ = CloseHandle(pi.hProcess);
            bail!("ResumeThread failed (Win32 err {err:?})");
        }
        let _ = CloseHandle(pi.hThread);

        // Drop parent's copy of child-end handles so the child's exit
        // closes the pipe and our ReadFile returns 0 bytes (EOF).
        drop(stdout_write);
        drop(stderr_write);

        Ok(SpawnHandles {
            process: SafeHandle::new(pi.hProcess),
            job,
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

/// Which captured stream a chunk belongs to — selects the live-tail
/// ring it gets mirrored into.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Drain a child pipe to a `String`, mirroring each chunk to the live
/// ring as it arrives. Returns `(decoded, partial_error)` with the same
/// contract as `process.rs::drain_to_string`: a non-EOF `ReadFile`
/// failure (broken child, partial read) keeps the bytes read so far and
/// surfaces the error so the caller can warn + annotate stderr, instead
/// of dropping it silently.
fn read_to_string(
    handle: OwnedHandle,
    live: Option<Arc<LiveTail>>,
    stream: Stream,
) -> (String, Option<anyhow::Error>) {
    let mut buf = Vec::<u8>::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut err: Option<anyhow::Error> = None;
    let raw = handle.as_raw_handle_value();
    loop {
        let mut read: u32 = 0;
        let ok = unsafe { ReadFile(HANDLE(raw), Some(&mut chunk), Some(&mut read), None) };
        if let Err(e) = ok {
            // ERROR_BROKEN_PIPE is the normal EOF once the child closes
            // its write end of the anonymous pipe — not a failure.
            // Anything else (e.g. a partial read on a crashed child) is
            // real: keep the partial capture and report it.
            let code = unsafe { GetLastError() };
            if code != ERROR_BROKEN_PIPE {
                err = Some(anyhow!("ReadFile failed: {e} (Win32 {code:?})"));
            }
            break;
        }
        if read == 0 {
            break;
        }
        let slice = &chunk[..read as usize];
        buf.extend_from_slice(slice);
        // Mirror to the live ring as the bytes arrive (the final
        // `from_utf8_lossy` of the whole buffer below is unchanged).
        if let Some(lt) = &live {
            match stream {
                Stream::Stdout => lt.push_stdout(slice),
                Stream::Stderr => lt.push_stderr(slice),
            }
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), err)
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

/// Kill the child on kill/timeout. Prefers the Job Object (whole
/// tree — host + every descendant) so an orphaned grandchild can't
/// keep the stdout/stderr pipes open and wedge the drain. Falls back
/// to single-process `TerminateProcess` only when no Job was assigned.
fn terminate_tree(job: &Option<JobObject>, process: HANDLE) {
    if let Some(j) = job {
        j.terminate();
    } else {
        terminate(process);
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

fn build_command_line(
    cmd: &Command,
) -> Result<(Vec<u16>, Option<crate::process::TempPowerShellLaunch>)> {
    // PowerShell launcher pattern (see
    // `crate::process::TempPowerShellLaunch` for the rationale):
    // - The launcher carries the UTF-8 console-encoding prelude
    //   (#43, ja-JP / DE / KR / CN users get clean stdout instead
    //   of CP932 / OEM bytes).
    // - The launcher invokes the user script via `&` so the user
    //   script's `[CmdletBinding()] / param(...)` headers stay at
    //   the top of their physical file (PowerShell rejects them
    //   anywhere else).
    // - Staging dir is `%ProgramData%/Kanade/agent-scripts-<uuid>/`
    //   which inherits "Users: Read & execute" — important here
    //   because the agent (LocalSystem) writes the file but the
    //   child runs as the user, and `C:\Windows\Temp` would block
    //   the child's read.
    let mut launch: Option<crate::process::TempPowerShellLaunch> = None;
    let path_owned: String;
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => {
            let staged = crate::process::TempPowerShellLaunch::stage(&cmd.script)?;
            path_owned = staged.launcher_path().to_string_lossy().into_owned();
            launch = Some(staged);
            (
                "powershell.exe",
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    path_owned.as_str(),
                ],
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
    Ok((wide, launch))
}

/// Launch a GUI process **detached** in the active console session, as
/// the logged-in user (Phase E emergency fallback, #102). Fire-and-forget:
/// no stdio pipes, no wait, no job object — the agent only needs to
/// *start* the Client App (which then shows its own toast and stays
/// hidden until clicked); it doesn't capture output or manage the
/// child's lifetime.
///
/// Mirrors the `RunAs::User` token path of [`run_command_in_user_session`]
/// but strips everything that path needs for capture / kill / timeout,
/// leaving the minimal "start it in the user's interactive session"
/// core. `DETACHED_PROCESS` + `lpDesktop = winsta0\\default` so the
/// child belongs to the interactive desktop (its window can appear when
/// the user clicks the toast) without the agent owning a console.
///
/// Errors when there's no console user or the spawn fails; the caller
/// logs and moves on (a missing fallback must not break anything).
pub(crate) fn launch_detached_in_user_session(
    exe: &std::path::Path,
    args: &[&str],
) -> anyhow::Result<()> {
    // SAFETY: every Win32 call is checked; the token / env block are
    // freed via RAII; the command-line and desktop buffers outlive the
    // CreateProcessAsUserW call; both returned handles are closed.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            bail!("no active console session (no logged-in user)");
        }
        let token = acquire_token(RunAs::User, session)?;

        // Mutable, NUL-terminated wide command line. Each token is quoted
        // and escaped per `CommandLineToArgvW` rules: a run of N
        // backslashes before a `"` becomes 2N+1 (so the quote is
        // literal), and a trailing run before the closing quote becomes
        // 2N (so a path like `C:\dir\` doesn't escape the quote). Our only
        // args today are an exe path + a UUID, but the helper is general.
        let mut cmd = String::new();
        push_quoted_arg(&mut cmd, &exe.to_string_lossy());
        for a in args {
            cmd.push(' ');
            push_quoted_arg(&mut cmd, a);
        }
        let mut cmd_buf: Vec<u16> = cmd.encode_utf16().collect();
        cmd_buf.push(0);

        let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env_block, Some(token.raw()), false).is_ok();
        if !env_ok {
            warn!(
                target: "kanade_agent::process_as_user",
                "CreateEnvironmentBlock failed; client inherits the agent's env",
            );
        }
        // Reuse the file-level RAII guard (identical to a local one).
        let env_guard = EnvBlockGuard(if env_ok {
            env_block
        } else {
            std::ptr::null_mut()
        });

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().collect();
        desktop.push(0);
        si.lpDesktop = PWSTR(desktop.as_mut_ptr());

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let flags = CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS;
        let env_ptr = if env_guard.0.is_null() {
            None
        } else {
            Some(env_guard.0 as *const _ as _)
        };

        CreateProcessAsUserW(
            Some(token.raw()),
            PWSTR::null(),
            Some(PWSTR(cmd_buf.as_mut_ptr())),
            None,
            None,
            false, // bInheritHandles = FALSE — no pipes to inherit.
            flags,
            env_ptr,
            PWSTR::null(), // inherit the agent's cwd (irrelevant for a GUI app)
            &si,
            &mut pi,
        )
        .map_err(|e| {
            anyhow!(
                "CreateProcessAsUserW(client) failed: {e:?} (Win32 {:?})",
                GetLastError(),
            )
        })?;

        // Fire-and-forget: drop both handles, never wait on the GUI.
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(())
    }
}

/// Append one argument to a Windows command line, quoted and escaped per
/// the `CommandLineToArgvW` rules: a run of N backslashes immediately
/// before a `"` is doubled and a `\` added (2N+1) so the quote is
/// literal, and a trailing run before the closing quote is doubled (2N)
/// so a value like `C:\dir\` doesn't escape it.
fn push_quoted_arg(cmd: &mut String, arg: &str) {
    cmd.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    cmd.push('\\');
                }
                cmd.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    cmd.push('\\');
                }
                cmd.push(c);
                backslashes = 0;
            }
        }
    }
    for _ in 0..backslashes * 2 {
        cmd.push('\\');
    }
    cmd.push('"');
}

#[cfg(test)]
mod launch_tests {
    use super::push_quoted_arg;

    fn quoted(arg: &str) -> String {
        let mut s = String::new();
        push_quoted_arg(&mut s, arg);
        s
    }

    #[test]
    fn plain_arg_is_just_quoted() {
        assert_eq!(quoted("--show-notification"), "\"--show-notification\"");
        assert_eq!(quoted("notif-9f3a"), "\"notif-9f3a\"");
    }

    #[test]
    fn trailing_backslashes_are_doubled() {
        // `C:\dir\` → `"C:\dir\\"` so the closing quote isn't escaped.
        assert_eq!(quoted(r"C:\dir\"), "\"C:\\dir\\\\\"");
    }

    #[test]
    fn embedded_quote_is_escaped() {
        // `a"b` → `"a\"b"`.
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        // `\"` → backslash then quote → `"\\\"" ` (2*1+1 = 3 backslashes).
        assert_eq!(quoted("\\\""), "\"\\\\\\\"\"");
    }
}
