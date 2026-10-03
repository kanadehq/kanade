//! Durable admission ledger for commands delivered over NATS.
//!
//! The in-memory request-id cache this replaces was empty after every restart,
//! so a retained or replayed command could run again the moment the agent came
//! back. The ledger records, on disk and *before* a delivery is acknowledged or
//! anything is launched, that a `(pc_id, request_id)` was admitted, what bytes
//! it arrived as, and how far it got.
//!
//! # Storage
//!
//! One JSON file per request, named after a hash of the key, in a dedicated
//! directory — the same file-per-request shape the result outbox uses, written
//! tmp → fsync → rename so a reader never sees half a record and a crash leaves
//! either the old record or the new one. A file store is enough here: every
//! guarantee below is a single-record atomic replace guarded by one in-process
//! lock, and no query ever spans records. An embedded database would add a
//! dependency without adding a guarantee.
//!
//! # States
//!
//! ```text
//! Pending ──mark_launching──▶ Launching ──finish──▶ Finished{outbox_enqueued}
//!    └───────────────────────────finish──────────────▲
//! ```
//!
//! `Launching` is written before any side effect (the started event, the
//! process spawn) and stands for both "launching" and "running": on restart it
//! is indeterminate. `Finished` carries the outcome, written to the ledger
//! *before* it is queued for upload, so an outcome is never in neither place.
//!
//! # What this is not
//!
//! Durable duplicate suppression, not exactly-once side effects. Recording
//! before spawning leaves a window where a crash means nothing started, and the
//! conservative policy never relaunches an uncertain start. Rolling the disk
//! back, deleting the ledger, or reinstalling the agent fall outside it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use kanade_shared::ExecResult;
use kanade_shared::bootstrap::COMMAND_STREAM_MAX_AGE;
use kanade_shared::signing::SigHeaders;
use kanade_shared::wire::{Command, EXIT_RESTARTED_OUTCOME_UNKNOWN};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

/// Slack added on top of the broker's retention before a tombstone may be
/// collected. Legacy commands carry no expiry of their own, so the only bound
/// on when the broker can still redeliver one is its retention window, and the
/// agent's clock may disagree with the broker's by some margin.
pub const ADMISSION_CLOCK_ALLOWANCE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a terminal record is kept, counted from the later of admission and
/// completion: the broker horizon plus [`ADMISSION_CLOCK_ALLOWANCE`].
pub const TOMBSTONE_RETENTION: Duration =
    Duration::from_secs(COMMAND_STREAM_MAX_AGE.as_secs() + ADMISSION_CLOCK_ALLOWANCE.as_secs());

/// Hard cap on the number of records. A full ledger refuses new admissions
/// visibly; it never evicts a record that is still live.
pub const MAX_ENTRIES: usize = 100_000;

/// Hard cap on the ledger's total size on disk (records embed the verified
/// command and, once finished, its captured output).
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// How often the maintenance task re-queues unsent outcomes and collects
/// expired tombstones.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Bumped only for an incompatible record layout.
const SCHEMA: u32 = 1;

/// A rename can fail transiently on Windows while a scanner holds the file.
const RENAME_ATTEMPTS: u32 = 5;
const RENAME_BACKOFF: Duration = Duration::from_millis(40);

#[derive(Debug)]
pub enum LedgerError {
    /// The ledger is at its entry or byte cap, even after collecting what was
    /// collectable. New admissions are refused; nothing live is evicted.
    Full { entries: usize, bytes: u64 },
    /// A read or write failed. Never an admission and never permission to
    /// launch.
    Io(String),
    /// A record exists but cannot be parsed. It is left in place untouched —
    /// deleting it would let the id be admitted again.
    Corrupt(String),
    /// A transition that the record's current state does not allow.
    State(String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { entries, bytes } => write!(
                f,
                "admission ledger is full ({entries} records, {bytes} bytes)"
            ),
            Self::Io(e) => write!(f, "admission ledger I/O failed: {e}"),
            Self::Corrupt(e) => write!(f, "admission ledger record is unreadable: {e}"),
            Self::State(e) => write!(f, "admission ledger state error: {e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

/// The signature headers as received, kept so a `Pending` record can be
/// re-verified against the keys trusted *now* when it is recovered.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct StoredHeaders {
    sig_b64: Option<String>,
    kid: Option<String>,
    alg: Option<String>,
    at_ms: Option<String>,
}

impl From<&SigHeaders> for StoredHeaders {
    fn from(h: &SigHeaders) -> Self {
        Self {
            sig_b64: h.sig_b64.clone(),
            kid: h.kid.clone(),
            alg: h.alg.clone(),
            at_ms: h.at_ms.clone(),
        }
    }
}

impl From<StoredHeaders> for SigHeaders {
    fn from(h: StoredHeaders) -> Self {
        Self {
            sig_b64: h.sig_b64,
            kid: h.kid,
            alg: h.alg,
            at_ms: h.at_ms,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
enum State {
    /// Admitted, nothing started.
    Pending,
    /// A side effect is about to start or has started; the outcome is not
    /// durable yet.
    Launching { at: DateTime<Utc> },
    /// The outcome is durable. `outbox_enqueued` is set only once the result
    /// file really landed in the outbox.
    Finished {
        result: Box<ExecResult>,
        outbox_enqueued: bool,
        finished_at: DateTime<Utc>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Record {
    schema: u32,
    pc_id: String,
    request_id: String,
    /// SHA-256 of the received payload bytes. Immutable: it is what tells a
    /// redelivery from a different message reusing the id.
    fingerprint: String,
    subject: String,
    payload_b64: String,
    headers: StoredHeaders,
    /// The command as the verifier produced it.
    command: Command,
    envelope_deadline: Option<DateTime<Utc>>,
    /// The id the eventual result is published under, fixed at admission so a
    /// re-published outcome lands on the same backend row.
    result_id: String,
    admitted_at: DateTime<Utc>,
    state: State,
}

/// What the intake hands the ledger for a verified command.
pub struct NewAdmission {
    pub command: Command,
    pub envelope_deadline: Option<DateTime<Utc>>,
    pub subject: String,
    pub payload: Vec<u8>,
    pub headers: SigHeaders,
}

/// Where an already-admitted id stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Admitted and not finished: pending, launching or running.
    InFlight,
    /// Finished. `outbox_enqueued` says whether the outcome has reached the
    /// upload queue.
    Finished {
        exit_code: i32,
        outbox_enqueued: bool,
    },
}

pub enum AdmitOutcome {
    /// Newly admitted and durable. The ticket is the only licence to launch.
    Admitted(Ticket),
    /// The same id with the same bytes was admitted before.
    Duplicate(Standing),
    /// The same id arrived with different bytes. Not executed, not a retry.
    Conflict,
}

/// A command that has been moved forward by recovery.
pub struct RecoveredPending {
    pub ticket: Ticket,
    pub command: Command,
    pub subject: String,
    pub payload: Vec<u8>,
    pub headers: SigHeaders,
}

#[derive(Default)]
pub struct Recovery {
    /// Admitted but never started: to be re-verified and run locally.
    pub pending: Vec<RecoveredPending>,
    /// `Launching` records reported as "outcome unknown".
    pub unknown_reported: usize,
    /// Finished records whose outcome was re-queued for upload.
    pub requeued: usize,
}

struct Limits {
    max_entries: usize,
    max_bytes: u64,
}

#[derive(Default)]
struct Stats {
    entries: usize,
    bytes: u64,
}

pub struct Ledger {
    dir: PathBuf,
    outbox_dir: PathBuf,
    obs_dir: PathBuf,
    pc_id: String,
    /// Serialises every read-modify-write, which is what makes concurrent
    /// arrival of one id from the live and replay paths yield one admission.
    /// Also owns the size accounting.
    inner: Mutex<Stats>,
    limits: Limits,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Whether the last write failed, so the failure is announced on the
    /// transition rather than once per delivery.
    unavailable: AtomicBool,
    /// Outcomes whose ledger write and outbox write both failed. Held in
    /// memory and retried by maintenance, because the alternative is to drop a
    /// real result and let the next start replace it with "outcome unknown".
    unsaved: Mutex<Vec<(String, ExecResult)>>,
    #[cfg(test)]
    fault: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of the received bytes, hex.
pub fn fingerprint_of(payload: &[u8]) -> String {
    hex(Sha256::digest(payload).as_slice())
}

fn io_err(what: &str, e: impl std::fmt::Display) -> LedgerError {
    LedgerError::Io(format!("{what}: {e}"))
}

impl Ledger {
    /// Open (creating if needed) the ledger rooted at `dir`.
    ///
    /// Never fails: a ledger that cannot be opened must not take the agent
    /// down with it, and every later admission attempt against it fails
    /// visibly instead — which is the fail-safe direction, since a failed
    /// admission neither launches nor acknowledges.
    pub fn open(dir: PathBuf, outbox_dir: PathBuf, obs_dir: PathBuf, pc_id: String) -> Arc<Self> {
        Self::open_with(
            dir,
            outbox_dir,
            obs_dir,
            pc_id,
            Limits {
                max_entries: MAX_ENTRIES,
                max_bytes: MAX_BYTES,
            },
            Box::new(Utc::now),
        )
    }

    fn open_with(
        dir: PathBuf,
        outbox_dir: PathBuf,
        obs_dir: PathBuf,
        pc_id: String,
        limits: Limits,
        clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Arc<Self> {
        let mut stats = Stats::default();
        match std::fs::create_dir_all(&dir) {
            Ok(()) => scan(&dir, &mut stats),
            Err(e) => error!(
                dir = %dir.display(),
                error = %e,
                "admission ledger directory cannot be created; commands will be refused until it can",
            ),
        }
        Arc::new(Self {
            dir,
            outbox_dir,
            obs_dir,
            pc_id,
            inner: Mutex::new(stats),
            limits,
            clock,
            unavailable: AtomicBool::new(false),
            unsaved: Mutex::new(Vec::new()),
            #[cfg(test)]
            fault: AtomicBool::new(false),
        })
    }

    pub fn outbox_dir(&self) -> &Path {
        &self.outbox_dir
    }

    pub fn pc_id(&self) -> &str {
        &self.pc_id
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// File name for a key: a hash, so a request id with path separators or
    /// reserved characters cannot escape the directory or collide with a
    /// device name on Windows.
    fn path_for(&self, request_id: &str) -> PathBuf {
        let mut h = Sha256::new();
        h.update(self.pc_id.as_bytes());
        h.update([0u8]);
        h.update(request_id.as_bytes());
        self.dir
            .join(format!("{}.json", hex(h.finalize().as_slice())))
    }

    fn read(&self, path: &Path) -> Result<Option<Record>, LedgerError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&format!("read {}", path.display()), e)),
        };
        serde_json::from_slice::<Record>(&bytes)
            .map(Some)
            .map_err(|e| LedgerError::Corrupt(format!("{}: {e}", path.display())))
    }

    /// Atomically replace `path` with `rec`: write a temp file, fsync it,
    /// rename over the target, fsync the directory. The record is durable when
    /// this returns `Ok`.
    fn write(&self, stats: &mut Stats, path: &Path, rec: &Record) -> Result<(), LedgerError> {
        #[cfg(test)]
        if self.fault.load(Ordering::SeqCst) {
            return Err(LedgerError::Io("injected write fault".into()));
        }
        let bytes = serde_json::to_vec(rec).map_err(|e| io_err("serialise record", e))?;
        let tmp = path.with_extension("json.tmp");
        let write_tmp = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()
        };
        write_tmp().map_err(|e| io_err(&format!("write {}", tmp.display()), e))?;
        let old_len = std::fs::metadata(path).ok().map(|m| m.len());
        rename_with_retry(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io_err(&format!("rename to {}", path.display()), e)
        })?;
        if let Err(e) = crate::outbox::sync_dir(&self.dir) {
            // Durability of the rename is unconfirmed, so this is not a commit.
            // A brand-new record is withdrawn so the id is not left reserved by
            // an admission that was never acknowledged or launched.
            if old_len.is_none() {
                let _ = std::fs::remove_file(path);
            }
            return Err(io_err(&format!("sync {}", self.dir.display()), e));
        }
        match old_len {
            Some(old) => stats.bytes = stats.bytes.saturating_sub(old) + bytes.len() as u64,
            None => {
                stats.entries += 1;
                stats.bytes += bytes.len() as u64;
            }
        }
        Ok(())
    }

    /// Admit a verified command: record the id, the payload fingerprint, the
    /// verified command and `Pending` in one atomic step.
    pub fn admit(self: &Arc<Self>, new: NewAdmission) -> Result<AdmitOutcome, LedgerError> {
        let fingerprint = fingerprint_of(&new.payload);
        let request_id = new.command.request_id.clone();
        let path = self.path_for(&request_id);
        let mut stats = lock(&self.inner);

        if let Some(existing) = self.read(&path)? {
            if existing.fingerprint != fingerprint {
                return Ok(AdmitOutcome::Conflict);
            }
            return Ok(AdmitOutcome::Duplicate(standing_of(&existing.state)));
        }

        if stats.entries >= self.limits.max_entries || stats.bytes >= self.limits.max_bytes {
            // Only terminal, already-uploaded records are collectable, and
            // only past the retention horizon; anything else stays.
            self.gc_locked(&mut stats, self.now());
            if stats.entries >= self.limits.max_entries || stats.bytes >= self.limits.max_bytes {
                return Err(LedgerError::Full {
                    entries: stats.entries,
                    bytes: stats.bytes,
                });
            }
        }

        let rec = Record {
            schema: SCHEMA,
            pc_id: self.pc_id.clone(),
            request_id: request_id.clone(),
            fingerprint,
            subject: new.subject,
            payload_b64: base64::engine::general_purpose::STANDARD.encode(&new.payload),
            headers: StoredHeaders::from(&new.headers),
            command: new.command,
            envelope_deadline: new.envelope_deadline,
            result_id: uuid::Uuid::new_v4().to_string(),
            admitted_at: self.now(),
            state: State::Pending,
        };
        self.write(&mut stats, &path, &rec)?;
        Ok(AdmitOutcome::Admitted(Ticket {
            ledger: self.clone(),
            request_id,
            result_id: rec.result_id,
        }))
    }

    /// Move a record forward, holding the lock across read and write.
    fn transition(
        &self,
        request_id: &str,
        f: impl FnOnce(&mut Record) -> Result<(), LedgerError>,
    ) -> Result<Record, LedgerError> {
        let path = self.path_for(request_id);
        let mut stats = lock(&self.inner);
        let mut rec = self
            .read(&path)?
            .ok_or_else(|| LedgerError::State(format!("no record for {request_id}")))?;
        f(&mut rec)?;
        self.write(&mut stats, &path, &rec)?;
        Ok(rec)
    }

    /// Re-queue the stored outcome of an already-admitted id whose result never
    /// reached the upload queue. Used for a duplicate delivery; a no-op for any
    /// other state.
    pub fn republish(&self, request_id: &str) -> Result<bool, LedgerError> {
        let path = self.path_for(request_id);
        let mut stats = lock(&self.inner);
        let Some(mut rec) = self.read(&path)? else {
            return Ok(false);
        };
        let State::Finished {
            result,
            outbox_enqueued: false,
            ..
        } = &rec.state
        else {
            return Ok(false);
        };
        crate::outbox::enqueue(&self.outbox_dir, result)
            .map_err(|e| io_err("enqueue outcome to outbox", format!("{e:#}")))?;
        if let State::Finished {
            outbox_enqueued, ..
        } = &mut rec.state
        {
            *outbox_enqueued = true;
        }
        self.write(&mut stats, &path, &rec)?;
        Ok(true)
    }

    /// Record the agent's restart-time view and hand back what must still run.
    ///
    /// * `Finished` without an enqueued outcome: re-queued.
    /// * `Launching`: indeterminate — the process may still be running or may
    ///   have finished — so it is **not** relaunched. It is reported once as a
    ///   failure with [`EXIT_RESTARTED_OUTCOME_UNKNOWN`].
    /// * `Pending`: returned for local execution; the caller re-evaluates the
    ///   start gates, since a decision made before the restart may be stale.
    pub fn recover(self: &Arc<Self>) -> Recovery {
        let mut out = Recovery::default();
        let mut pending: Vec<(DateTime<Utc>, RecoveredPending)> = Vec::new();
        for path in self.record_paths() {
            let mut stats = lock(&self.inner);
            let rec = match self.read(&path) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => {
                    error!(error = %e, "admission ledger: skipping unreadable record during recovery");
                    continue;
                }
            };
            match &rec.state {
                State::Finished {
                    outbox_enqueued: false,
                    ..
                } => match self.requeue_locked(&mut stats, &path, rec) {
                    Ok(()) => out.requeued += 1,
                    Err(e) => {
                        warn!(error = %e, "admission ledger: could not re-queue an outcome; will retry")
                    }
                },
                State::Finished { .. } => {}
                State::Launching { at } => {
                    let mut rec = rec.clone();
                    let now = self.now();
                    // The run may have finished and queued its real outcome
                    // directly (the ledger write failed, the outbox write did
                    // not). That file is the outcome, not "unknown": adopt it
                    // rather than overwrite it with a placeholder.
                    match self.queued_outcome(&rec.request_id) {
                        Ok(Some(result)) => {
                            let finished_at = result.finished_at;
                            rec.state = State::Finished {
                                result: Box::new(result),
                                outbox_enqueued: true,
                                finished_at,
                            };
                            match self.write(&mut stats, &path, &rec) {
                                Ok(()) => out.requeued += 1,
                                Err(e) => {
                                    warn!(error = %e, "admission ledger: could not adopt a queued outcome; will retry next start")
                                }
                            }
                            continue;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(request_id = %rec.request_id, error = %e, "admission ledger: an outbox result exists but cannot be read; leaving the record for the next start");
                            continue;
                        }
                    }
                    let result = self.unknown_result(&rec, *at, now);
                    rec.state = State::Finished {
                        result: Box::new(result),
                        outbox_enqueued: false,
                        finished_at: now,
                    };
                    warn!(
                        request_id = %rec.request_id,
                        "agent restarted while this command was launching or running; outcome unknown, not relaunching",
                    );
                    if let Err(e) = self.write(&mut stats, &path, &rec) {
                        warn!(error = %e, "admission ledger: could not record the unknown outcome; will retry next start");
                        continue;
                    }
                    out.unknown_reported += 1;
                    if let Err(e) = self.requeue_locked(&mut stats, &path, rec) {
                        warn!(error = %e, "admission ledger: could not queue the unknown-outcome report; will retry");
                    }
                }
                State::Pending => {
                    let payload = base64::engine::general_purpose::STANDARD
                        .decode(&rec.payload_b64)
                        .unwrap_or_default();
                    pending.push((
                        rec.admitted_at,
                        RecoveredPending {
                            ticket: Ticket {
                                ledger: self.clone(),
                                request_id: rec.request_id.clone(),
                                result_id: rec.result_id.clone(),
                            },
                            command: rec.command.clone(),
                            subject: rec.subject.clone(),
                            payload,
                            headers: rec.headers.clone().into(),
                        },
                    ));
                }
            }
        }
        pending.sort_by_key(|(at, _)| *at);
        out.pending = pending.into_iter().map(|(_, p)| p).collect();
        self.gc(self.now());
        out
    }

    /// Queue every `Finished` outcome that has not reached the outbox. Run
    /// periodically so a transient outbox failure does not wait for a restart.
    pub fn requeue_unsent(&self) -> usize {
        let mut n = self.retry_unsaved();
        for path in self.record_paths() {
            let mut stats = lock(&self.inner);
            if let Ok(Some(rec)) = self.read(&path)
                && matches!(
                    rec.state,
                    State::Finished {
                        outbox_enqueued: false,
                        ..
                    }
                )
                && self.requeue_locked(&mut stats, &path, rec).is_ok()
            {
                n += 1;
            }
        }
        n
    }

    /// Retry outcomes that could not be recorded when their run finished.
    fn retry_unsaved(&self) -> usize {
        let held = std::mem::take(&mut *lock(&self.unsaved));
        let mut n = 0;
        for (request_id, result) in held {
            match self.persist_finished(&request_id, &result) {
                Ok(()) | Err(LedgerError::State(_)) => {
                    if self.republish(&request_id).is_ok() {
                        n += 1;
                    }
                }
                Err(_) => lock(&self.unsaved).push((request_id, result)),
            }
        }
        n
    }

    fn persist_finished(&self, request_id: &str, result: &ExecResult) -> Result<(), LedgerError> {
        let finished_at = self.now();
        self.transition(request_id, |rec| {
            if matches!(rec.state, State::Finished { .. }) {
                return Err(LedgerError::State(format!(
                    "{} already has an outcome",
                    rec.request_id
                )));
            }
            rec.state = State::Finished {
                result: Box::new(result.clone()),
                outbox_enqueued: false,
                finished_at,
            };
            Ok(())
        })
        .map(|_| ())
    }

    fn requeue_locked(
        &self,
        stats: &mut Stats,
        path: &Path,
        mut rec: Record,
    ) -> Result<(), LedgerError> {
        let State::Finished {
            result,
            outbox_enqueued,
            ..
        } = &mut rec.state
        else {
            return Ok(());
        };
        crate::outbox::enqueue(&self.outbox_dir, result)
            .map_err(|e| io_err("enqueue outcome to outbox", format!("{e:#}")))?;
        *outbox_enqueued = true;
        self.write(stats, path, &rec)
    }

    fn unknown_result(
        &self,
        rec: &Record,
        launched_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> ExecResult {
        ExecResult {
            // The id fixed at admission, which the started event also used, so
            // the report closes the row the start opened instead of adding one.
            result_id: rec.result_id.clone(),
            request_id: rec.request_id.clone(),
            exec_id: rec.command.exec_id.clone(),
            parent_result_id: None,
            pc_id: self.pc_id.clone(),
            exit_code: EXIT_RESTARTED_OUTCOME_UNKNOWN,
            skipped: Some(false),
            stdout: String::new(),
            stderr: "agent restarted during execution; outcome unknown (the command is not relaunched automatically)".into(),
            started_at: launched_at,
            finished_at: now,
            stdout_object: None,
            stderr_object: None,
            manifest_id: Some(rec.command.id.clone()),
            collect_object: None,
        }
    }

    fn record_paths(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect(),
            Err(e) => {
                warn!(error = %e, dir = %self.dir.display(), "admission ledger: read_dir failed");
                Vec::new()
            }
        }
    }

    /// Whether the result outbox, or its quarantine of results that could not
    /// be published, still holds a result for `request_id`.
    fn outbox_holds(&self, request_id: &str) -> bool {
        let name = format!("{request_id}.json");
        // Anything other than a definite "not there" counts as held: a
        // directory that cannot be inspected must never be read as empty.
        match self.outbox_dir.join(&name).try_exists() {
            Ok(false) => {}
            _ => return true,
        }
        match std::fs::read_dir(self.outbox_dir.join(crate::outbox_retry::STUCK_DIR)) {
            Ok(rd) => rd.into_iter().any(|e| match e {
                Ok(e) => e.file_name().to_string_lossy().starts_with(&name),
                Err(_) => true,
            }),
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }

    /// The result already sitting in the outbox for `request_id`, if any.
    fn queued_outcome(&self, request_id: &str) -> Result<Option<ExecResult>, LedgerError> {
        let path = self.outbox_dir.join(format!("{request_id}.json"));
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<ExecResult>(&b)
                .map(Some)
                .map_err(|e| LedgerError::Corrupt(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&format!("read {}", path.display()), e)),
        }
    }

    /// Collect tombstones past their retention. Returns how many were removed.
    pub fn gc(&self, now: DateTime<Utc>) -> usize {
        let mut stats = lock(&self.inner);
        self.gc_locked(&mut stats, now)
    }

    /// Only a record that is `Finished`, whose outcome is queued, and whose
    /// horizon — the later of admission and completion — plus
    /// [`TOMBSTONE_RETENTION`] has passed is removed. A record whose horizon
    /// lies in the future (the clock moved backwards) is kept, as is anything
    /// unreadable: removing either could make a redeliverable command
    /// executable again.
    fn gc_locked(&self, stats: &mut Stats, now: DateTime<Utc>) -> usize {
        let retention = chrono::Duration::from_std(TOMBSTONE_RETENTION)
            .unwrap_or_else(|_| chrono::Duration::days(8));
        let mut removed = 0;
        for path in self.record_paths() {
            let Ok(Some(rec)) = self.read(&path) else {
                continue;
            };
            let State::Finished {
                outbox_enqueued: true,
                finished_at,
                ..
            } = &rec.state
            else {
                continue;
            };
            let horizon = rec.admitted_at.max(*finished_at);
            if horizon > now || now - horizon <= retention {
                continue;
            }
            // `outbox_enqueued` only says the file was queued, not that it was
            // uploaded. While the outbox (or its quarantine) still holds the
            // result, this record is its only other copy.
            if self.outbox_holds(&rec.request_id) {
                continue;
            }
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                stats.entries = stats.entries.saturating_sub(1);
                stats.bytes = stats.bytes.saturating_sub(len);
                removed += 1;
            }
        }
        if removed > 0 {
            debug!(removed, "admission ledger: collected expired tombstones");
        }
        removed
    }

    /// Note whether the ledger is currently writable. Returns `true` when the
    /// answer changed, which is when it is worth an observability event.
    fn note_health(&self, failing: bool) -> bool {
        self.unavailable.swap(failing, Ordering::SeqCst) != failing
    }

    /// Make a ledger failure visible: always in the log, and on the obs outbox
    /// when the ledger *becomes* unavailable, so a stuck disk does not turn
    /// into one event per delivery.
    pub fn report_failure(&self, request_id: &str, e: &LedgerError) {
        error!(
            request_id,
            error = %e,
            "ADMISSION LEDGER FAILURE: the command is neither admitted nor launched, and its delivery is not acknowledged",
        );
        if self.note_health(true) {
            self.emit_obs(
                "command_admission_unavailable",
                format!("unavailable:{}", self.now().timestamp_millis()),
                serde_json::json!({ "request_id": request_id, "error": e.to_string() }),
            );
        }
    }

    /// The ledger accepted a write again after having failed.
    pub fn report_healthy(&self) {
        if self.note_health(false) {
            info!("admission ledger is writable again");
        }
    }

    /// Queue a diagnostic on the obs outbox. Best-effort: a failure here must
    /// not turn a refusal into a crash.
    pub fn emit_obs(&self, kind: &str, event_record_id: String, payload: serde_json::Value) {
        let event = kanade_shared::wire::ObsEvent {
            pc_id: self.pc_id.clone(),
            at: self.now(),
            kind: kind.to_string(),
            source: "agent:admission".to_string(),
            event_record_id: Some(event_record_id),
            payload,
        };
        let queued = crate::obs_outbox::ensure_outbox_dir(&self.obs_dir)
            .and_then(|()| crate::obs_outbox::enqueue(&self.obs_dir, &event).map(|_| ()));
        if let Err(e) = queued {
            warn!(error = %e, kind, "admission ledger: could not queue a diagnostic event");
        }
    }

    #[cfg(test)]
    pub(crate) fn set_fault(&self, on: bool) {
        self.fault.store(on, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn record_count_on_disk(&self) -> usize {
        self.record_paths().len()
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, u64) {
        let s = lock(&self.inner);
        (s.entries, s.bytes)
    }

    #[cfg(test)]
    fn state_of(&self, request_id: &str) -> Option<State> {
        self.read(&self.path_for(request_id))
            .ok()
            .flatten()
            .map(|r| r.state)
    }
}

fn standing_of(state: &State) -> Standing {
    match state {
        State::Pending | State::Launching { .. } => Standing::InFlight,
        State::Finished {
            result,
            outbox_enqueued,
            ..
        } => Standing::Finished {
            exit_code: result.exit_code,
            outbox_enqueued: *outbox_enqueued,
        },
    }
}

/// Count what is already on disk and clear leftovers of interrupted writes. A
/// `.tmp` file is never a record — the rename is the commit point — so
/// removing one cannot lose an admission.
fn scan(dir: &Path, stats: &mut Stats) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => {
                stats.entries += 1;
                stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
            Some("tmp") => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if attempt + 1 < RENAME_ATTEMPTS {
                    std::thread::sleep(RENAME_BACKOFF);
                }
            }
        }
    }
    Err(last.expect("loop ran at least once"))
}

/// The licence to launch one admitted command, and the only way to move its
/// record forward. Held by whoever runs the command.
#[derive(Clone)]
pub struct Ticket {
    ledger: Arc<Ledger>,
    request_id: String,
    result_id: String,
}

impl Ticket {
    /// The id the outcome is published under.
    pub fn result_id(&self) -> &str {
        &self.result_id
    }

    #[cfg(test)]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Record that a side effect is about to start. Must succeed before
    /// anything is launched: if it fails the command does not run.
    pub fn mark_launching(&self) -> Result<(), LedgerError> {
        let at = self.ledger.now();
        self.ledger
            .transition(&self.request_id, |rec| match rec.state {
                State::Pending => {
                    rec.state = State::Launching { at };
                    Ok(())
                }
                _ => Err(LedgerError::State(format!(
                    "{} is not pending; refusing to launch it again",
                    rec.request_id
                ))),
            })
            .map(|_| ())
            .inspect_err(|e| self.ledger.report_failure(&self.request_id, e))
    }

    /// Record the outcome and queue it for upload: ledger first, then outbox,
    /// then the ledger again to say the outbox has it. A crash between any two
    /// of these leaves the outcome in the ledger, where recovery finds it.
    ///
    /// The result is published under the ticket's fixed `result_id`.
    pub fn finish(&self, result: ExecResult) -> Result<(), LedgerError> {
        self.finish_inner(result, true)
    }

    /// As [`Self::finish`], keeping the result's own `result_id` — for the
    /// outcomes whose id is derived from the command so a repeat collapses.
    pub fn finish_keep_id(&self, result: ExecResult) -> Result<(), LedgerError> {
        self.finish_inner(result, false)
    }

    fn finish_inner(&self, mut result: ExecResult, own_id: bool) -> Result<(), LedgerError> {
        if own_id {
            result.result_id = self.result_id.clone();
        }
        if let Err(e) = self.persist_outcome(&result) {
            // The ledger could not take the outcome. Queue it anyway: a
            // result nobody ever sees is worse than one reported twice.
            error!(
                request_id = %self.request_id,
                error = %e,
                "admission ledger: could not record the outcome; queueing it directly",
            );
            let queued = crate::outbox::enqueue(&self.ledger.outbox_dir, &result);
            // Whether or not the direct queue worked, the ledger still says
            // `launching`; keep the real outcome so maintenance can record it
            // once the disk recovers, instead of a restart replacing it with
            // "outcome unknown".
            if !matches!(e, LedgerError::State(_)) {
                lock(&self.ledger.unsaved).push((self.request_id.clone(), result));
            }
            if let Err(qe) = queued {
                error!(request_id = %self.request_id, error = %qe, "outcome could not be queued directly either; holding it in memory for retry");
            }
            self.ledger.report_failure(&self.request_id, &e);
            return Err(e);
        }
        self.queue_outcome()
    }

    /// Step one of finishing: the outcome becomes durable in the ledger.
    fn persist_outcome(&self, result: &ExecResult) -> Result<(), LedgerError> {
        self.ledger.persist_finished(&self.request_id, result)
    }

    /// Step two: queue the recorded outcome and mark it queued.
    fn queue_outcome(&self) -> Result<(), LedgerError> {
        self.ledger.republish(&self.request_id).map(|_| ())
    }
}

/// Re-queue unsent outcomes and collect expired tombstones on a timer, so
/// neither waits for the next restart.
pub fn spawn_maintenance(ledger: Arc<Ledger>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MAINTENANCE_INTERVAL).await;
            let l = ledger.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let n = l.requeue_unsent();
                if n > 0 {
                    info!(n, "admission ledger: re-queued unsent outcomes");
                }
                l.gc(Utc::now());
            })
            .await;
        }
    })
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub(crate) struct Fixture {
        pub root: tempfile::TempDir,
    }

    impl Fixture {
        pub fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }
        pub fn ledger_dir(&self) -> PathBuf {
            self.root.path().join("ledger")
        }
        pub fn outbox_dir(&self) -> PathBuf {
            self.root.path().join("outbox")
        }
        pub fn obs_dir(&self) -> PathBuf {
            self.root.path().join("obs")
        }
        pub fn open(&self) -> Arc<Ledger> {
            Ledger::open(
                self.ledger_dir(),
                self.outbox_dir(),
                self.obs_dir(),
                "PC1".into(),
            )
        }
        pub fn open_at(&self, now: DateTime<Utc>) -> Arc<Ledger> {
            Ledger::open_with(
                self.ledger_dir(),
                self.outbox_dir(),
                self.obs_dir(),
                "PC1".into(),
                Limits {
                    max_entries: MAX_ENTRIES,
                    max_bytes: MAX_BYTES,
                },
                Box::new(move || now),
            )
        }
        pub fn outbox_files(&self) -> Vec<ExecResult> {
            let Ok(rd) = std::fs::read_dir(self.outbox_dir()) else {
                return Vec::new();
            };
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
                .collect()
        }
    }

    pub(crate) fn command(request_id: &str) -> Command {
        serde_json::from_value(serde_json::json!({
            "id": "job",
            "version": "1",
            "request_id": request_id,
            "shell": "powershell",
            "script": "Write-Output hi",
            "timeout_secs": 60,
        }))
        .expect("a minimal command deserialises")
    }

    pub(crate) fn admission(request_id: &str, payload: &[u8]) -> NewAdmission {
        NewAdmission {
            command: command(request_id),
            envelope_deadline: None,
            subject: "commands.all".into(),
            payload: payload.to_vec(),
            headers: SigHeaders::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use chrono::TimeZone;

    fn result_for(request_id: &str, exit_code: i32) -> ExecResult {
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        ExecResult {
            result_id: String::new(),
            request_id: request_id.into(),
            exec_id: None,
            parent_result_id: None,
            pc_id: "PC1".into(),
            exit_code,
            skipped: Some(false),
            stdout: "out".into(),
            stderr: String::new(),
            started_at: t,
            finished_at: t,
            stdout_object: None,
            stderr_object: None,
            manifest_id: Some("job".into()),
            collect_object: None,
        }
    }

    fn admitted(l: &Arc<Ledger>, id: &str) -> Ticket {
        match l.admit(admission(id, id.as_bytes())).unwrap() {
            AdmitOutcome::Admitted(t) => t,
            _ => panic!("expected a fresh admission"),
        }
    }

    #[test]
    fn concurrent_arrival_of_one_id_yields_one_admission() {
        let fx = Fixture::new();
        let l = fx.open();
        let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (l, wins, barrier) = (l.clone(), wins.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    if matches!(
                        l.admit(admission("req-1", b"same")).unwrap(),
                        AdmitOutcome::Admitted(_)
                    ) {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(wins.load(Ordering::SeqCst), 1);
        assert_eq!(l.stats().0, 1);
    }

    #[test]
    fn the_ledger_survives_a_restart_and_a_redelivery_is_a_duplicate() {
        let fx = Fixture::new();
        {
            let l = fx.open();
            let t = admitted(&l, "req-1");
            t.mark_launching().unwrap();
            t.finish(result_for("req-1", 0)).unwrap();
        }
        let l = fx.open();
        assert_eq!(l.stats().0, 1, "size accounting is rebuilt from disk");
        match l.admit(admission("req-1", b"req-1")).unwrap() {
            AdmitOutcome::Duplicate(Standing::Finished {
                exit_code: 0,
                outbox_enqueued: true,
            }) => {}
            _ => panic!("a redelivered id must be a duplicate of the finished run"),
        }
    }

    #[test]
    fn the_same_id_with_different_bytes_is_a_conflict_and_leaves_the_record_alone() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        assert!(matches!(
            l.admit(admission("req-1", b"other bytes")).unwrap(),
            AdmitOutcome::Conflict
        ));
        // The original is still the admitted one and can still proceed.
        t.mark_launching().unwrap();
        assert!(matches!(l.state_of("req-1"), Some(State::Launching { .. })));
    }

    #[test]
    fn a_failed_write_is_not_an_admission() {
        let fx = Fixture::new();
        let l = fx.open();
        l.set_fault(true);
        assert!(l.admit(admission("req-1", b"x")).is_err());
        l.set_fault(false);
        // Nothing was recorded, so the id is still admittable.
        assert!(matches!(
            l.admit(admission("req-1", b"x")).unwrap(),
            AdmitOutcome::Admitted(_)
        ));
    }

    #[test]
    fn a_launching_ticket_cannot_launch_twice() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        t.mark_launching().unwrap();
        assert!(t.mark_launching().is_err());
    }

    #[test]
    fn crash_after_admission_recovers_the_command_as_pending() {
        let fx = Fixture::new();
        {
            let l = fx.open();
            admitted(&l, "req-1");
        }
        let l = fx.open();
        let r = l.recover();
        assert_eq!(r.pending.len(), 1);
        assert_eq!(r.pending[0].ticket.request_id(), "req-1");
        assert_eq!(r.pending[0].payload, b"req-1");
        // A redelivery in the meantime is a duplicate, not a second admission.
        assert!(matches!(
            l.admit(admission("req-1", b"req-1")).unwrap(),
            AdmitOutcome::Duplicate(Standing::InFlight)
        ));
    }

    #[test]
    fn crash_after_launching_reports_an_unknown_outcome_once_and_never_relaunches() {
        let fx = Fixture::new();
        let rid: String;
        {
            let l = fx.open();
            let t = admitted(&l, "req-1");
            rid = t.result_id().to_string();
            t.mark_launching().unwrap();
        }
        let l = fx.open();
        let r = l.recover();
        assert!(
            r.pending.is_empty(),
            "an uncertain start is never relaunched"
        );
        assert_eq!(r.unknown_reported, 1);
        let out = fx.outbox_files();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].exit_code, EXIT_RESTARTED_OUTCOME_UNKNOWN);
        assert_eq!(out[0].skipped, Some(false), "a failure, not a skip");
        assert!(out[0].stderr.contains("outcome unknown"));
        assert_eq!(
            out[0].result_id, rid,
            "closes the row the start event opened"
        );

        // Restarting again reports nothing further.
        let r = fx.open().recover();
        assert_eq!(r.unknown_reported, 0);
        assert!(r.pending.is_empty());
        assert!(matches!(
            fx.open().admit(admission("req-1", b"req-1")).unwrap(),
            AdmitOutcome::Duplicate(Standing::Finished { .. })
        ));
    }

    #[test]
    fn crash_after_the_outcome_is_persisted_requeues_it_without_relaunching() {
        let fx = Fixture::new();
        {
            let l = fx.open();
            let t = admitted(&l, "req-1");
            t.mark_launching().unwrap();
            // Ledger step of `finish` only: the process died before queueing.
            t.persist_outcome(&result_for("req-1", 3)).unwrap();
            assert!(fx.outbox_files().is_empty());
        }
        let l = fx.open();
        let r = l.recover();
        assert!(r.pending.is_empty());
        assert_eq!(r.requeued, 1);
        let out = fx.outbox_files();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].exit_code, 3,
            "the real outcome is reported, not 'unknown'"
        );
    }

    #[test]
    fn crash_before_upload_leaves_the_outcome_queued_and_the_id_closed() {
        let fx = Fixture::new();
        {
            let l = fx.open();
            let t = admitted(&l, "req-1");
            t.mark_launching().unwrap();
            t.finish(result_for("req-1", 0)).unwrap();
            assert_eq!(fx.outbox_files().len(), 1);
        }
        // The upload never happened; the file is still queued. A restart must
        // neither relaunch nor queue a second copy under another id.
        let l = fx.open();
        let r = l.recover();
        assert!(r.pending.is_empty());
        assert_eq!(r.requeued, 0);
        assert_eq!(fx.outbox_files().len(), 1);
        assert!(matches!(
            l.admit(admission("req-1", b"req-1")).unwrap(),
            AdmitOutcome::Duplicate(Standing::Finished {
                outbox_enqueued: true,
                ..
            })
        ));
    }

    #[test]
    fn a_duplicate_of_an_unqueued_outcome_republishes_it() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        t.mark_launching().unwrap();
        t.persist_outcome(&result_for("req-1", 0)).unwrap();
        assert!(matches!(
            l.admit(admission("req-1", b"req-1")).unwrap(),
            AdmitOutcome::Duplicate(Standing::Finished {
                outbox_enqueued: false,
                ..
            })
        ));
        assert!(l.republish("req-1").unwrap());
        assert_eq!(fx.outbox_files().len(), 1);
        assert!(!l.republish("req-1").unwrap(), "already queued");
    }

    #[test]
    fn the_result_is_published_under_the_id_fixed_at_admission() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        let id = t.result_id().to_string();
        t.mark_launching().unwrap();
        t.finish(result_for("req-1", 0)).unwrap();
        assert_eq!(fx.outbox_files()[0].result_id, id);
    }

    #[test]
    fn tombstones_survive_gc_inside_the_retention_window_and_go_after_it() {
        let fx = Fixture::new();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let l = fx.open_at(t0);
        let t = admitted(&l, "done");
        t.mark_launching().unwrap();
        t.finish(result_for("done", 0)).unwrap();

        let retention = chrono::Duration::from_std(TOMBSTONE_RETENTION).unwrap();
        // The seven-day broker horizon alone is not enough: the allowance counts.
        let broker = chrono::Duration::from_std(COMMAND_STREAM_MAX_AGE).unwrap();
        assert_eq!(l.gc(t0 + broker + chrono::Duration::minutes(1)), 0);
        assert_eq!(l.gc(t0 + retention), 0, "the boundary itself is still kept");
        assert!(matches!(
            l.admit(admission("done", b"done")).unwrap(),
            AdmitOutcome::Duplicate(_)
        ));
        // Clock moved backwards: never collect.
        assert_eq!(l.gc(t0 - chrono::Duration::days(30)), 0);
        // The drain uploaded and removed the queued result.
        std::fs::remove_file(fx.outbox_dir().join("done.json")).unwrap();
        assert_eq!(l.gc(t0 + retention + chrono::Duration::seconds(1)), 1);
        assert_eq!(l.stats().0, 0);
    }

    #[test]
    fn gc_never_touches_unresolved_or_unqueued_records() {
        let fx = Fixture::new();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let l = fx.open_at(t0);
        admitted(&l, "pending");
        admitted(&l, "launching").mark_launching().unwrap();
        let t = admitted(&l, "unsent");
        t.mark_launching().unwrap();
        t.persist_outcome(&result_for("unsent", 0)).unwrap();
        // A corrupt record is also kept.
        std::fs::write(fx.ledger_dir().join("garbage.json"), b"{not json").unwrap();

        assert_eq!(l.gc(t0 + chrono::Duration::days(3650)), 0);
        assert!(l.state_of("pending").is_some());
        assert!(l.state_of("launching").is_some());
        assert!(l.state_of("unsent").is_some());
    }

    #[test]
    fn a_full_ledger_refuses_new_admissions_instead_of_evicting() {
        let fx = Fixture::new();
        let l = Ledger::open_with(
            fx.ledger_dir(),
            fx.outbox_dir(),
            fx.obs_dir(),
            "PC1".into(),
            Limits {
                max_entries: 2,
                max_bytes: MAX_BYTES,
            },
            Box::new(Utc::now),
        );
        admitted(&l, "a");
        admitted(&l, "b");
        assert!(matches!(
            l.admit(admission("c", b"c")),
            Err(LedgerError::Full { .. })
        ));
        // Both live records are intact, and an existing id is still answered.
        assert!(l.state_of("a").is_some() && l.state_of("b").is_some());
        assert!(matches!(
            l.admit(admission("a", b"a")).unwrap(),
            AdmitOutcome::Duplicate(_)
        ));
        // Finishing an existing record is not blocked by the cap.
        let t = Ticket {
            ledger: l.clone(),
            request_id: "a".into(),
            result_id: "r".into(),
        };
        t.mark_launching().unwrap();
        t.finish(result_for("a", 0)).unwrap();
    }

    #[test]
    fn a_corrupt_record_blocks_its_id_rather_than_being_overwritten() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        let path = l.path_for("req-1");
        drop(t);
        std::fs::write(&path, b"{truncated").unwrap();
        assert!(matches!(
            l.admit(admission("req-1", b"req-1")),
            Err(LedgerError::Corrupt(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"{truncated");
    }

    #[test]
    fn leftover_temp_files_are_cleared_on_open() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.ledger_dir()).unwrap();
        let tmp = fx.ledger_dir().join("abc.json.tmp");
        std::fs::write(&tmp, b"half").unwrap();
        let l = fx.open();
        assert!(!tmp.exists());
        assert_eq!(l.stats().0, 0);
    }

    #[test]
    fn request_ids_with_path_characters_stay_inside_the_directory() {
        let fx = Fixture::new();
        let l = fx.open();
        admitted(&l, "../../escape");
        assert!(l.path_for("../../escape").starts_with(fx.ledger_dir()));
        assert_eq!(l.stats().0, 1);
    }

    #[test]
    fn an_outcome_that_could_not_be_recorded_is_retained_and_retried() {
        let fx = Fixture::new();
        let l = fx.open();
        let t = admitted(&l, "req-1");
        t.mark_launching().unwrap();
        l.set_fault(true);
        // Ledger write fails; make the outbox fail too.
        std::fs::write(fx.outbox_dir(), b"not a directory").unwrap();
        assert!(t.finish(result_for("req-1", 7)).is_err());
        l.set_fault(false);
        std::fs::remove_file(fx.outbox_dir()).unwrap();
        assert_eq!(l.requeue_unsent(), 1);
        let out = fx.outbox_files();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].exit_code, 7);
        assert!(matches!(
            l.state_of("req-1"),
            Some(State::Finished {
                outbox_enqueued: true,
                ..
            })
        ));
    }

    #[test]
    fn gc_keeps_a_record_while_the_outbox_still_holds_its_result() {
        let fx = Fixture::new();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let l = fx.open_at(t0);
        let t = admitted(&l, "req-1");
        t.mark_launching().unwrap();
        t.finish(result_for("req-1", 0)).unwrap();
        let late = t0 + chrono::Duration::days(3650);
        assert_eq!(l.gc(late), 0, "result still queued in the outbox");
        std::fs::remove_file(fx.outbox_dir().join("req-1.json")).unwrap();
        assert_eq!(l.gc(late), 1, "uploaded and past retention");
    }

    #[test]
    fn recovery_adopts_an_outcome_already_queued_instead_of_overwriting_it() {
        let fx = Fixture::new();
        {
            let l = fx.open();
            let t = admitted(&l, "req-1");
            t.mark_launching().unwrap();
            // The ledger could not take the outcome but the outbox did.
            l.set_fault(true);
            assert!(t.finish(result_for("req-1", 5)).is_err());
        }
        let l = fx.open();
        let r = l.recover();
        assert_eq!(r.unknown_reported, 0);
        let out = fx.outbox_files();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].exit_code, 5, "the real outcome survives the restart");
        assert!(matches!(
            l.state_of("req-1"),
            Some(State::Finished {
                outbox_enqueued: true,
                ..
            })
        ));
    }
}
