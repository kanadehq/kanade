//! Length-prefixed JSON framing for KLP (SPEC §2.12.2).
//!
//! Each frame on the wire is:
//!
//! ```text
//! [len: 4 bytes LE u32] [body: len bytes UTF-8 JSON]
//! ```
//!
//! `len` is an unsigned 32-bit little-endian integer giving the byte
//! length of the JSON body that follows. Body is UTF-8-encoded
//! JSON. Frames > 1 MiB are rejected (SPEC §2.12.2's
//! `stdout_chunk` splitting rule exists exactly so this cap isn't
//! pressured).
//!
//! Why hand-rolled instead of `tokio_util::codec::LengthDelimitedCodec`:
//! the codec ships with KLP-friendly defaults (4-byte LE u32 head)
//! and would work, but the per-tick overhead of pulling in another
//! tower-style crate for a 50-line read/write pair isn't worth it
//! — we own the I/O loop and need explicit control over the size
//! cap rejection (which would otherwise become a `Result::Err`
//! mid-stream and need re-synchronisation).

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// SPEC §2.12.2 max message size. Larger frames return
/// [`std::io::ErrorKind::InvalidData`]; the agent then sends a
/// `PayloadTooLarge` error (KLP code -32005) and closes the
/// connection — the client SHOULD have split via `stdout_chunk`
/// before pushing this hard.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Read one length-prefixed JSON frame from `reader`. Returns the
/// raw body bytes (caller decodes JSON). On `eof` before any bytes
/// are read, returns [`std::io::ErrorKind::UnexpectedEof`] — caller
/// should treat that as a clean disconnect, not a protocol error.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("KLP frame {len} bytes exceeds 1 MiB cap"),
        ));
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

/// Write one length-prefixed JSON frame to `writer`. `body` is the
/// already-encoded JSON; this function only adds the 4-byte LE
/// header. Returns [`std::io::ErrorKind::InvalidData`] if `body`
/// would exceed the 1 MiB cap — caller MUST chunk before sending.
pub async fn write_frame<W>(writer: &mut W, body: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
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
    use tokio::io::duplex;

    #[tokio::test]
    async fn round_trip_short_frame() {
        let (mut a, mut b) = duplex(64 * 1024);
        let body = br#"{"jsonrpc":"2.0","id":"u1","method":"system.ping"}"#;
        write_frame(&mut a, body).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn read_frame_rejects_oversize_header() {
        // Hand-craft a frame whose header claims 2 MiB body.
        let (mut a, mut b) = duplex(8);
        let oversize_len = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        // Write header only — the size check fires before any body
        // is read, so a never has to send 2 MiB.
        tokio::io::AsyncWriteExt::write_all(&mut a, &oversize_len)
            .await
            .unwrap();
        let err = read_frame(&mut b).await.expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn write_frame_rejects_oversize_body() {
        let (mut a, _b) = duplex(64);
        let body = vec![b'x'; MAX_FRAME_BYTES + 1];
        let err = write_frame(&mut a, &body).await.expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_frame_eof_before_header_is_unexpected_eof() {
        // Close immediately — caller should see UnexpectedEof and
        // treat as clean disconnect.
        let (a, mut b) = duplex(8);
        drop(a);
        let err = read_frame(&mut b).await.expect_err("must error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn multiple_frames_in_sequence_decode_independently() {
        // The reader is one-frame-at-a-time; multiple writes should
        // still come out in order. This guards against any future
        // refactor accidentally buffering past a frame boundary.
        let (mut a, mut b) = duplex(64 * 1024);
        let f1 = br#"{"id":"1","method":"system.ping"}"#;
        let f2 = br#"{"id":"2","method":"system.handshake"}"#;
        write_frame(&mut a, f1).await.unwrap();
        write_frame(&mut a, f2).await.unwrap();
        let g1 = read_frame(&mut b).await.unwrap();
        let g2 = read_frame(&mut b).await.unwrap();
        assert_eq!(g1, f1);
        assert_eq!(g2, f2);
    }
}
