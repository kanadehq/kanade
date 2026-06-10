//! Client-side KLP transport — Named Pipe client + framed JSON-RPC.
//!
//! Owned by the Tauri runtime (one [`KlpClient`] per app instance,
//! shared via `tauri::State`). On startup, [`KlpClient::connect`]
//! opens the agent's Named Pipe (`\\.\pipe\kanade-agent`), runs the
//! SPEC §2.12.6 handshake, then **splits the pipe** into a write half
//! (guarded by a mutex for serialised sends) and a read half owned by
//! a permanent reader task.
//!
//! # Reader-task demultiplex
//!
//! The agent interleaves two kinds of agent→client frames on one pipe
//! (SPEC §2.12.3): correlated `Response`s and unsolicited push
//! `Notification`s (`jobs.progress`, `state.changed`, …). The reader
//! task reads every frame and routes it:
//!
//! - `Response` → the matching in-flight [`KlpClient::request`] call,
//!   looked up by id in the `pending` map and delivered over a
//!   oneshot.
//! - `Notification` → the `notifications` broadcast, which the Tauri
//!   layer forwards to the WebView as an event (see
//!   [`KlpClient::subscribe`]).
//!
//! This split is what lets pushes arrive while the app is idle (the
//! old inline-read model only read the pipe during a request, so a
//! `jobs.progress` between requests was invisible) and lets concurrent
//! requests multiplex over the one pipe (each correlates its own id
//! instead of assuming the next frame is its reply).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kanade_shared::ipc::envelope::{
    JSONRPC_VERSION, RpcMessage, RpcNotification, RpcRequest, RpcResponse, RpcResponsePayload,
};
use kanade_shared::ipc::handshake::{HandshakeParams, HandshakeResult, PROTOCOL_V1};
use kanade_shared::ipc::jobs::{
    JobsExecuteParams, JobsExecuteResult, JobsKillParams, JobsKillResult, JobsListParams,
    JobsListResult,
};
use kanade_shared::ipc::method;
use kanade_shared::ipc::state::{StateSnapshot, StateSnapshotParams};
use kanade_shared::ipc::system::{PingParams, PingResult};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf, split};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::{Mutex, broadcast, oneshot, watch};
use tracing::{debug, info, warn};

/// SPEC §2.12.1 — the well-known Named Pipe the agent listens on.
const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";

/// SPEC §2.12.2 — 4-byte LE length prefix; 1 MiB cap.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Bounded capacity of the push-notification broadcast. A slow
/// WebView subscriber that falls this far behind drops the oldest
/// notifications (broadcast lag) rather than stalling the reader task
/// — progress UX, not a transactional stream.
const NOTIFICATION_CAPACITY: usize = 256;

/// Client-side product identifier sent in the handshake. Surfaced
/// in the agent's audit log + tracing spans (SPEC §2.12.12).
const CLIENT_NAME: &str = "kanade-client";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-request deadline for [`KlpClient::request`] (#469). A dropped
/// connection already unblocks a waiter (the reader clears `pending`,
/// so the oneshot sender drops and the await errors), so this only
/// bounds the "connection alive, agent silent on this id" case — a
/// wedged agent-side handler. Generous on purpose: every v1 handler
/// replies promptly (`jobs.execute` returns a `run_id` immediately and
/// streams progress as async pushes), so no legitimate request blocks
/// on long work; 30 s is far past any real reply latency.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// In-flight request registry: request id → the oneshot the reader
/// task fulfils when the matching `Response` arrives.
///
/// `std::sync::Mutex` (not the tokio one used for `write`): it only
/// ever guards fast in-memory `HashMap` ops — never held across an
/// await — so a sync mutex is both correct and lets [`PendingGuard`]
/// clean up a cancelled request from inside `Drop` (a tokio mutex
/// can't be locked synchronously there).
type Pending = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>>;

/// RAII cleanup for one in-flight request's `pending` entry. Removes
/// the id on drop — whether the request completed, errored, or its
/// future was cancelled (dropped before the response arrived) — so a
/// cancelled or write-failed request can't leak its `oneshot::Sender`
/// in the map until the connection closes. A successful response was
/// already removed by the reader, so the drop-time removal is then a
/// harmless no-op.
struct PendingGuard {
    pending: Pending,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// Shared KLP connection. Cheap to clone (everything behind `Arc`),
/// so each Tauri command clones it out of `AppState` and runs its
/// round-trip without holding the app-state mutex across the await.
#[derive(Clone)]
pub struct KlpClient {
    /// Write half of the split pipe. Mutex-guarded so concurrent
    /// requests serialise their frame writes (a half-written frame
    /// would desync the agent's reader).
    write: Arc<Mutex<WriteHalf<NamedPipeClient>>>,
    /// In-flight requests awaiting a correlated response.
    pending: Pending,
    /// Cached handshake result (one-shot per connection).
    handshake: Arc<HandshakeResult>,
    /// Push-notification fan-out. The reader task is the sole sender;
    /// the Tauri layer subscribes and forwards to the WebView.
    notifications: broadcast::Sender<RpcNotification>,
    /// Flips to `true` when the reader task exits (pipe closed — agent
    /// restart / crash). The Tauri supervisor awaits this via
    /// [`KlpClient::wait_closed`] to drive reconnection (#468).
    closed: watch::Receiver<bool>,
}

impl KlpClient {
    /// Open the pipe, run the SPEC §2.12.6 handshake, split the pipe,
    /// and spawn the reader task. Bubbles up the underlying error so
    /// the Tauri `setup` callback can surface a clear "agent not
    /// running" banner if the pipe isn't there yet.
    pub async fn connect() -> Result<Self> {
        let mut pipe = ClientOptions::new()
            .open(PIPE_NAME)
            .with_context(|| format!("open Named Pipe {PIPE_NAME}"))?;
        info!(pipe = PIPE_NAME, "KLP client: pipe connected");

        // Handshake inline on the un-split pipe — it's the one
        // strictly-ordered request/response exchange (must complete
        // before any push can arrive), so the reader task only takes
        // over afterwards.
        let handshake = handshake(&mut pipe).await.context("KLP handshake")?;
        info!(
            agent_version = %handshake.agent_version,
            user = %handshake.session.user,
            session_id = handshake.session.session_id,
            pc_id = %handshake.session.pc_id,
            "KLP client: handshake complete",
        );

        let (read, write) = split(pipe);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        // `closed` flips true when the reader task exits, so the
        // supervisor can reconnect (#468).
        let (closed_tx, closed_rx) = watch::channel(false);
        tokio::spawn(reader_loop(
            read,
            pending.clone(),
            notifications.clone(),
            closed_tx,
        ));

        Ok(Self {
            write: Arc::new(Mutex::new(write)),
            pending,
            handshake: Arc::new(handshake),
            notifications,
            closed: closed_rx,
        })
    }

    /// Resolve once the connection has closed (the reader task exited —
    /// the agent's pipe went away). The Tauri supervisor awaits this to
    /// trigger reconnection. Returns immediately if already closed.
    pub async fn wait_closed(&self) {
        let mut rx = self.closed.clone();
        // `wait_for` returns Ok when the predicate holds; Err means the
        // sender dropped (reader gone) — both mean "closed".
        let _ = rx.wait_for(|&closed| closed).await;
    }

    /// Cached handshake result. Returned to the UI on every
    /// `get_handshake` invoke — handshake is one-shot per
    /// connection so the cache is the source of truth.
    pub fn handshake(&self) -> Arc<HandshakeResult> {
        self.handshake.clone()
    }

    /// Subscribe to agent→client push notifications (`jobs.progress`,
    /// `state.changed`, …). The Tauri layer drains this and re-emits
    /// each notification to the WebView as a `klp://notification`
    /// event. A subscriber only receives notifications sent *after*
    /// it subscribes (broadcast semantics) — subscribe right after
    /// `connect` so no early progress is missed.
    pub fn subscribe(&self) -> broadcast::Receiver<RpcNotification> {
        self.notifications.subscribe()
    }

    /// Round-trip one request through the pipe. Mints a UUID id,
    /// registers a pending waiter, writes the frame, and awaits the
    /// reader task's correlated response.
    ///
    /// `params` is the typed request payload; `R` is the per-method
    /// result struct (e.g. [`PingResult`]).
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        // SPEC §2.12.3 prefers UUID v7 (time-sortable, easier log
        // correlation); the workspace's `uuid` feature set doesn't
        // enable v7 today so we use v4. Switch when the workspace pin
        // grows the `v7` feature.
        let id = uuid::Uuid::new_v4().to_string();
        let req = RpcRequest::new(&id, method, params).context("encode KLP request")?;
        let body = serde_json::to_vec(&req).context("serialise KLP request")?;

        // Register the waiter BEFORE writing so a fast reply can't
        // arrive before we're listening for it.
        let rx = {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id.clone(), tx);
            rx
        };
        // From here on, any early return / cancellation removes the
        // pending entry (no leaked sender). The reader removes it on a
        // successful delivery, making this drop a no-op then.
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id: id.clone(),
        };

        // Write under the (async) write lock, then release it before
        // awaiting the response so the next request can write while
        // this one waits.
        let write_result = {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &body).await
        };
        write_result.context("write frame")?;

        // The reader task delivers the id-matched response (or drops
        // the sender on disconnect, which surfaces as RecvError).
        //
        // Bounded by REQUEST_TIMEOUT (#469): a live-but-silent agent
        // (wedged handler that never replies to THIS id) would otherwise
        // await forever. On timeout, `_guard` drops the pending entry so
        // a late reply is discarded rather than delivered to a gone
        // waiter. A disconnect still unblocks faster via RecvError.
        let resp = await_response(rx, method, &id).await?;
        decode_response::<R>(resp)
    }

    /// Convenience wrapper for `system.ping`.
    pub async fn ping(&self) -> Result<PingResult> {
        self.request::<PingParams, PingResult>(method::SYSTEM_PING, &PingParams::default())
            .await
    }

    /// `state.snapshot` — the full endpoint health bundle the Health
    /// tab renders (#290).
    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request::<StateSnapshotParams, StateSnapshot>(
            method::STATE_SNAPSHOT,
            &StateSnapshotParams::default(),
        )
        .await
    }

    /// `jobs.list` — the user-invokable job catalog for the three job
    /// tabs (#291). `params.category` narrows to one tab.
    pub async fn jobs_list(&self, params: &JobsListParams) -> Result<JobsListResult> {
        self.request::<JobsListParams, JobsListResult>(method::JOBS_LIST, params)
            .await
    }

    /// `jobs.execute` — run a user-invokable job by id; returns the
    /// `run_id` whose `jobs.progress` pushes arrive via
    /// [`KlpClient::subscribe`] (#291).
    pub async fn jobs_execute(&self, id: &str) -> Result<JobsExecuteResult> {
        self.request::<JobsExecuteParams, JobsExecuteResult>(
            method::JOBS_EXECUTE,
            &JobsExecuteParams { id: id.to_string() },
        )
        .await
    }

    /// `jobs.kill` — request termination of a run this connection
    /// started (#291).
    pub async fn jobs_kill(&self, run_id: &str) -> Result<JobsKillResult> {
        self.request::<JobsKillParams, JobsKillResult>(
            method::JOBS_KILL,
            &JobsKillParams {
                run_id: run_id.to_string(),
            },
        )
        .await
    }
}

/// Permanent reader task: demultiplex every agent→client frame.
/// Generic over the read half so it's unit-testable with an in-memory
/// duplex pipe. Exits when the pipe closes / errors; on exit it drops
/// every pending sender so in-flight `request` calls fail fast instead
/// of hanging forever.
async fn reader_loop<R: AsyncRead + Unpin>(
    mut read: R,
    pending: Pending,
    notifications: broadcast::Sender<RpcNotification>,
    closed_tx: watch::Sender<bool>,
) {
    loop {
        let bytes = match read_frame(&mut read).await {
            Ok(b) => b,
            Err(e) => {
                debug!(error = %e, "klp reader: pipe closed, exiting");
                break;
            }
        };
        let msg: RpcMessage = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "klp reader: undecodable frame, skipping");
                continue;
            }
        };
        match msg {
            RpcMessage::Response(resp) => match resp.id.as_deref() {
                Some(id) => {
                    // Remove (releasing the lock) BEFORE sending so the
                    // sync mutex isn't held across the oneshot send.
                    let waiter = pending.lock().unwrap().remove(id);
                    match waiter {
                        // Receiver dropped (request cancelled) → ignore.
                        Some(tx) => {
                            let _ = tx.send(resp);
                        }
                        None => {
                            debug!(id, "klp reader: response for unknown/expired request")
                        }
                    }
                }
                None => debug!("klp reader: response without id, ignoring"),
            },
            RpcMessage::Notification(notif) => {
                // Err only when there are no live subscribers — fine,
                // a push with nobody listening is just dropped.
                let _ = notifications.send(notif);
            }
            RpcMessage::Request(_) => {
                debug!("klp reader: agent sent a Request (unexpected), ignoring");
            }
        }
    }
    // Connection gone: fail every in-flight request rather than leave
    // it awaiting a oneshot that will never resolve.
    pending.lock().unwrap().clear();
    // Signal the supervisor to reconnect (#468). Ignore the error —
    // a dropped receiver just means the KlpClient is already gone.
    let _ = closed_tx.send(true);
}

/// Pull the typed result out of an [`RpcResponse`]; map error
/// envelopes to `anyhow::Error` with the SPEC §2.12.9 detail
/// preserved so the UI sees the same message the agent logged.
fn decode_response<R: DeserializeOwned>(resp: RpcResponse) -> Result<R> {
    match resp.payload {
        RpcResponsePayload::Ok { result } => {
            serde_json::from_value(result).context("decode typed result")
        }
        RpcResponsePayload::Err { error } => {
            let detail = error
                .data
                .as_ref()
                .map(|d| d.detail.clone())
                .unwrap_or_default();
            bail!("KLP error {} ({}): {detail}", error.code, error.message);
        }
    }
}

/// Await the reader task's correlated response under the
/// [`REQUEST_TIMEOUT`] deadline (#469). Three outcomes:
///
/// - reply delivered → `Ok(resp)`;
/// - oneshot sender dropped (connection closed, reader cleared
///   `pending`) → a "closed before response" error;
/// - deadline elapsed (connection alive, agent silent on this id) →
///   a "did not respond within Ns" error.
///
/// Split out of [`KlpClient::request`] so the timeout/closed branches
/// are unit-testable without a live pipe.
async fn await_response(
    rx: oneshot::Receiver<RpcResponse>,
    method: &str,
    id: &str,
) -> Result<RpcResponse> {
    match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(_)) => bail!("KLP connection closed before response (id {id})"),
        Err(_) => bail!(
            "KLP agent did not respond to {method} within {}s (id {id})",
            REQUEST_TIMEOUT.as_secs(),
        ),
    }
}

async fn handshake(pipe: &mut NamedPipeClient) -> Result<HandshakeResult> {
    // Same v4-instead-of-v7 note as in `KlpClient::request`.
    let id = uuid::Uuid::new_v4().to_string();
    let req = RpcRequest::new(
        &id,
        method::SYSTEM_HANDSHAKE,
        &HandshakeParams {
            client: CLIENT_NAME.to_string(),
            client_version: CLIENT_VERSION.to_string(),
            protocol: vec![PROTOCOL_V1],
            features: vec![],
        },
    )
    .context("encode handshake request")?;
    let body = serde_json::to_vec(&req).context("serialise handshake request")?;
    write_frame(pipe, &body).await.context("write handshake")?;
    let resp_bytes = read_frame(pipe).await.context("read handshake response")?;
    let msg: RpcMessage = serde_json::from_slice(&resp_bytes).context("decode envelope")?;
    let RpcMessage::Response(resp) = msg else {
        bail!("expected handshake Response, got {msg:?}");
    };
    if resp.id.as_deref() != Some(id.as_str()) {
        bail!(
            "handshake response id mismatch: expected {id:?}, got {:?}",
            resp.id
        );
    }
    if resp.jsonrpc != JSONRPC_VERSION {
        debug!(jsonrpc = %resp.jsonrpc, "unexpected jsonrpc field (proceeding)");
    }
    decode_response::<HandshakeResult>(resp)
}

// ---- Framing (mirror of `kanade_agent::klp::framing`) ----

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("KLP frame {len} bytes exceeds 1 MiB cap"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("KLP frame {} bytes exceeds 1 MiB cap", body.len()),
        ));
    }
    let len = (body.len() as u32).to_le_bytes();
    writer.write_all(&len).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::ipc::jobs::{JobProgress, RunStatus};

    /// Encode a JSON value as a length-prefixed frame (mirror of the
    /// agent writer, for tests).
    async fn push_frame<W: AsyncWrite + Unpin>(w: &mut W, value: &serde_json::Value) {
        let body = serde_json::to_vec(value).unwrap();
        write_frame(w, &body).await.unwrap();
    }

    #[tokio::test]
    async fn reader_routes_response_to_pending_request() {
        // A duplex pipe stands in for the Named Pipe: we write agent
        // frames into `agent_side`, the reader reads `client_side`.
        let (client_side, mut agent_side) = tokio::io::duplex(64 * 1024);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (notifications, _rx) = broadcast::channel(16);

        // Pre-register a waiter for id "req-1", as `request` would.
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert("req-1".into(), tx);

        let (closed_tx, _closed_rx) = watch::channel(false);
        tokio::spawn(reader_loop(
            client_side,
            pending.clone(),
            notifications,
            closed_tx,
        ));

        push_frame(
            &mut agent_side,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": "req-1",
                "result": { "run_id": "run-xyz" }
            }),
        )
        .await;

        let resp = rx.await.expect("reader should deliver the response");
        assert_eq!(resp.id.as_deref(), Some("req-1"));
        assert!(pending.lock().unwrap().is_empty(), "pending entry consumed");
    }

    #[tokio::test]
    async fn reader_forwards_notification_to_subscribers() {
        let (client_side, mut agent_side) = tokio::io::duplex(64 * 1024);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (notifications, mut sub) = broadcast::channel(16);

        let (closed_tx, _closed_rx) = watch::channel(false);
        tokio::spawn(reader_loop(client_side, pending, notifications, closed_tx));

        let progress = JobProgress {
            run_id: "run-1".into(),
            status: RunStatus::Running,
            stdout_chunk: None,
            stderr_chunk: None,
            exit_code: None,
        };
        let notif = RpcNotification::new(method::JOBS_PROGRESS, &progress).unwrap();
        push_frame(&mut agent_side, &serde_json::to_value(&notif).unwrap()).await;

        let got = sub.recv().await.expect("notification forwarded");
        assert_eq!(got.method, method::JOBS_PROGRESS);
        assert_eq!(got.params["run_id"], "run-1");
        assert_eq!(got.params["status"], "running");
    }

    #[tokio::test]
    async fn reader_exit_fails_pending_requests() {
        // When the pipe closes, in-flight requests must error out
        // (oneshot sender dropped) rather than hang forever.
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (notifications, _rx) = broadcast::channel(16);

        let (tx, rx) = oneshot::channel::<RpcResponse>();
        pending.lock().unwrap().insert("req-orphan".into(), tx);

        let (closed_tx, mut closed_rx) = watch::channel(false);
        let handle = tokio::spawn(reader_loop(client_side, pending, notifications, closed_tx));

        // Drop the agent side → EOF → reader exits → clears pending +
        // signals closed.
        drop(agent_side);
        handle.await.unwrap();

        assert!(
            rx.await.is_err(),
            "pending request should be failed, not hung"
        );
        // The reader signalled the supervisor that the connection died.
        assert!(
            closed_rx.wait_for(|&c| c).await.is_ok(),
            "reader must signal closed on exit (#468 reconnect trigger)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_response_times_out_when_agent_silent() {
        // Hold the sender so the oneshot never resolves: the agent is
        // alive but never replies to this id (#469). Under
        // `start_paused = true` the runtime auto-advances the paused
        // clock to the next pending deadline once this task is the only
        // thing blocking, so `timeout`'s `Sleep` fires deterministically
        // and instantly — no explicit `tokio::time::advance` needed.
        let (_tx, rx) = oneshot::channel::<RpcResponse>();

        let err = await_response(rx, method::JOBS_EXECUTE, "req-wedged")
            .await
            .expect_err("silent agent must time out");
        let msg = err.to_string();
        assert!(msg.contains("did not respond"), "got: {msg}");
        assert!(msg.contains(method::JOBS_EXECUTE), "names method: {msg}");
    }

    #[tokio::test(start_paused = true)]
    async fn await_response_reports_closed_connection() {
        // Sender dropped before any reply → the reader cleared `pending`
        // on disconnect. This must surface as "closed", distinct from a
        // timeout, and resolve immediately (no waiting out the deadline).
        let (tx, rx) = oneshot::channel::<RpcResponse>();
        drop(tx);

        let err = await_response(rx, method::SYSTEM_PING, "req-gone")
            .await
            .expect_err("dropped sender must error");
        assert!(err.to_string().contains("closed"), "got: {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn await_response_delivers_reply() {
        let (tx, rx) = oneshot::channel::<RpcResponse>();
        let resp = RpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some("req-ok".into()),
            payload: RpcResponsePayload::Ok {
                result: serde_json::json!({ "ok": true }),
            },
        };
        tx.send(resp).unwrap();

        let got = await_response(rx, method::SYSTEM_PING, "req-ok")
            .await
            .expect("delivered reply");
        assert_eq!(got.id.as_deref(), Some("req-ok"));
    }
}
