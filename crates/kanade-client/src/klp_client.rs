//! Client-side KLP transport — Named Pipe client + framed JSON-RPC.
//!
//! Owned by the Tauri runtime (one [`KlpClient`] per app
//! instance, shared via `tauri::State`). On startup, [`connect`]
//! opens the agent's Named Pipe (`\\.\pipe\kanade-agent`), runs
//! the SPEC §2.12.6 handshake, and stashes the
//! [`HandshakeResult`] for the UI to read. Subsequent
//! request/response calls (e.g. `system.ping`) go through
//! [`KlpClient::request`], which generates a UUID v7 id,
//! correlates the response, and decodes the result into the
//! caller's typed struct.
//!
//! Push notifications (`state.changed` etc.) land in a follow-up
//! PR — this skeleton only does the request/response side. The
//! reader half is consumed inline on each `request` call; a
//! future split into a permanent reader task will demultiplex
//! responses + notifications.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use kanade_shared::ipc::envelope::{
    JSONRPC_VERSION, RpcMessage, RpcRequest, RpcResponse, RpcResponsePayload,
};
use kanade_shared::ipc::handshake::{HandshakeParams, HandshakeResult, PROTOCOL_V1};
use kanade_shared::ipc::method;
use kanade_shared::ipc::system::{PingParams, PingResult};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// SPEC §2.12.1 — the well-known Named Pipe the agent listens on.
const PIPE_NAME: &str = r"\\.\pipe\kanade-agent";

/// SPEC §2.12.2 — 4-byte LE length prefix; 1 MiB cap.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Client-side product identifier sent in the handshake. Surfaced
/// in the agent's audit log + tracing spans (SPEC §2.12.12).
const CLIENT_NAME: &str = "kanade-client";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared KLP connection. Wraps the raw pipe in an `Arc<Mutex<…>>`
/// so the Tauri commands invoked concurrently by the WebView each
/// serialise their request/response round-trip — the request side
/// generates a unique id and reads back the single matching
/// frame; multiplexing concurrent requests requires the reader-
/// task split that lands with push subscriptions later.
#[derive(Clone)]
pub struct KlpClient {
    inner: Arc<Mutex<NamedPipeClient>>,
    handshake: Arc<HandshakeResult>,
}

impl KlpClient {
    /// Open the pipe, run the SPEC §2.12.6 handshake, and return a
    /// ready client. Bubbles up the underlying error so the Tauri
    /// `setup` callback can surface a clear "agent not running"
    /// banner if the pipe isn't there yet.
    pub async fn connect() -> Result<Self> {
        let pipe = ClientOptions::new()
            .open(PIPE_NAME)
            .with_context(|| format!("open Named Pipe {PIPE_NAME}"))?;
        info!(pipe = PIPE_NAME, "KLP client: pipe connected");

        let mut pipe = pipe;
        let handshake = handshake(&mut pipe).await.context("KLP handshake")?;
        info!(
            agent_version = %handshake.agent_version,
            user = %handshake.session.user,
            session_id = handshake.session.session_id,
            pc_id = %handshake.session.pc_id,
            "KLP client: handshake complete",
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(pipe)),
            handshake: Arc::new(handshake),
        })
    }

    /// Cached handshake result. Returned to the UI on every
    /// `get_handshake` invoke — handshake is one-shot per
    /// connection so the cache is the source of truth.
    pub fn handshake(&self) -> Arc<HandshakeResult> {
        self.handshake.clone()
    }

    /// Round-trip one request through the pipe. Mints a UUID v7
    /// id, writes the frame, reads back the response, validates
    /// the id correlates, and decodes the result.
    ///
    /// `params` is the typed request payload; `R` is the
    /// per-method result struct (e.g. [`PingResult`]).
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        // SPEC §2.12.3 prefers UUID v7 (time-sortable, easier
        // log correlation); the workspace's `uuid` feature set
        // doesn't enable v7 today so we use v4. Switch when the
        // workspace pin grows the `v7` feature.
        let id = uuid::Uuid::new_v4().to_string();
        let req = RpcRequest::new(&id, method, params).context("encode KLP request")?;
        let body = serde_json::to_vec(&req).context("serialise KLP request")?;

        let mut pipe = self.inner.lock().await;
        write_frame(&mut *pipe, &body)
            .await
            .context("write frame")?;
        let resp_bytes = read_frame(&mut *pipe).await.context("read frame")?;
        drop(pipe);

        let msg: RpcMessage =
            serde_json::from_slice(&resp_bytes).context("decode KLP response envelope")?;
        let RpcMessage::Response(resp) = msg else {
            bail!("expected Response, got {msg:?}");
        };
        if resp.id.as_deref() != Some(id.as_str()) {
            bail!("response id mismatch: expected {id:?}, got {:?}", resp.id);
        }
        decode_response::<R>(resp)
    }

    /// Convenience wrapper for `system.ping`. Returned to the
    /// Tauri `ping_agent` command verbatim.
    pub async fn ping(&self) -> Result<PingResult> {
        self.request::<PingParams, PingResult>(method::SYSTEM_PING, &PingParams::default())
            .await
    }
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
