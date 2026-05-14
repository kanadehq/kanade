use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::{Command, Shell};
use kanade_shared::subject;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tracing::warn;

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
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => (
            "powershell",
            vec!["-NoProfile", "-NonInteractive", "-Command", &cmd.script],
        ),
        Shell::Cmd => ("cmd", vec!["/C", &cmd.script]),
    };
    let mut child = ProcessCommand::new(program)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
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
            let mut kill_sub = client
                .subscribe(subject::kill(jid))
                .await
                .with_context(|| format!("subscribe kill.{jid}"))?;
            tokio::select! {
                status = child.wait() => {
                    let s = status?;
                    OutcomeInner::Completed(s.code().unwrap_or(-1))
                }
                _ = kill_sub.next() => {
                    if let Err(e) = child.kill().await {
                        warn!(error = %e, "child.kill failed (process may already be dead)");
                    }
                    OutcomeInner::Killed
                }
                _ = tokio::time::sleep(timeout_dur) => {
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
