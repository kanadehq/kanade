//! Bounded capture of a child process's stdout / stderr.
//!
//! The agent used to accumulate whatever a script wrote, in full, with no
//! upper bound: a `Vec<u8>` that grew for as long as the child kept writing.
//! A daily housekeeping job in dry-run mode listed every candidate file it
//! would have deleted, nothing ever deleted them, and the listing grew about
//! 10 MiB a day until one run emitted **563 MiB** of stdout (#1320).
//!
//! What that cost is worth spelling out, because none of it is obvious from
//! the capture site:
//!
//! - **The agent's own RSS.** The bytes are held in memory for the whole run
//!   before anything sees them, so the ceiling on a script's output was the
//!   ceiling on the agent's memory.
//! - **The fleet's result storage.** `OBJECT_RESULT_OUTPUT` is capped at
//!   [`DEFAULT_RESULT_OUTPUT_CAP_MIB`] — 1 GiB, shared by every host for 30
//!   days. One 563 MiB result takes over half of it. That bucket filling is
//!   what stopped result delivery fleet-wide.
//! - **The agent's CPU, afterwards.** An unpublishable result stays in the
//!   outbox and is re-read and re-parsed once a second, forever (#1319).
//!
//! Truncating is not a substitute for fixing any of those; it removes the
//! input that makes them reachable from a single job.
//!
//! ## Why head *and* tail
//!
//! Keeping only the head loses the failure — a script that dies after a long
//! listing puts its error last. Keeping only the tail loses what the run was
//! doing. Both ends are cheap, so both are kept, with the marker between them
//! saying exactly what is missing.
//!
//! ## Visibly, not silently
//!
//! The marker carries the original byte count. Dropping bytes silently would
//! be worse than the unbounded behaviour it replaces: an operator reading a
//! truncated listing as complete draws conclusions from it.

use kanade_shared::kv::STDOUT_INLINE_THRESHOLD;

/// Largest capture kept per stream, before truncation.
///
/// Derived from the two limits it sits between rather than chosen freely:
///
/// - Above [`STDOUT_INLINE_THRESHOLD`] (256 KiB) by 32×, so a genuinely
///   verbose job still spills to the Object Store and keeps its output. The
///   cap bounds the pathological case; it is not a budget for normal jobs.
/// - Far below `DEFAULT_RESULT_OUTPUT_CAP_MIB` (1 GiB, fleet-wide, 30 days),
///   so no single run can take a meaningful bite out of shared storage. At
///   this ceiling a result is ~0.8% of the bucket instead of 55%.
///
/// Applied per stream, so a run emitting the maximum on both stdout and
/// stderr is bounded at twice this.
pub const MAX_CAPTURE_BYTES: usize = 32 * STDOUT_INLINE_THRESHOLD;

/// The relationship the cap is chosen for, enforced at compile time rather
/// than in a test: output between the inline threshold and the cap must still
/// be able to travel by Object Store with its bytes intact. Lowering the cap
/// past the threshold would silently turn "spill to the Object Store" into
/// "truncate", so it should not compile.
const _: () = assert!(MAX_CAPTURE_BYTES > STDOUT_INLINE_THRESHOLD);

/// Share of the budget kept from the start. The remainder is the tail.
const HEAD_FRACTION: usize = 3;

/// How a budget divides into head and tail.
///
/// One definition, because the split was computed in two places — once in
/// `new()` to size the head's initial allocation and once in `push()` to
/// decide where bytes go. They agreed, but nothing made them: changing the
/// ratio at one site only would have desynced the reservation from the real
/// split, silently. Same reasoning as everything else in this file — one rule
/// should have one implementation.
const fn split(cap: usize) -> (usize, usize) {
    let head = cap / HEAD_FRACTION * (HEAD_FRACTION - 1);
    (head, cap - head)
}

/// Accumulates a child's output, keeping the first and last bytes and
/// counting everything.
///
/// Bounded by construction: memory never exceeds the cap plus one chunk,
/// regardless of how long the child runs or how much it writes.
pub struct CappedOutput {
    cap: usize,
    head: Vec<u8>,
    /// Bytes after the head, kept as a sliding window over the tail.
    tail: std::collections::VecDeque<u8>,
    total: u64,
}

impl CappedOutput {
    pub fn new(cap: usize) -> Self {
        let (head_cap, _) = split(cap);
        Self {
            cap,
            head: Vec::with_capacity(head_cap.min(64 * 1024)),
            tail: std::collections::VecDeque::new(),
            total: 0,
        }
    }

    /// Bytes the child wrote, including any dropped.
    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn truncated(&self) -> bool {
        self.total > self.cap as u64
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.cap == 0 {
            return;
        }
        let (head_cap, tail_cap) = split(self.cap);
        let mut rest = bytes;
        if self.head.len() < head_cap {
            let take = (head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() || tail_cap == 0 {
            return;
        }
        // Only the last `tail_cap` bytes of `rest` can survive, so a single
        // enormous chunk costs one pass rather than a push-and-pop per byte.
        let keep = &rest[rest.len().saturating_sub(tail_cap)..];
        self.tail.extend(keep.iter().copied());
        while self.tail.len() > tail_cap {
            self.tail.pop_front();
        }
    }

    /// Decode to a `String`, splicing in the truncation marker when bytes
    /// were dropped.
    ///
    /// `from_utf8_lossy` throughout, as before: the head may end and the tail
    /// may begin mid-codepoint, and a replacement character at a seam is a
    /// better outcome than refusing the whole capture. It is also why the
    /// marker is added after decoding — inserting it into the byte stream
    /// would put ASCII either side of a partial codepoint and change where
    /// the replacement lands.
    pub fn finish(self) -> String {
        let (a, b) = self.tail.as_slices();
        let mut tail_bytes = Vec::with_capacity(a.len() + b.len());
        tail_bytes.extend_from_slice(a);
        tail_bytes.extend_from_slice(b);
        if !self.truncated() {
            // Under the cap the two halves are simply consecutive: the head
            // filled and the rest went to the tail without anything being
            // evicted. Decode them as ONE byte string, or a multi-byte
            // character straddling the boundary decodes as two replacement
            // characters instead of itself.
            let mut all = self.head;
            all.extend_from_slice(&tail_bytes);
            return String::from_utf8_lossy(&all).into_owned();
        }
        let head = String::from_utf8_lossy(&self.head).into_owned();
        let tail = String::from_utf8_lossy(&tail_bytes).into_owned();
        let dropped = self.total - (self.head.len() + tail_bytes.len()) as u64;
        format!(
            "{head}\n\
             [kanade] output truncated: {total} bytes written, {dropped} dropped \
             (kept the first {head_len} and last {tail_len}).\n\
             [kanade] stdout is the control path for a run, not a bulk transfer \
             channel — use a `collect:` job to ship large files.\n",
            total = self.total,
            head_len = self.head.len(),
            tail_len = self.tail.len(),
        ) + &tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(cap: usize, chunks: &[&[u8]]) -> String {
        let mut c = CappedOutput::new(cap);
        for ch in chunks {
            c.push(ch);
        }
        c.finish()
    }

    #[test]
    fn the_split_always_accounts_for_the_whole_budget() {
        // The property the two call sites relied on implicitly: head + tail
        // is the cap exactly, at every size including the ones where the
        // integer division loses a remainder.
        for cap in [0usize, 1, 2, 3, 4, 5, 10, 11, 99, 100, MAX_CAPTURE_BYTES] {
            let (h, t) = split(cap);
            assert_eq!(h + t, cap, "cap {cap} split into {h} + {t}");
        }
    }

    #[test]
    fn output_under_the_cap_is_returned_verbatim() {
        let out = feed(100, &[b"hello ", b"world"]);
        assert_eq!(out, "hello world");
        assert!(!out.contains("truncated"));
    }

    #[test]
    fn output_at_exactly_the_cap_is_not_truncated() {
        // The boundary is `>` not `>=`: a run that fits exactly should not be
        // told it lost bytes.
        let out = feed(10, &[b"0123456789"]);
        assert_eq!(out, "0123456789");
    }

    #[test]
    fn truncation_keeps_both_ends_and_says_what_it_dropped() {
        let body: Vec<u8> = (0..1000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let out = feed(120, &[&body]);
        // The head is what the run started with…
        assert!(out.starts_with("abcdefghij"), "out: {}", &out[..40]);
        // …and the tail is what it ended with, which is where an error lands.
        assert!(out.ends_with(std::str::from_utf8(&body[body.len() - 10..]).unwrap()));
        assert!(out.contains("1000 bytes written"));
        assert!(out.contains("880 dropped"));
    }

    #[test]
    fn the_tail_is_the_last_bytes_across_many_chunks() {
        // Regression shape: a tail assembled per-chunk rather than as a
        // sliding window would keep the end of the FIRST overflowing chunk.
        let mut c = CappedOutput::new(30);
        for i in 0..100u8 {
            c.push(&[b'0' + (i % 10)]);
        }
        let out = c.finish();
        assert!(
            out.ends_with("6789"),
            "out ends: {:?}",
            &out[out.len() - 8..]
        );
    }

    #[test]
    fn one_huge_chunk_is_bounded_the_same_as_many_small_ones() {
        let big = vec![b'x'; 10 * 1024 * 1024];
        let mut c = CappedOutput::new(1024);
        c.push(&big);
        assert_eq!(c.total(), 10 * 1024 * 1024);
        assert!(c.head.len() + c.tail.len() <= 1024);
    }

    #[test]
    fn a_zero_cap_keeps_nothing_but_still_counts() {
        let mut c = CappedOutput::new(0);
        c.push(b"discarded");
        assert_eq!(c.total(), 9);
        assert!(c.truncated());
        assert!(c.finish().contains("9 bytes written"));
    }

    #[test]
    fn a_codepoint_split_by_the_boundary_does_not_lose_the_rest() {
        // 3-byte characters, cut mid-sequence at both seams. The decode is
        // lossy by design; what must not happen is losing surrounding text.
        let s = "あいうえお".repeat(50);
        let out = feed(64, &[s.as_bytes()]);
        assert!(out.contains("truncated"));
        assert!(out.starts_with('あ'), "out: {out}");
    }

    #[test]
    fn the_cap_leaves_room_for_the_object_store_path() {
        // The cap exists to bound the pathological case, not to force every
        // verbose job inline. The ordering itself is a `const` assertion at
        // the constant; this exercises the behaviour that ordering is for.
        let out = feed(
            MAX_CAPTURE_BYTES,
            &[&vec![b'x'; STDOUT_INLINE_THRESHOLD * 4]],
        );
        assert!(!out.contains("truncated"));
    }
}
