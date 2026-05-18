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
/// Spec §2.6 Layer 3 — if `cmd.job_id` is set, subscribe to `kill.{job_id}`
/// in parallel; a kill message causes `child.kill().await` and the outcome
/// is reported as `Killed`. A command without a `job_id` (e.g. ad-hoc CLI
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

    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => (
            "powershell",
            vec!["-NoProfile", "-NonInteractive", "-Command", &cmd.script],
        ),
        Shell::Cmd => ("cmd", vec!["/C", &cmd.script]),
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

    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stdout_handle {
            s.read_to_string(&mut buf).await?;
        }
        Ok::<_, anyhow::Error>(buf)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stderr_handle {
            s.read_to_string(&mut buf).await?;
        }
        Ok::<_, anyhow::Error>(buf)
    });

    let timeout_dur = Duration::from_secs(cmd.timeout_secs.max(1));

    let inner = match &cmd.job_id {
        Some(jid) => {
            let kill_subject = subject::kill(jid);
            let mut kill_sub = client
                .subscribe(kill_subject.clone())
                .await
                .with_context(|| format!("subscribe {kill_subject}"))?;
            // Flush so the server has registered our SUB before any publish
            // can race past us.
            client.flush().await.ok();
            info!(job_id = %jid, subject = %kill_subject, "kill listener armed");

            tokio::select! {
                status = child.wait() => {
                    info!(job_id = %jid, "child exited (wait arm fired)");
                    let s = status?;
                    OutcomeInner::Completed(s.code().unwrap_or(-1))
                }
                msg = kill_sub.next() => {
                    info!(job_id = %jid, has_msg = msg.is_some(), "kill arm fired");
                    if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill failed (process may already be dead)");
                    }
                    OutcomeInner::Killed
                }
                _ = tokio::time::sleep(timeout_dur) => {
                    info!(job_id = %jid, "timeout arm fired");
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

    let stdout = stdout_task
        .await
        .map_err(|e| anyhow::anyhow!("stdout task join: {e}"))?
        .unwrap_or_default();
    let stderr = stderr_task
        .await
        .map_err(|e| anyhow::anyhow!("stderr task join: {e}"))?
        .unwrap_or_default();

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
/// `oneshot::Receiver<()>` kill channel. We subscribe to `kill.{job_id}`
/// here and forward "fired" into the channel, so the Win32 path's
/// inner `tokio::select!` can use a plain oneshot.
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
        // Spawn the kill bridge only when there's a job_id to listen
        // for — ad-hoc / scheduler-less exec paths skip it.
        let bridge = if let Some(jid) = cmd.job_id.clone() {
            let nats = client.clone();
            let subject = subject::kill(&jid);
            Some(tokio::spawn(async move {
                match nats.subscribe(subject.clone()).await {
                    Ok(mut sub) => {
                        // flush before await so the broker has SUB
                        nats.flush().await.ok();
                        info!(job_id = %jid, subject = %subject, "kill listener armed (user-session path)");
                        if sub.next().await.is_some() {
                            info!(job_id = %jid, "kill received → forwarding to user-session waiter");
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
