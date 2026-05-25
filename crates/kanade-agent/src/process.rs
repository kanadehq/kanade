use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::{Command, RunAs, Shell};
use rand::Rng;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tracing::{info, warn};
use uuid::Uuid;

/// #43 / PR γ-bot-review: one source of truth for the PowerShell
/// console-encoding prelude. Both the System path (this file) and the
/// `run_as: user` path (`process_as_user.rs`) inject it, and the
/// `powershell_prelude_forces_utf8_output` unit test asserts against
/// this same constant — so if anyone tweaks the prelude shape in one
/// place, the test catches drift automatically. Pre-fix the literal
/// was duplicated three times.
pub(crate) const POWERSHELL_UTF8_PRELUDE: &str = "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; \
     $OutputEncoding = [Console]::OutputEncoding; ";

/// Wrap a user PowerShell script with [`POWERSHELL_UTF8_PRELUDE`] so the
/// resulting `-File` body forces UTF-8 on both `[Console]::OutputEncoding`
/// and `$OutputEncoding` before the user script runs. User scripts can
/// still override the encoding mid-run (assignment is just a property
/// set), but the default for any script that doesn't touch encoding
/// becomes UTF-8 instead of the system OEM codepage.
pub(crate) fn with_powershell_utf8_prelude(user_script: &str) -> String {
    format!("{POWERSHELL_UTF8_PRELUDE}{user_script}")
}

/// PowerShell script staged to a temp `.ps1` file so `powershell -File`
/// can read it as a full script (param() / [CmdletBinding()] / …
/// headers all parse correctly). `powershell -Command "<body>"`
/// parses the body as a command-line expression and rejects
/// function-style headers — the install-kanade-backend test on
/// 2026-05-26 hit this when operators tried to ship
/// `scripts/deploy-backend.ps1` (a normal .ps1 with a param block)
/// via `script_object`.
///
/// Cleanup-on-drop: the file is removed when the struct goes out
/// of scope. PowerShell reads the script into memory at startup,
/// so deleting the file mid-run doesn't affect the running
/// process — but we still keep the struct alive for the function's
/// duration in case PowerShell ever needs to re-read (e.g. to
/// dot-source itself).
pub(crate) struct TempPowerShellScript {
    path: PathBuf,
}

impl TempPowerShellScript {
    /// Stage `body` to a fresh `.ps1` under
    /// `%TEMP%/kanade-agent-scripts/`. Prepends a UTF-8 BOM so
    /// `powershell -File` reads the body as UTF-8 regardless of the
    /// system codepage — matches what the `-Command`-mode prelude
    /// was achieving for output, but for the SCRIPT FILE itself.
    pub fn write(body: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join("kanade-agent-scripts");
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(format!("kanade-{}.ps1", Uuid::new_v4().simple()));
        // UTF-8 BOM (0xEF 0xBB 0xBF) — PowerShell uses it to detect
        // UTF-8 encoding without a `chcp 65001` dance. Without it,
        // a ja-JP host running default CP932 would mis-parse any
        // multi-byte sequence in the script body (operator strings,
        // comments, etc.).
        let mut bytes = Vec::with_capacity(3 + body.len());
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        bytes.extend_from_slice(body.as_bytes());
        std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPowerShellScript {
    fn drop(&mut self) {
        // Best-effort. Leak on rare error is OK — temp dir gets GC'd
        // by Windows eventually, and the agent isn't producing
        // many of these (one per Command execution).
        let _ = std::fs::remove_file(&self.path);
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

/// Spawn the command's shell child, race wait / kill / timeout, collect
/// stdout+stderr.
///
/// Spec §2.6 Layer 3 — if `cmd.exec_id` is set, subscribe to `kill.{exec_id}`
/// in parallel; a kill message causes `child.kill().await` and the outcome
/// is reported as `Killed`. A command without an `exec_id` (e.g. ad-hoc CLI
/// runs) still respects `timeout_secs`.
pub async fn run_command_with_kill(
    client: &async_nats::Client,
    cmd: &Command,
) -> Result<ExecOutcome> {
    // Spec §2.5.1 jitter — sleep a random `[0, jitter_secs)` interval
    // before spawning the child so a wide fan-out doesn't hit the OS at
    // the same instant on every PC.
    if let Some(j) = cmd.jitter_secs.filter(|&s| s > 0) {
        let secs = rand::rng().random_range(0..j);
        info!(
            jitter_secs = j,
            sleep_secs = secs,
            "applying jitter before exec"
        );
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    // v0.21: run_as: user / system_gui take a separate Win32 path
    // (CreateProcessAsUserW). System (default) stays on tokio::process
    // — backward-compatible for every pre-v0.21 manifest in the wild.
    if !matches!(cmd.run_as, RunAs::System) {
        return run_in_user_session_dispatch(client, cmd).await;
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
    // Keep `_temp_script` alive through the spawn + wait/race so
    // the `.ps1` on disk outlives the child's startup (PowerShell
    // reads the body into memory at parse time, but holding the
    // file longer is defensive against future dot-source / re-parse
    // patterns).
    let _temp_script;
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => {
            // `-File <tempfile>` (vs the old `-Command "<body>"`):
            // a body with `[CmdletBinding()] / param(...)` headers
            // is valid as a script file but a parse error as a
            // command-line expression. Switching to -File means
            // operators can ship normal .ps1 files through
            // `script_object` without rewriting them. UTF-8 prelude
            // still leads the body (controls stdout encoding); the
            // BOM-prefixed file makes PowerShell parse the body
            // itself as UTF-8 regardless of system codepage.
            //
            // `-ExecutionPolicy Bypass` is needed because -File
            // honors the host's ExecutionPolicy (Restricted blocks
            // .ps1 entirely); -Command mode silently bypassed it.
            // Adding Bypass keeps behavioural parity with the
            // pre-fix path.
            let body = with_powershell_utf8_prelude(&cmd.script);
            _temp_script = Some(TempPowerShellScript::write(&body)?);
            let path = _temp_script
                .as_ref()
                .unwrap()
                .path()
                .to_str()
                .context("kanade-agent-scripts temp path is not valid UTF-8")?;
            (
                "powershell",
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    path,
                ],
            )
        }
        Shell::Cmd => {
            _temp_script = None;
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
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut err: Option<anyhow::Error> = None;
        if let Some(mut s) = stdout_handle
            && let Err(e) = s.read_to_end(&mut buf).await
        {
            err = Some(anyhow::Error::new(e));
        }
        (String::from_utf8_lossy(&buf).into_owned(), err)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut err: Option<anyhow::Error> = None;
        if let Some(mut s) = stderr_handle
            && let Err(e) = s.read_to_end(&mut buf).await
        {
            err = Some(anyhow::Error::new(e));
        }
        (String::from_utf8_lossy(&buf).into_owned(), err)
    });

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
            info!(exec_id = %eid, subject = %kill_subject, "kill listener armed");

            tokio::select! {
                status = child.wait() => {
                    info!(exec_id = %eid, "child exited (wait arm fired)");
                    let s = status?;
                    OutcomeInner::Completed(s.code().unwrap_or(-1))
                }
                msg = kill_sub.next() => {
                    info!(exec_id = %eid, has_msg = msg.is_some(), "kill arm fired");
                    if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill failed (process may already be dead)");
                    }
                    OutcomeInner::Killed
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    info!(exec_id = %eid, "timeout arm fired");
                    if let Err(e) = child.kill().await {
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
                    if let Err(e) = child.kill().await {
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

/// Glue between the main `run_command_with_kill` (which expects a
/// NATS subscriber-based kill signal) and `process_as_user`'s
/// `oneshot::Receiver<()>` kill channel. We subscribe to `kill.{exec_id}`
/// here and forward "fired" into the channel, so the Win32 path's
/// inner `tokio::select!` can use a plain oneshot.
//
// `clippy::needless_return` is silenced because the function body is
// a pair of mutually-exclusive `#[cfg(...)]` blocks: on non-Windows
// the first block needs `return` to exit the function (there's no
// fall-through tail expression — the Windows block is compiled out).
// The lint inspects each cfg branch in isolation and misses that.
#[allow(clippy::needless_return)]
async fn run_in_user_session_dispatch(
    client: &async_nats::Client,
    cmd: &Command,
) -> Result<ExecOutcome> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = client;
        warn!(
            run_as = ?cmd.run_as,
            "run_as: user / system_gui is Windows-only — falling back to inherited identity",
        );
        // Synthesise an immediate "stub" outcome rather than silently
        // running as the wrong identity on a non-Windows agent. Real
        // operators are on Windows anyway; this branch exists to keep
        // the workspace cross-compile-clean.
        return Ok(ExecOutcome::Completed {
            exit_code: 0,
            stdout: String::new(),
            stderr: format!(
                "run_as: {:?} is Windows-only; non-Windows agents skip the script.\n",
                cmd.run_as
            ),
        });
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
                        info!(exec_id = %eid, subject = %subject, "kill listener armed (user-session path)");
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
        let outcome =
            crate::process_as_user::run_command_in_user_session(cmd, cmd.run_as, timeout, kill_rx)
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
    fn powershell_prelude_forces_utf8_output() {
        // CodeRabbit #83 nitpick: assert against the production
        // helper, not a duplicated literal. If anyone tweaks the
        // prelude shape, both the production paths AND this test
        // pick up the change automatically.
        let user_script = "Write-Output 'hello'";
        let combined = super::with_powershell_utf8_prelude(user_script);
        assert!(combined.contains("[Console]::OutputEncoding"));
        assert!(combined.contains("UTF8Encoding"));
        assert!(combined.ends_with(user_script));
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
    fn temp_powershell_script_writes_bom_then_body() {
        let script = "[CmdletBinding()] param([string]$X='a'); Write-Output $X";
        let staged = super::TempPowerShellScript::write(script).expect("write");
        let bytes = std::fs::read(staged.path()).expect("read back");
        // BOM is the first three bytes — PowerShell uses it to
        // detect UTF-8 without depending on the system codepage.
        assert_eq!(
            &bytes[..3],
            &[0xEF, 0xBB, 0xBF],
            "BOM not at start of staged file",
        );
        let body_bytes = &bytes[3..];
        assert_eq!(std::str::from_utf8(body_bytes).unwrap(), script);
        // Filename ends with `.ps1` so `powershell -File` recognises
        // it as a script (not a plain text file).
        assert_eq!(
            staged.path().extension().and_then(|s| s.to_str()),
            Some("ps1")
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
}
