//! Wire types for the #1140 remote-assistance relay.
//!
//! Three planes, three shapes, for reasons the measurements in #1142 made
//! concrete:
//!
//! - **Control** (`remote.ctrl.<pc_id>`, request/reply): [`RemoteCtrl`] /
//!   [`RemoteCtrlReply`]. JSON — a handful of these exist per session.
//! - **Input** (`remote.input.<session_id>`): [`RemoteInput`]. JSON — small,
//!   structured, and bursty at human speed, so encoding overhead is noise.
//! - **Frames** (`remote.frame.<session_id>`): [`FrameMeta`] in NATS
//!   **headers**, raw encoded image bytes as the payload.
//!
//! # Why frames don't use JSON
//!
//! Every other wire type in this crate is a JSON body, so the frame plane
//! deliberately breaks the house style. A JSON envelope has to base64 the
//! pixels, which costs a flat 33%. #1142 measured a typical session at
//! ~4.0 Mbps of dirty-rect tiles, so the envelope alone would burn ~1.3
//! Mbps per viewer to carry nothing — on the same overlay link the endpoint
//! uses for real work. Headers keep the metadata structured and typed while
//! the pixels travel as bytes.
//!
//! # Tiles, not frames
//!
//! One message carries **one tile** — a single changed rectangle. #1142
//! measured mean changed area at 12.8% of the screen and 1.7–2.5 tiles per
//! frame, so a frame is normally 2–3 messages of ~94 KB. Sending whole
//! frames instead would have cost 32 Mbps against 4.0, which is the finding
//! that made dirty-rect encoding a requirement rather than an optimisation.
//!
//! The sender is responsible for keeping a tile under [`MAX_TILE_BYTES`];
//! a full-screen change encodes to ~700 KB and must be split.

use serde::{Deserialize, Serialize};

/// Largest encoded tile a single message may carry.
///
/// Derived from [`crate::kv::NATS_PAYLOAD_BUDGET`] rather than restating
/// its value, so raising the broker's `max_payload` moves this without
/// anyone remembering that it should. It stays its *own* constant because
/// "how far may one screen tile grow" and "when does stdout spill to the
/// Object Store" ([`crate::kv::STDOUT_INLINE_THRESHOLD`]) are different
/// questions that merely share a broker limit — collapsing them would mean
/// a future tile-specific ceiling (headers do consume some of the budget)
/// silently retuning result overflow.
///
/// Comfortably above the ~94 KB mean tile #1142 measured; a full-screen
/// change encodes to ~700 KB and the sender must split it.
pub const MAX_TILE_BYTES: usize = crate::kv::NATS_PAYLOAD_BUDGET;

/// How a tile's pixels are encoded.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TileEncoding {
    /// Baseline JPEG. The default: universally decodable by a browser
    /// `<img>`/`createImageBitmap`, and what #1142 measured against.
    #[default]
    Jpeg,
    /// WebP. Smaller at equal quality, but the encoder is heavier — #1142
    /// showed encode time is already 88% of the per-frame budget, so this
    /// is here as a wire-format option to evaluate, not a default to adopt
    /// blind.
    Webp,
}

impl TileEncoding {
    /// Header value / MIME subtype.
    pub fn as_str(self) -> &'static str {
        match self {
            TileEncoding::Jpeg => "jpeg",
            TileEncoding::Webp => "webp",
        }
    }

    /// Parse a header value. Unknown encodings yield `None` so a receiver
    /// can skip a tile it cannot decode instead of tearing down a session
    /// that is otherwise fine.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpeg" => Some(TileEncoding::Jpeg),
            "webp" => Some(TileEncoding::Webp),
            _ => None,
        }
    }

    /// MIME type, for handing the payload straight to a browser.
    pub fn mime(self) -> &'static str {
        match self {
            TileEncoding::Jpeg => "image/jpeg",
            TileEncoding::Webp => "image/webp",
        }
    }
}

/// Metadata for one tile, carried in NATS headers alongside the raw image
/// payload.
///
/// Also `Serialize`/`Deserialize`: the same metadata travels as JSON over
/// the agent's in-session IPC pipe, where there are no NATS headers to put
/// it in. One type describing a tile, two encodings of it depending on which
/// hop it is crossing — the alternative was a near-duplicate struct that
/// would drift the first time a field was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// Monotonic per-session frame counter. Tiles of the same frame share
    /// it, which is what lets a viewer decide whether it is assembling a
    /// coherent picture or has started receiving the next one.
    pub frame_seq: u64,
    /// Index of this tile within its frame, `0..tile_count`.
    pub tile_index: u16,
    /// How many tiles this frame was split into. A viewer that has seen all
    /// of them can present the frame; one that is missing some can still
    /// paint what arrived — tiles are independent images, so a dropped tile
    /// degrades to a stale rectangle rather than a corrupt screen.
    pub tile_count: u16,
    /// Tile origin in desktop pixels.
    pub x: u32,
    pub y: u32,
    /// Tile size in pixels.
    pub w: u32,
    pub h: u32,
    /// Full desktop size, repeated on every tile.
    ///
    /// Redundant by design: a viewer that joins mid-session, or reconnects,
    /// can size its canvas from the first tile it happens to receive
    /// without waiting for a control round-trip or a full frame.
    pub screen_w: u32,
    pub screen_h: u32,
    /// Agent-side capture time, milliseconds since the Unix epoch.
    ///
    /// For measuring one-way latency (#1140 risk 2 — whether control across
    /// the overlay is usable at all). Clocks are not synchronised between
    /// endpoint and backend, so treat the absolute value as untrustworthy;
    /// the *differences* between consecutive frames from one agent are what
    /// carry signal.
    pub captured_at_ms: u64,
}

/// NATS header names for [`FrameMeta`]. Namespaced so they cannot collide
/// with anything the broker or a future feature adds.
pub mod header {
    /// Which kind of message this is: a tile, or a gap in the stream.
    /// Present on every frame-plane message so a viewer can dispatch before
    /// trying to parse geometry that a gap does not have.
    pub const KIND: &str = "Kanade-Kind";

    pub const FRAME_SEQ: &str = "Kanade-Frame-Seq";
    pub const TILE_INDEX: &str = "Kanade-Tile-Index";
    pub const TILE_COUNT: &str = "Kanade-Tile-Count";
    pub const TILE_X: &str = "Kanade-Tile-X";
    pub const TILE_Y: &str = "Kanade-Tile-Y";
    pub const TILE_W: &str = "Kanade-Tile-W";
    pub const TILE_H: &str = "Kanade-Tile-H";
    pub const SCREEN_W: &str = "Kanade-Screen-W";
    pub const SCREEN_H: &str = "Kanade-Screen-H";
    pub const ENCODING: &str = "Kanade-Encoding";
    pub const CAPTURED_AT: &str = "Kanade-Captured-At";
}

/// What a frame-plane message carries.
///
/// The gap variant is why this enum exists. A locked workstation, a UAC
/// prompt or a display mode change stops capture dead (see the agent's
/// `screen_capture`), and a viewer only ever told about tiles cannot
/// distinguish "the picture is still because nothing is changing" from "we
/// cannot see the screen at all". Those need different words on screen, so
/// they need different messages on the wire.
///
/// Gaps ride the frame plane rather than the control plane because they are
/// part of what the viewer is *showing*, not a request or a reply — and
/// because arriving in order with the tiles is what makes them meaningful.
/// A gap delivered out of band could easily be painted after the frames
/// that already resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Headers carry a [`FrameMeta`]; payload is the encoded image.
    Tile,
    /// Headers carry nothing else; payload is a UTF-8 reason string.
    Gap,
    /// Capture works again, but there is nothing new to show yet. No
    /// headers beyond the kind, no payload.
    ///
    /// Without this, recovery is only observable when the next tile
    /// happens to arrive — and a desktop that is reachable but unchanged
    /// produces no tiles at all. An operator would keep reading "screen
    /// unavailable" for as long as nobody moved a window, long after
    /// capture had recovered.
    ///
    /// The alternative was to force a full frame on recovery, which would
    /// have meant sending several hundred KB to say "nothing changed" —
    /// the opposite of the bandwidth argument that shaped this whole
    /// format.
    Resumed,
}

impl FrameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Tile => "tile",
            FrameKind::Gap => "gap",
            FrameKind::Resumed => "resumed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tile" => Some(FrameKind::Tile),
            "gap" => Some(FrameKind::Gap),
            "resumed" => Some(FrameKind::Resumed),
            _ => None,
        }
    }
}

/// Headers for a gap message. The reason travels as the payload, not a
/// header, because it is free text of unbounded length and headers are a
/// poor place for that.
pub fn gap_headers() -> async_nats::HeaderMap {
    let mut h = async_nats::HeaderMap::new();
    h.insert(header::KIND, FrameKind::Gap.as_str());
    h
}

/// Headers for a resumed message. Carries nothing but the kind — the
/// message exists to mark a transition, not to deliver content.
pub fn resumed_headers() -> async_nats::HeaderMap {
    let mut h = async_nats::HeaderMap::new();
    h.insert(header::KIND, FrameKind::Resumed.as_str());
    h
}

/// Read the kind off a frame-plane message.
///
/// A message with no `Kanade-Kind` header is rejected rather than assumed:
/// every publisher of this plane sets it, so a missing one means the
/// message did not come from a publisher that agrees with this contract.
pub fn frame_kind(h: &async_nats::HeaderMap) -> Result<FrameKind, FrameMetaError> {
    let v = h
        .get(header::KIND)
        .ok_or(FrameMetaError::Missing(header::KIND))?;
    FrameKind::parse(v.as_str()).ok_or_else(|| FrameMetaError::UnknownKind(v.as_str().to_string()))
}

/// A frame header was missing or unparseable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameMetaError {
    #[error("missing frame header `{0}`")]
    Missing(&'static str),
    #[error("frame header `{name}` is not a valid number: {value:?}")]
    NotANumber { name: &'static str, value: String },
    #[error("unknown tile encoding {0:?}")]
    UnknownEncoding(String),
    #[error("unknown frame kind {0:?}")]
    UnknownKind(String),
    #[error("tile {w}x{h} at ({x},{y}) does not fit a {screen_w}x{screen_h} screen")]
    OutOfBounds {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        screen_w: u32,
        screen_h: u32,
    },
    #[error("tile_index {index} is out of range for tile_count {count}")]
    IndexOutOfRange { index: u16, count: u16 },
}

impl FrameMeta {
    /// Write this metadata into a NATS header map, together with the
    /// encoding (which lives outside the struct because it describes the
    /// payload, not the geometry).
    pub fn to_headers(&self, encoding: TileEncoding) -> async_nats::HeaderMap {
        let mut h = async_nats::HeaderMap::new();
        h.insert(header::KIND, FrameKind::Tile.as_str());
        h.insert(header::FRAME_SEQ, self.frame_seq.to_string().as_str());
        h.insert(header::TILE_INDEX, self.tile_index.to_string().as_str());
        h.insert(header::TILE_COUNT, self.tile_count.to_string().as_str());
        h.insert(header::TILE_X, self.x.to_string().as_str());
        h.insert(header::TILE_Y, self.y.to_string().as_str());
        h.insert(header::TILE_W, self.w.to_string().as_str());
        h.insert(header::TILE_H, self.h.to_string().as_str());
        h.insert(header::SCREEN_W, self.screen_w.to_string().as_str());
        h.insert(header::SCREEN_H, self.screen_h.to_string().as_str());
        h.insert(header::ENCODING, encoding.as_str());
        h.insert(
            header::CAPTURED_AT,
            self.captured_at_ms.to_string().as_str(),
        );
        h
    }

    /// Parse metadata back out of a NATS header map.
    ///
    /// Validates geometry as well as presence: a tile that claims to fall
    /// outside its own screen would otherwise reach a viewer's canvas draw
    /// and either throw or silently paint nothing, far from the corrupt
    /// header that caused it.
    pub fn from_headers(h: &async_nats::HeaderMap) -> Result<(Self, TileEncoding), FrameMetaError> {
        let meta = Self {
            frame_seq: num(h, header::FRAME_SEQ)?,
            tile_index: num(h, header::TILE_INDEX)?,
            tile_count: num(h, header::TILE_COUNT)?,
            x: num(h, header::TILE_X)?,
            y: num(h, header::TILE_Y)?,
            w: num(h, header::TILE_W)?,
            h: num(h, header::TILE_H)?,
            screen_w: num(h, header::SCREEN_W)?,
            screen_h: num(h, header::SCREEN_H)?,
            captured_at_ms: num(h, header::CAPTURED_AT)?,
        };

        let enc_raw = h
            .get(header::ENCODING)
            .ok_or(FrameMetaError::Missing(header::ENCODING))?
            .as_str();
        let encoding = TileEncoding::parse(enc_raw)
            .ok_or_else(|| FrameMetaError::UnknownEncoding(enc_raw.to_string()))?;

        meta.validate()?;
        Ok((meta, encoding))
    }

    /// Geometry sanity. Split out so a sender can check before publishing
    /// rather than leaving it to the receiver to reject.
    pub fn validate(&self) -> Result<(), FrameMetaError> {
        if self.tile_count == 0 || self.tile_index >= self.tile_count {
            return Err(FrameMetaError::IndexOutOfRange {
                index: self.tile_index,
                count: self.tile_count,
            });
        }
        // Saturating so a bogus header can't wrap into an in-bounds answer.
        let right = self.x.saturating_add(self.w);
        let bottom = self.y.saturating_add(self.h);
        if self.w == 0 || self.h == 0 || right > self.screen_w || bottom > self.screen_h {
            return Err(FrameMetaError::OutOfBounds {
                x: self.x,
                y: self.y,
                w: self.w,
                h: self.h,
                screen_w: self.screen_w,
                screen_h: self.screen_h,
            });
        }
        Ok(())
    }
}

/// Read one numeric header.
fn num<T: std::str::FromStr>(
    h: &async_nats::HeaderMap,
    name: &'static str,
) -> Result<T, FrameMetaError> {
    let raw = h.get(name).ok_or(FrameMetaError::Missing(name))?.as_str();
    raw.parse().map_err(|_| FrameMetaError::NotANumber {
        name,
        value: raw.to_string(),
    })
}

/// Backend → agent control message on `remote.ctrl.<pc_id>`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RemoteCtrl {
    /// Begin streaming. The backend mints `session_id`; the agent uses it
    /// to derive its publish and input subjects.
    Start {
        session_id: String,
        /// Display output to capture, 0 = primary.
        #[serde(default)]
        output_index: u32,
        /// JPEG/WebP quality, 1–100.
        #[serde(default = "default_quality")]
        quality: u8,
        /// Upper bound on frames per second. The agent may deliver fewer
        /// (an idle desktop produces nothing); it must not exceed this.
        #[serde(default = "default_max_fps")]
        max_fps: u8,
        /// Whether this session may inject input, decided by the backend
        /// from the operator's features — **not** something the agent
        /// infers. An endpoint must never have to reason about who is
        /// allowed to drive it.
        #[serde(default)]
        allow_input: bool,
    },
    /// Stop streaming and tear the session down. Idempotent: stopping an
    /// unknown session succeeds, so a retry after a lost reply is safe.
    Stop { session_id: String },
    /// Adjust a live session without restarting it. Absent fields keep
    /// their current value.
    Tune {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_fps: Option<u8>,
    },
}

fn default_quality() -> u8 {
    75
}

fn default_max_fps() -> u8 {
    10
}

/// Agent → backend reply to a [`RemoteCtrl`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
pub struct RemoteCtrlReply {
    /// The agent acted on the request.
    pub accepted: bool,
    /// Why not, when `accepted` is false — no capture-capable session, the
    /// user declined consent, another session already holds the output.
    /// Surfaced to the operator verbatim, so it must read as an
    /// explanation rather than an error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Desktop size, when known. Lets the SPA size its canvas before the
    /// first tile arrives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_h: Option<u32>,
}

impl RemoteCtrlReply {
    /// Accepted, with the geometry the operator's canvas needs.
    pub fn accepted(screen_w: u32, screen_h: u32) -> Self {
        Self {
            accepted: true,
            reason: None,
            screen_w: Some(screen_w),
            screen_h: Some(screen_h),
        }
    }

    /// Refused, with a human-readable reason.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            reason: Some(reason.into()),
            screen_w: None,
            screen_h: None,
        }
    }
}

/// Which mouse button an event refers to.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Backend → agent input event on `remote.input.<session_id>`.
///
/// Coordinates are **desktop pixels**, not viewer pixels: the SPA scales a
/// 3840x1600 desktop into whatever canvas it has, and letting the endpoint
/// receive viewer-space coordinates would make correctness depend on the
/// browser window size at the far end of a lossy link.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteInput {
    MouseMove {
        x: u32,
        y: u32,
    },
    MouseButton {
        button: MouseButton,
        /// True on press, false on release. Sent as separate events rather
        /// than as clicks so drag, and any modifier held across the
        /// gesture, survive the trip.
        down: bool,
        x: u32,
        y: u32,
    },
    MouseWheel {
        /// Wheel delta in the platform's native units.
        delta: i32,
        /// Horizontal scroll rather than vertical.
        #[serde(default)]
        horizontal: bool,
    },
    /// A key transition, addressed by Windows virtual-key code.
    ///
    /// Deliberately not a character: shortcuts (Ctrl+C, Alt+Tab) are about
    /// physical keys, and a character-only channel cannot express them.
    Key {
        vk: u16,
        down: bool,
    },
    /// Literal text, for IME composition and anything else a virtual-key
    /// stream cannot express. Complements [`RemoteInput::Key`] rather than
    /// replacing it — Japanese input is the case that forces this to exist.
    Text {
        text: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FrameMeta {
        FrameMeta {
            frame_seq: 42,
            tile_index: 1,
            tile_count: 3,
            x: 100,
            y: 200,
            w: 300,
            h: 400,
            screen_w: 3840,
            screen_h: 1600,
            captured_at_ms: 1_753_500_000_000,
        }
    }

    #[test]
    fn frame_meta_round_trips_through_headers() {
        let m = meta();
        let h = m.to_headers(TileEncoding::Jpeg);
        let (back, enc) = FrameMeta::from_headers(&h).expect("parse");
        assert_eq!(back, m);
        assert_eq!(enc, TileEncoding::Jpeg);
    }

    #[test]
    fn frame_meta_round_trips_webp() {
        let h = meta().to_headers(TileEncoding::Webp);
        let (_, enc) = FrameMeta::from_headers(&h).expect("parse");
        assert_eq!(enc, TileEncoding::Webp);
    }

    #[test]
    fn missing_header_is_named_in_the_error() {
        let mut h = meta().to_headers(TileEncoding::Jpeg);
        // async-nats has no remove(); rebuild without the one under test.
        let mut stripped = async_nats::HeaderMap::new();
        for name in [
            header::FRAME_SEQ,
            header::TILE_INDEX,
            header::TILE_COUNT,
            header::TILE_X,
            header::TILE_Y,
            header::TILE_W,
            header::TILE_H,
            header::SCREEN_W,
            header::SCREEN_H,
            header::CAPTURED_AT,
        ] {
            if let Some(v) = h.get(name) {
                stripped.insert(name, v.as_str());
            }
        }
        // ENCODING deliberately omitted.
        h = stripped;
        assert_eq!(
            FrameMeta::from_headers(&h).unwrap_err(),
            FrameMetaError::Missing(header::ENCODING)
        );
    }

    #[test]
    fn non_numeric_header_reports_name_and_value() {
        let mut h = meta().to_headers(TileEncoding::Jpeg);
        h.insert(header::TILE_W, "wide");
        assert_eq!(
            FrameMeta::from_headers(&h).unwrap_err(),
            FrameMetaError::NotANumber {
                name: header::TILE_W,
                value: "wide".to_string(),
            }
        );
    }

    #[test]
    fn unknown_encoding_is_rejected_with_its_value() {
        let mut h = meta().to_headers(TileEncoding::Jpeg);
        h.insert(header::ENCODING, "avif");
        assert_eq!(
            FrameMeta::from_headers(&h).unwrap_err(),
            FrameMetaError::UnknownEncoding("avif".to_string())
        );
    }

    #[test]
    fn tile_outside_the_screen_is_rejected() {
        let mut m = meta();
        m.x = 3800;
        m.w = 100; // 3900 > 3840
        assert!(matches!(
            m.validate(),
            Err(FrameMetaError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn tile_flush_with_the_screen_edge_is_accepted() {
        // Off-by-one guard: right == screen_w is in bounds, not out.
        let mut m = meta();
        m.x = 3540;
        m.w = 300;
        m.y = 1200;
        m.h = 400;
        assert!(m.validate().is_ok(), "{:?}", m.validate());
    }

    #[test]
    fn zero_sized_tile_is_rejected() {
        let mut m = meta();
        m.w = 0;
        assert!(matches!(
            m.validate(),
            Err(FrameMetaError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn overflowing_geometry_cannot_wrap_into_bounds() {
        // x + w would wrap to a small number without saturating arithmetic,
        // letting a corrupt header pass validation.
        let mut m = meta();
        m.x = u32::MAX;
        m.w = 100;
        assert!(matches!(
            m.validate(),
            Err(FrameMetaError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn tile_index_past_the_count_is_rejected() {
        let mut m = meta();
        m.tile_index = 3;
        m.tile_count = 3;
        assert!(matches!(
            m.validate(),
            Err(FrameMetaError::IndexOutOfRange { .. })
        ));
    }

    #[test]
    fn zero_tile_count_is_rejected() {
        let mut m = meta();
        m.tile_index = 0;
        m.tile_count = 0;
        assert!(matches!(
            m.validate(),
            Err(FrameMetaError::IndexOutOfRange { .. })
        ));
    }

    #[test]
    fn from_headers_validates_geometry_not_just_presence() {
        let mut m = meta();
        m.w = 9000;
        let h = m.to_headers(TileEncoding::Jpeg);
        assert!(matches!(
            FrameMeta::from_headers(&h),
            Err(FrameMetaError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn tile_headers_declare_their_kind() {
        let h = meta().to_headers(TileEncoding::Jpeg);
        assert_eq!(frame_kind(&h).expect("kind"), FrameKind::Tile);
    }

    #[test]
    fn gap_headers_declare_their_kind_and_carry_no_geometry() {
        let h = gap_headers();
        assert_eq!(frame_kind(&h).expect("kind"), FrameKind::Gap);
        // A gap has no tile to describe; asking for one must fail cleanly
        // rather than yield a zero-sized tile a viewer would try to paint.
        assert!(FrameMeta::from_headers(&h).is_err());
    }

    #[test]
    fn a_message_without_a_kind_is_rejected() {
        // Not defaulted to Tile: a missing kind means the publisher does not
        // share this contract, and guessing would hand geometry parsing a
        // message that may have none.
        let h = async_nats::HeaderMap::new();
        assert_eq!(
            frame_kind(&h).unwrap_err(),
            FrameMetaError::Missing(header::KIND)
        );
    }

    #[test]
    fn an_unknown_kind_is_rejected_with_its_value() {
        let mut h = async_nats::HeaderMap::new();
        h.insert(header::KIND, "cursor");
        assert_eq!(
            frame_kind(&h).unwrap_err(),
            FrameMetaError::UnknownKind("cursor".to_string())
        );
    }

    #[test]
    fn frame_kind_keys_round_trip() {
        for k in [FrameKind::Tile, FrameKind::Gap, FrameKind::Resumed] {
            assert_eq!(FrameKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(FrameKind::parse("tiles"), None);
    }

    #[test]
    fn resumed_headers_declare_their_kind_and_nothing_else() {
        let h = resumed_headers();
        assert_eq!(frame_kind(&h).expect("kind"), FrameKind::Resumed);
        // Like a gap, it has no tile to describe.
        assert!(FrameMeta::from_headers(&h).is_err());
    }

    #[test]
    fn ctrl_start_round_trips_with_defaults() {
        let json = r#"{"op":"start","session_id":"s1"}"#;
        let c: RemoteCtrl = serde_json::from_str(json).expect("parse");
        assert_eq!(
            c,
            RemoteCtrl::Start {
                session_id: "s1".into(),
                output_index: 0,
                quality: 75,
                max_fps: 10,
                allow_input: false,
            }
        );
    }

    #[test]
    fn ctrl_start_defaults_to_view_only() {
        // The safe default matters: a control message that forgot the field
        // must not hand over the keyboard.
        let c: RemoteCtrl = serde_json::from_str(r#"{"op":"start","session_id":"s1"}"#).unwrap();
        match c {
            RemoteCtrl::Start { allow_input, .. } => assert!(!allow_input),
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_variants_round_trip() {
        for c in [
            RemoteCtrl::Start {
                session_id: "s1".into(),
                output_index: 1,
                quality: 60,
                max_fps: 15,
                allow_input: true,
            },
            RemoteCtrl::Stop {
                session_id: "s1".into(),
            },
            RemoteCtrl::Tune {
                session_id: "s1".into(),
                quality: Some(50),
                max_fps: None,
            },
        ] {
            let s = serde_json::to_string(&c).expect("encode");
            assert_eq!(serde_json::from_str::<RemoteCtrl>(&s).expect("decode"), c);
        }
    }

    #[test]
    fn tune_omits_absent_fields() {
        let s = serde_json::to_string(&RemoteCtrl::Tune {
            session_id: "s1".into(),
            quality: Some(50),
            max_fps: None,
        })
        .unwrap();
        assert!(!s.contains("max_fps"), "{s}");
    }

    #[test]
    fn ctrl_reply_constructors_are_consistent() {
        let ok = RemoteCtrlReply::accepted(1920, 1080);
        assert!(ok.accepted && ok.reason.is_none());
        assert_eq!((ok.screen_w, ok.screen_h), (Some(1920), Some(1080)));

        let no = RemoteCtrlReply::refused("user declined");
        assert!(!no.accepted);
        assert_eq!(no.reason.as_deref(), Some("user declined"));
        assert_eq!((no.screen_w, no.screen_h), (None, None));
    }

    #[test]
    fn input_variants_round_trip() {
        for i in [
            RemoteInput::MouseMove { x: 10, y: 20 },
            RemoteInput::MouseButton {
                button: MouseButton::Right,
                down: true,
                x: 1,
                y: 2,
            },
            RemoteInput::MouseWheel {
                delta: -120,
                horizontal: false,
            },
            RemoteInput::Key {
                vk: 0x41,
                down: true,
            },
            RemoteInput::Text {
                text: "こんにちは".into(),
            },
        ] {
            let s = serde_json::to_string(&i).expect("encode");
            assert_eq!(serde_json::from_str::<RemoteInput>(&s).expect("decode"), i);
        }
    }

    #[test]
    fn input_text_survives_multibyte() {
        // The IME case this variant exists for.
        let i = RemoteInput::Text {
            text: "日本語入力".into(),
        };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<RemoteInput>(&s).unwrap(), i);
    }

    #[test]
    fn tile_encoding_key_round_trips() {
        for e in [TileEncoding::Jpeg, TileEncoding::Webp] {
            assert_eq!(TileEncoding::parse(e.as_str()), Some(e));
        }
        assert_eq!(TileEncoding::parse("png"), None);
    }

    #[test]
    fn max_tile_bytes_derives_from_the_shared_budget() {
        // Against the constant, never the literal: an earlier draft asserted
        // `== 256 * 1024`, which passes just as happily when the two
        // constants have drifted apart — the exact failure the assertion was
        // supposed to be guarding.
        assert_eq!(MAX_TILE_BYTES, crate::kv::NATS_PAYLOAD_BUDGET);
        assert_eq!(MAX_TILE_BYTES, crate::kv::STDOUT_INLINE_THRESHOLD);
        // Compile-time, not runtime: a budget that outgrew the broker's
        // publish ceiling should fail the build, not one test run.
        const { assert!(MAX_TILE_BYTES < crate::kv::NATS_DEFAULT_MAX_PAYLOAD) };
    }
}
