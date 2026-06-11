//! Boot sentinel: auto-rollback to a last-known-good binary when a
//! freshly-swapped binary crash-loops on startup (#582).
//!
//! Both `kanade-backend` and `kanade-agent` are **self-replacing**
//! Windows services: an update overwrites the running exe and the
//! Service Control Manager restarts it. If the new binary crashes
//! during early boot (exactly what the #573 JetStream regression did
//! to the backend on 2026-06-11), nothing rolls it back — the SCM
//! just restarts the same broken exe forever.
//!
//! This module gates each boot. The swap step [`arm_for_swap`] writes
//! a sentinel and snapshots the outgoing (known-good) binary to
//! `<exe>.last-good`. Every boot calls [`check_on_boot`] as the very
//! first thing in `main()` — before NATS, the DB, or any bootstrap
//! that can fail — which increments a persisted attempt counter and,
//! once it crosses the crash-loop threshold, restores `.last-good`
//! over the live exe and **quarantines** the failed version so the
//! autonomous self-update path won't immediately re-deploy it (which
//! would loop rollout↔rollback forever). [`confirm_healthy`], called
//! once the process is genuinely up, promotes the running exe to the
//! new last-good and clears the sentinel.
//!
//! The attempt counter is persisted BEFORE the crashy code runs, so a
//! hard crash still advances it: boot 1..N each bump the counter, and
//! the boot that crosses the threshold rolls back, after which the SCM
//! restarts into `.last-good`.
//!
//! ## Windows exe lock
//!
//! A running exe is locked on Windows (no overwrite), but a *rename*
//! of the running exe IS allowed. So the rollback renames the live exe
//! aside (`<exe>.rollback-bak`) and copies `.last-good` into place,
//! then the caller exits so the SCM relaunches the restored binary.
//! The same rename-then-replace works on Unix and in unit tests (where
//! the "exe" is just a temp file), so the logic is testable everywhere.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// Filenames under the data dir / next to the exe.
const SENTINEL_FILE: &str = ".boot-sentinel.json";
const QUARANTINE_FILE: &str = ".boot-quarantine.json";
const LAST_GOOD_SUFFIX: &str = "last-good";
const ROLLBACK_BAK_SUFFIX: &str = "rollback-bak";

/// Crash-loop threshold. Boot attempts `1..=N` proceed; attempt
/// `N+1` triggers the rollback (the check is `attempts <= max`). So
/// the default 3 gives a freshly-swapped binary three chances to
/// confirm healthy and rolls back on the fourth boot — enough to ride
/// out a one-off transient (slow disk, flaky first NATS connect)
/// without masking a genuinely broken binary.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct Sentinel {
    /// The version that was swapped in and is awaiting confirmation.
    version: String,
    /// Boot attempts so far for that version (incremented before the
    /// boot can crash).
    attempts: u32,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
struct Quarantine {
    /// Versions that crash-looped on boot and were rolled back. The
    /// self-update path refuses to swap to any version listed here.
    versions: Vec<String>,
}

/// What [`check_on_boot`] decided. On `RolledBack` the caller MUST
/// exit (non-zero) so the service manager relaunches the restored
/// last-good binary.
#[derive(Debug, PartialEq, Eq)]
pub enum BootDecision {
    /// No pending swap, or the swap is still within its attempt
    /// budget — continue booting normally.
    Proceed,
    /// The swapped-in binary crash-looped; `.last-good` has been
    /// restored over the live exe. Exit now and let the SCM relaunch.
    RolledBack { from: String },
}

/// Per-role boot guard. Construct once at the top of `main()`.
pub struct BootSentinel {
    sentinel_path: PathBuf,
    quarantine_path: PathBuf,
    exe: PathBuf,
    last_good: PathBuf,
    version: String,
}

impl BootSentinel {
    /// `data_dir` holds the sentinel/quarantine state; `exe` is the
    /// live binary path (`std::env::current_exe()` in production);
    /// `version` is this binary's own version string.
    pub fn new(data_dir: &Path, exe: PathBuf, version: impl Into<String>) -> Self {
        let last_good = sibling(&exe, LAST_GOOD_SUFFIX);
        Self {
            sentinel_path: data_dir.join(SENTINEL_FILE),
            quarantine_path: data_dir.join(QUARANTINE_FILE),
            exe,
            last_good,
            version: version.into(),
        }
    }

    /// Call FIRST in `main()`, before anything that can crash.
    ///
    /// - No sentinel → `Proceed`.
    /// - Sentinel for a different version (we already rolled back, or
    ///   last-good is now live) → clear it, `Proceed`.
    /// - Sentinel for THIS version → bump attempts; attempts
    ///   `1..=max_attempts` `Proceed`, and the first that EXCEEDS
    ///   `max_attempts` rolls back to `.last-good` + quarantines the
    ///   bad version and returns `RolledBack`.
    pub fn check_on_boot(&self, max_attempts: u32) -> BootDecision {
        let Some(mut sentinel) = self.read_sentinel() else {
            return BootDecision::Proceed;
        };
        if sentinel.version != self.version {
            // A different binary is running than the sentinel expected
            // — the swap already resolved (rollback or a later update).
            // Stale marker; drop it and boot normally.
            let _ = fs::remove_file(&self.sentinel_path);
            return BootDecision::Proceed;
        }

        sentinel.attempts += 1;
        info!(
            version = %self.version,
            attempts = sentinel.attempts,
            max = max_attempts,
            "boot sentinel: unconfirmed swap, recording boot attempt",
        );
        // Persist the bumped count BEFORE returning so a crash later
        // this boot still advances the counter.
        self.write_sentinel(&sentinel);

        if sentinel.attempts <= max_attempts {
            return BootDecision::Proceed;
        }

        // Crash-loop confirmed → roll back.
        match self.rollback() {
            Ok(true) => {
                self.quarantine(&self.version);
                let _ = fs::remove_file(&self.sentinel_path);
                error!(
                    version = %self.version,
                    attempts = sentinel.attempts,
                    "boot sentinel: crash-loop — rolled back to last-good and quarantined this version",
                );
                BootDecision::RolledBack {
                    from: self.version.clone(),
                }
            }
            Ok(false) => {
                // No last-good to roll back to (first install). We
                // can't restore a binary, but still quarantine the bad
                // version so that IF a good binary ever comes up it
                // won't re-deploy this one — and so the self-update
                // path's refusal is consistent. We keep Proceeding
                // (nothing better to do than let it keep trying).
                self.quarantine(&self.version);
                error!(
                    version = %self.version,
                    "boot sentinel: crash-loop but no last-good binary to roll back to; \
                     quarantined the version and continuing (no rollback target)",
                );
                BootDecision::Proceed
            }
            Err(e) => {
                error!(error = %e, "boot sentinel: rollback failed; continuing without it");
                BootDecision::Proceed
            }
        }
    }

    /// Call once the process is confirmed healthy (backend: serving;
    /// agent: NATS connected + first heartbeat). Promotes the live exe
    /// to `.last-good` and clears the sentinel, so this version becomes
    /// the rollback target for the next swap.
    pub fn confirm_healthy(&self) -> io::Result<()> {
        // Only promote/clear when there's an unconfirmed swap for THIS
        // version — a plain restart of an already-good binary has no
        // sentinel and shouldn't churn last-good.
        let pending = matches!(self.read_sentinel(), Some(s) if s.version == self.version);
        if pending {
            if let Err(e) = fs::copy(&self.exe, &self.last_good) {
                warn!(error = %e, "boot sentinel: could not promote exe to last-good");
            } else {
                info!(version = %self.version, "boot sentinel: confirmed healthy, promoted to last-good");
            }
            let _ = fs::remove_file(&self.sentinel_path);
        } else if !self.last_good.exists() {
            // First-ever healthy boot with no swap in flight: seed
            // last-good so a future bad swap has something to fall
            // back to.
            if let Err(e) = fs::copy(&self.exe, &self.last_good) {
                warn!(error = %e, "boot sentinel: could not seed last-good");
            } else {
                info!(version = %self.version, "boot sentinel: seeded initial last-good");
            }
        }
        Ok(())
    }

    /// Call at swap time (deploy / self-update), before restarting into
    /// the new binary. Snapshots the CURRENT (outgoing, known-good) exe
    /// to `.last-good` and writes a fresh sentinel for `new_version` so
    /// the next boot is gated.
    ///
    /// `current_exe` is the still-running good binary (copy it now,
    /// before it's overwritten by the swap).
    pub fn arm_for_swap(&self, current_exe: &Path, new_version: &str) -> io::Result<()> {
        // The outgoing binary booted fine (it's running), so it's the
        // rollback target.
        fs::copy(current_exe, &self.last_good)?;
        self.write_sentinel(&Sentinel {
            version: new_version.to_string(),
            attempts: 0,
        });
        info!(
            new_version,
            "boot sentinel: armed for swap (last-good snapshotted)"
        );
        Ok(())
    }

    /// True if `version` was rolled back after a failed boot. The
    /// self-update path consults this before swapping so a bad rollout
    /// target isn't re-attempted in a loop.
    pub fn is_quarantined(&self, version: &str) -> bool {
        self.read_quarantine().versions.iter().any(|v| v == version)
    }

    /// Every quarantined version (#582 Phase 2). The agent reports
    /// these in its heartbeat so the SPA rollout view can flag which
    /// PCs failed to adopt a target.
    pub fn quarantined_versions(&self) -> Vec<String> {
        self.read_quarantine().versions
    }

    /// Drop `version` from quarantine (operator re-published a fixed
    /// binary under the same version string).
    pub fn clear_quarantine(&self, version: &str) -> io::Result<()> {
        let mut q = self.read_quarantine();
        let before = q.versions.len();
        q.versions.retain(|v| v != version);
        if q.versions.len() != before {
            self.write_quarantine(&q);
        }
        Ok(())
    }

    // ── internals ────────────────────────────────────────────────

    /// Rename the live exe aside and copy `.last-good` into its place.
    /// Returns `Ok(false)` when there's no last-good to restore.
    fn rollback(&self) -> io::Result<bool> {
        if !self.last_good.exists() {
            return Ok(false);
        }
        let bak = sibling(&self.exe, ROLLBACK_BAK_SUFFIX);
        // Best-effort: a leftover .rollback-bak from a prior cycle
        // would block the rename.
        let _ = fs::remove_file(&bak);
        // Rename of a running/locked exe is permitted on Windows; the
        // copy then lands a fresh file at the exe path.
        fs::rename(&self.exe, &bak)?;
        if let Err(e) = fs::copy(&self.last_good, &self.exe) {
            // The exe path is now EMPTY (renamed to .rollback-bak,
            // copy failed). Put the original back so the next SCM
            // restart isn't left with no binary at all — mirroring the
            // compensating rollback in self_update::swap_and_restart.
            match fs::rename(&bak, &self.exe) {
                Ok(()) => warn!(
                    error = %e,
                    "boot sentinel: last-good copy failed; restored the original exe in place",
                ),
                Err(restore_err) => error!(
                    error = %e,
                    restore_error = %restore_err,
                    exe = ?self.exe,
                    backup = ?bak,
                    "boot sentinel: last-good copy failed AND restore failed — service binary path \
                     is EMPTY; manual repair required (rename the .rollback-bak file back)",
                ),
            }
            return Err(e);
        }
        Ok(true)
    }

    fn quarantine(&self, version: &str) {
        let mut q = self.read_quarantine();
        if !q.versions.iter().any(|v| v == version) {
            q.versions.push(version.to_string());
            self.write_quarantine(&q);
        }
    }

    fn read_sentinel(&self) -> Option<Sentinel> {
        let bytes = fs::read(&self.sentinel_path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "boot sentinel: corrupt sentinel, ignoring");
                let _ = fs::remove_file(&self.sentinel_path);
                None
            }
        }
    }

    fn write_sentinel(&self, s: &Sentinel) {
        match serde_json::to_vec(s) {
            Ok(bytes) => {
                if let Err(e) = atomic_write(&self.sentinel_path, &bytes) {
                    warn!(error = %e, "boot sentinel: write sentinel failed");
                }
            }
            Err(e) => warn!(error = %e, "boot sentinel: encode sentinel failed"),
        }
    }

    fn read_quarantine(&self) -> Quarantine {
        fs::read(&self.quarantine_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_quarantine(&self, q: &Quarantine) {
        match serde_json::to_vec(q) {
            Ok(bytes) => {
                if let Err(e) = atomic_write(&self.quarantine_path, &bytes) {
                    warn!(error = %e, "boot sentinel: write quarantine failed");
                }
            }
            Err(e) => warn!(error = %e, "boot sentinel: encode quarantine failed"),
        }
    }
}

/// `<path>.<suffix>` (e.g. `kanade-agent.exe` → `kanade-agent.exe.last-good`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// Write via a temp file + rename so a crash mid-write never leaves a
/// torn sentinel/quarantine the next boot would misread. Creates the
/// parent dir first — on a clean install the data dir may not exist
/// yet when the first swap arms the sentinel.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = sibling(path, "tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a sentinel over a temp dir with a fake "exe" containing
    /// `body`, at `version`.
    fn fixture(version: &str, body: &str) -> (TempDir, BootSentinel) {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("kanade-agent.exe");
        fs::write(&exe, body).unwrap();
        let s = BootSentinel::new(dir.path(), exe, version);
        (dir, s)
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn no_sentinel_proceeds() {
        let (_d, s) = fixture("1.0.0", "v1");
        assert_eq!(s.check_on_boot(3), BootDecision::Proceed);
    }

    #[test]
    fn arm_snapshots_last_good_and_writes_sentinel() {
        let (_d, s) = fixture("1.0.0", "v1-good");
        // Pretend a new binary will be swapped in; arm with the
        // current (good) exe.
        s.arm_for_swap(&s.exe.clone(), "2.0.0").unwrap();
        assert_eq!(read(&s.last_good), "v1-good");
        assert!(s.sentinel_path.exists());
    }

    #[test]
    fn healthy_swap_confirms_and_promotes() {
        let (_d, s) = fixture("1.0.0", "v1-good");
        s.arm_for_swap(&s.exe.clone(), "2.0.0").unwrap();
        // Now the 2.0.0 binary boots. Simulate by writing new exe body
        // + a 2.0.0 sentinel-aware guard.
        fs::write(&s.exe, "v2").unwrap();
        let s2 = BootSentinel::new(s.sentinel_path.parent().unwrap(), s.exe.clone(), "2.0.0");
        assert_eq!(s2.check_on_boot(3), BootDecision::Proceed);
        s2.confirm_healthy().unwrap();
        // last-good is now v2; sentinel cleared.
        assert_eq!(read(&s2.last_good), "v2");
        assert!(!s2.sentinel_path.exists());
        assert!(!s2.is_quarantined("2.0.0"));
    }

    #[test]
    fn crash_loop_rolls_back_and_quarantines() {
        let (_d, s) = fixture("1.0.0", "v1-good");
        s.arm_for_swap(&s.exe.clone(), "2.0.0").unwrap();
        // The 2.0.0 binary is now live and crash-loops.
        fs::write(&s.exe, "v2-broken").unwrap();
        let bad = BootSentinel::new(s.sentinel_path.parent().unwrap(), s.exe.clone(), "2.0.0");

        // attempts 1..3 proceed (each boot would then crash).
        assert_eq!(bad.check_on_boot(3), BootDecision::Proceed); // 1
        assert_eq!(bad.check_on_boot(3), BootDecision::Proceed); // 2
        assert_eq!(bad.check_on_boot(3), BootDecision::Proceed); // 3
        // 4th attempt crosses the threshold → rollback.
        assert_eq!(
            bad.check_on_boot(3),
            BootDecision::RolledBack {
                from: "2.0.0".into()
            }
        );
        // Live exe restored to the good binary; bad version quarantined;
        // sentinel cleared.
        assert_eq!(read(&bad.exe), "v1-good");
        assert!(bad.is_quarantined("2.0.0"));
        assert!(!bad.sentinel_path.exists());
    }

    #[test]
    fn rollback_without_last_good_proceeds_but_quarantines() {
        // No arm/last-good: a sentinel exists but nothing to restore.
        let (_d, s) = fixture("2.0.0", "v2-broken");
        s.write_sentinel(&Sentinel {
            version: "2.0.0".into(),
            attempts: 5,
        });
        // attempts already past max; rollback finds no last-good, so we
        // Proceed (can't restore) but still quarantine so a future good
        // binary won't re-deploy this one.
        assert_eq!(s.check_on_boot(3), BootDecision::Proceed);
        assert!(s.is_quarantined("2.0.0"));
    }

    #[test]
    fn stale_sentinel_for_other_version_is_cleared() {
        let (_d, s) = fixture("1.0.0", "v1");
        // A sentinel left for a version we are NOT (e.g. we are the
        // rolled-back last-good).
        s.write_sentinel(&Sentinel {
            version: "2.0.0".into(),
            attempts: 9,
        });
        assert_eq!(s.check_on_boot(3), BootDecision::Proceed);
        assert!(!s.sentinel_path.exists());
    }

    #[test]
    fn quarantine_clear_roundtrip() {
        let (_d, s) = fixture("1.0.0", "v1");
        s.quarantine("2.0.0");
        s.quarantine("2.0.1");
        assert!(s.is_quarantined("2.0.0"));
        assert!(s.is_quarantined("2.0.1"));
        s.clear_quarantine("2.0.0").unwrap();
        assert!(!s.is_quarantined("2.0.0"));
        assert!(s.is_quarantined("2.0.1"));
    }

    #[test]
    fn attempt_counter_persists_across_checks() {
        // Each check_on_boot simulates a separate boot of the same
        // crashing binary; the counter must accumulate via the file.
        let (_d, s) = fixture("1.0.0", "good");
        s.arm_for_swap(&s.exe.clone(), "2.0.0").unwrap();
        fs::write(&s.exe, "broken").unwrap();
        let dir = s.sentinel_path.parent().unwrap().to_path_buf();
        let mk = || BootSentinel::new(&dir, s.exe.clone(), "2.0.0");
        assert_eq!(mk().check_on_boot(2), BootDecision::Proceed); // 1
        assert_eq!(mk().check_on_boot(2), BootDecision::Proceed); // 2
        assert!(matches!(
            mk().check_on_boot(2),
            BootDecision::RolledBack { .. }
        )); // 3 > 2
    }
}
