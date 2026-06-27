//! #855 session supervisor — keeps a `--session-agent` child alive in the
//! active console session and feeds its in-session idle reading into
//! `env_gate`'s console-idle cache.
//!
//! Why: a SYSTEM service can't read truthful per-session idle. WTS
//! `LastInputTime` is stale for an active console session (a working user reads
//! as days-idle), and `GetLastInputInfo` is session-affine. So the agent
//! launches a tiny `--session-agent` child *inside* the user session (via the
//! `RunAs::User` token dance in `process_as_user`); the child reads
//! `GetLastInputInfo` and prints `{"idle_ms":N}` lines, which this supervisor
//! reads back into `env_gate::set_console_idle`. Both consumers
//! (`idle_sampler` and the #418 `require:` gate) then read the right idle via
//! the unchanged `console_idle()`.
//!
//! Polling `WTSGetActiveConsoleSessionId` is the source of truth — the SCM
//! `SessionChange` callback only fires in service mode and never for a user
//! already logged on at agent start, so a poll loop is the robust base.
//! The child's Job is `KILL_ON_JOB_CLOSE`, so it dies with this agent (no
//! orphan across self-update / crash). A user *can* kill the child (it runs
//! with their token); the supervisor respawns it with capped backoff, and
//! while it's down the cache goes stale → `console_idle()` returns MAX (idle).

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::env_gate::set_console_idle;
use crate::process_as_user::{active_console_session, read_lines, spawn_session_agent_child};

/// Re-check the console session id at this cadence (logon/switch latency floor)
/// both while idle-waiting and while a child runs.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Run forever (spawned once at agent start). `exe` is the agent binary,
/// relaunched as `--session-agent` inside the user session.
pub async fn run(exe: PathBuf) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match active_console_session() {
            None => {
                // No console user → cache stale → console_idle() returns MAX.
                set_console_idle(None);
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Some(session) => {
                let clean = pump_one(&exe, session).await;
                // Child gone: the freshness guard returns MAX until the next
                // child reports, so clearing now is belt-and-suspenders.
                set_console_idle(None);
                if clean {
                    backoff = BACKOFF_MIN; // session ended cleanly → reset
                } else {
                    warn!(
                        target: "kanade_agent::session_supervisor",
                        backoff_s = backoff.as_secs(),
                        "session-agent ended unexpectedly — backing off before respawn",
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
}

/// Spawn one session-agent for `session` and pump its stdout until it dies or
/// the console session changes/ends. Returns `true` when it stopped because the
/// session went away/changed (expected), `false` on an unexpected child death
/// (the caller then backs off before respawning).
async fn pump_one(exe: &Path, session: u32) -> bool {
    let exe_owned = exe.to_path_buf();
    let mut child = match tokio::task::spawn_blocking(move || spawn_session_agent_child(&exe_owned))
        .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            warn!(target: "kanade_agent::session_supervisor", error = %e, "spawn session-agent failed");
            return false;
        }
        Err(e) => {
            warn!(target: "kanade_agent::session_supervisor", error = %e, "spawn join failed");
            return false;
        }
    };
    info!(target: "kanade_agent::session_supervisor", session, "session-agent started");

    // Reader thread: stdout lines → channel. read_lines blocks on ReadFile;
    // it returns (closing the channel) when the child exits / is killed.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let Some(stdout) = child.take_stdout() else {
        return false;
    };
    let reader = tokio::task::spawn_blocking(move || {
        read_lines(stdout, |line| {
            let _ = tx.send(line.to_string());
        });
    });

    let mut ended_cleanly = false;
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(l) => {
                    if let Some(ms) = parse_idle_ms(&l) {
                        set_console_idle(Some(Duration::from_millis(ms)));
                    }
                }
                None => break, // reader EOF → child died (unexpected)
            },
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                if active_console_session() != Some(session) {
                    info!(
                        target: "kanade_agent::session_supervisor",
                        "console session changed/ended — stopping session-agent",
                    );
                    ended_cleanly = true;
                    break;
                }
            }
        }
    }
    // Kill the child (whole tree) so its read/write threads unblock via pipe
    // EOF, then join the reader. KILL_ON_JOB_CLOSE also covers agent exit.
    child.terminate();
    let _ = reader.await;
    ended_cleanly
}

/// Parse a `{"idle_ms":N}` line into milliseconds. Returns `None` for the
/// `null` form or anything unparseable (kept lenient — the child only ever
/// prints this one shape).
fn parse_idle_ms(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    v.get("idle_ms")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::parse_idle_ms;

    #[test]
    fn parses_idle_ms_and_ignores_junk() {
        assert_eq!(parse_idle_ms(r#"{"idle_ms":2500}"#), Some(2500));
        assert_eq!(parse_idle_ms(r#"  {"idle_ms":0}  "#), Some(0));
        assert_eq!(parse_idle_ms(r#"{"idle_ms":null}"#), None);
        assert_eq!(parse_idle_ms("not json"), None);
        assert_eq!(parse_idle_ms(""), None);
        assert_eq!(parse_idle_ms(r#"{"other":1}"#), None);
    }
}
