//! #1140 PR3b — the agent side of the remote-assistance relay.
//!
//! Serves `remote.ctrl.<pc_id>` (request/reply), and while a session is live
//! runs the in-session capture child and forwards its tiles to
//! `remote.frame.<session_id>`.
//!
//! # Why control is addressed by pc_id
//!
//! The agent must already be subscribed when the first request arrives. A
//! session-scoped control subject would have nothing listening at the moment
//! an operator clicks "connect", so the subject that starts a session cannot
//! itself be session-scoped. Frames go the other way — session-scoped, so
//! two operators viewing one machine never demultiplex each other and a
//! torn-down session's late frames land where nobody reads.
//!
//! # One session per machine
//!
//! A second `Start` while a session is live is refused rather than queued or
//! multiplexed. DXGI allows one duplication per output, so a second capture
//! child would simply fail to attach — and refusing with a clear reason is
//! better than two operators silently fighting over one duplication. The
//! refusal names the session that holds it, so the second operator knows to
//! ask rather than retry.
//!
//! # The child does the work
//!
//! Capture, encoding and tile splitting all live in the child process (see
//! `capture_frame_io`, `capture_encode`) because a Session 0 service cannot
//! read a desktop at all. This module is the plumbing between that child's
//! stdout and NATS: it owns the session lifecycle and nothing else.

#![cfg(target_os = "windows")]

use std::sync::Arc;

use kanade_shared::subject;
use kanade_shared::wire::{RemoteCtrl, RemoteCtrlReply, gap_headers, resumed_headers};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::capture_frame_io::{FrameHeader, read_frame};
use crate::process_as_user::{SessionAgentChild, spawn_session_child};

/// What the reader thread hands to the publisher task.
enum Outbound {
    Tile {
        header: Box<FrameHeader>,
        payload: Vec<u8>,
    },
    Gap(String),
    Resumed,
}

/// A live capture session.
struct Live {
    session_id: String,
    /// Killing this kills the capture child and, by breaking its stdout,
    /// unblocks the reader thread.
    child: SessionAgentChild,
}

impl Live {
    fn stop(self) {
        self.child.terminate();
    }
}

/// Serve `remote.ctrl.<pc_id>` forever.
pub async fn serve(client: async_nats::Client, pc_id: String, exe: std::path::PathBuf) {
    let subj = subject::remote_ctrl(&pc_id);
    let live: Arc<Mutex<Option<Live>>> = Arc::new(Mutex::new(None));

    // Outer reconnect loop, same shape as `ping::serve`: a failed subscribe
    // (broker still booting) or a closed subscription must not permanently
    // kill the responder.
    loop {
        let mut sub = match client.subscribe(subj.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(subject = %subj, error = %e, "remote ctrl subscribe failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        info!(subject = %subj, "remote control responder ready");

        use futures::StreamExt;
        while let Some(msg) = sub.next().await {
            let Some(reply) = msg.reply.clone() else {
                warn!(subject = %subj, "remote ctrl without reply subject — skipping");
                continue;
            };
            let ctrl: RemoteCtrl = match serde_json::from_slice(&msg.payload) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "remote ctrl: undecodable request");
                    let r = RemoteCtrlReply::refused(format!("undecodable request: {e}"));
                    respond(&client, reply, &r).await;
                    continue;
                }
            };

            let response = handle(&client, &live, &exe, ctrl).await;
            respond(&client, reply, &response).await;
        }
        warn!(subject = %subj, "remote ctrl subscription closed; reopening");
    }
}

async fn respond(client: &async_nats::Client, reply: async_nats::Subject, r: &RemoteCtrlReply) {
    match serde_json::to_vec(r) {
        Ok(bytes) => {
            if let Err(e) = client.publish(reply, bytes.into()).await {
                warn!(error = %e, "publish remote ctrl reply");
            }
        }
        Err(e) => warn!(error = %e, "serialize remote ctrl reply"),
    }
}

async fn handle(
    client: &async_nats::Client,
    live: &Arc<Mutex<Option<Live>>>,
    exe: &std::path::Path,
    ctrl: RemoteCtrl,
) -> RemoteCtrlReply {
    match ctrl {
        RemoteCtrl::Start {
            session_id,
            output_index,
            quality,
            max_fps,
            allow_input: _,
        } => {
            let mut guard = live.lock().await;
            match decide_start(guard.as_ref().map(|l| l.session_id.as_str()), &session_id) {
                StartOutcome::AlreadyRunning => {
                    // Idempotent, mirroring Stop. A lost reply or a viewer
                    // reconnecting re-issues the same Start, and refusing it
                    // would tell the operator their own session was someone
                    // else's — while the session they are asking for is in
                    // fact already running for them.
                    return RemoteCtrlReply {
                        accepted: true,
                        ..Default::default()
                    };
                }
                StartOutcome::HeldByOther(other) => {
                    return RemoteCtrlReply::refused(format!(
                        "session {other} already has this machine's display"
                    ));
                }
                StartOutcome::Start => {}
            }

            match start(client, exe, &session_id, output_index, quality, max_fps).await {
                Ok(l) => {
                    info!(session = %session_id, "remote session started");
                    *guard = Some(l);
                    // Screen geometry is deliberately not reported here: the
                    // child has not captured a frame yet, and waiting for one
                    // would stall the operator's click behind a display
                    // round-trip. Every tile repeats the screen size in its
                    // headers precisely so a viewer can size itself from the
                    // first one that arrives.
                    RemoteCtrlReply {
                        accepted: true,
                        reason: None,
                        screen_w: None,
                        screen_h: None,
                    }
                }
                Err(e) => {
                    warn!(session = %session_id, error = %e, "remote session failed to start");
                    RemoteCtrlReply::refused(format!("could not start capture: {e}"))
                }
            }
        }

        RemoteCtrl::Stop { session_id } => {
            let mut guard = live.lock().await;
            let held = guard.as_ref().map(|l| l.session_id.as_str());
            match decide_stop(held, &session_id) {
                StopOutcome::Stop => {
                    if let Some(l) = guard.take() {
                        l.stop();
                    }
                    info!(session = %session_id, "remote session stopped");
                    RemoteCtrlReply {
                        accepted: true,
                        ..Default::default()
                    }
                }
                StopOutcome::NothingToDo => RemoteCtrlReply {
                    accepted: true,
                    ..Default::default()
                },
                StopOutcome::HeldByOther(other) => RemoteCtrlReply::refused(format!(
                    "this machine holds session {other}, not {session_id}"
                )),
            }
        }

        // Retuning means restarting the child: quality and frame rate are
        // start-up arguments to it. Deferred rather than faked — a Tune that
        // silently did nothing would be worse than one that says so.
        RemoteCtrl::Tune { .. } => {
            RemoteCtrlReply::refused("tune is not implemented yet; stop and start instead")
        }
    }
}

/// What a `Start` request should do.
#[derive(Debug, PartialEq, Eq)]
enum StartOutcome {
    /// Nothing is running; spawn a capture child.
    Start,
    /// This exact session is already live; accept without doing anything.
    AlreadyRunning,
    /// A *different* session holds this machine — carries its id.
    HeldByOther(String),
}

/// Decide what a `Start` for `requested` should do given what is `held`.
///
/// The `AlreadyRunning` case is why this is not just a `is_some()` check.
/// `Start` has the same delivery problem `Stop` does: a reply can be lost,
/// and a reconnecting viewer re-issues its `Start`. Refusing that told the
/// operator their own session id "already has this machine's display" —
/// naming their session back at them as though a colleague had taken it,
/// while the thing they asked for was in fact already running.
///
/// Only a *different* id is a genuine conflict, and only that is refused.
fn decide_start(held: Option<&str>, requested: &str) -> StartOutcome {
    match held {
        Some(h) if h == requested => StartOutcome::AlreadyRunning,
        Some(h) => StartOutcome::HeldByOther(h.to_string()),
        None => StartOutcome::Start,
    }
}

/// What a `Stop` request should do.
#[derive(Debug, PartialEq, Eq)]
enum StopOutcome {
    /// Tear the session down.
    Stop,
    /// Nothing is running; report success anyway.
    NothingToDo,
    /// A *different* session holds this machine — carries its id.
    HeldByOther(String),
}

/// Decide what a `Stop` for `requested` should do given what is `held`.
///
/// Split out from the async handler because the safety property here is
/// worth testing directly: **a Stop must never tear down a session it did
/// not name**. Two operators can each hold a stale view of this machine, and
/// an unmatched Stop that fell through to "just stop whatever is running"
/// would let one of them silently kick the other off mid-session.
///
/// Stopping when nothing runs succeeds rather than erroring: the request is
/// idempotent, so a retry after a lost reply is safe and an operator closing
/// a tab twice is not a failure.
fn decide_stop(held: Option<&str>, requested: &str) -> StopOutcome {
    match held {
        Some(h) if h == requested => StopOutcome::Stop,
        Some(h) => StopOutcome::HeldByOther(h.to_string()),
        None => StopOutcome::NothingToDo,
    }
}

/// Spawn the capture child and the tasks that pump its output to NATS.
async fn start(
    client: &async_nats::Client,
    exe: &std::path::Path,
    session_id: &str,
    output_index: u32,
    quality: u8,
    max_fps: u8,
) -> anyhow::Result<Live> {
    // Clamped here rather than trusted. These arrive over the network: the
    // backend mints them from an operator's request, and the documented
    // 1-100 range is a contract, not an enforcement. `JpegEncoder` does not
    // validate quality either, and a zero fps would divide by zero in the
    // child's pacing. The CLI path already clamps; this path is the one a
    // buggy or hostile control message can drive.
    let quality = quality.clamp(1, 100).to_string();
    let max_fps = max_fps.max(1).to_string();
    let output_index = output_index.to_string();
    let exe = exe.to_path_buf();

    // The WTS token dance, CreateEnvironmentBlock and CreateProcessAsUserW
    // are blocking Win32 calls. Running them straight on a tokio worker
    // would stall whatever else is scheduled there — the ping responder, the
    // heartbeat — for the duration of process creation.
    // `session_supervisor` already wraps the identical call this way.
    let mut child = tokio::task::spawn_blocking(move || {
        let args = [
            "--session-capture",
            "--session-capture-quality",
            quality.as_str(),
            "--session-capture-max-fps",
            max_fps.as_str(),
            "--session-capture-output",
            output_index.as_str(),
        ];
        spawn_session_child(&exe, &args)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn join failed: {e}"))??;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| anyhow::anyhow!("capture child has no stdout"))?;

    // Bounded so a broker that stops accepting publishes applies back
    // pressure to the reader instead of growing a queue of stale frames in
    // memory. Dropping old frames would be defensible too, but a viewer
    // showing a consistent few-seconds-old picture beats one showing a
    // random mix of ages.
    let (tx, mut rx) = mpsc::channel::<Outbound>(8);

    // Win32 anonymous pipes are blocking-only, so the reader owns a thread.
    tokio::task::spawn_blocking(move || {
        let mut reader = PipeReader { handle: stdout };
        loop {
            match read_frame(&mut reader) {
                Ok(msg) => {
                    let out = if let Some(reason) = msg.as_gap() {
                        Outbound::Gap(reason)
                    } else if msg.is_resumed() {
                        Outbound::Resumed
                    } else {
                        Outbound::Tile {
                            header: Box::new(msg.header),
                            payload: msg.payload,
                        }
                    };
                    // blocking_send applies the back pressure described
                    // above; an error means the publisher is gone.
                    if tx.blocking_send(out).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    warn!(error = %e, "capture pipe framing error; ending session");
                    break;
                }
            }
        }
    });

    let client = client.clone();
    let frame_subject = subject::remote_frame(session_id);
    let sid = session_id.to_string();
    tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let publish = match out {
                Outbound::Tile { header, payload } => match *header {
                    FrameHeader::Tile { meta, encoding } => {
                        client
                            .publish_with_headers(
                                frame_subject.clone(),
                                meta.to_headers(encoding),
                                payload.into(),
                            )
                            .await
                    }
                    // The reader only builds Tile for messages that are
                    // neither a gap nor a resume marker.
                    FrameHeader::Gap | FrameHeader::Resumed => continue,
                },
                Outbound::Gap(reason) => {
                    client
                        .publish_with_headers(
                            frame_subject.clone(),
                            gap_headers(),
                            reason.into_bytes().into(),
                        )
                        .await
                }
                // Payload-free: the marker's whole content is its kind.
                Outbound::Resumed => {
                    client
                        .publish_with_headers(
                            frame_subject.clone(),
                            resumed_headers(),
                            bytes::Bytes::new(),
                        )
                        .await
                }
            };
            if let Err(e) = publish {
                warn!(session = %sid, error = %e, "publish remote frame");
            }
        }
        info!(session = %sid, "remote frame publisher ended");
    });

    Ok(Live {
        session_id: session_id.to_string(),
        child,
    })
}

/// `std::io::Read` over a Win32 pipe handle, so `read_frame` can drive it.
struct PipeReader {
    handle: std::os::windows::io::OwnedHandle,
}

impl std::io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError, HANDLE};
        use windows::Win32::Storage::FileSystem::ReadFile;

        let raw = HANDLE(self.handle.as_raw_handle() as isize as *mut core::ffi::c_void);
        let mut read: u32 = 0;
        // SAFETY: `buf` is a valid writable slice; `raw` is a live pipe
        // handle owned by this struct for the call's duration.
        let ok = unsafe { ReadFile(raw, Some(buf), Some(&mut read), None) };
        match ok {
            // Zero bytes on a blocking pipe read also means end of stream.
            Ok(()) => Ok(read as usize),
            Err(e) => {
                // ERROR_BROKEN_PIPE is how a closed write end normally
                // surfaces — the child exited, which is this process's
                // ordinary end of life, not a failure. Everything else
                // (invalid handle, out of memory) is real, and collapsing
                // it into the same silent EOF would make a broken session
                // indistinguishable from a finished one in the logs.
                // Matches the discrimination `process_as_user::read_lines`
                // already does for the idle sensor's pipe.
                // SAFETY: no args; reads this thread's last error code.
                let code = unsafe { GetLastError() };
                if code != ERROR_BROKEN_PIPE {
                    warn!(error = %e, ?code, "capture pipe read failed");
                }
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_on_an_idle_machine_starts() {
        assert_eq!(decide_start(None, "s1"), StartOutcome::Start);
    }

    #[test]
    fn restarting_the_same_session_is_idempotent() {
        // The bug this fixes: a lost reply or a viewer reconnect re-issues
        // the same Start, and refusing it reported the caller's own session
        // as the one holding the machine.
        assert_eq!(decide_start(Some("s1"), "s1"), StartOutcome::AlreadyRunning);
    }

    #[test]
    fn start_while_another_session_holds_the_display_is_refused() {
        // The genuine conflict — DXGI allows one duplication per output.
        assert_eq!(
            decide_start(Some("s1"), "s2"),
            StartOutcome::HeldByOther("s1".to_string())
        );
    }

    #[test]
    fn start_matches_session_ids_exactly() {
        assert!(matches!(
            decide_start(Some("session-1"), "session-10"),
            StartOutcome::HeldByOther(_)
        ));
        assert!(matches!(
            decide_start(Some("S1"), "s1"),
            StartOutcome::HeldByOther(_)
        ));
    }

    #[test]
    fn start_and_stop_agree_on_what_counts_as_the_same_session() {
        // The two decisions must not disagree: a Start that considers a
        // session "already running" and a Stop that considers the same id
        // "held by other" would leave a session nobody can end.
        for (held, req) in [("s1", "s1"), ("s1", "s2"), ("session-1", "session-10")] {
            let same_for_start =
                matches!(decide_start(Some(held), req), StartOutcome::AlreadyRunning);
            let same_for_stop = matches!(decide_stop(Some(held), req), StopOutcome::Stop);
            assert_eq!(
                same_for_start, same_for_stop,
                "disagreed on {held} vs {req}"
            );
        }
    }

    #[test]
    fn stop_tears_down_the_session_it_names() {
        assert_eq!(decide_stop(Some("s1"), "s1"), StopOutcome::Stop);
    }

    #[test]
    fn stop_for_an_unheld_session_never_touches_the_live_one() {
        // The property this exists for: an operator's stale Stop must not
        // kick a colleague off the machine.
        assert_eq!(
            decide_stop(Some("s1"), "s2"),
            StopOutcome::HeldByOther("s1".to_string())
        );
    }

    #[test]
    fn stop_with_nothing_running_succeeds() {
        // Idempotent: a retry after a lost reply, or a tab closed twice.
        assert_eq!(decide_stop(None, "s1"), StopOutcome::NothingToDo);
    }

    #[test]
    fn session_ids_are_matched_exactly() {
        // Not a prefix or case-insensitive match — ids come from the
        // backend and a near-miss means a different session.
        assert!(matches!(
            decide_stop(Some("session-1"), "session-10"),
            StopOutcome::HeldByOther(_)
        ));
        assert!(matches!(
            decide_stop(Some("S1"), "s1"),
            StopOutcome::HeldByOther(_)
        ));
    }
}
