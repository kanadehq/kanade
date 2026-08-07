//! Per-file retry state for the three outbox drain loops.
//!
//! All three (`outbox`, `events_outbox`, `obs_outbox`) poll their directory
//! once a second and try every file they find. That is right when the broker
//! is down — the outage clears and everything flushes within a second — and
//! wrong when a *particular file* cannot be published, because "try
//! everything, every second, forever" has no other ending.
//!
//! #499 already gave the **undecodable** case an ending: a parse failure
//! deletes the file and moves on. The **undeliverable** case had none.
//!
//! What that cost, measured on one host (#1319): 157 files totalling 1.14 GiB
//! in the outbox, two of them ~550 MB, after `OBJECT_RESULT_OUTPUT` hit its
//! cap and every result over the inline threshold became unpublishable. The
//! agent held ~74% of a core with RSS spiking to ~1 GiB for about two days,
//! re-reading and re-parsing 1.14 GiB every second — because `publish_one`
//! begins with `std::fs::read` of the whole file, so the *read* is the cost,
//! not the publish.
//!
//! And `agent.log` said nothing. The per-file failure was logged at `debug!`,
//! so an agent at INFO burned a core in silence. It was found by looking in
//! the outbox directory.
//!
//! Three things follow, and they are separable:
//!
//! 1. **Back off per file.** A transient outage still clears in a second,
//!    because backoff only advances on repeated failure of the *same* file.
//! 2. **Skip before reading.** The check has to happen before `publish_one`
//!    is called at all, or it saves nothing — the read is the expense.
//! 3. **Stop, eventually.** A file that has failed enough times is not going
//!    to succeed by being tried again; it is quarantined so the loop stops
//!    seeing it and the payload survives for diagnosis. That is exactly what
//!    recovery required by hand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// First delay after a failure, and the base the schedule doubles from.
///
/// Matches the drain interval: one failure costs nothing extra, so the
/// common case (broker down, everything fails once, comes back) behaves
/// exactly as it did before.
const BASE_DELAY: Duration = Duration::from_secs(1);

/// Ceiling on the backoff.
///
/// A guard on the schedule, not a delay the drains normally reach. With
/// `QUARANTINE_AFTER` at 10 the measured sequence is 1, 2, 4, … 256 s, and
/// the first delay the ceiling would clamp is the 10th — the same failure
/// that returns [`AfterFailure::Quarantine`]. So it is computed and then the
/// file leaves the queue: the ceiling exists to bound the schedule if that
/// threshold is ever raised, and to keep the arithmetic total, rather than to
/// govern a wait anyone observes today.
///
/// Stated because the obvious reading — "a stuck file keeps retrying every
/// five minutes, so it drains by itself once an operator fixes the cause" —
/// is FALSE. Quarantine is terminal without operator action: the file is
/// moved to `stuck/` and nothing puts it back. That is the trade this module
/// makes (a core is worth more than an automatic recovery nobody was
/// waiting on), but it is only an honest trade if someone learns the file is
/// there.
const MAX_DELAY: Duration = Duration::from_secs(300);

/// Consecutive failures before a file is quarantined.
///
/// Doubling from 1 s puts the 10th attempt at 1+2+4+…+256 = 511 s of
/// accumulated waiting — roughly 8.5 minutes. Long enough that no ordinary
/// broker blip gets there, short enough that a genuinely stuck file stops
/// costing anything within the hour.
///
/// Raising it makes [`MAX_DELAY`] start to bind; see the note there.
const QUARANTINE_AFTER: u32 = 10;

/// Failure count at which the log escalates from `debug!` to `warn!`.
///
/// Not the first failure: a broker restart would then warn once per queued
/// file, which is noise on the one occasion the queue is largest. By the
/// fifth consecutive failure of the same file something is wrong with the
/// file or its destination, which is operator-actionable.
const WARN_AFTER: u32 = 5;

/// Subdirectory holding files the drain gave up on.
///
/// Inside the outbox dir, so the move is a rename on the same volume, and
/// harmless to the scan: `drain_once` keeps only paths whose extension is
/// `json`, and a directory has none.
pub const STUCK_DIR: &str = "stuck";

#[derive(Debug, Clone, Copy)]
struct Attempt {
    failures: u32,
    /// When this file may be tried again.
    next: Instant,
}

/// What the caller should do with a file that just failed.
#[derive(Debug, PartialEq, Eq)]
pub enum AfterFailure {
    /// Leave it; it will be retried once the backoff elapses.
    Retry,
    /// It has failed too many times — move it out of the way.
    Quarantine,
}

/// Per-file failure counts and next-attempt times for one drain loop.
///
/// Keyed by path. Entries are dropped when a file succeeds, is quarantined,
/// or disappears, so the map tracks the *currently stuck* set rather than
/// everything the loop has ever seen.
#[derive(Debug, Default)]
pub struct RetryLedger {
    attempts: HashMap<PathBuf, Attempt>,
}

impl RetryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this file is due for another attempt.
    ///
    /// Call this BEFORE reading the file. Answering after the read would
    /// leave the actual cost — `std::fs::read` plus a `serde_json` parse of
    /// the whole payload — exactly where it was.
    pub fn is_due(&self, path: &Path, now: Instant) -> bool {
        self.attempts.get(path).is_none_or(|a| now >= a.next)
    }

    /// Record a failure and say what to do next.
    pub fn record_failure(&mut self, path: &Path, now: Instant) -> AfterFailure {
        let entry = self.attempts.entry(path.to_path_buf()).or_insert(Attempt {
            failures: 0,
            next: now,
        });
        entry.failures += 1;
        // Doubling from the base, saturating at the ceiling. `1 << n`
        // overflows for a long-lived stuck file, so the shift is bounded
        // rather than the product.
        let shift = entry.failures.saturating_sub(1).min(16);
        entry.next = now + (BASE_DELAY * (1u32 << shift)).min(MAX_DELAY);
        if entry.failures >= QUARANTINE_AFTER {
            AfterFailure::Quarantine
        } else {
            AfterFailure::Retry
        }
    }

    /// Consecutive failures recorded for this file (0 if none).
    pub fn failures(&self, path: &Path) -> u32 {
        self.attempts.get(path).map_or(0, |a| a.failures)
    }

    /// Whether a failure at this count deserves an operator's attention.
    pub fn should_warn(&self, path: &Path) -> bool {
        self.failures(path) >= WARN_AFTER
    }

    pub fn record_success(&mut self, path: &Path) {
        self.attempts.remove(path);
    }

    /// Drop state for files that are no longer on disk.
    ///
    /// Without this the map grows for the life of the agent: a file that
    /// fails once and is then removed by any other path (an operator
    /// clearing the queue, a quarantine) would keep its entry forever.
    pub fn retain_present(&mut self, present: &[PathBuf]) {
        // Through a set, not `slice::contains`: a linear scan per tracked
        // file is quadratic in the queue length, and the queue is longest
        // exactly when the broker has been down — the moment this loop is
        // least able to afford it.
        let present: std::collections::HashSet<&Path> =
            present.iter().map(PathBuf::as_path).collect();
        self.attempts.retain(|p, _| present.contains(p.as_path()));
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.attempts.len()
    }
}

/// Move a file out of the drain loop's way, preserving it for diagnosis.
///
/// Returns whether the file actually left the directory.
///
/// That return value is load-bearing, not informational. The caller clears
/// the file's ledger entry on quarantine, and clearing it for a file that is
/// still there restarts it at zero failures with no backoff — which is the
/// 1 Hz re-read loop this module exists to remove, reinstated in the failure
/// path. A file that could not be moved must keep its history and stay
/// backed off.
///
/// The move itself stays best-effort: a read-only directory or a full disk
/// is not something a caller could handle better, so it is logged and the
/// backoff continues to keep the file cheap.
pub fn quarantine(path: &Path, label: &str) -> bool {
    let Some(dir) = path.parent() else {
        warn!(path = %path.display(), "{label}: cannot quarantine a file with no parent");
        return false;
    };
    let stuck = dir.join(STUCK_DIR);
    if let Err(e) = std::fs::create_dir_all(&stuck) {
        warn!(error = %e, dir = %stuck.display(), "{label}: cannot create quarantine dir");
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let Some(dest) = free_name(&stuck, name) else {
        warn!(
            dir = %stuck.display(),
            "{label}: no free name in the quarantine dir; leaving the file in place",
        );
        return false;
    };
    match std::fs::rename(path, &dest) {
        Ok(()) => {
            info!(
                from = %path.display(),
                to = %dest.display(),
                "{label}: file could not be published after {QUARANTINE_AFTER} attempts — quarantined",
            );
            true
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "{label}: quarantine move failed; leaving the file in place",
            );
            false
        }
    }
}

/// A destination that does not already exist.
///
/// `std::fs::rename` replaces an existing destination silently, and outbox
/// names are deterministic — `enqueue` writes `{result_id}.json`. Re-enqueuing
/// the same id and getting stuck again would then destroy the payload kept
/// from the first attempt, which is the one thing quarantining is for.
///
/// The first file keeps its plain name so the common case stays readable;
/// later ones get `.1`, `.2`, … Bounded rather than looping forever, because
/// an unbounded search would be its own way of spending a core.
fn free_name(dir: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let plain = dir.join(name);
    if !plain.exists() {
        return Some(plain);
    }
    (1..1000).find_map(|n| {
        let mut alt = name.to_os_string();
        alt.push(format!(".{n}"));
        let p = dir.join(alt);
        (!p.exists()).then_some(p)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn an_unseen_file_is_due_immediately() {
        let l = RetryLedger::new();
        assert!(l.is_due(&p("a.json"), Instant::now()));
    }

    #[test]
    fn one_failure_delays_by_the_drain_interval_not_more() {
        // The common case is a broker outage: every file fails once, the
        // broker returns, and the queue must flush on the next tick as it
        // always did. Backoff only bites on REPEATED failure of one file.
        let mut l = RetryLedger::new();
        let t = Instant::now();
        assert_eq!(l.record_failure(&p("a.json"), t), AfterFailure::Retry);
        assert!(!l.is_due(&p("a.json"), t));
        assert!(l.is_due(&p("a.json"), t + BASE_DELAY));
    }

    /// The schedule past `QUARANTINE_AFTER` is unreachable through the
    /// drains, which quarantine the file first. Pinned anyway: it is what
    /// keeps the arithmetic total, and it is the only description of the
    /// region a future increase of that threshold would expose.
    #[test]
    fn the_delay_doubles_and_then_stops_growing() {
        let mut l = RetryLedger::new();
        let mut t = Instant::now();
        let mut seen = Vec::new();
        for _ in 0..12 {
            l.record_failure(&p("a.json"), t);
            let next = l.attempts[&p("a.json")].next;
            seen.push(next - t);
            t = next;
        }
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[2], Duration::from_secs(4));
        // …and never past the ceiling, however long it stays stuck.
        assert!(seen.iter().all(|d| *d <= MAX_DELAY));
        assert_eq!(*seen.last().unwrap(), MAX_DELAY);
    }

    #[test]
    fn a_long_stuck_file_does_not_overflow_the_shift() {
        // `1 << failures` would panic in debug once failures passed 31.
        // Bounding the shift rather than the product is what keeps this
        // arithmetic total.
        let mut l = RetryLedger::new();
        let t = Instant::now();
        for _ in 0..200 {
            l.record_failure(&p("a.json"), t);
        }
        assert_eq!(l.attempts[&p("a.json")].next - t, MAX_DELAY);
    }

    #[test]
    fn the_ceiling_is_never_actually_waited_on_at_the_current_threshold() {
        // The measured schedule, so the doc comments cannot drift from it:
        // the largest delay a drain ever waits out is the 9th (256 s), and
        // the first one the ceiling would clamp arrives with `Quarantine`.
        let mut l = RetryLedger::new();
        let mut t = Instant::now();
        let mut waited = Duration::ZERO;
        for n in 1..=QUARANTINE_AFTER {
            let outcome = l.record_failure(&p("a.json"), t);
            let delay = l.attempts[&p("a.json")].next - t;
            if n < QUARANTINE_AFTER {
                assert_eq!(outcome, AfterFailure::Retry);
                assert!(delay < MAX_DELAY, "delay {delay:?} at failure {n}");
                waited += delay;
            } else {
                assert_eq!(outcome, AfterFailure::Quarantine);
                assert_eq!(delay, MAX_DELAY, "the ceiling first binds here…");
                // …and is never waited out, because the file leaves the queue.
            }
            t = l.attempts[&p("a.json")].next;
        }
        assert_eq!(waited, Duration::from_secs(511)); // ≈ 8.5 min
    }

    #[test]
    fn it_gives_up_after_the_quarantine_threshold() {
        let mut l = RetryLedger::new();
        let t = Instant::now();
        for i in 1..QUARANTINE_AFTER {
            assert_eq!(
                l.record_failure(&p("a.json"), t),
                AfterFailure::Retry,
                "failure {i} should still retry"
            );
        }
        assert_eq!(l.record_failure(&p("a.json"), t), AfterFailure::Quarantine);
    }

    #[test]
    fn warning_starts_late_enough_to_survive_a_broker_restart() {
        // A restart fails every queued file once. Warning on the first
        // failure would produce one warn per file precisely when the queue
        // is longest — noise at the worst moment.
        let mut l = RetryLedger::new();
        let t = Instant::now();
        l.record_failure(&p("a.json"), t);
        assert!(!l.should_warn(&p("a.json")));
        for _ in 1..WARN_AFTER {
            l.record_failure(&p("a.json"), t);
        }
        assert!(l.should_warn(&p("a.json")));
    }

    #[test]
    fn success_clears_the_history() {
        let mut l = RetryLedger::new();
        let t = Instant::now();
        l.record_failure(&p("a.json"), t);
        l.record_failure(&p("a.json"), t);
        l.record_success(&p("a.json"));
        assert_eq!(l.failures(&p("a.json")), 0);
        assert!(l.is_due(&p("a.json"), t));
    }

    #[test]
    fn files_that_left_the_directory_stop_being_tracked() {
        let mut l = RetryLedger::new();
        let t = Instant::now();
        l.record_failure(&p("a.json"), t);
        l.record_failure(&p("b.json"), t);
        assert_eq!(l.tracked(), 2);
        l.retain_present(&[p("b.json")]);
        assert_eq!(l.tracked(), 1);
        assert_eq!(l.failures(&p("a.json")), 0);
    }

    #[test]
    fn backoff_is_per_file_not_global() {
        // The whole point: one stuck file must not delay the others. A
        // global backoff would have made the observed incident worse, not
        // better — the 155 healthy-sized files were waiting behind the two
        // large ones.
        let mut l = RetryLedger::new();
        let t = Instant::now();
        for _ in 0..5 {
            l.record_failure(&p("stuck.json"), t);
        }
        assert!(!l.is_due(&p("stuck.json"), t));
        assert!(l.is_due(&p("fresh.json"), t));
    }

    /// Isolated temp dir, named per test so they can run in parallel.
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kanade-outbox-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_failed_quarantine_says_so() {
        // The caller clears the ledger entry on a successful quarantine.
        // Reporting success for a file still sitting in the directory would
        // restart it at zero failures with no backoff — this module's own
        // bug, in the failure path. Forced here by making `stuck` a FILE, so
        // `create_dir_all` cannot succeed.
        let dir = tmp("failed");
        std::fs::write(dir.join(STUCK_DIR), b"not a directory").unwrap();
        let f = dir.join("a.json");
        std::fs::write(&f, b"{}").unwrap();

        assert!(!quarantine(&f, "outbox"));
        assert!(
            f.exists(),
            "the file must stay put so its backoff still applies"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_does_not_overwrite_an_earlier_payload() {
        // Outbox names are deterministic (`{result_id}.json`), so the same
        // name can arrive twice. `fs::rename` would replace silently and
        // destroy the bytes the first quarantine was preserving.
        let dir = tmp("collide");
        let f = dir.join("a.json");

        std::fs::write(&f, b"first").unwrap();
        assert!(quarantine(&f, "outbox"));
        std::fs::write(&f, b"second").unwrap();
        assert!(quarantine(&f, "outbox"));

        let stuck = dir.join(STUCK_DIR);
        assert_eq!(std::fs::read(stuck.join("a.json")).unwrap(), b"first");
        assert_eq!(std::fs::read(stuck.join("a.json.1")).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_moves_the_file_and_keeps_its_bytes() {
        let dir = std::env::temp_dir().join(format!("kanade-outbox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.json");
        std::fs::write(&f, b"{\"payload\":1}").unwrap();

        assert!(quarantine(&f, "outbox"));

        assert!(!f.exists(), "the drain loop must stop seeing it");
        let moved = dir.join(STUCK_DIR).join("a.json");
        assert_eq!(
            std::fs::read(&moved).unwrap(),
            b"{\"payload\":1}",
            "the payload is preserved for diagnosis, not deleted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
