use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::live_tail::LiveTail;

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::{Command, RunAs, Shell};
use rand::RngExt;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// #43: PowerShell console-encoding prelude. Lives in the
/// launcher script (see [`TempPowerShellLaunch`]) so the user
/// script's `[CmdletBinding()] / param(...)` headers stay at the
/// top of their own physical file (PowerShell rejects them
/// anywhere else). Both `run_as: System` and `run_as: user /
/// system_gui` paths route through the same launcher.
pub(crate) const POWERSHELL_UTF8_PRELUDE: &str = "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; \
     $OutputEncoding = [Console]::OutputEncoding; ";

/// Process-wide staging directory for temp `.ps1` files.
///
/// Layout: `%ProgramData%/Kanade/agent-scripts/<uuid>/` on Windows,
/// `$TMPDIR/kanade-agent-<uuid>/` elsewhere (dev / tests only —
/// non-Windows skips the static `agent-scripts/` segment because
/// `$TMPDIR` is world-writable and a predictable parent would
/// open up a symlink-redirect attack; see staging_dir for the
/// full rationale).
///
/// Hierarchy rationale: the static `agent-scripts/` segment is the
/// logical category (greppable / observable / single-command
/// cleanup with `rmdir agent-scripts\`); the per-process `<uuid>/`
/// subdir provides isolation. Bundling the two into one segment
/// (`agent-scripts-<uuid>/`) — as the first pass did — made
/// per-process dirs look like siblings of the category itself,
/// which is harder to scan visually and to clean up in bulk.
///
/// Why `%ProgramData%` and not `%TEMP%`: the agent runs as
/// LocalSystem; `%TEMP%` for SYSTEM is `C:\Windows\Temp` whose
/// default ACL does NOT grant Users read access to SYSTEM-created
/// files. That breaks `run_as: user / system_gui` (child runs as a
/// non-admin user and would get "access denied" reading the
/// staged script). `C:\ProgramData` propagates an inherited
/// "Users: Read & execute" ACE to files SYSTEM creates inside it,
/// which is exactly what the user-session child needs. Scripts
/// already travel over NATS where the operator can see them, so
/// local-read by other users on the box is an acceptable trade.
///
/// Security shape:
/// - The static parent (`Kanade/agent-scripts/`) is created with
///   `create_dir_all`; pre-existence is fine because we never
///   write directly to it — only ever to a UUID subdir.
/// - The per-process UUID subdir is created with `create_dir`
///   (non-clobber). An attacker can't pre-create it because the
///   UUID is unguessable.
/// - Per-file staging uses `OpenOptions::create_new` (Windows
///   `CREATE_NEW` / POSIX `O_EXCL`) — defence in depth against
///   the astronomically-unlikely UUID collision and against
///   anyone with write into the UUID dir (Users default ACL
///   doesn't grant write, so this is belt-and-braces).
///
/// Gotcha for script authors targeting `run_as: user`:
/// `$PSScriptRoot` (= this UUID dir) is **read-only** for the
/// child user — the default ProgramData ACL grants Users
/// "Read & Execute" but not Modify. A user script that does
/// `New-Item $PSScriptRoot\out.txt` or similar will get access
/// denied. Use `$env:TEMP` / `$env:LOCALAPPDATA` / an absolute
/// path under the user's profile instead. Even for `run_as:
/// System` (where SYSTEM can write here), the dir is cleaned up
/// on script exit, so writing siblings is fragile either way.
fn staging_dir() -> Result<PathBuf> {
    static SLOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().expect("staging_dir mutex poisoned");
    if let Some(p) = guard.as_ref() {
        return Ok(p.clone());
    }
    // Platforms diverge here for security reasons (Gemini PR #231
    // HIGH): the windows path has an admin-controlled category
    // parent under `%ProgramData%`, so we can safely nest a
    // grep-friendly static `agent-scripts/` segment in. On
    // non-Windows the natural parent is `$TMPDIR` (typically
    // `/tmp/`), which is world-writable; a predictable static
    // child like `/tmp/kanade-agent-scripts/` could be
    // pre-created by another local user as a symlink to (say)
    // `/etc/`, and a subsequent `create_dir_all` would follow it
    // and let us write files outside the intended tree. We mitigate
    // by skipping the static segment entirely on non-Windows —
    // the per-process UUID dir (created with `create_dir`,
    // non-clobber) sits directly under `$TMPDIR`, with an
    // unguessable name that can't be pre-empted.
    let dir = if cfg!(target_os = "windows") {
        let category = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("Kanade")
            .join("agent-scripts");
        std::fs::create_dir_all(&category)
            .with_context(|| format!("create_dir_all {}", category.display()))?;
        category.join(Uuid::new_v4().simple().to_string())
    } else {
        std::env::temp_dir().join(format!("kanade-agent-{}", Uuid::new_v4().simple()))
    };
    std::fs::create_dir(&dir).with_context(|| format!("create_dir {}", dir.display()))?;
    *guard = Some(dir.clone());
    Ok(dir)
}

/// A PowerShell script staged to a single temp `.ps1` file.
///
/// Used as a building block by [`TempPowerShellLaunch`]; not invoked
/// directly by the spawn path anymore (the spawn path stages a
/// launcher/user pair so user-script `[CmdletBinding()] / param(...)`
/// blocks stay at the top of their file).
///
/// Cleanup-on-drop: the file is removed when the struct goes out of
/// scope. PowerShell reads the script into memory at parse time, so
/// deleting the file mid-run doesn't affect the running process.
pub(crate) struct TempPowerShellScript {
    path: PathBuf,
}

impl TempPowerShellScript {
    /// Stage `body` to a fresh BOM-prefixed `.ps1` under the
    /// per-process [`staging_dir`]. Uses `create_new` semantics
    /// (Windows `CREATE_NEW` / POSIX `O_EXCL`) so an attacker can't
    /// substitute the file via a TOCTOU race against the UUID name.
    pub fn write(body: &str) -> Result<Self> {
        let dir = staging_dir()?;
        let path = dir.join(format!("kanade-{}.ps1", Uuid::new_v4().simple()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create_new {}", path.display()))?;
        // UTF-8 BOM (0xEF 0xBB 0xBF) — PowerShell uses it to detect
        // UTF-8 encoding without a `chcp 65001` dance. Without it,
        // a ja-JP host running default CP932 would mis-parse any
        // multi-byte sequence in the script body.
        f.write_all(&[0xEF, 0xBB, 0xBF])
            .with_context(|| format!("write BOM {}", path.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write body {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPowerShellScript {
    fn drop(&mut self) {
        // Best-effort. The staging dir itself is never removed
        // (process-lifetime, cleaned by Storage Sense / TEMP GC).
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A staged user script + launcher pair invoked as
/// `powershell -File <launcher>`.
///
/// Why a pair instead of one file: PowerShell only honors
/// `[CmdletBinding()] / param(...)` when they're at the **top** of
/// the script's physical file — prepending an encoding prelude to
/// the same file (the simpler approach) silently breaks any
/// operator-shipped `.ps1` that opens with those headers (e.g.
/// `scripts/deploy/backend.ps1`, surfaced by the
/// install-kanade-backend live test on 2026-05-26).
///
/// Solution: the launcher sets `[Console]::OutputEncoding` etc.
/// then calls the user script via `&` (call operator), which spawns
/// a fresh script scope where the user's `param(...)` block applies
/// to the user file's own arguments.
pub(crate) struct TempPowerShellLaunch {
    launcher: TempPowerShellScript,
    // Kept alive for the launcher's `-File` invocation. The
    // `_user` underscore marks it dead-code-wise; presence in the
    // struct is the entire point.
    _user: TempPowerShellScript,
}

impl TempPowerShellLaunch {
    pub fn stage(user_body: &str) -> Result<Self> {
        let user = TempPowerShellScript::write(user_body)?;
        // PowerShell single-quoted string literal — escape embedded
        // `'` by doubling. `to_string_lossy` instead of fallible
        // `to_str` so a TEMP path with non-UTF-8 surrogates (rare
        // on Windows but technically representable) still produces
        // a path we can hand to PowerShell.
        let user_path = user.path().to_string_lossy().replace('\'', "''");
        // No explicit `exit $LASTEXITCODE` propagation: that would
        // make us exit nonzero even when the user script HANDLED a
        // native command's failure (`$LASTEXITCODE` remains set
        // from the last native call, not the script's overall
        // status). PowerShell's default is to exit 0 unless the
        // user script itself calls `exit N` — which IS propagated
        // because `exit` aborts the host process, not just the
        // call-operator scope.
        let launcher_body = format!("{POWERSHELL_UTF8_PRELUDE}& '{user_path}' @args\n");
        let launcher = TempPowerShellScript::write(&launcher_body)?;
        Ok(Self {
            launcher,
            _user: user,
        })
    }

    pub fn launcher_path(&self) -> &Path {
        self.launcher.path()
    }
}

/// Outcome of a child-process run after kill / timeout / completion races.
pub enum ExecOutcome {
    Completed {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    Killed {
        stdout: String,
        stderr: String,
    },
    Timeout {
        stdout: String,
        stderr: String,
    },
}

/// Spec §2.5.1 jitter — sleep a random `[0, jitter_secs)` interval so a
/// wide fan-out doesn't hit the OS at the same instant on every PC.
///
/// Called from `handle_command` *before* `started_at` is stamped (it used
/// to live at the top of `run_command_with_kill`, i.e. after the
/// timestamp). Keeping the stagger-wait outside the timing window means
/// the recorded duration (`finished_at - started_at`) measures only the
/// script's real runtime — matching `timeout:`, which has always bounded
/// the post-jitter execution alone. Ad-hoc `kanade run` sets
/// `jitter_secs: None`, so this is a no-op there.
///
/// The `> 1` guard (not `> 0`): `jitter_secs == 1` would make the range
/// `0..1`, which `random_range` collapses to a constant `0` — a
/// zero-duration sleep plus a misleading "applying jitter" log line. Skip
/// it so a 1-second jitter config is a true no-op.
pub async fn apply_jitter(cmd: &Command) {
    if let Some(j) = cmd.jitter_secs.filter(|&s| s > 1) {
        let secs = rand::rng().random_range(0..j);
        info!(
            jitter_secs = j,
            sleep_secs = secs,
            "applying jitter before exec"
        );
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

/// Spawn the command's shell child, race wait / kill / timeout, collect
/// stdout+stderr.
///
/// Spec §2.6 Layer 3 — if `cmd.exec_id` is set, subscribe to `kill.{exec_id}`
/// in parallel; a kill message causes `child.kill().await` and the outcome
/// is reported as `Killed`. A command without an `exec_id` (e.g. ad-hoc CLI
/// runs) still respects `timeout_secs`.
/// `live` (when `Some`) is the in-flight job's live-tail ring buffer:
/// every stdout/stderr chunk we read is appended to it as we go, so the
/// `job.tail.<pc_id>` handler can serve a running job's output to the
/// SPA. `None` for paths that don't want live capture (e.g. ad-hoc
/// `kanade run` from the CLI). It never affects the final captured
/// strings — those are still the complete byte stream.
pub async fn run_command_with_kill(
    client: &async_nats::Client,
    cmd: &Command,
    live: Option<Arc<LiveTail>>,
) -> Result<ExecOutcome> {
    // v0.21: run_as: user / system_gui take a separate Win32 path
    // (CreateProcessAsUserW). System (default) stays on tokio::process
    // — backward-compatible for every pre-v0.21 manifest in the wild.
    if !matches!(cmd.run_as, RunAs::System) {
        return run_in_user_session_dispatch(client, cmd, live).await;
    }

    // #43: belt-and-braces. The tolerant decoder (below, around the
    // stdout_task / stderr_task spawn) keeps the capture useful even
    // when PowerShell emits CP932; this prelude makes the child
    // write UTF-8 to begin with, matching what the operator sees
    // when they test the script locally with the same `powershell`
    // binary. Combined, the agent's capture pipeline is both correct
    // AND consistent with manifest authors' local-test results.
    // cmd.exe doesn't have an equivalent one-liner that survives
    // across legacy / unicode commands; the Cmd branch relies on the
    // tolerant decoder alone.
    // Keep `_launch` alive through spawn + wait so both staged
    // files (launcher + user script) outlive PowerShell's parse.
    // PowerShell loads scripts into memory at parse time, so
    // removal-on-drop after wait is safe.
    // Both `_launch` and `launcher_path_owned` are declared in the
    // outer scope so their backing storage outlives the `args` Vec
    // (which borrows `&str` into `launcher_path_owned`). `Option`
    // sidesteps the "value assigned but never read" lint that
    // dummy-init in the Cmd branch would trip.
    //
    // `-File <launcher>` (vs the old `-Command "<body>"`): a body
    // with `[CmdletBinding()] / param(...)` headers is valid as a
    // script file but a parse error as a command-line expression.
    // The launcher sets UTF-8 console encoding then
    // `& '<user.ps1>' @args` — that call-operator boundary means
    // headers at the top of the user file stay at the top of THEIR
    // physical script, which is what PowerShell requires.
    //
    // `-ExecutionPolicy Bypass` is needed because -File honors the
    // host's ExecutionPolicy (Restricted blocks .ps1 entirely);
    // -Command mode silently bypassed it. Adding Bypass keeps
    // behavioural parity with the pre-fix path.
    let _launch: Option<TempPowerShellLaunch>;
    let launcher_path_owned: Option<String>;
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => {
            let launch = TempPowerShellLaunch::stage(&cmd.script)?;
            launcher_path_owned = Some(launch.launcher_path().to_string_lossy().into_owned());
            _launch = Some(launch);
            (
                "powershell",
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    launcher_path_owned.as_deref().unwrap(),
                ],
            )
        }
        Shell::Cmd => {
            _launch = None;
            // launcher_path_owned stays uninitialized — the Cmd
            // args Vec doesn't borrow from it, so the compiler
            // doesn't require an init on this branch.
            ("cmd", vec!["/C", &cmd.script])
        }
    };
    let mut builder = ProcessCommand::new(program);
    builder
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cmd.cwd.as_deref().filter(|s| !s.is_empty()) {
        // v0.21.2: expand `~` / `%FOO%` against the agent's own
        // token before handing to current_dir (which itself does
        // no expansion).
        #[cfg(target_os = "windows")]
        {
            match crate::cwd_expand::open_self_token()
                .and_then(|tok| crate::cwd_expand::expand(dir, tok.handle()))
            {
                Ok(expanded) => {
                    builder.current_dir(expanded);
                }
                Err(e) => {
                    warn!(error = %e, raw_cwd = %dir, "cwd expansion failed; using raw value");
                    builder.current_dir(dir);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            builder.current_dir(dir);
        }
    }
    let mut child = builder
        .spawn()
        .with_context(|| format!("spawn {program}"))?;

    // Job Object: put the host (`powershell` / `cmd`) — and every
    // descendant it spawns — into a kernel Job so a kill/timeout can
    // terminate the WHOLE tree at once. Without this, `child.kill()`
    // only reaps the host; a grandchild (e.g. a job that runs
    // `claude`) would be orphaned AND keep the inherited stdout/stderr
    // pipe handles open, so the `read_to_end` drain below would never
    // hit EOF and this fn would hang forever — leaving the Activity
    // row stuck on "実行中" after a 強制終了 click. On non-Windows
    // `job` is always `None` and we fall back to the single-process
    // kill. Assign failure (e.g. an OS without nested-Job support)
    // also degrades to the single-process path with a warning.
    let job: Option<crate::job_object::JobObject> = {
        #[cfg(target_os = "windows")]
        {
            match child.raw_handle() {
                Some(h) => {
                    match crate::job_object::JobObject::assign_handle(
                        windows::Win32::Foundation::HANDLE(h),
                    ) {
                        Ok(j) => Some(j),
                        Err(e) => {
                            warn!(error = %e, "job object assign failed; kill falls back to single-process terminate");
                            None
                        }
                    }
                }
                None => None,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    };

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // #43: `read_to_string` is strict UTF-8 — a single invalid byte
    // sequence makes it return `Err(InvalidData)` AND discard
    // everything read so far. On ja-JP Windows that fires whenever
    // a PowerShell child emits CP932-encoded Japanese on stdout
    // (default `[Console]::OutputEncoding` is the system OEM
    // codepage, not UTF-8). The whole inventory probe output was
    // silently lost. Read as bytes + `String::from_utf8_lossy` so
    // we keep every byte of useful output; invalid runs become
    // U+FFFD and don't poison the rest of the capture. Same fix
    // for any future locale / cmd-shell / 3rd-party tool that
    // emits non-UTF-8 — not specific to PowerShell.
    //
    // Gemini #83 fix: return `(String, Option<Error>)` instead of
    // `Result<String, Error>` so a mid-stream I/O failure (broken
    // pipe, child crash partway through writing, etc.) preserves
    // every byte we DID manage to read instead of throwing the
    // partial buffer away with `?`. The caller logs the error +
    // annotates stderr with a marker but keeps the partial capture.
    // v0.4x: chunked drain instead of `read_to_end`. We still keep
    // every byte and decode the COMPLETE buffer with `from_utf8_lossy`
    // at the end (identical final output to the old path — the #43 /
    // Gemini #83 partial-capture + error-preserving semantics are
    // unchanged). The only difference: each chunk is *also* appended
    // to the live-tail ring as it arrives, so the `job.tail` handler
    // can serve a running job's output. The ring stores raw bytes and
    // lossy-decodes the whole buffer on snapshot, so a multi-byte char
    // split across two `read` calls doesn't produce a spurious U+FFFD.
    let live_out = live.clone();
    let stdout_task =
        tokio::spawn(async move { drain_to_string(stdout_handle, live_out, Stream::Stdout).await });
    let live_err = live.clone();
    let stderr_task =
        tokio::spawn(async move { drain_to_string(stderr_handle, live_err, Stream::Stderr).await });

    let timeout_dur = Duration::from_secs(cmd.timeout_secs.max(1));

    let inner = match &cmd.exec_id {
        Some(eid) => {
            let kill_subject = subject::kill(eid);
            let mut kill_sub = client
                .subscribe(kill_subject.clone())
                .await
                .with_context(|| format!("subscribe {kill_subject}"))?;
            // Flush so the server has registered our SUB before any publish
            // can race past us.
            client.flush().await.ok();
            debug!(exec_id = %eid, subject = %kill_subject, "kill listener armed");

            tokio::select! {
                status = child.wait() => {
                    debug!(exec_id = %eid, "child exited (wait arm fired)");
                    let s = status?;
                    OutcomeInner::Completed(s.code().unwrap_or(-1))
                }
                msg = kill_sub.next() => {
                    info!(exec_id = %eid, has_msg = msg.is_some(), "kill arm fired");
                    // Terminate the whole Job (host + descendants) so
                    // orphaned grandchildren can't keep the pipes open
                    // and hang the drain. Fall back to the single
                    // child when no Job was assigned.
                    if let Some(j) = &job {
                        j.terminate();
                    } else if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill failed (process may already be dead)");
                    }
                    OutcomeInner::Killed
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    info!(exec_id = %eid, "timeout arm fired");
                    if let Some(j) = &job {
                        j.terminate();
                    } else if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill on timeout failed");
                    }
                    OutcomeInner::Timeout
                }
            }
        }
        None => {
            tokio::select! {
                status = child.wait() => {
                    let s = status?;
                    OutcomeInner::Completed(s.code().unwrap_or(-1))
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    if let Some(j) = &job {
                        j.terminate();
                    } else if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill on timeout failed");
                    }
                    OutcomeInner::Timeout
                }
            }
        }
    };

    // #43 / Gemini #83 fix: pre-fix `unwrap_or_default()` here
    // silently swallowed the reader task's inner `anyhow::Error`
    // (broken pipe, partial read on child crash, the now-impossible
    // UTF-8 InvalidData, etc.) — producing `stdout: ""` with no log,
    // no annotation. That's the worst kind of failure for a fleet
    // tool: "exit 0 with empty capture" registers as a normal
    // success. The reader tasks now return `(partial_string,
    // Option<Error>)` so we KEEP whatever bytes we managed to read
    // before the failure (`from_utf8_lossy` already applied) and
    // additionally surface the error via warn-log + a marker in
    // stderr so the result row is self-explanatory in the SPA /
    // Activity detail view.
    let (stdout, stdout_err) = stdout_task
        .await
        .map_err(|e| anyhow::anyhow!("stdout task join: {e}"))?;
    if let Some(e) = stdout_err {
        warn!(error = %e, "stdout capture failed (kept partial)");
    }
    let (mut stderr, stderr_err) = stderr_task
        .await
        .map_err(|e| anyhow::anyhow!("stderr task join: {e}"))?;
    if let Some(e) = stderr_err {
        warn!(error = %e, "stderr capture failed (kept partial)");
        // Append the marker AFTER the partial bytes so the partial
        // capture stays first (operators read top-down). Newline
        // separator handles the common case where the partial
        // stream lacked a trailing \n.
        stderr.push_str(&format!("\n[agent: stderr capture failed: {e}]\n"));
    }

    Ok(match inner {
        OutcomeInner::Completed(code) => ExecOutcome::Completed {
            exit_code: code,
            stdout,
            stderr,
        },
        OutcomeInner::Killed => ExecOutcome::Killed { stdout, stderr },
        OutcomeInner::Timeout => ExecOutcome::Timeout { stdout, stderr },
    })
}

enum OutcomeInner {
    Completed(i32),
    Killed,
    Timeout,
}

/// Which captured stream a chunk belongs to — selects the live-tail
/// ring it gets mirrored into.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Drain a child pipe to a `String` in chunks, mirroring each chunk to
/// the live-tail ring as it arrives.
///
/// Returns `(decoded, partial_error)` with the SAME contract as the
/// old `read_to_end` path: on a mid-stream I/O error we keep every
/// byte read so far (`from_utf8_lossy` applied to the whole buffer)
/// and surface the error so the caller can log + annotate. The live
/// ring sees only the bytes that actually arrived before the error.
async fn drain_to_string<R>(
    reader: Option<R>,
    live: Option<Arc<LiveTail>>,
    stream: Stream,
) -> (String, Option<anyhow::Error>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut err: Option<anyhow::Error> = None;
    if let Some(mut s) = reader {
        // 8 KiB chunks: large enough that a chatty job doesn't thrash
        // the read loop, small enough that the live tail updates
        // promptly (a 5 s SPA poll sees output within one chunk).
        let mut chunk = [0u8; 8 * 1024];
        loop {
            match s.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    let slice = &chunk[..n];
                    buf.extend_from_slice(slice);
                    if let Some(lt) = &live {
                        match stream {
                            Stream::Stdout => lt.push_stdout(slice),
                            Stream::Stderr => lt.push_stderr(slice),
                        }
                    }
                }
                Err(e) => {
                    err = Some(anyhow::Error::new(e));
                    break;
                }
            }
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), err)
}

/// Glue between the main `run_command_with_kill` (which expects a
/// NATS subscriber-based kill signal) and `process_as_user`'s
/// `oneshot::Receiver<()>` kill channel. We subscribe to `kill.{exec_id}`
/// here and forward "fired" into the channel, so the Win32 path's
/// inner `tokio::select!` can use a plain oneshot.
//
async fn run_in_user_session_dispatch(
    client: &async_nats::Client,
    cmd: &Command,
    live: Option<Arc<LiveTail>>,
) -> Result<ExecOutcome> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = client;
        let _ = live;
        warn!(
            run_as = ?cmd.run_as,
            "run_as: user / system_gui is Windows-only — falling back to inherited identity",
        );
        // Synthesise an immediate "stub" outcome rather than silently
        // running as the wrong identity on a non-Windows agent. Real
        // operators are on Windows anyway; this branch exists to keep
        // the workspace cross-compile-clean.
        Ok(ExecOutcome::Completed {
            exit_code: 0,
            stdout: String::new(),
            stderr: format!(
                "run_as: {:?} is Windows-only; non-Windows agents skip the script.\n",
                cmd.run_as
            ),
        })
    }

    #[cfg(target_os = "windows")]
    {
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        // Spawn the kill bridge only when there's an exec_id to listen
        // for — ad-hoc / scheduler-less exec paths skip it.
        let bridge = if let Some(eid) = cmd.exec_id.clone() {
            let nats = client.clone();
            let subject = subject::kill(&eid);
            Some(tokio::spawn(async move {
                match nats.subscribe(subject.clone()).await {
                    Ok(mut sub) => {
                        // flush before await so the broker has SUB
                        nats.flush().await.ok();
                        debug!(exec_id = %eid, subject = %subject, "kill listener armed (user-session path)");
                        if sub.next().await.is_some() {
                            info!(exec_id = %eid, "kill received → forwarding to user-session waiter");
                            let _ = kill_tx.send(());
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, %subject, "subscribe kill failed (user-session path)")
                    }
                }
            }))
        } else {
            None
        };

        let timeout = Duration::from_secs(cmd.timeout_secs.max(1));
        let outcome = crate::process_as_user::run_command_in_user_session(
            cmd, cmd.run_as, timeout, kill_rx, live,
        )
        .await;

        if let Some(b) = bridge {
            b.abort();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    /// Mirror the production stdout/stderr reader: read every byte
    /// then `from_utf8_lossy`. Used to assert that invalid UTF-8
    /// (e.g. CP932-encoded Japanese on a non-Unicode console)
    /// doesn't wipe the capture the way `read_to_string` did pre-#43.
    async fn capture_lossy<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> String {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn cp932_japanese_bytes_are_kept_lossy_not_dropped() {
        // CP932 (Shift-JIS) for "ちつ" — the byte sequence that
        // triggers `read_to_string`'s strict-UTF-8 rejection.
        // Pre-fix, the entire stdout buffer was discarded; post-
        // fix, the bytes survive (as U+FFFD) and the rest of the
        // payload around them stays intact.
        let raw: Vec<u8> = vec![
            b'{', b'"', b'k', b'"', b':', b'"', 0x82, 0xbf, 0x82, 0xc2, b'"', b'}',
        ];
        let captured = capture_lossy(tokio::io::BufReader::new(&raw[..])).await;
        // The structural ASCII (`{"k":"…"}`) survives — that's
        // what was being lost pre-fix.
        assert!(captured.starts_with("{\"k\":\""), "ASCII frame preserved");
        assert!(captured.ends_with("\"}"), "ASCII frame preserved");
        // The Japanese bytes become U+FFFD replacement chars (not
        // dropped silently).
        assert!(captured.contains('\u{FFFD}'), "invalid runs marked");
    }

    #[tokio::test]
    async fn pure_utf8_payload_round_trips() {
        let raw = "こんにちは {\"ok\": true}".as_bytes().to_vec();
        let captured = capture_lossy(tokio::io::BufReader::new(&raw[..])).await;
        assert_eq!(captured, "こんにちは {\"ok\": true}");
    }

    #[tokio::test]
    async fn empty_stream_yields_empty_string() {
        let raw: Vec<u8> = Vec::new();
        let captured = capture_lossy(tokio::io::BufReader::new(&raw[..])).await;
        assert_eq!(captured, "");
    }

    #[test]
    fn powershell_prelude_constant_shape() {
        // Defensive: ensures the prelude itself ends with `; ` (so
        // the user script slots in cleanly without an explicit
        // newline) and contains both the Console + $OutputEncoding
        // statements operators expect when they read agent.log
        // or the script that actually ran.
        assert!(super::POWERSHELL_UTF8_PRELUDE.ends_with("; "));
        assert!(super::POWERSHELL_UTF8_PRELUDE.contains("[Console]::OutputEncoding"));
        assert!(super::POWERSHELL_UTF8_PRELUDE.contains("$OutputEncoding"));
    }

    #[test]
    fn temp_powershell_script_writes_bom_then_body_verbatim() {
        // Regression for the CodeRabbit finding on PR #230 first
        // pass: the staged user file MUST contain the body
        // verbatim. Any prelude prepended here would push
        // `[CmdletBinding()] / param(...)` headers off the top of
        // the file and re-introduce the parse error the PR was
        // meant to fix.
        let script = "[CmdletBinding()] param([string]$X='a'); Write-Output $X";
        let staged = super::TempPowerShellScript::write(script).expect("write");
        let bytes = std::fs::read(staged.path()).expect("read back");
        assert_eq!(
            &bytes[..3],
            &[0xEF, 0xBB, 0xBF],
            "BOM not at start of staged file",
        );
        let body_bytes = &bytes[3..];
        assert_eq!(
            std::str::from_utf8(body_bytes).unwrap(),
            script,
            "user body must be verbatim — no prelude prefix",
        );
        assert_eq!(
            staged.path().extension().and_then(|s| s.to_str()),
            Some("ps1"),
        );
    }

    #[test]
    fn temp_powershell_script_drop_removes_file() {
        let staged = super::TempPowerShellScript::write("Write-Output 'x'").expect("write");
        let path = staged.path().to_path_buf();
        assert!(path.exists(), "file should exist before drop");
        drop(staged);
        assert!(!path.exists(), "file should be gone after drop");
    }

    #[test]
    fn temp_powershell_launch_user_file_has_no_prelude() {
        let user_script =
            "[CmdletBinding()] param([string]$Foo = 'bar'); Write-Output \"got:$Foo\"";
        let launch = super::TempPowerShellLaunch::stage(user_script).expect("stage");
        // Find the user file via the launcher body (it embeds the
        // single-quoted path). We don't expose the user path
        // directly because callers never need it — but the test
        // does, to assert "no prelude on the user side".
        let launcher_text = std::fs::read_to_string(launch.launcher_path()).expect("read launcher");
        let start = launcher_text
            .find("& '")
            .expect("launcher should invoke user script via call operator");
        let after = &launcher_text[start + 3..];
        let end = after.find('\'').expect("launcher path closes its quote");
        let user_path_in_launcher: String = after[..end].replace("''", "'");

        let user_bytes = std::fs::read(&user_path_in_launcher).expect("read user file");
        // BOM + body verbatim. NO prelude. The launcher carries the
        // prelude so the user file's headers stay at the top.
        assert_eq!(&user_bytes[..3], &[0xEF, 0xBB, 0xBF]);
        let body = std::str::from_utf8(&user_bytes[3..]).unwrap();
        assert_eq!(body, user_script);
        assert!(
            !body.contains("[Console]::OutputEncoding"),
            "user file must not carry the encoding prelude",
        );
    }

    #[test]
    fn temp_powershell_launch_launcher_carries_prelude_then_invokes_user() {
        let launch = super::TempPowerShellLaunch::stage("Write-Output 'hi'").expect("stage");
        let launcher_text = std::fs::read_to_string(launch.launcher_path()).expect("read launcher");
        assert!(
            launcher_text.contains("[Console]::OutputEncoding"),
            "launcher must set console encoding before invoking user",
        );
        assert!(
            launcher_text.contains("& '") && launcher_text.contains("' @args"),
            "launcher must invoke user via call operator with @args splat",
        );
        // Prelude precedes the call operator (encoding takes
        // effect before any user output).
        let prelude_pos = launcher_text.find("[Console]::OutputEncoding").unwrap();
        let call_pos = launcher_text.find("& '").unwrap();
        assert!(prelude_pos < call_pos);
    }

    #[test]
    fn temp_powershell_launch_drop_removes_both_files() {
        let launch = super::TempPowerShellLaunch::stage("Write-Output 'x'").expect("stage");
        let launcher_path = launch.launcher_path().to_path_buf();
        // Pull user path out of the launcher body before drop.
        let launcher_text = std::fs::read_to_string(&launcher_path).expect("read launcher");
        let start = launcher_text.find("& '").unwrap() + 3;
        let end = launcher_text[start..].find('\'').unwrap();
        let user_path =
            std::path::PathBuf::from(launcher_text[start..start + end].replace("''", "'"));

        assert!(launcher_path.exists());
        assert!(user_path.exists());
        drop(launch);
        assert!(!launcher_path.exists(), "launcher must be removed on drop");
        assert!(!user_path.exists(), "user file must be removed on drop");
    }
}
