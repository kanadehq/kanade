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

use std::sync::Arc;

use anyhow::{Context, Result};
use kanade_shared::ipc::envelope::{RpcMessage, RpcResponse};
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::{debug, info, warn};

use crate::klp::auth::resolve_peer;
use crate::klp::connection::ConnectionState;
use crate::klp::dispatcher::dispatch_request;
use crate::klp::framing::{read_frame, write_frame};
use crate::klp::security::PipeSecurity;

/// SPEC §2.12.1 — Windows Named Pipe endpoint.
pub const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";

/// Shared configuration injected into every spawned per-connection
/// task. Kept small and cheap to clone so each connection gets its
/// own copy without lifetime gymnastics.
#[derive(Clone)]
pub struct ListenerContext {
    pub pc_id: Arc<str>,
    pub agent_version: Arc<str>,
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

async fn handle_connection(mut pipe: NamedPipeServer, ctx: ListenerContext) -> Result<()> {
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

    let mut conn = ConnectionState::new(peer, ctx.pc_id.to_string(), ctx.agent_version.to_string());

    loop {
        let frame = match read_frame(&mut pipe).await {
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
                    write_anonymous_error(&mut pipe, ErrorKind::PayloadTooLarge, &e.to_string())
                        .await;
                return Ok(());
            }
            Err(e) => {
                // ConnectionReset / ConnectionAborted / generic
                // I/O errors mean the pipe is already dead;
                // trying to write a response would just emit a
                // confusing follow-up error log. Close silently.
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
                let _ =
                    write_anonymous_error(&mut pipe, ErrorKind::ParseError, &e.to_string()).await;
                // SPEC §2.12 doesn't require closing on parse
                // error; staying open lets the client recover
                // by sending a well-formed frame next.
                continue;
            }
        };

        match msg {
            RpcMessage::Request(req) => {
                let resp = dispatch_request(&mut conn, req);
                let body = serde_json::to_vec(&resp).context("encode RpcResponse")?;
                if let Err(e) = write_frame(&mut pipe, &body).await {
                    warn!(error = %e, "KLP write error; closing connection");
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
                // push subscriptions land, this stays a debug
                // log (push responses aren't expected either).
                debug!(id = ?resp.id, "KLP unexpected client → agent response, ignoring");
            }
        }
    }
}

async fn write_anonymous_error(
    pipe: &mut NamedPipeServer,
    kind: ErrorKind,
    detail: &str,
) -> Result<()> {
    let err = RpcError::new(kind, detail);
    let resp = RpcResponse::err_anonymous(err);
    let body = serde_json::to_vec(&resp).context("encode anonymous error response")?;
    write_frame(pipe, &body)
        .await
        .context("write anonymous error")?;
    Ok(())
}
