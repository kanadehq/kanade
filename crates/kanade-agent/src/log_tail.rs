//! Bounded reverse tail-read of a log file, shared by the two
//! "give me the last N lines of agent.log" paths:
//!
//! - the KLP `system.log_tail` handler (issue #289, where this
//!   logic originally lived), and
//! - the NATS `logs.fetch.<pc_id>` handler (issue #514 — it used to
//!   slurp the whole file with `tokio::fs::read`, so a chatty
//!   long-uptime endpoint with a hundreds-of-MB log spiked agent
//!   RSS by the full file size per request).
//!
//! Lives outside `klp/` because the KLP tree is
//! `#[cfg(target_os = "windows")]` while `logs.fetch` is
//! cross-platform.

use std::io;

/// Files at or below this size take the simple whole-file read
/// path. Past it, [`read_tail_lines`] seeks near the end and reads
/// forward so a chatty long-uptime endpoint with a hundreds-of-MB
/// log doesn't balloon agent RSS just to serve a few hundred tail
/// lines (issue #289). Daily-rotated logs are normally well under
/// this, so the common case never pays the extra complexity.
pub(crate) const TAIL_WHOLE_FILE_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Per-line byte budget for the reverse-read seek heuristic. The
/// agent's `tracing` lines run comfortably under this; over-
/// estimating only costs a slightly larger initial read, never
/// correctness — the window grows and retries if it held too few
/// lines.
pub(crate) const TAIL_AVG_LINE_BYTES: u64 = 2 * 1024;

/// Hard cap on how far back [`read_tail_lines`] will seek while
/// hunting for enough lines. Bounds worst-case memory for
/// pathological logs (one giant line, or no newlines at all). On
/// hitting it we return whatever the window held rather than walk
/// the whole file.
pub(crate) const TAIL_MAX_WINDOW_BYTES: u64 = 32 * 1024 * 1024;

/// Read the last `requested` lines of `path`, oldest-first (the
/// order `str::lines` yields), without slurping the whole file when
/// it's large.
///
/// Small files (≤ [`TAIL_WHOLE_FILE_THRESHOLD`]) take the plain
/// whole-file read — the added seek/retry machinery isn't worth it
/// for the daily-rotated common case. Larger files seek to roughly
/// `len - requested × avg_line` and read forward, doubling the
/// window (capped at [`TAIL_MAX_WINDOW_BYTES`]) until it holds
/// enough lines or reaches the start of the file.
///
/// Propagates `io::Error` (callers map `NotFound` to an empty
/// result or an error message as fits their protocol).
pub(crate) async fn read_tail_lines(
    path: &std::path::Path,
    requested: usize,
) -> io::Result<Vec<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    if requested == 0 {
        return Ok(vec![]);
    }

    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();

    if len <= TAIL_WHOLE_FILE_THRESHOLD {
        let mut body = String::new();
        file.read_to_string(&mut body).await?;
        return Ok(tail_of(&body, requested));
    }

    // Large file: seek near the end and read forward, growing the
    // window until it holds enough lines (or we reach offset 0 /
    // the cap). Start from a line-count-based estimate.
    let mut window = (requested as u64)
        .saturating_mul(TAIL_AVG_LINE_BYTES)
        .clamp(TAIL_AVG_LINE_BYTES, TAIL_MAX_WINDOW_BYTES);

    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).await?;
        let mut buf = Vec::with_capacity(window.min(len) as usize);
        // `take(window)` makes the window a *hard* byte bound. The
        // agent's own log is appended to continuously, so the file
        // can grow between `metadata()` above and this read; a plain
        // `read_to_end` would chase the moving EOF and could blow
        // past `window` (and `TAIL_MAX_WINDOW_BYTES` on the capped
        // iteration), defeating the whole point of the bounded read.
        (&mut file).take(window).read_to_end(&mut buf).await?;

        // When we seeked past offset 0 the first line is almost
        // certainly a mid-line fragment — drop everything up to and
        // including the first `\n`. `\n` (0x0A) never appears inside
        // a multibyte UTF-8 sequence, so the remainder sits on a
        // clean UTF-8 boundary even though the seek itself wasn't
        // char-aligned. No newline in the whole window means a
        // single line longer than `window`: drop it all and grow.
        let usable: &[u8] = if start > 0 {
            match buf.iter().position(|&b| b == b'\n') {
                Some(nl) => &buf[nl + 1..],
                None => &[],
            }
        } else {
            &buf
        };

        let text = String::from_utf8_lossy(usable);
        let lines = tail_of(&text, requested);

        // Done when we have enough, can't go back further, or hit
        // the cap (return whatever the bounded window held).
        if lines.len() >= requested || start == 0 || window >= TAIL_MAX_WINDOW_BYTES {
            return Ok(lines);
        }
        window = window.saturating_mul(2).min(TAIL_MAX_WINDOW_BYTES);
    }
}

/// Last `n` lines of `body`, trailing `\r\n` / `\n` stripped, in
/// file order. Shared by both read paths in [`read_tail_lines`].
///
/// `str::Lines` is a `DoubleEndedIterator`, so we walk backward and
/// `take(n)` instead of collecting every line: O(n) lines scanned
/// and allocated rather than O(buffer) — matters on the large-file
/// path where the window can hold far more lines than requested.
fn tail_of(body: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = body.lines().rev().take(n).map(str::to_string).collect();
    lines.reverse();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn small_file_returns_exact_tail() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "a\nb\nc\nd\n").unwrap();
        let lines = read_tail_lines(f.path(), 2).await.unwrap();
        assert_eq!(lines, vec!["c", "d"]);
    }

    #[tokio::test]
    async fn zero_requested_short_circuits() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "a\nb\n").unwrap();
        let lines = read_tail_lines(f.path(), 0).await.unwrap();
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn missing_file_propagates_not_found() {
        let e = read_tail_lines(std::path::Path::new("definitely-missing.log"), 5)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn large_file_seek_path_returns_exact_tail() {
        // Past TAIL_WHOLE_FILE_THRESHOLD so the seek path runs; the
        // mid-line fragment at the seek boundary must be dropped.
        let f = NamedTempFile::new().unwrap();
        let pad = "x".repeat(64);
        let count = 100_000usize;
        let body = (1..=count)
            .map(|i| format!("line-{i:08}-{pad}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.len() as u64 > TAIL_WHOLE_FILE_THRESHOLD);
        std::fs::write(f.path(), body).unwrap();

        let lines = read_tail_lines(f.path(), 100).await.unwrap();
        assert_eq!(lines.len(), 100);
        assert_eq!(lines[0], format!("line-{:08}-{pad}", count - 99));
        assert_eq!(lines.last().unwrap(), &format!("line-{count:08}-{pad}"));
    }
}
