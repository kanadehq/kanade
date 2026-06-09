//! In-memory live tail registry for in-flight job output.
//!
//! The command path reads a child's stdout/stderr incrementally (see
//! `process.rs`) and, while doing so, appends every chunk to a capped
//! ring buffer keyed by `result_id`. The `job.tail.<pc_id>` request/
//! reply handler (`job_tail.rs`) reads those buffers so the SPA can
//! poll a running job's output every few seconds — the same UX as the
//! agent-log auto-refresh, but scoped to one job.
//!
//! Design notes:
//! - **Process-wide singleton.** Like `process::staging_dir`'s
//!   `OnceLock`, the registry is genuinely process-global shared state
//!   (every running job's reader writes into it; the one tail server
//!   reads from it). Threading an `Arc` through every command-dispatch
//!   path (groups / replay / live sub / local scheduler) the way
//!   `check_sink` is threaded would touch a dozen signatures for no
//!   benefit — the buffer is pure ephemeral memory, never persisted.
//! - **Bounded memory.** Each stream keeps only the trailing
//!   [`LIVE_TAIL_CAP`] bytes; older bytes are dropped (and a
//!   `truncated` flag set) so a chatty long-running job can't balloon
//!   the agent's RSS.
//! - **Grace retention.** A finished job's buffer lingers for
//!   [`GRACE`] so the SPA's next poll still sees the final tail (plus
//!   `running = false`) before the persisted `execution_results` row
//!   has finished projecting. Eviction is lazy — swept on the next
//!   `register` / `get` — so no background reaper task is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Trailing bytes retained per stream. 128 KiB each (256 KiB/job
/// worst case) keeps a single job's live view well under NATS' 1 MB
/// default payload ceiling while still showing a meaningful window of
/// "what just scrolled past". Matches the spirit of `logs.rs`'s
/// 800 KB reply cap without competing with it.
pub const LIVE_TAIL_CAP: usize = 128 * 1024;

/// How long a finished job's buffer is retained before lazy eviction.
/// Covers the outbox → JetStream → projector lag so the SPA's final
/// poll still gets the tail before falling back to the stored row.
const GRACE: Duration = Duration::from_secs(60);

/// Amortise the O(N) front-drain. Draining `buf` from the front on
/// *every* push once at the cap is O(N) per write — pathological for a
/// chatty small-chunk job. Instead let the buffer overshoot the cap by
/// up to `SLACK` before physically reclaiming, so a drain happens at
/// most once per `SLACK` bytes (amortised O(1)). `snapshot` always
/// exposes only the trailing `LIVE_TAIL_CAP` bytes regardless of the
/// slack currently held.
const SLACK: usize = 16 * 1024;

/// A single stream's capped ring buffer.
#[derive(Default)]
struct Ring {
    buf: Vec<u8>,
    /// A physical front-drain has dropped older bytes. Note the
    /// *displayed* output can be a suffix even when this is false — see
    /// `snapshot`, which also trims any slack above the cap.
    truncated: bool,
}

impl Ring {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // Only reclaim once we're SLACK past the cap, so the O(N) drain
        // is amortised over many pushes. We drain back down to exactly
        // the cap.
        if self.buf.len() > LIVE_TAIL_CAP + SLACK {
            let overflow = self.buf.len() - LIVE_TAIL_CAP;
            self.buf.drain(..overflow);
            self.truncated = true;
        }
    }

    /// Lossy-decode the trailing `LIVE_TAIL_CAP` bytes. Returns
    /// `(text, trimmed)` where `trimmed` is true when anything was
    /// dropped from the front of the displayed output — either a
    /// physical drain happened, or the buffer is holding slack above
    /// the cap. The front is snapped to a UTF-8 char boundary so the
    /// cut never produces a leading U+FFFD that wasn't in the output.
    fn snapshot(&self) -> (String, bool) {
        let mut start = self.buf.len().saturating_sub(LIVE_TAIL_CAP);
        let trimmed = start > 0 || self.truncated;
        if trimmed {
            while start < self.buf.len() && (self.buf[start] & 0b1100_0000) == 0b1000_0000 {
                start += 1;
            }
        }
        (
            String::from_utf8_lossy(&self.buf[start..]).into_owned(),
            trimmed,
        )
    }
}

/// Live capture for one in-flight job.
pub struct LiveTail {
    stdout: Mutex<Ring>,
    stderr: Mutex<Ring>,
    /// Set false by `LiveHandle::drop` once the child has exited.
    running: Mutex<bool>,
    /// When the child finished, for lazy grace-window eviction.
    finished_at: Mutex<Option<Instant>>,
}

impl LiveTail {
    fn new() -> Self {
        Self {
            stdout: Mutex::new(Ring::default()),
            stderr: Mutex::new(Ring::default()),
            running: Mutex::new(true),
            finished_at: Mutex::new(None),
        }
    }

    /// Append a freshly-read stdout chunk.
    pub fn push_stdout(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.stdout.lock().unwrap().push(chunk);
    }

    /// Append a freshly-read stderr chunk.
    pub fn push_stderr(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.stderr.lock().unwrap().push(chunk);
    }

    /// Current tail snapshot: `(stdout, stderr, stdout_truncated,
    /// stderr_truncated, running)`.
    pub fn snapshot(&self) -> Snapshot {
        let (stdout, stdout_truncated) = self.stdout.lock().unwrap().snapshot();
        let (stderr, stderr_truncated) = self.stderr.lock().unwrap().snapshot();
        Snapshot {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            running: *self.running.lock().unwrap(),
        }
    }

    fn mark_finished(&self) {
        *self.running.lock().unwrap() = false;
        *self.finished_at.lock().unwrap() = Some(Instant::now());
    }

    fn evictable(&self) -> bool {
        matches!(*self.finished_at.lock().unwrap(), Some(t) if t.elapsed() >= GRACE)
    }
}

/// Snapshot returned to the `job.tail` handler.
pub struct Snapshot {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub running: bool,
}

type Map = Mutex<HashMap<String, Arc<LiveTail>>>;

fn registry() -> &'static Map {
    static REGISTRY: OnceLock<Map> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lazily drop buffers whose grace window has elapsed. Called on every
/// register/get so the map can't grow unbounded without a reaper task.
fn sweep(map: &mut HashMap<String, Arc<LiveTail>>) {
    map.retain(|_, tail| !tail.evictable());
}

/// Register a fresh live buffer for `result_id` and return an RAII
/// handle. The reader tasks append to `handle.tail()`; dropping the
/// handle (end of `handle_command`) marks the job finished and starts
/// the grace window. The buffer is evicted lazily on a later sweep.
pub fn register(result_id: &str) -> LiveHandle {
    let tail = Arc::new(LiveTail::new());
    let mut map = registry().lock().unwrap();
    sweep(&mut map);
    map.insert(result_id.to_string(), tail.clone());
    LiveHandle { tail }
}

/// Look up the live buffer for `result_id`, if the agent still holds
/// one (running, or finished but within the grace window).
pub fn get(result_id: &str) -> Option<Arc<LiveTail>> {
    let mut map = registry().lock().unwrap();
    sweep(&mut map);
    map.get(result_id).cloned()
}

/// RAII handle held by `handle_command` for the duration of a run.
pub struct LiveHandle {
    tail: Arc<LiveTail>,
}

impl LiveHandle {
    /// The shared buffer to hand to the stdout/stderr reader tasks.
    pub fn tail(&self) -> Arc<LiveTail> {
        self.tail.clone()
    }
}

impl Drop for LiveHandle {
    fn drop(&mut self) {
        // Mark finished but keep the entry — the grace window lets the
        // SPA's final poll still read the tail. Lazy sweep evicts it.
        self.tail.mark_finished();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_snapshot_round_trip() {
        let tail = LiveTail::new();
        tail.push_stdout(b"hello ");
        tail.push_stdout(b"world");
        tail.push_stderr(b"oops");
        let s = tail.snapshot();
        assert_eq!(s.stdout, "hello world");
        assert_eq!(s.stderr, "oops");
        assert!(!s.stdout_truncated);
        assert!(s.running);
    }

    #[test]
    fn snapshot_trims_to_cap_via_slack() {
        let mut ring = Ring::default();
        ring.push(&vec![b'a'; LIVE_TAIL_CAP]);
        let (_s, trimmed) = ring.snapshot();
        assert!(!trimmed, "exactly cap → nothing dropped");
        // Within the slack window: no physical drain yet, but snapshot
        // already trims to the trailing cap bytes.
        ring.push(b"bbbb");
        assert!(!ring.truncated, "slack not yet exceeded → no drain");
        let (s, trimmed) = ring.snapshot();
        assert!(trimmed);
        assert!(s.ends_with("bbbb"));
        assert!(s.len() <= LIVE_TAIL_CAP);
    }

    #[test]
    fn ring_physically_drains_past_cap_plus_slack() {
        let mut ring = Ring::default();
        ring.push(&vec![b'a'; LIVE_TAIL_CAP + SLACK + 100]);
        // Overshooting the slack triggers one physical drain back to cap.
        assert!(ring.truncated);
        assert_eq!(ring.buf.len(), LIVE_TAIL_CAP);
    }

    #[test]
    fn truncated_front_snaps_to_char_boundary() {
        let mut ring = Ring::default();
        // Multi-byte char at the very front, then fill so the snapshot
        // trim point (len - cap) lands *inside* that char.
        ring.push("あ".as_bytes()); // 3 bytes at the front
        ring.push(&vec![b'x'; LIVE_TAIL_CAP - 2]); // total = cap + 1 → trim 1
        let (s, trimmed) = ring.snapshot();
        assert!(trimmed);
        // snapshot must not start with a stray continuation byte.
        assert!(
            !s.starts_with('\u{FFFD}'),
            "front snapped to boundary: {s:?}"
        );
    }

    #[test]
    fn register_get_and_finish_flag() {
        let handle = register("res-test-1");
        handle.tail().push_stdout(b"live");
        let got = get("res-test-1").expect("registered");
        assert!(got.snapshot().running);
        assert_eq!(got.snapshot().stdout, "live");
        drop(handle);
        // Still present within grace, but no longer running.
        let after = get("res-test-1").expect("retained in grace");
        assert!(!after.snapshot().running);
    }
}
