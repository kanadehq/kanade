//! #1140 PR3a — framing for tiles crossing the agent's in-session IPC pipe.
//!
//! The capture child runs inside the logged-in user's session (it has to —
//! a Session 0 service cannot read a desktop) and hands encoded tiles up to
//! the agent over its stdout pipe. That pipe carries **binary**, so the
//! NDJSON the `--session-agent` idle sensor uses does not apply here.
//!
//! Frame layout, matching KLP's 4-byte little-endian length prefix so the
//! agent has one framing convention rather than two:
//!
//! ```text
//! [body_len: u32 LE] [meta_len: u16 LE] [meta: meta_len bytes UTF-8 JSON] [payload: rest]
//! ```
//!
//! `body_len` counts everything after itself, so a reader can skip a frame
//! it cannot parse and stay in sync — worth having because the payload is
//! opaque bytes that may contain anything, including something that looks
//! like a length prefix.
//!
//! # Why not JSON with the image inlined
//!
//! Same reason the NATS plane keeps pixels out of JSON (see
//! `kanade_shared::wire::remote`): base64 costs a flat 33%, and #1142
//! measured ~94 KB per tile. Paying that twice — once on this pipe, once on
//! the wire — for data that is about to be forwarded verbatim would be
//! pure waste.

#![cfg(target_os = "windows")]

use std::io::{self, Read};

use kanade_shared::wire::{FrameMeta, TileEncoding};
use serde::{Deserialize, Serialize};

/// Largest frame this pipe will carry.
///
/// Deliberately above `kanade_shared::wire::MAX_TILE_BYTES` (256 KB): the
/// NATS budget is what bounds a tile *on the wire*, and the encoder splits
/// to respect it. This cap is a runaway guard on the local pipe, so it
/// tolerates a tile that is momentarily larger than the wire allows and
/// lets the agent report a clear error rather than a truncated read.
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// The JSON half of a frame: what this message is, and everything about it
/// except the bytes.
///
/// Tagged rather than "a tile, possibly empty" because the two carry
/// genuinely different information. A gap means capture stopped — the
/// screen is locked, a UAC prompt is up, the display mode changed — and a
/// consumer has to be able to say so rather than silently hold the last
/// picture, which reads to an operator as a frozen but live screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FrameHeader {
    /// Payload is the encoded image.
    Tile {
        #[serde(flatten)]
        meta: FrameMeta,
        encoding: TileEncoding,
    },
    /// Payload is a UTF-8 reason string.
    Gap,
    /// Capture works again but has nothing new to show. No payload.
    ///
    /// Needed because a recovered-but-unchanged desktop produces no tiles:
    /// without an explicit marker, the consumer would keep believing the
    /// last gap until something on screen happened to move.
    Resumed,
}

/// A frame read off the pipe.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedTile {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl CapturedTile {
    /// The tile metadata, when this is a tile.
    pub fn as_tile(&self) -> Option<(&FrameMeta, TileEncoding)> {
        match &self.header {
            FrameHeader::Tile { meta, encoding } => Some((meta, *encoding)),
            FrameHeader::Gap | FrameHeader::Resumed => None,
        }
    }

    /// True when this marks capture recovering with nothing to show yet.
    pub fn is_resumed(&self) -> bool {
        matches!(self.header, FrameHeader::Resumed)
    }

    /// The gap reason, when this is a gap. Lossy-decoded: a mangled reason
    /// is still worth showing, and failing to report a gap because its
    /// explanation had a bad byte would be the wrong trade.
    pub fn as_gap(&self) -> Option<String> {
        match &self.header {
            FrameHeader::Gap => Some(String::from_utf8_lossy(&self.payload).into_owned()),
            FrameHeader::Tile { .. } | FrameHeader::Resumed => None,
        }
    }
}

/// Serialise one tile into a frame.
pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> io::Result<Vec<u8>> {
    let meta = serde_json::to_vec(header)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("encode meta: {e}")))?;
    let meta_len = u16::try_from(meta.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame meta is {} bytes, over the u16 prefix", meta.len()),
        )
    })?;

    let body_len = 2 + meta.len() + payload.len();
    if body_len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body {body_len} exceeds {MAX_FRAME_BYTES}"),
        ));
    }
    let body_len = u32::try_from(body_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame body length overflow"))?;

    let mut out = Vec::with_capacity(4 + body_len as usize);
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&meta_len.to_le_bytes());
    out.extend_from_slice(&meta);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Parse a frame body (everything after the `body_len` prefix).
pub fn decode_body(body: &[u8]) -> io::Result<CapturedTile> {
    if body.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body shorter than its meta-length prefix",
        ));
    }
    let meta_len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let rest = &body[2..];
    if rest.len() < meta_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame claims {meta_len} bytes of meta but only {} remain",
                rest.len()
            ),
        ));
    }
    let header: FrameHeader = serde_json::from_slice(&rest[..meta_len])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode meta: {e}")))?;
    Ok(CapturedTile {
        header,
        payload: rest[meta_len..].to_vec(),
    })
}

/// Read exactly one frame from a blocking reader.
///
/// Returns `UnexpectedEof` when the pipe closes cleanly between frames —
/// the caller should treat that as "the child exited", not a protocol
/// error. Win32 anonymous pipes are blocking-only, so this is sync by
/// necessity and belongs on a `spawn_blocking` thread.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<CapturedTile> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let body_len = u32::from_le_bytes(len_bytes) as usize;

    if body_len > MAX_FRAME_BYTES {
        // Cannot resynchronise: we would have to trust the same length we
        // just rejected in order to skip it. Fail the stream instead so the
        // supervisor restarts the child rather than reading pixels as
        // headers forever.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body {body_len} exceeds {MAX_FRAME_BYTES}"),
        ));
    }

    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body)?;
    decode_body(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> FrameHeader {
        FrameHeader::Tile {
            meta: FrameMeta {
                frame_seq: 7,
                tile_index: 0,
                tile_count: 2,
                x: 10,
                y: 20,
                w: 100,
                h: 50,
                screen_w: 1920,
                screen_h: 1080,
                captured_at_ms: 1_753_500_000_000,
            },
            encoding: TileEncoding::Jpeg,
        }
    }

    #[test]
    fn frame_round_trips() {
        let h = header();
        let payload = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let bytes = encode_frame(&h, &payload).expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert_eq!(got.header, h);
        assert_eq!(got.payload, payload);
    }

    #[test]
    fn two_frames_read_back_in_order() {
        // The property that matters on a stream: no framing drift between
        // consecutive tiles of different sizes.
        let mut buf = Vec::new();
        let h1 = header();
        let mut h2 = header();
        if let FrameHeader::Tile { meta, .. } = &mut h2 {
            meta.tile_index = 1;
        }
        buf.extend(encode_frame(&h1, &[1, 2, 3]).unwrap());
        buf.extend(encode_frame(&h2, &vec![9u8; 1000]).unwrap());

        let mut cur = std::io::Cursor::new(buf);
        let a = read_frame(&mut cur).expect("first");
        let b = read_frame(&mut cur).expect("second");
        assert_eq!(a.as_tile().expect("tile").0.tile_index, 0);
        assert_eq!(a.payload, vec![1, 2, 3]);
        assert_eq!(b.as_tile().expect("tile").0.tile_index, 1);
        assert_eq!(b.payload.len(), 1000);
    }

    #[test]
    fn empty_payload_round_trips() {
        // A zero-byte tile should never be produced, but the framing must
        // not corrupt the stream if one ever is.
        let bytes = encode_frame(&header(), &[]).expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert!(got.payload.is_empty());
    }

    #[test]
    fn payload_containing_a_length_prefix_does_not_desync() {
        // The exact reason body_len covers the whole body: JPEG bytes can
        // contain anything, including something that reads as a frame
        // header.
        let payload = vec![0x00, 0x00, 0x10, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = encode_frame(&header(), &payload).unwrap();
        buf.extend(encode_frame(&header(), &[7, 7]).unwrap());

        let mut cur = std::io::Cursor::new(buf);
        let a = read_frame(&mut cur).expect("first");
        assert_eq!(a.payload, payload);
        let b = read_frame(&mut cur).expect("second");
        assert_eq!(b.payload, vec![7, 7]);
    }

    #[test]
    fn clean_eof_between_frames_is_unexpected_eof() {
        // The supervisor distinguishes "child exited" from "protocol broke"
        // on this error kind.
        let mut cur = std::io::Cursor::new(Vec::<u8>::new());
        let e = read_frame(&mut cur).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_body_is_unexpected_eof() {
        let mut bytes = encode_frame(&header(), &[1, 2, 3, 4]).unwrap();
        bytes.truncate(bytes.len() - 2);
        let mut cur = std::io::Cursor::new(bytes);
        let e = read_frame(&mut cur).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_declared_length_is_rejected_without_allocating() {
        // A corrupt prefix must not turn into a multi-gigabyte allocation.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cur = std::io::Cursor::new(bytes);
        let e = read_frame(&mut cur).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encoding_an_oversized_frame_fails_rather_than_emitting_it() {
        let e = encode_frame(&header(), &vec![0u8; MAX_FRAME_BYTES + 1]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn meta_length_shorter_than_declared_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&500u16.to_le_bytes());
        body.extend_from_slice(b"{}");
        let e = decode_body(&body).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn body_too_short_for_its_prefix_is_rejected() {
        let e = decode_body(&[0x01]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unparseable_meta_is_rejected() {
        let mut body = Vec::new();
        let bad = b"not json";
        body.extend_from_slice(&(bad.len() as u16).to_le_bytes());
        body.extend_from_slice(bad);
        let e = decode_body(&body).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_gap_round_trips_with_its_reason() {
        let bytes = encode_frame(&FrameHeader::Gap, b"desktop access lost").expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert_eq!(got.as_gap().as_deref(), Some("desktop access lost"));
        assert!(got.as_tile().is_none());
    }

    #[test]
    fn a_gap_with_invalid_utf8_still_reports_something() {
        // Losing the gap because its explanation had a bad byte would be the
        // wrong trade: the operator needs to know capture stopped.
        let bytes = encode_frame(&FrameHeader::Gap, &[0xFF, 0xFE]).expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert!(got.as_gap().is_some());
    }

    #[test]
    fn a_tile_is_not_mistaken_for_a_gap() {
        let bytes = encode_frame(&header(), &[0xFF, 0xD8]).expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert!(got.as_gap().is_none());
        assert!(got.as_tile().is_some());
    }

    #[test]
    fn gaps_and_tiles_interleave_on_one_stream() {
        // The ordering property the whole design rests on: a gap must land
        // between the tiles it separates, not beside them.
        let mut buf = Vec::new();
        buf.extend(encode_frame(&header(), &[1, 2]).unwrap());
        buf.extend(encode_frame(&FrameHeader::Gap, b"locked").unwrap());
        buf.extend(encode_frame(&header(), &[3, 4]).unwrap());

        let mut cur = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cur).unwrap().as_tile().is_some());
        assert_eq!(
            read_frame(&mut cur).unwrap().as_gap().as_deref(),
            Some("locked")
        );
        assert!(read_frame(&mut cur).unwrap().as_tile().is_some());
    }

    #[test]
    fn a_resumed_marker_round_trips_and_is_not_a_gap() {
        let bytes = encode_frame(&FrameHeader::Resumed, &[]).expect("encode");
        let mut cur = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cur).expect("read");
        assert!(got.is_resumed());
        assert!(got.as_gap().is_none(), "resumed must not read as a gap");
        assert!(got.as_tile().is_none());
    }

    #[test]
    fn a_gap_recovery_sequence_survives_the_stream() {
        // The sequence the protocol exists for: capture stops, capture
        // recovers with nothing to show, then a tile finally arrives. All
        // three have to be distinguishable and in order.
        let mut buf = Vec::new();
        buf.extend(encode_frame(&FrameHeader::Gap, b"locked").unwrap());
        buf.extend(encode_frame(&FrameHeader::Resumed, &[]).unwrap());
        buf.extend(encode_frame(&header(), &[0xFF, 0xD8]).unwrap());

        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cur).unwrap().as_gap().as_deref(),
            Some("locked")
        );
        assert!(read_frame(&mut cur).unwrap().is_resumed());
        assert!(read_frame(&mut cur).unwrap().as_tile().is_some());
    }

    #[test]
    fn header_json_is_flat() {
        // `#[serde(flatten)]` keeps the meta fields at the top level, so the
        // IPC shape stays readable when dumped for debugging.
        let s = serde_json::to_string(&header()).unwrap();
        assert!(s.contains("\"kind\":\"tile\""), "{s}");
        assert!(s.contains("\"frame_seq\":7"), "{s}");
        assert!(s.contains("\"encoding\":\"jpeg\""), "{s}");
        assert!(!s.contains("\"meta\""), "{s}");
    }
}
