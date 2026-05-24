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
//! follow-up PR; until then the listener is `#[cfg(target_os =
//! "windows")]` gated, and `klp::server::spawn` is a no-op on
//! non-Windows.
//!
//! Security descriptor: this PR uses the OS-default SD (full
//! control: LocalSystem + administrators + creator owner; read:
//! Everyone). SPEC §2.12.1 wants `Authenticated Users RW, deny
//! Everyone / Anonymous` — that needs a hand-built SECURITY_
//! DESCRIPTOR via Win32 ACL APIs, which is verbose enough to be
//! its own focused PR. Documented as a known limitation; the
//! Client App PR can't actually ship without this being tightened
//! (a non-admin user can't write to the pipe under the default
//! SD), so the SD upgrade is the precursor to client work, not
//! optional.

use std::sync::Arc;

use anyhow::Result;

/// Shared configuration injected into every spawned per-connection
/// task. Kept small and cheap to clone so each connection gets its
/// own copy without lifetime gymnastics.
#[derive(Clone)]
pub struct ListenerContext {
    pub pc_id: Arc<str>,
    pub agent_version: Arc<str>,
}

/// Spawn the KLP listener. On Windows this returns once the
/// listener is up and running (the loop runs forever inside the
/// spawned task); on non-Windows this is a no-op that logs the
/// skip and returns immediately.
///
/// The returned `JoinHandle` is detached by the caller in
/// production — there's no graceful-shutdown path in the foundation
/// PR. A future PR adds a `CancellationToken` once we have a use
/// case for it (e.g. service stop must drain in-flight handlers).
pub fn spawn(ctx: ListenerContext) -> tokio::task::JoinHandle<Result<()>> {
    #[cfg(target_os = "windows")]
    {
        tokio::spawn(windows_impl::run(ctx))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ctx;
        tracing::info!("KLP listener skipped — only Windows is supported in this PR");
        tokio::spawn(async { Ok(()) })
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::ListenerContext;
    use anyhow::{Context, Result};
    use kanade_shared::ipc::envelope::{RpcMessage, RpcResponse};
    use kanade_shared::ipc::error::{ErrorKind, RpcError};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tracing::{debug, info, warn};

    use crate::klp::auth::resolve_peer;
    use crate::klp::connection::ConnectionState;
    use crate::klp::dispatcher::dispatch_request;
    use crate::klp::framing::{read_frame, write_frame};

    /// SPEC §2.12.1 — Windows Named Pipe endpoint.
    pub const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";

    pub async fn run(ctx: ListenerContext) -> Result<()> {
        // `first_pipe_instance(true)` makes the initial create fail
        // loudly if another process is squatting the pipe name —
        // safer than silently sharing a name.
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(PIPE_NAME)
            .with_context(|| format!("create Named Pipe {PIPE_NAME}"))?;
        info!(pipe = PIPE_NAME, "KLP listener ready");

        loop {
            if let Err(e) = server.connect().await {
                warn!(error = %e, "KLP server.connect() failed; retrying");
                // Recreate before retrying so a broken handle
                // doesn't poison the loop.
                server = ServerOptions::new().create(PIPE_NAME).with_context(|| {
                    format!("recreate Named Pipe {PIPE_NAME} after connect error")
                })?;
                continue;
            }

            // Re-arm BEFORE spawning the connection task so the next
            // client doesn't see a brief "no listener" window.
            let next = ServerOptions::new()
                .create(PIPE_NAME)
                .with_context(|| format!("re-create Named Pipe {PIPE_NAME}"))?;
            let connected = std::mem::replace(&mut server, next);

            let task_ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(connected, task_ctx).await {
                    warn!(error = %e, "KLP connection task failed");
                }
            });
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

        let mut conn =
            ConnectionState::new(peer, ctx.pc_id.to_string(), ctx.agent_version.to_string());

        loop {
            let frame = match read_frame(&mut pipe).await {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!(user = %conn.peer.user, "KLP client disconnected (EOF)");
                    return Ok(());
                }
                Err(e) => {
                    warn!(error = %e, "KLP frame read error; closing connection");
                    let _ = write_anonymous_error(
                        &mut pipe,
                        ErrorKind::PayloadTooLarge,
                        &e.to_string(),
                    )
                    .await;
                    return Ok(());
                }
            };

            let msg: RpcMessage = match serde_json::from_slice(&frame) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "KLP JSON parse error");
                    let _ = write_anonymous_error(&mut pipe, ErrorKind::ParseError, &e.to_string())
                        .await;
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
}
