//! `GET /api/remote/{pc_id}/ws` — the operator's end of the #1140
//! remote-assistance relay.
//!
//! The endpoint never listens. It holds one outbound NATS connection and
//! publishes tiles to `remote.frame.<session_id>`; this handler is the hop
//! that turns that into something a browser can render. It opens a
//! WebSocket, mints a session id, asks the agent to start capturing, and
//! forwards every frame-plane message until either side goes away.
//!
//! # Why this route authenticates itself
//!
//! A browser cannot set an `Authorization` header on a WebSocket. The two
//! remaining places to put a credential are the query string and the
//! subprotocol list, and `?token=` is unacceptable for a socket that streams
//! someone's screen: it lands in browser history, proxy access logs and any
//! `Referer` the page emits. So the credential rides
//! `Sec-WebSocket-Protocol` as `bearer.<jwt>`, which keeps it in a header.
//!
//! That means the `/api/*` middleware cannot authenticate this route — it
//! only reads `Authorization` — so [`crate::auth::verify`] allow-lists the
//! path and this handler calls [`crate::auth::verify_bearer`] itself. It is
//! the same verifier the middleware uses (that is the whole reason #1152
//! extracted it), so the DB stays authoritative here too: a disabled account
//! or a revoked page permission takes effect on the next connect rather than
//! at the token's `exp`.
//!
//! Bypassing the middleware also bypasses [`crate::auth::require_operator`]
//! and [`crate::auth::require_features`], because both read the [`Claims`]
//! the middleware would have injected. This handler therefore performs
//! **all three** checks itself — identity, role, and page permission. The
//! route is still registered in [`super::feature_for_path`]: that table
//! documents itself as the single auditable route→feature map, so a route
//! missing from it reads as "commons, open to everyone", which this is not.
//!
//! # Socket framing
//!
//! Every message the socket carries is binary and shaped:
//!
//! ```text
//! [u32 LE meta_len][meta JSON][payload]
//! ```
//!
//! the same shape the agent's capture child uses on its stdout pipe. The SPA
//! reads it with one `DataView` for the length, one `TextDecoder` for the
//! meta, and hands the tail straight to a `Blob` / `createImageBitmap`
//! without copying the pixels again.
//!
//! JSON-with-base64 was rejected for the same reason it was rejected on the
//! NATS hop (#1142): a typical session is ~4.0 Mbps of tiles, and base64
//! would spend ~1.3 Mbps of it encoding nothing.
//!
//! `meta.kind` mirrors [`FrameKind`] (`tile` / `gap` / `resumed`) plus two
//! kinds that exist only on this hop: `started`, which carries the geometry
//! from the agent's accept so the SPA can size its canvas before the first
//! tile, and `ended`, which explains why the stream stopped. Without
//! `ended`, every failure — an offline endpoint, a refused session, a
//! backend-side error — would reach the operator as an unexplained socket
//! close.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use kanade_shared::feature::Feature;
use kanade_shared::subject;
use kanade_shared::wire::{
    FrameKind, FrameMeta, RemoteCtrl, RemoteCtrlReply, TileEncoding, frame_kind,
};
use serde::Serialize;
use std::time::Duration;
use tracing::{info, warn};

use super::AppState;
use crate::auth::{Claims, Role, verify_bearer};

/// The subprotocol the SPA must offer and the server echoes back. A
/// WebSocket handshake that offers subprotocols and gets none back is closed
/// by the browser, so this has to be selected explicitly — and it must be
/// *this* one, never the `bearer.` entry, or the credential would come back
/// in a response header.
pub const SUBPROTOCOL: &str = "kanade.remote.v1";

/// Prefix marking the credential entry in the offered subprotocol list.
const BEARER_PREFIX: &str = "bearer.";

/// How long to wait for the agent to answer `Start`.
///
/// Not a network round-trip: the agent has to spawn a capture child into the
/// interactive session and probe the desktop before it can answer, so this is
/// sized for process start, not for latency. An unreachable endpoint fails
/// much faster than this — core NATS answers "no responders" immediately —
/// so the timeout only governs an agent that accepted the request and then
/// stalled.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait for the agent to acknowledge `Stop` during teardown.
/// Short on purpose: the viewer has already gone, nobody is waiting on the
/// answer, and `Stop` is idempotent (#1151) so a lost reply costs nothing —
/// the next `Stop` or the agent's own session bookkeeping still tears it
/// down.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Metadata prefixed to every socket message. Serialised as the `meta JSON`
/// segment described in the module doc.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SocketMeta {
    /// The agent accepted; the stream is live. Geometry is `Option` because
    /// a well-behaved agent reports it but the wire type allows its absence,
    /// and a viewer can also size itself from the first tile's `screen_w` /
    /// `screen_h` (which is why those are repeated on every tile).
    Started {
        session_id: String,
        screen_w: Option<u32>,
        screen_h: Option<u32>,
        /// Always false until PR5 (#1140). Sent from the start so the SPA
        /// reads the capability off the wire instead of assuming it.
        allow_input: bool,
    },
    /// A tile. `payload` is the encoded image.
    Tile {
        #[serde(flatten)]
        meta: FrameMeta,
        encoding: TileEncoding,
    },
    /// Capture stopped — locked workstation, UAC prompt, display change.
    ///
    /// The reason travels in the meta rather than the payload (where the
    /// NATS hop puts it) because the SPA already parses the meta as JSON;
    /// leaving it in the payload would force a second `Blob`→text decode for
    /// a short string that is never rendered as an image.
    Gap { reason: String },
    /// Capture works again. Distinct from a tile because a recovered desktop
    /// that nobody is touching produces no tiles at all — see [`FrameKind`].
    Resumed,
    /// The stream is over and why. Always the last message on the socket.
    Ended { reason: String },
}

/// Frame a socket message: `[u32 LE meta_len][meta JSON][payload]`.
///
/// The length prefix covers only the metadata, not the whole message: the
/// payload's length is whatever remains, so a viewer never has to trust two
/// numbers to agree. (The agent's stdout pipe *does* prefix a total length,
/// because a pipe is a byte stream with no message boundaries. A WebSocket
/// already delivers whole messages, so that field would be redundant here.)
fn frame(meta: &SocketMeta, payload: &[u8]) -> Vec<u8> {
    // A serialisation failure here is not reachable — `SocketMeta` is a
    // closed enum of owned primitives with no map keys — but it must not
    // take the socket down if that ever stops being true.
    let json = serde_json::to_vec(meta).unwrap_or_else(|e| {
        warn!(error = %e, "serialising socket meta");
        br#"{"kind":"ended","reason":"backend could not encode this message"}"#.to_vec()
    });
    let mut out = Vec::with_capacity(4 + json.len() + payload.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(payload);
    out
}

/// Pull the bearer credential out of an offered subprotocol list.
///
/// `Sec-WebSocket-Protocol: kanade.remote.v1, bearer.eyJhbGci...` — the
/// header is a comma-separated list of tokens, and a JWT is made only of
/// characters that are legal in one (base64url plus `.`), so it needs no
/// further encoding.
fn bearer_from_protocols(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix(BEARER_PREFIX))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// True when `claims` may reach a route owned by `feature`.
///
/// Mirrors [`crate::auth::require_features`] for a handler that the layer
/// cannot cover: an unrestricted caller (`None`) passes, a restricted one
/// must list the feature.
fn feature_allowed(claims: &Claims, feature: Feature) -> bool {
    claims
        .allowed_features
        .as_ref()
        .is_none_or(|allowed| allowed.contains(&feature))
}

pub async fn ws(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = bearer_from_protocols(&headers);
    let claims = match verify_bearer(&state.pool, token.as_deref()).await {
        Ok(c) => c,
        Err(reason) => {
            warn!(pc_id, reason, "remote ws: auth rejected");
            return (StatusCode::UNAUTHORIZED, reason).into_response();
        }
    };

    // Watching a live desktop is not a read of projected data — it is an
    // intrusive act on someone's machine, and PR5 turns this same socket
    // into control. Requiring operator now avoids having to take the
    // capability away from viewers later.
    if !claims.role().allows(Role::Operator) {
        warn!(pc_id, sub = %claims.sub, role = claims.role().as_str(), "remote ws: role denied");
        return (
            StatusCode::FORBIDDEN,
            "operator role required to view a remote screen",
        )
            .into_response();
    }
    if !feature_allowed(&claims, Feature::Remote) {
        warn!(pc_id, sub = %claims.sub, "remote ws: feature denied");
        return (
            StatusCode::FORBIDDEN,
            "account not permitted to access this page (requires remote)",
        )
            .into_response();
    }

    let operator = claims.sub.clone();
    upgrade
        // Echo the protocol, never the credential.
        .protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| relay(socket, state, pc_id, operator))
}

async fn relay(mut socket: WebSocket, state: AppState, pc_id: String, operator: String) {
    let session_id = format!("sess-{}", uuid::Uuid::new_v4());

    // Subscribe BEFORE asking the agent to start. Core NATS has no replay:
    // anything published between the agent accepting and this subscription
    // becoming active is gone, and what the agent publishes first is the
    // opening frame of the desktop — the one frame the operator cannot do
    // without, because an idle desktop produces no second one.
    let frames = match state
        .nats
        .subscribe(subject::remote_frame(&session_id))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(pc_id, session_id, error = %e, "remote ws: frame subscribe failed");
            end(&mut socket, format!("backend could not subscribe: {e}")).await;
            return;
        }
    };

    let start = RemoteCtrl::Start {
        session_id: session_id.clone(),
        output_index: 0,
        quality: 75,
        max_fps: 10,
        // View-only until PR5 (#1140) adds input injection. The backend
        // decides this, never the agent — an endpoint must not have to
        // reason about who is allowed to drive it.
        allow_input: false,
    };
    let reply = match request_ctrl(&state, &pc_id, &start, START_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => {
            let live = e.may_have_started();
            let reason = e.into_reason();
            warn!(pc_id, session_id, reason, live, "remote ws: start failed");
            end(&mut socket, reason).await;
            // A failed `Start` does not mean an unstarted one. If the agent
            // may have acted on it — it answered too late, or answered
            // something we could not read — a capture child could be running
            // with nobody left to stop it.
            if live {
                stop_session(&state, &pc_id, &session_id).await;
            }
            return;
        }
    };
    if !reply.accepted {
        let reason = reply
            .reason
            .unwrap_or_else(|| "the endpoint refused the session".to_string());
        info!(pc_id, session_id, operator, reason, "remote ws: refused");
        // No Stop: the agent never opened a session, and stopping a session
        // it does not hold is exactly the request #1149 made it refuse.
        end(&mut socket, reason).await;
        return;
    }

    info!(pc_id, session_id, operator, "remote ws: streaming");
    let opened = frame(
        &SocketMeta::Started {
            session_id: session_id.clone(),
            screen_w: reply.screen_w,
            screen_h: reply.screen_h,
            allow_input: false,
        },
        &[],
    );
    if socket.send(Message::Binary(opened.into())).await.is_ok() {
        pump(&mut socket, frames).await;
    }

    stop_session(&state, &pc_id, &session_id).await;
    info!(pc_id, session_id, operator, "remote ws: closed");
}

/// Tear the session down on the endpoint.
///
/// Called on every path out of [`relay`] that could have left a session
/// live. A capture child nobody stops burns the endpoint's CPU and holds its
/// display against the next operator, so a redundant `Stop` is always the
/// cheaper mistake: it is idempotent (#1151), and a `Stop` naming a session
/// the machine does not hold is refused without touching whoever does hold
/// it (#1149).
async fn stop_session(state: &AppState, pc_id: &str, session_id: &str) {
    let stop = RemoteCtrl::Stop {
        session_id: session_id.to_owned(),
    };
    if let Err(e) = request_ctrl(state, pc_id, &stop, STOP_TIMEOUT).await {
        let reason = e.into_reason();
        warn!(
            pc_id,
            session_id, reason, "remote ws: stop not acknowledged"
        );
    }
}

/// Forward frame-plane messages to the socket until either side stops.
///
/// A slow viewer applies backpressure here — `send` awaits, the subscriber's
/// buffer fills, and the broker eventually drops messages for this
/// subscription. That is the right failure for this stream: tiles are
/// independent snapshots, so a viewer that cannot keep up should fall behind
/// by *skipping* rather than by accumulating a queue of screens that were
/// current a minute ago.
///
/// Both arms of the `select!` are `Stream::next`, which is cancel-safe — the
/// arm that loses the race has not consumed anything.
async fn pump(socket: &mut WebSocket, mut frames: async_nats::Subscriber) {
    loop {
        tokio::select! {
            msg = frames.next() => {
                let Some(msg) = msg else {
                    // The subscription ended (broker gone / connection
                    // dropped). Nothing more will arrive on it.
                    end(socket, "the frame stream ended".to_string()).await;
                    return;
                };
                let Some((meta, payload)) = translate(&msg) else { continue };
                if socket.send(Message::Binary(frame(&meta, payload).into())).await.is_err() {
                    return; // viewer gone
                }
            }
            incoming = socket.recv() => {
                // Nothing is expected from the viewer until PR5 carries
                // input on this socket; today the only meaningful event is
                // it going away. `None` = closed, `Err` = broken.
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Turn one frame-plane NATS message into a socket message, or `None` when
/// it cannot be understood.
///
/// A malformed message is dropped rather than closing the session: the
/// stream is a series of independent tiles, so one unreadable message costs
/// a stale rectangle, while tearing the socket down over it would cost the
/// operator the whole session.
fn translate(msg: &async_nats::Message) -> Option<(SocketMeta, &[u8])> {
    let headers = msg.headers.as_ref()?;
    let kind = match frame_kind(headers) {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "remote ws: undecodable frame kind");
            return None;
        }
    };
    match kind {
        FrameKind::Tile => match FrameMeta::from_headers(headers) {
            Ok((meta, encoding)) => Some((SocketMeta::Tile { meta, encoding }, &msg.payload[..])),
            Err(e) => {
                warn!(error = %e, "remote ws: undecodable tile meta");
                None
            }
        },
        FrameKind::Gap => Some((
            SocketMeta::Gap {
                reason: String::from_utf8_lossy(&msg.payload).into_owned(),
            },
            &[][..],
        )),
        FrameKind::Resumed => Some((SocketMeta::Resumed, &[][..])),
    }
}

/// Why a control request failed, split by the only distinction that changes
/// what the backend must do next: whether the agent could have acted on it.
///
/// "The request failed" is not the same as "nothing happened". A `Start` that
/// times out may still be starting — [`START_TIMEOUT`] is sized for spawning
/// a capture child, so an endpoint that is merely slow can accept just after
/// the window closes — and one whose reply we cannot parse definitely
/// reached an agent that processed it. Treating those like an unreachable
/// machine leaves a capture child running with nobody left to stop it.
#[derive(Debug)]
enum CtrlError {
    /// The request provably never reached an agent: it could not be encoded,
    /// or core NATS answered "no responders". No session can exist.
    NotDelivered(String),
    /// The agent may have acted on it. Assume a session exists and tear it
    /// down; a `Stop` too many costs nothing.
    Indeterminate(String),
}

impl CtrlError {
    /// True when a session could be live despite the failure.
    fn may_have_started(&self) -> bool {
        matches!(self, CtrlError::Indeterminate(_))
    }

    /// The reason, already phrased for the operator.
    fn into_reason(self) -> String {
        match self {
            CtrlError::NotDelivered(r) | CtrlError::Indeterminate(r) => r,
        }
    }
}

/// Send a control request and decode the agent's reply.
async fn request_ctrl(
    state: &AppState,
    pc_id: &str,
    ctrl: &RemoteCtrl,
    timeout: Duration,
) -> Result<RemoteCtrlReply, CtrlError> {
    let payload = serde_json::to_vec(ctrl)
        .map_err(|e| CtrlError::NotDelivered(format!("backend encode failed: {e}")))?;
    let request = state
        .nats
        .request(subject::remote_ctrl(pc_id), payload.into());

    let msg = match tokio::time::timeout(timeout, request).await {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => {
            // Only "no responders" proves nothing is listening. Anything
            // else — including the client's *own* request timeout, which can
            // be shorter than ours — leaves it open whether the agent got it.
            return Err(match e.kind() {
                async_nats::client::RequestErrorKind::NoResponders => {
                    CtrlError::NotDelivered(format!("{pc_id} is not reachable: no agent listening"))
                }
                _ => CtrlError::Indeterminate(format!("{pc_id} did not answer: {e}")),
            });
        }
        Err(_) => {
            return Err(CtrlError::Indeterminate(format!(
                "{pc_id} did not answer within {}s",
                timeout.as_secs()
            )));
        }
    };

    serde_json::from_slice(&msg.payload)
        .map_err(|e| CtrlError::Indeterminate(format!("{pc_id} sent a bad reply: {e}")))
}

/// Send a final `ended` message and close. Best effort — the socket may
/// already be gone, which is why every error here is ignored.
async fn end(socket: &mut WebSocket, reason: String) {
    let bytes = frame(&SocketMeta::Ended { reason }, &[]);
    let _ = socket.send(Message::Binary(bytes.into())).await;
    let _ = socket.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_nats::HeaderMap as NatsHeaders;

    fn parse_frame(bytes: &[u8]) -> (serde_json::Value, &[u8]) {
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let meta = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        (meta, &bytes[4 + len..])
    }

    #[test]
    fn frame_prefixes_meta_length_only() {
        let bytes = frame(&SocketMeta::Resumed, b"ignored-payload");
        let (meta, payload) = parse_frame(&bytes);
        assert_eq!(meta["kind"], "resumed");
        // The prefix covers the meta, so the payload is whatever is left —
        // a viewer never has to reconcile two lengths.
        assert_eq!(payload, b"ignored-payload");
    }

    #[test]
    fn tile_meta_is_flat_so_the_spa_reads_one_object() {
        let meta = FrameMeta {
            frame_seq: 7,
            tile_index: 1,
            tile_count: 3,
            x: 10,
            y: 20,
            w: 100,
            h: 50,
            screen_w: 3840,
            screen_h: 1600,
            captured_at_ms: 1_700_000_000_000,
        };
        let bytes = frame(
            &SocketMeta::Tile {
                meta,
                encoding: TileEncoding::Jpeg,
            },
            &[0xFF, 0xD8, 0xFF],
        );
        let (json, payload) = parse_frame(&bytes);
        assert_eq!(json["kind"], "tile");
        // Flattened, not nested under `meta` — PR4 reads `m.frame_seq`.
        assert_eq!(json["frame_seq"], 7);
        assert_eq!(json["tile_count"], 3);
        assert_eq!(json["screen_w"], 3840);
        assert_eq!(json["encoding"], "jpeg");
        assert_eq!(payload, &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn socket_kinds_match_the_wire_vocabulary() {
        // The three relayed kinds must spell the same words the frame plane
        // does; PR4 dispatches on one string for both hops.
        for (meta, want) in [
            (
                SocketMeta::Tile {
                    meta: FrameMeta {
                        frame_seq: 0,
                        tile_index: 0,
                        tile_count: 1,
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        screen_w: 1,
                        screen_h: 1,
                        captured_at_ms: 0,
                    },
                    encoding: TileEncoding::Jpeg,
                },
                FrameKind::Tile.as_str(),
            ),
            (
                SocketMeta::Gap {
                    reason: "locked".into(),
                },
                FrameKind::Gap.as_str(),
            ),
            (SocketMeta::Resumed, FrameKind::Resumed.as_str()),
        ] {
            let (json, _) = parse_frame(&frame(&meta, &[]));
            assert_eq!(json["kind"], want);
        }
    }

    #[test]
    fn gap_reason_moves_into_the_meta() {
        let mut headers = NatsHeaders::new();
        headers.insert(kanade_shared::wire::remote_header::KIND, "gap");
        let msg = message(headers, b"the workstation is locked".to_vec());
        let (meta, payload) = translate(&msg).expect("translated");
        assert_eq!(
            meta,
            SocketMeta::Gap {
                reason: "the workstation is locked".into()
            }
        );
        // Payload emptied — the reason is in the meta now, not both places.
        assert!(payload.is_empty());
    }

    #[test]
    fn undecodable_frames_are_dropped_not_fatal() {
        // Headers present but empty — no `Kanade-Kind`. Guessing "tile"
        // here would hand a message with no geometry to the geometry parser.
        assert!(translate(&message(NatsHeaders::new(), vec![])).is_none());

        // A kind nobody knows: a newer agent inventing a fourth one must not
        // take an older backend's session down.
        let mut unknown = NatsHeaders::new();
        unknown.insert(kanade_shared::wire::remote_header::KIND, "hologram");
        assert!(translate(&message(unknown, vec![])).is_none());

        // Says tile, carries no geometry.
        let mut headerless_tile = NatsHeaders::new();
        headerless_tile.insert(kanade_shared::wire::remote_header::KIND, "tile");
        assert!(translate(&message(headerless_tile, vec![1, 2, 3])).is_none());
    }

    fn message(headers: NatsHeaders, payload: Vec<u8>) -> async_nats::Message {
        async_nats::Message {
            subject: "remote.frame.sess-test".into(),
            reply: None,
            payload: payload.into(),
            headers: Some(headers),
            status: None,
            description: None,
            length: 0,
        }
    }

    fn protocols(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, value.parse().unwrap());
        h
    }

    #[test]
    fn bearer_is_read_out_of_the_protocol_list() {
        assert_eq!(
            bearer_from_protocols(&protocols("kanade.remote.v1, bearer.abc.def.ghi")).as_deref(),
            Some("abc.def.ghi")
        );
        // Order must not matter, and neither must the spacing.
        assert_eq!(
            bearer_from_protocols(&protocols("bearer.tok,kanade.remote.v1")).as_deref(),
            Some("tok")
        );
    }

    #[test]
    fn a_missing_or_empty_credential_is_absent_not_blank() {
        // Nothing offered at all.
        assert!(bearer_from_protocols(&HeaderMap::new()).is_none());
        // Protocol offered, no credential.
        assert!(bearer_from_protocols(&protocols("kanade.remote.v1")).is_none());
        // The prefix with nothing after it must not read as a token —
        // `verify_bearer` would then report "invalid token" for what is
        // really a missing one.
        assert!(bearer_from_protocols(&protocols("bearer.")).is_none());
    }

    #[test]
    fn only_proven_non_delivery_skips_the_teardown() {
        // The agent said nothing in time, or said something unreadable — in
        // both cases it may be capturing right now, and this is the only
        // signal that decides whether anything ever stops it.
        assert!(CtrlError::Indeterminate("timed out".into()).may_have_started());
        assert!(CtrlError::Indeterminate("bad reply".into()).may_have_started());
        // Nothing was listening: a Stop would be talking to no-one.
        assert!(!CtrlError::NotDelivered("no agent listening".into()).may_have_started());

        // The reason survives the classification — it is what the operator
        // reads in the `ended` message.
        assert_eq!(
            CtrlError::Indeterminate("PC1 did not answer".into()).into_reason(),
            "PC1 did not answer"
        );
    }

    #[test]
    fn feature_gate_matches_the_middleware() {
        let claims = |allowed: Option<Vec<Feature>>| Claims {
            sub: "op".into(),
            exp: 0,
            aud: None,
            roles: vec!["operator".into()],
            allowed_features: allowed,
        };
        // Unrestricted account.
        assert!(feature_allowed(&claims(None), Feature::Remote));
        // Restricted, and permitted.
        assert!(feature_allowed(
            &claims(Some(vec![Feature::Remote, Feature::Audit])),
            Feature::Remote
        ));
        // Restricted to other pages.
        assert!(!feature_allowed(
            &claims(Some(vec![Feature::Audit])),
            Feature::Remote
        ));
        // Commons-only (`[]`) is a real restriction, not "unrestricted".
        assert!(!feature_allowed(&claims(Some(vec![])), Feature::Remote));
    }
}
