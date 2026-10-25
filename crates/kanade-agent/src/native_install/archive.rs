//! Bounded, in-memory inspection of the admin CLI release archive.
//!
//! The release workflow packs exactly one regular file, named
//! `kanade-aarch64-apple-darwin`, into `kanade-aarch64-apple-darwin.tar.gz`
//! (`tar czf dist/$stem.tar.gz $stem`). That member is the only thing this
//! module returns; its *install* name (`kanade`) is a separate, fixed policy
//! of the installer and is never taken from the archive.
//!
//! Nothing is written to disk here. Every other shape is refused: absolute or
//! `..` paths, directories, links, devices, FIFOs, duplicate or extra members
//! (including AppleDouble `._*` files), set-id modes, and any archive whose
//! compressed or decompressed size exceeds a fixed cap. Tar/pax metadata can
//! rename a member, so the *resolved* name is compared byte-for-byte with the
//! expected one.

use std::io::{self, Read};

use flate2::read::GzDecoder;
use tar::EntryType;

/// The single member the release workflow packs for the arm64 macOS CLI.
pub const EXPECTED_MEMBER: &str = "kanade-aarch64-apple-darwin";
/// Largest accepted `.tar.gz` (compressed) — checked by the caller's size
/// binding and again here.
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
/// Largest decompressed tar stream (headers included).
pub const MAX_STREAM_BYTES: u64 = 192 * 1024 * 1024;
/// Largest accepted executable.
pub const MAX_MEMBER_BYTES: u64 = 160 * 1024 * 1024;
/// More entries than this is never a valid CLI archive; counting stops here.
const MAX_ENTRIES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive is larger than the {MAX_ARCHIVE_BYTES} byte limit")]
    ArchiveTooLarge,
    #[error("decompressed archive exceeds its size limit")]
    StreamTooLarge,
    #[error("archive member exceeds its size limit")]
    MemberTooLarge,
    #[error("archive is not a readable gzip tar: {0}")]
    Malformed(String),
    #[error("archive has no members")]
    Empty,
    #[error("unexpected archive member {0:?}")]
    UnexpectedMember(String),
    #[error("archive member {0:?} is not a regular file")]
    NotRegularFile(String),
    #[error("archive contains more than the one expected member")]
    ExtraMember,
    #[error("archive member has a set-id or sticky mode")]
    UnsafeMode,
    #[error("archive member is not executable")]
    NotExecutable,
    #[error("archive member is empty")]
    EmptyMember,
}

/// Errors out of the decompressor once the stream cap is crossed, so a gzip
/// bomb stops after `MAX_STREAM_BYTES` of output instead of exhausting memory.
struct Capped<R> {
    inner: R,
    left: u64,
}

const CAP_MARK: &str = "kanade-stream-cap";

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            // Distinguish "exactly at the cap and more follows" from a clean
            // end: probe one byte.
            let mut one = [0u8; 1];
            return match self.inner.read(&mut one)? {
                0 => Ok(0),
                _ => Err(io::Error::other(CAP_MARK)),
            };
        }
        let max = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= n as u64;
        Ok(n)
    }
}

fn map_io(e: io::Error) -> ArchiveError {
    if e.to_string() == CAP_MARK {
        ArchiveError::StreamTooLarge
    } else {
        ArchiveError::Malformed(e.to_string())
    }
}

/// Inspect `archive` and return the bytes of the one expected member.
pub fn extract_cli_member(archive: &[u8]) -> Result<Vec<u8>, ArchiveError> {
    if archive.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(ArchiveError::ArchiveTooLarge);
    }
    let gz = Capped {
        inner: GzDecoder::new(archive),
        left: MAX_STREAM_BYTES,
    };
    let mut tar = tar::Archive::new(gz);
    // Never apply anything to a filesystem; we only read entry bodies.
    tar.set_unpack_xattrs(false);
    tar.set_preserve_permissions(false);
    let entries = tar.entries().map_err(map_io)?;

    let mut member: Option<Vec<u8>> = None;
    for (seen, entry) in entries.enumerate() {
        if seen >= MAX_ENTRIES {
            return Err(ArchiveError::ExtraMember);
        }
        let mut entry = entry.map_err(map_io)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let kind = entry.header().entry_type();
        if member.is_some() {
            return Err(ArchiveError::ExtraMember);
        }
        if entry.path_bytes().as_ref() != EXPECTED_MEMBER.as_bytes() {
            return Err(ArchiveError::UnexpectedMember(name));
        }
        if !matches!(kind, EntryType::Regular) {
            return Err(ArchiveError::NotRegularFile(name));
        }
        if entry.link_name_bytes().is_some() {
            return Err(ArchiveError::NotRegularFile(name));
        }
        let mode = entry.header().mode().map_err(map_io)?;
        if mode & 0o7000 != 0 {
            return Err(ArchiveError::UnsafeMode);
        }
        if mode & 0o100 == 0 {
            return Err(ArchiveError::NotExecutable);
        }
        let size = entry.size();
        if size == 0 {
            return Err(ArchiveError::EmptyMember);
        }
        if size > MAX_MEMBER_BYTES {
            return Err(ArchiveError::MemberTooLarge);
        }
        let mut buf = Vec::with_capacity(size.min(16 * 1024 * 1024) as usize);
        (&mut entry)
            .take(size + 1)
            .read_to_end(&mut buf)
            .map_err(map_io)?;
        if buf.len() as u64 != size {
            return Err(ArchiveError::Malformed("member length mismatch".into()));
        }
        member = Some(buf);
    }
    member.ok_or(ArchiveError::Empty)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A gzip tar with the given raw `(name, type, mode, link, body)` members.
    /// Names are written verbatim into the header so traversal names that the
    /// `tar` builder would refuse can still be produced.
    pub fn build(members: &[(&str, u8, u32, &str, &[u8])]) -> Vec<u8> {
        let mut raw = Vec::new();
        for (name, kind, mode, link, body) in members {
            let mut h = tar::Header::new_ustar();
            {
                let old = h.as_old_mut();
                old.name[..name.len()].copy_from_slice(name.as_bytes());
                old.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
            h.set_mode(*mode);
            h.set_size(body.len() as u64);
            h.set_entry_type(tar::EntryType::new(*kind));
            h.set_cksum();
            raw.extend_from_slice(h.as_bytes());
            raw.extend_from_slice(body);
            let pad = (512 - body.len() % 512) % 512;
            raw.extend(std::iter::repeat_n(0u8, pad));
        }
        raw.extend(std::iter::repeat_n(0u8, 1024));
        gzip(&raw)
    }

    pub fn gzip(raw: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
    }

    pub fn good(body: &[u8]) -> Vec<u8> {
        build(&[(EXPECTED_MEMBER, b'0', 0o755, "", body)])
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn the_expected_member_is_returned_verbatim() {
        assert_eq!(extract_cli_member(&good(b"payload")).unwrap(), b"payload");
    }

    #[test]
    fn traversal_absolute_and_renamed_members_are_refused() {
        for name in [
            "../kanade-aarch64-apple-darwin",
            "/usr/local/bin/kanade",
            "./kanade-aarch64-apple-darwin",
            "kanade",
            "dir/kanade-aarch64-apple-darwin",
        ] {
            let a = build(&[(name, b'0', 0o755, "", b"x")]);
            assert!(
                matches!(
                    extract_cli_member(&a),
                    Err(ArchiveError::UnexpectedMember(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn links_devices_and_directories_are_refused() {
        for &kind in b"123456" {
            let a = build(&[(EXPECTED_MEMBER, kind, 0o755, "target", b"")]);
            assert!(
                matches!(extract_cli_member(&a), Err(ArchiveError::NotRegularFile(_))),
                "type {}",
                kind as char
            );
        }
    }

    #[test]
    fn extra_duplicate_and_appledouble_members_are_refused() {
        let dup = build(&[
            (EXPECTED_MEMBER, b'0', 0o755, "", b"a"),
            (EXPECTED_MEMBER, b'0', 0o755, "", b"b"),
        ]);
        assert_eq!(extract_cli_member(&dup), Err(ArchiveError::ExtraMember));
        let extra = build(&[
            (EXPECTED_MEMBER, b'0', 0o755, "", b"a"),
            ("._kanade-aarch64-apple-darwin", b'0', 0o644, "", b"meta"),
        ]);
        assert_eq!(extract_cli_member(&extra), Err(ArchiveError::ExtraMember));
        let first_junk = build(&[
            ("._kanade-aarch64-apple-darwin", b'0', 0o644, "", b"meta"),
            (EXPECTED_MEMBER, b'0', 0o755, "", b"a"),
        ]);
        assert!(matches!(
            extract_cli_member(&first_junk),
            Err(ArchiveError::UnexpectedMember(_))
        ));
    }

    #[test]
    fn unsafe_modes_and_degenerate_members_are_refused() {
        for mode in [0o4755, 0o2755, 0o1755] {
            let a = build(&[(EXPECTED_MEMBER, b'0', mode, "", b"x")]);
            assert_eq!(extract_cli_member(&a), Err(ArchiveError::UnsafeMode));
        }
        let a = build(&[(EXPECTED_MEMBER, b'0', 0o644, "", b"x")]);
        assert_eq!(extract_cli_member(&a), Err(ArchiveError::NotExecutable));
        let a = build(&[(EXPECTED_MEMBER, b'0', 0o755, "", b"")]);
        assert_eq!(extract_cli_member(&a), Err(ArchiveError::EmptyMember));
        assert_eq!(
            extract_cli_member(&gzip(&[0u8; 1024])),
            Err(ArchiveError::Empty)
        );
    }

    #[test]
    fn garbage_and_truncated_archives_are_refused_not_panicked() {
        assert!(matches!(
            extract_cli_member(b"not gzip at all"),
            Err(ArchiveError::Malformed(_)) | Err(ArchiveError::Empty)
        ));
        let mut a = good(&vec![7u8; 4096]);
        a.truncate(a.len() / 2);
        assert!(extract_cli_member(&a).is_err());
    }

    #[test]
    fn decompression_bombs_stop_at_the_stream_cap() {
        // A pax extended header claiming more than the stream cap: the tar
        // reader would buffer it, so the cap on decompressed bytes must stop
        // it. The body is zeros, which gzip squeezes to almost nothing.
        let body = vec![0u8; (MAX_STREAM_BYTES + 4096) as usize];
        let bomb = build(&[("PaxHeader/x", b'x', 0o644, "", &body)]);
        assert!((bomb.len() as u64) < MAX_ARCHIVE_BYTES);
        assert_eq!(extract_cli_member(&bomb), Err(ArchiveError::StreamTooLarge));
    }

    #[test]
    fn a_member_over_the_member_cap_is_refused_from_its_header() {
        let mut h = tar::Header::new_ustar();
        h.as_old_mut().name[..EXPECTED_MEMBER.len()].copy_from_slice(EXPECTED_MEMBER.as_bytes());
        h.set_mode(0o755);
        h.set_size(MAX_MEMBER_BYTES + 1);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        let a = gzip(h.as_bytes());
        assert_eq!(extract_cli_member(&a), Err(ArchiveError::MemberTooLarge));
    }

    #[test]
    fn oversized_compressed_input_is_refused_before_decoding() {
        let big = vec![1u8; MAX_ARCHIVE_BYTES as usize + 1];
        assert_eq!(extract_cli_member(&big), Err(ArchiveError::ArchiveTooLarge));
    }
}
