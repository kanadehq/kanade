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

/// The JSON half of a frame: everything about the tile except its pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameHeader {
    #[serde(flatten)]
    pub meta: FrameMeta,
    pub encoding: TileEncoding,
}

/// A frame read off the pipe.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedTile {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
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
        FrameHeader {
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
        let mut h1 = header();
        h1.meta.tile_index = 0;
        let mut h2 = header();
        h2.meta.tile_index = 1;
        buf.extend(encode_frame(&h1, &[1, 2, 3]).unwrap());
        buf.extend(encode_frame(&h2, &vec![9u8; 1000]).unwrap());

        let mut cur = std::io::Cursor::new(buf);
        let a = read_frame(&mut cur).expect("first");
        let b = read_frame(&mut cur).expect("second");
        assert_eq!(a.header.meta.tile_index, 0);
        assert_eq!(a.payload, vec![1, 2, 3]);
        assert_eq!(b.header.meta.tile_index, 1);
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
    fn header_json_is_flat() {
        // `#[serde(flatten)]` keeps the meta fields at the top level, so the
        // IPC shape stays readable when dumped for debugging.
        let s = serde_json::to_string(&header()).unwrap();
        assert!(s.contains("\"frame_seq\":7"), "{s}");
        assert!(s.contains("\"encoding\":\"jpeg\""), "{s}");
        assert!(!s.contains("\"meta\""), "{s}");
    }
}
