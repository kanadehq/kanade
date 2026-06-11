//! Windows Named Pipe listener for KLP (SPEC §2.12.1).
//!
//! One listener, multiple concurrent connections (FUS / RDP / a
//! few admin tabs all welcome). Standard tokio Named Pipe re-arm
//! pattern: create initial instance → `connect()` blocks until a
//! client shows up → re-create the next instance immediately so
//! the next connect lands without a race → spawn the connected
//! pipe into a per-connection task.
//!
//! Linux UDS (SPEC §2.12.1's `/run/kanade/agent.sock`) lives in a
//! follow-up PR; the entire `klp` module is `#[cfg(target_os =
//! "windows")]` gated in `main.rs` until that lands. Non-Windows
//! CI builds skip the module rather than compile cross-platform
//! scaffolding that the production code path can't reach (avoids
//! dead-code warnings on clippy's Linux/macOS jobs).
//!
//! Security descriptor: every pipe instance is created with the
//! SDDL-derived [`PipeSecurity`] (`Authenticated Users RW, deny
//! Anonymous` per SPEC §2.12.1). The SD is built once on listener
//! startup and reused for every re-arm because Windows copies the
//! SD into each new pipe handle internally
//! (`CreateNamedPipe`-with-`lpSecurityAttributes`). See
//! `crate::klp::security` for the SDDL breakdown.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use kanade_shared::ipc::envelope::{RpcMessage, RpcResponse};
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::notifications::Notification;
use kanade_shared::ipc::state::StateSnapshot;
use kanade_shared::wire::EffectiveConfig;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::klp::auth::resolve_peer;
use crate::klp::connection::ConnectionState;
use crate::klp::dispatcher::dispatch_request;
use crate::klp::framing::{read_frame, write_frame};
use crate::klp::security::PipeSecurity;

/// SPEC §2.12.1 — Windows Named Pipe endpoint.
pub const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";

/// Bounded capacity of the per-connection push channel that bridges
/// dispatcher responses + subscription forwarders to the writer
/// task. Small enough that a runaway forwarder can't OOM the
/// agent; large enough that bursty state-change pushes don't
/// stall a healthy client.
const PUSH_CHANNEL_CAPACITY: usize = 64;

/// Shared configuration injected into every spawned per-connection
/// task. Kept small and cheap to clone so each connection gets its
/// own copy without lifetime gymnastics: `Arc<str>` strings,
/// `watch::Receiver` (Arc-backed) for the live config + state
/// views, and a per-conn `PathBuf` (one allocation per accept,
/// fine — handlers don't run in tight loops).
#[derive(Clone)]
pub struct ListenerContext {
    pub pc_id: Arc<str>,
    pub agent_version: Arc<str>,
    /// Live view of the agent's effective config (Sprint 6's
    /// supervisor watch channel). `system.version` reads the
    /// current `target_version`; future handlers will read other
    /// fields.
    pub config_rx: watch::Receiver<EffectiveConfig>,
    /// Live view of the latest endpoint state snapshot produced
    /// by `klp::state::eval_loop`. `state.snapshot` returns
    /// `borrow().clone()`; `state.subscribe`'s forwarder awaits
    /// `changed()` and pushes a `state.changed` notification per
    /// tick.
    pub state_rx: watch::Receiver<StateSnapshot>,
    /// On-disk path to the log-file template (the bare
    /// `cfg.log.path`; the active file resolves to
    /// `<stem>.YYYY-MM-DD.<ext>` via
    /// [`crate::logs::locate_active_file`]). `system.log_tail`
    /// reads it to bundle recent log lines into a support
    /// response.
    pub log_path: PathBuf,
    /// NATS client handed to each connection so cold-path KV
    /// handlers (`jobs.list`) can re-read the `BUCKET_JOBS` catalog
    /// at fire time. Cheap to clone (`async_nats::Client` is
    /// Arc-backed) and reconnect-resilient, so the listener never
    /// holds a stale KV handle.
    pub nats: async_nats::Client,
    /// Process-wide notification broadcast sender (KLP Phase E). Fed
    /// by [`crate::klp::notify_bus`]; each connection derives a fresh
    /// receiver via `ConnectionState::notif_subscribe` when the client
    /// calls `notifications.subscribe`. A `broadcast::Sender` is cheap
    /// to clone (Arc-backed).
    pub notif_tx: broadcast::Sender<Notification>,
}

/// Spawn the KLP listener. Returns immediately with a detached
/// `JoinHandle`; the loop runs forever inside the spawned task.
///
/// The foundation PR has no graceful-shutdown path. A future PR
/// adds a `CancellationToken` once we have a use case (e.g.
/// service stop must drain in-flight handlers).
pub fn spawn(ctx: ListenerContext) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(run(ctx))
}

async fn run(ctx: ListenerContext) -> Result<()> {
    // Build the SECURITY_DESCRIPTOR once and reuse for every
    // pipe instance — Windows copies the SD into each new pipe
    // handle internally, so the same SA pointer is safe across
    // arbitrarily many `create` calls. Built before any pipe
    // operations so a malformed SDDL fails fast at agent startup
    // instead of mid-loop.
    let security = PipeSecurity::new().context("build KLP pipe SECURITY_DESCRIPTOR")?;

    // `first_pipe_instance(true)` makes the initial create fail
    // loudly if another process is squatting the pipe name —
    // safer than silently sharing a name. This one creation IS
    // allowed to bubble up because there's no working state yet
    // and the agent operator should see the failure on startup.
    //
    // SAFETY: `security.as_ptr()` is valid for the duration of
    // this synchronous call; `security` is borrowed (not moved)
    // through the loop so the pointer stays valid.
    let mut server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(PIPE_NAME, security.as_ptr())
    }
    .with_context(|| format!("create Named Pipe {PIPE_NAME}"))?;
    info!(
        pipe = PIPE_NAME,
        sd = "Authenticated Users RW, deny Anonymous",
        "KLP listener ready",
    );

    loop {
        if let Err(e) = server.connect().await {
            warn!(error = %e, "KLP server.connect() failed; reseating listener");
            // A connect failure usually means the current handle
            // is broken; reseat with the same retry policy used
            // for re-arm below so the listener doesn't die from a
            // transient OS hiccup.
            server = create_with_retry(&security).await;
            continue;
        }

        // Re-arm BEFORE spawning the connection task so the next
        // client doesn't see a brief "no listener" window.
        let next = create_with_retry(&security).await;
        let connected = std::mem::replace(&mut server, next);

        let task_ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(connected, task_ctx).await {
                warn!(error = %e, "KLP connection task failed");
            }
        });
    }
}

/// Re-create the Named Pipe instance, retrying with bounded
/// exponential backoff on transient failures. Returns only on
/// success — the listener task MUST stay alive for the agent's
/// lifetime, so a propagated `?` exit (foundation PR's earlier
/// approach) would let a momentary OS-resource pressure (handle
/// table full, etc.) permanently kill the KLP transport with no
/// path back short of an agent restart.
///
/// Backoff schedule: 200 ms, 400 ms, 800 ms, … capped at 30 s.
/// Logs each failure at WARN so operators can spot a persistent
/// issue in the agent log instead of a silent stall.
async fn create_with_retry(security: &PipeSecurity) -> NamedPipeServer {
    let mut delay_ms: u64 = 200;
    loop {
        // SAFETY: `security.as_ptr()` is valid for the duration
        // of this synchronous call (the caller borrows
        // `security` through the loop body).
        let result = unsafe {
            ServerOptions::new().create_with_security_attributes_raw(PIPE_NAME, security.as_ptr())
        };
        match result {
            Ok(server) => return server,
            Err(e) => {
                warn!(
                    error = %e,
                    delay_ms,
                    pipe = PIPE_NAME,
                    "KLP create() failed; backing off and retrying",
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(30_000);
            }
        }
    }
}

async fn handle_connection(pipe: NamedPipeServer, ctx: ListenerContext) -> Result<()> {
    // Auth BEFORE any I/O so the per-connection state is correct
    // from the very first frame.
    let peer = match resolve_peer(&pipe) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "KLP peer auth failed; closing connection");
            return Ok(());
        }
    };
    debug!(
        user = %peer.user,
        session_id = peer.session_id,
        "KLP peer connected",
    );

    // Split the pipe so the read loop can decode requests while
    // the writer task (and subscription forwarders) push outbound
    // frames concurrently. SPEC §2.12.3's notification path
    // requires this — without it, a long-running handler would
    // block any push from getting out.
    let (reader, writer) = tokio::io::split(pipe);
    let (push_tx, push_rx) = mpsc::channel::<Vec<u8>>(PUSH_CHANNEL_CAPACITY);

    let writer_log_pc = ctx.pc_id.to_string();
    let writer_handle = tokio::spawn(writer_task(writer, push_rx, writer_log_pc));

    let mut conn = ConnectionState::new(
        peer,
        ctx.pc_id.to_string(),
        ctx.agent_version.to_string(),
        ctx.config_rx.clone(),
        ctx.state_rx.clone(),
        ctx.log_path.clone(),
        push_tx.clone(),
    )
    .with_nats(ctx.nats.clone())
    .with_notifications(ctx.notif_tx.clone());

    let read_loop_result = run_read_loop(reader, &mut conn, &push_tx).await;

    // Tear down in order so the writer can drain its queue
    // cleanly:
    // 1. Drop the local `push_tx` clone (the read loop's handle).
    // 2. Drop `conn` — `SubscriptionRegistry::Drop` aborts each
    //    forwarder task, which causes their `push_tx` clones to
    //    drop on the next runtime poll.
    // 3. `await` the writer — its `push_rx.recv()` returns `None`
    //    once every sender has dropped, and only THEN does the
    //    writer exit. That order is what lets a parse / oversize
    //    error queued just before the read loop exits actually
    //    reach the client; an `abort()` here would discard it.
    drop(push_tx);
    drop(conn);
    let _ = writer_handle.await;

    read_loop_result
}

/// Per-connection read loop. Decodes inbound frames, runs the
/// dispatcher on requests, and pushes the response onto `push_tx`
/// (the writer task drains and writes to the pipe). Returns when
/// the client disconnects, the pipe errors out, or the writer
/// task exits (push_tx send returns Err).
async fn run_read_loop(
    mut reader: ReadHalf<NamedPipeServer>,
    conn: &mut ConnectionState,
    push_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!(user = %conn.peer.user, "KLP client disconnected (EOF)");
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Only `read_frame`'s oversize-header rejection
                // arrives as `InvalidData` (see framing.rs). Tell
                // the client they overflowed the 1 MiB cap so
                // they can split into `stdout_chunk`s next time.
                warn!(error = %e, "KLP oversize frame; closing connection");
                let _ =
                    push_anonymous_error(push_tx, ErrorKind::PayloadTooLarge, &e.to_string()).await;
                return Ok(());
            }
            Err(e) => {
                // ConnectionReset / ConnectionAborted / generic
                // I/O errors mean the pipe is already dead;
                // trying to push a response would just queue a
                // frame the writer can't deliver. Close silently.
                debug!(
                    error = %e,
                    user = %conn.peer.user,
                    "KLP connection torn down by I/O error",
                );
                return Ok(());
            }
        };

        let msg: RpcMessage = match serde_json::from_slice(&frame) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "KLP JSON parse error");
                let _ = push_anonymous_error(push_tx, ErrorKind::ParseError, &e.to_string()).await;
                // SPEC §2.12 doesn't require closing on parse
                // error; staying open lets the client recover
                // by sending a well-formed frame next.
                continue;
            }
        };

        match msg {
            RpcMessage::Request(req) => {
                let resp = dispatch_request(conn, req).await;
                let body = serde_json::to_vec(&resp).context("encode RpcResponse")?;
                if push_tx.send(body).await.is_err() {
                    // Writer task exited (pipe broken). No point
                    // trying to deliver further responses.
                    debug!(user = %conn.peer.user, "KLP push channel closed, exiting read loop");
                    return Ok(());
                }
            }
            RpcMessage::Notification(notif) => {
                // SPEC §2.12.3: notifications get no response.
                // v1 has no client → agent notifications, but
                // we route the same way for future-proofing.
                debug!(method = %notif.method, "KLP notification received (no response)");
            }
            RpcMessage::Response(resp) => {
                // Server-side shouldn't receive responses today
                // — the agent doesn't initiate requests. Once
                // client-side push lands, this stays a debug log
                // (push responses aren't expected either).
                debug!(id = ?resp.id, "KLP unexpected client → agent response, ignoring");
            }
        }
    }
}

/// Per-connection writer task. Drains the shared push channel
/// (responses + push notifications) and writes each frame to the
/// pipe. Exits when the channel is closed (all senders dropped)
/// or on a write error.
async fn writer_task(
    mut writer: WriteHalf<NamedPipeServer>,
    mut push_rx: mpsc::Receiver<Vec<u8>>,
    pc_id_for_log: String,
) {
    while let Some(body) = push_rx.recv().await {
        if let Err(e) = write_frame(&mut writer, &body).await {
            warn!(
                error = %e,
                pc_id = %pc_id_for_log,
                "KLP writer: pipe broken, exiting",
            );
            return;
        }
    }
    debug!(pc_id = %pc_id_for_log, "KLP writer: push channel closed, exiting");
}

/// Build + push an anonymous-id error response onto the shared
/// push channel. Used by the read loop for parse / oversize
/// errors that fire before a request id can be parsed.
async fn push_anonymous_error(
    push_tx: &mpsc::Sender<Vec<u8>>,
    kind: ErrorKind,
    detail: &str,
) -> Result<()> {
    let err = RpcError::new(kind, detail);
    let resp = RpcResponse::err_anonymous(err);
    let body = serde_json::to_vec(&resp).context("encode anonymous error response")?;
    push_tx
        .send(body)
        .await
        .map_err(|_| anyhow::anyhow!("KLP push channel closed"))?;
    Ok(())
}
