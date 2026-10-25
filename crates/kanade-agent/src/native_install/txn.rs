//! The install transaction: lock, evaluate, stage, back up, atomically swap,
//! verify, commit — with every intent persisted in a journal first and a
//! deterministic recovery for each phase a crash can leave behind.
//!
//! Layout (all private, owned by the installer principal):
//!
//! ```text
//! <install_dir>/kanade                      the CLI (0755)
//! <install_dir>/.kanade-deploy-staging/     0700, same filesystem as the CLI
//! <deploy_dir>/cli.receipt.json             receipt (0600)
//! <deploy_dir>/cli.journal.json             transaction journal (0600)
//! <deploy_dir>/cli.install.lock             cross-process lock (0600)
//! <deploy_dir>/backup/{kanade.prev,cli.receipt.prev.json}
//! ```
//!
//! A binary and a receipt in different directories cannot be replaced by one
//! atomic operation. The guarantee here is therefore: the binary replacement
//! is atomic and never leaves the install path empty; a verified copy of the
//! previous binary and receipt is on stable storage before it happens; and a
//! journal written before each step lets a restart tell exactly which of the
//! possible states it is looking at.

use std::path::PathBuf;
use std::time::Duration;

use kanade_shared::artifact::ArtifactSha256;
use kanade_shared::publisher_trust::{PublisherTrustStore, SystemClock, WallClock};

use super::admit::{InstallInputs, VerifiedPackage, verify_inputs};
use super::archive::MAX_STREAM_BYTES;
use super::fs::{Dir, FileLock, Kind, Owner, RenameMode, Stat};
use super::macho;
use super::model::*;
use super::probe::{VersionProbe, expected_line};

pub const LOCK_NAME: &str = "cli.install.lock";
pub const RECEIPT_NAME: &str = "cli.receipt.json";
pub const JOURNAL_NAME: &str = "cli.journal.json";
pub const STAGING_DIR: &str = ".kanade-deploy-staging";
pub const BACKUP_DIR: &str = "backup";
const RECEIPT_TMP: &str = "cli.receipt.json.tmp";
const JOURNAL_TMP: &str = "cli.journal.json.tmp";
const STAGED: &str = "kanade.new";
const RESTORE: &str = "kanade.restore";
const DISCARD: &str = "kanade.discard";
const BACKUP_BIN: &str = "kanade.prev";
const BACKUP_RECEIPT: &str = "cli.receipt.prev.json";
/// Default bounded wait for another installer.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(30);

/// A point where a test harness may inject a failure or a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Immediately before the journal is written for this phase.
    BeforePhase(Phase),
    /// Immediately after the journal for this phase is durable.
    AfterPhase(Phase),
    StageWrite,
    BackupWrite,
    Swap,
    SwapBack,
    ReceiptWrite,
    RestoreWrite,
    RestoreSwap,
    RestoreReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Pass,
    /// Fail the step with this OS error (e.g. `ENOSPC`).
    Errno(i32),
    /// Stop dead, as if the process had been killed: no cleanup, no rollback.
    Crash,
}

/// Fault-injection seam; production uses [`NoFaults`].
pub trait Faults: Send + Sync {
    fn at(&self, point: Point) -> Fault;
}

pub struct NoFaults;
impl Faults for NoFaults {
    fn at(&self, _: Point) -> Fault {
        Fault::Pass
    }
}

type R<T> = Result<T, InstallError>;

fn io_err(step: &str, e: std::io::Error) -> InstallError {
    InstallError::io(step, e)
}

fn digest(b: &[u8]) -> ArtifactSha256 {
    ArtifactSha256::of(b)
}

pub struct Installer {
    roots: Roots,
    probe: Box<dyn VersionProbe>,
    faults: Box<dyn Faults>,
    lock_wait: Duration,
    clock: Box<dyn WallClock>,
}

/// Open directories for one run.
struct Ctx {
    install: Dir,
    deploy: Dir,
    staging: Dir,
    backup: Dir,
    owner: Owner,
}

#[derive(Debug)]
enum Exe {
    Missing,
    Present {
        stat: Stat,
        bytes: Vec<u8>,
        sha: ArtifactSha256,
    },
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum RcptState {
    Missing,
    Valid { receipt: Receipt, raw: Vec<u8> },
    Corrupt(String),
}

#[allow(clippy::large_enum_variant)]
enum Plan {
    NoOp(Receipt),
    Go {
        kind: InstallKind,
        existing: Option<(Stat, Vec<u8>, ArtifactSha256)>,
        receipt_raw: Option<Vec<u8>>,
        adopted_from: Option<ExistingObservation>,
    },
}

impl Installer {
    pub fn new(roots: Roots, probe: Box<dyn VersionProbe>) -> Self {
        Self {
            roots,
            probe,
            faults: Box::new(NoFaults),
            lock_wait: DEFAULT_LOCK_WAIT,
            clock: Box::new(SystemClock::new()),
        }
    }

    pub fn with_clock(mut self, clock: Box<dyn WallClock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    pub fn with_faults(mut self, faults: Box<dyn Faults>) -> Self {
        self.faults = faults;
        self
    }

    fn hit(&self, p: Point) -> R<()> {
        match self.faults.at(p) {
            Fault::Pass => Ok(()),
            Fault::Errno(e) => Err(io_err(
                &format!("{p:?}"),
                std::io::Error::from_raw_os_error(e),
            )),
            Fault::Crash => Err(InstallError::Interrupted),
        }
    }

    fn install_path(&self) -> PathBuf {
        self.roots.install_dir.join(INSTALL_NAME)
    }

    fn staged_path(&self, name: &str) -> PathBuf {
        self.roots.install_dir.join(STAGING_DIR).join(name)
    }

    /// Install, update, adopt, or confirm the CLI. See the module docs for
    /// the guarantees; the returned error's [`InstallError::class`] says
    /// whether anything was changed and whether an operator is needed.
    pub fn install(
        &self,
        store: &PublisherTrustStore,
        inputs: &InstallInputs<'_>,
        approval: &Approval,
        exec: &ExecContext,
    ) -> R<InstallReport> {
        self.roots.validate_offline()?;
        exec.validate()?;
        // Cheap, side-effect-free rejection first: nothing is created for a
        // package that would be refused anyway.
        let pkg = verify_inputs(store, inputs)?;
        approval.validate(inputs.operation, exec, self.clock.unix_now() as i64)?;
        check_adoption_flag(&pkg, approval)?;
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } != self.roots.owner.uid {
                return Err(InstallError::UnsafeRoot(
                    "the installer must run as the principal that owns the install paths".into(),
                ));
            }
        }
        let owner = self.roots.owner;
        let deploy = Dir::open_absolute(&self.roots.deploy_dir, Some(0o700), owner)
            .map_err(|e| InstallError::UnsafeRoot(format!("deploy directory: {e}")))?;
        require_private_dir(&deploy, owner, "deploy directory")?;
        let _lock = FileLock::acquire(&deploy, LOCK_NAME, owner, self.lock_wait)
            .map_err(|e| InstallError::UnsafeRoot(format!("lock file: {e}")))?
            .ok_or(InstallError::LockTimeout)?;
        // Trust and admission are re-evaluated while holding the lock, so a
        // revocation that landed while we waited is honoured.
        let pkg = verify_inputs(store, inputs).inspect(|p| {
            debug_assert_eq!(p.binary_sha256, pkg.binary_sha256);
        })?;
        approval.validate(inputs.operation, exec, self.clock.unix_now() as i64)?;
        let install = Dir::open_absolute(&self.roots.install_dir, Some(0o755), owner)
            .map_err(|e| InstallError::UnsafeRoot(format!("install directory: {e}")))?;
        let st = install
            .stat_self()
            .map_err(|e| io_err("stat install dir", e))?;
        if st.uid != owner.uid || st.mode & 0o022 != 0 {
            return Err(InstallError::UnsafeRoot(
                "install directory must be owned by the installer principal and not group/world-writable"
                    .into(),
            ));
        }
        let staging = install
            .open_dir(STAGING_DIR, Some(0o700), owner)
            .map_err(|e| InstallError::UnsafeRoot(format!("staging directory: {e}")))?;
        require_private_dir(&staging, owner, "staging directory")?;
        let backup = deploy
            .open_dir(BACKUP_DIR, Some(0o700), owner)
            .map_err(|e| InstallError::UnsafeRoot(format!("backup directory: {e}")))?;
        require_private_dir(&backup, owner, "backup directory")?;
        let ctx = Ctx {
            install,
            deploy,
            staging,
            backup,
            owner,
        };

        let recovered = self.recover(&ctx)?;
        // Whatever a finished or abandoned transaction left behind (such as the
        // displaced inode of a committed swap) is ours to remove under the lock.
        self.clean_staging(&ctx)?;
        match self.plan(&ctx, &pkg, approval)? {
            Plan::NoOp(receipt) => Ok(InstallReport {
                outcome: InstallOutcome::NoOp,
                receipt,
                execution: ExecutionObservation::NotObserved,
                recovered,
            }),
            Plan::Go {
                kind,
                existing,
                receipt_raw,
                adopted_from,
            } => {
                let mut rep = self.run(
                    &ctx,
                    &pkg,
                    approval,
                    exec,
                    kind,
                    existing,
                    receipt_raw,
                    adopted_from,
                )?;
                rep.recovered = recovered;
                Ok(rep)
            }
        }
    }

    // ----- observation -----------------------------------------------------

    fn read_exe(&self, ctx: &Ctx) -> R<Exe> {
        match ctx
            .install
            .stat(INSTALL_NAME)
            .map_err(|e| io_err("stat cli", e))?
        {
            None => Ok(Exe::Missing),
            Some(s) if s.kind == Kind::Symlink => Err(InstallError::ForeignPath(
                "the install path is a symlink (possibly an externally managed install)".into(),
            )),
            Some(s) if s.kind != Kind::Regular => Err(InstallError::ForeignPath(
                "the install path is not a regular file".into(),
            )),
            Some(_) => {
                let (bytes, stat) = ctx
                    .install
                    .read_file(INSTALL_NAME, MAX_STREAM_BYTES)
                    .map_err(|e| io_err("read cli", e))?;
                if stat.nlink != 1 {
                    return Err(InstallError::ForeignPath(
                        "the executable has other hard links".into(),
                    ));
                }
                if stat.uid != ctx.owner.uid || stat.gid != ctx.owner.gid {
                    return Err(InstallError::ForeignPath(format!(
                        "the executable is owned by {}:{}, not the installer principal",
                        stat.uid, stat.gid
                    )));
                }
                if stat.mode & 0o022 != 0 {
                    return Err(InstallError::ForeignPath(
                        "the executable is group- or world-writable".into(),
                    ));
                }
                let sha = digest(&bytes);
                Ok(Exe::Present { stat, bytes, sha })
            }
        }
    }

    fn read_receipt(&self, ctx: &Ctx) -> R<RcptState> {
        match ctx
            .deploy
            .stat(RECEIPT_NAME)
            .map_err(|e| io_err("stat receipt", e))?
        {
            None => return Ok(RcptState::Missing),
            Some(s) if s.kind != Kind::Regular => {
                return Ok(RcptState::Corrupt("receipt is not a regular file".into()));
            }
            Some(_) => {}
        }
        let (raw, st) = match ctx.deploy.read_file(RECEIPT_NAME, MAX_STATE_FILE_BYTES) {
            Ok(v) => v,
            Err(e) => return Ok(RcptState::Corrupt(format!("unreadable: {e}"))),
        };
        if st.uid != ctx.owner.uid || st.mode & 0o077 != 0 || st.nlink != 1 {
            return Ok(RcptState::Corrupt(
                "receipt is not a private file of the installer principal".into(),
            ));
        }
        match Receipt::parse(&raw) {
            Ok(receipt) => Ok(RcptState::Valid { receipt, raw }),
            Err(m) => Ok(RcptState::Corrupt(m)),
        }
    }

    fn plan(&self, ctx: &Ctx, pkg: &VerifiedPackage, approval: &Approval) -> R<Plan> {
        let exe = self.read_exe(ctx)?;
        let receipt = self.read_receipt(ctx)?;
        match (exe, receipt) {
            (Exe::Missing, RcptState::Missing) => Ok(Plan::Go {
                kind: InstallKind::Fresh,
                existing: None,
                receipt_raw: None,
                adopted_from: None,
            }),
            (Exe::Missing, _) => Err(InstallError::Inconsistent(
                "a receipt exists but the executable is missing; an operator must resolve it"
                    .into(),
            )),
            (Exe::Present { .. }, RcptState::Corrupt(m)) => Err(InstallError::ReceiptCorrupt(m)),
            (Exe::Present { stat, bytes, sha }, RcptState::Missing) => {
                let adoption = match approval.adoption() {
                    None => {
                        return Err(InstallError::UnmanagedExisting(Box::new(
                            ExistingObservation {
                                sha256: sha,
                                version: String::new(),
                                uid: stat.uid,
                                gid: stat.gid,
                                mode: stat.mode,
                            },
                        )));
                    }
                    Some(a) => a,
                };
                // Only a structurally valid arm64 CLI is ever executed to
                // learn its version.
                macho::check_arm64_executable(&bytes)
                    .map_err(|e| InstallError::AdoptionMismatch(e.to_string()))?;
                let line = self
                    .probe
                    .version_line(&self.install_path())
                    .map_err(|e| InstallError::AdoptionMismatch(format!("version probe: {e}")))?;
                let version = line.strip_prefix("kanade ").unwrap_or(&line).to_owned();
                let observed = ExistingObservation {
                    sha256: sha,
                    version,
                    uid: stat.uid,
                    gid: stat.gid,
                    mode: stat.mode,
                };
                if &observed != adoption {
                    return Err(InstallError::AdoptionMismatch(
                        "digest, version, ownership or mode differs from the approved observation"
                            .into(),
                    ));
                }
                Ok(Plan::Go {
                    kind: InstallKind::Adopt,
                    existing: Some((stat, bytes, sha)),
                    receipt_raw: None,
                    adopted_from: Some(observed),
                })
            }
            (Exe::Present { stat, bytes, sha }, RcptState::Valid { receipt, raw }) => {
                let b = &receipt.body;
                if b.binary_sha256 != sha
                    || b.mode != stat.mode
                    || b.uid != stat.uid
                    || b.gid != stat.gid
                    || b.install_path != self.install_path().to_string_lossy()
                {
                    return Err(InstallError::Drift(
                        "the installed executable no longer matches its receipt".into(),
                    ));
                }
                if stat.mode != INSTALL_MODE {
                    return Err(InstallError::Drift("the executable mode changed".into()));
                }
                if b.identity == pkg.spec.artifact_identity
                    && b.artifact_sha256 == pkg.spec.artifact_sha256
                    && sha == pkg.binary_sha256
                {
                    return Ok(Plan::NoOp(receipt));
                }
                Ok(Plan::Go {
                    kind: InstallKind::Update,
                    existing: Some((stat, bytes, sha)),
                    receipt_raw: Some(raw),
                    adopted_from: None,
                })
            }
        }
    }

    // ----- journal ---------------------------------------------------------

    fn write_journal(&self, ctx: &Ctx, j: &JournalBody) -> R<()> {
        let bytes = JournalFile::new(j.clone()).to_bytes();
        ctx.deploy
            .remove_if_exists(JOURNAL_TMP)
            .map_err(|e| io_err("journal", e))?;
        ctx.deploy
            .write_new(JOURNAL_TMP, &bytes, 0o600, ctx.owner)
            .map_err(|e| io_err("journal write", e))?;
        ctx.deploy
            .rename_to(JOURNAL_TMP, &ctx.deploy, JOURNAL_NAME, RenameMode::Replace)
            .map_err(|e| io_err("journal commit", e))?;
        ctx.deploy.fsync().map_err(|e| io_err("journal sync", e))
    }

    fn persist(&self, ctx: &Ctx, j: &mut JournalBody, phase: Phase) -> R<()> {
        self.hit(Point::BeforePhase(phase))?;
        j.phase = phase;
        self.write_journal(ctx, j)?;
        self.hit(Point::AfterPhase(phase))
    }

    fn read_journal(&self, ctx: &Ctx) -> R<Option<JournalBody>> {
        match ctx
            .deploy
            .stat(JOURNAL_NAME)
            .map_err(|e| io_err("stat journal", e))?
        {
            None => Ok(None),
            Some(s) if s.kind != Kind::Regular => {
                Err(InstallError::JournalCorrupt("not a regular file".into()))
            }
            Some(_) => {
                let (raw, st) = ctx
                    .deploy
                    .read_file(JOURNAL_NAME, MAX_STATE_FILE_BYTES)
                    .map_err(|e| InstallError::JournalCorrupt(e.to_string()))?;
                if st.uid != ctx.owner.uid || st.mode & 0o077 != 0 || st.nlink != 1 {
                    return Err(InstallError::JournalCorrupt(
                        "not a private file of the installer principal".into(),
                    ));
                }
                JournalFile::parse(&raw)
                    .map(|f| Some(f.body))
                    .map_err(InstallError::JournalCorrupt)
            }
        }
    }

    // ----- the transaction -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        ctx: &Ctx,
        pkg: &VerifiedPackage,
        approval: &Approval,
        exec: &ExecContext,
        kind: InstallKind,
        existing: Option<(Stat, Vec<u8>, ArtifactSha256)>,
        receipt_raw: Option<Vec<u8>>,
        adopted_from: Option<ExistingObservation>,
    ) -> R<InstallReport> {
        let new_body = ReceiptBody {
            schema: RECEIPT_SCHEMA,
            identity: pkg.spec.artifact_identity.clone(),
            package_id: pkg.spec.package_id.clone(),
            artifact_sha256: pkg.spec.artifact_sha256,
            artifact_size_bytes: pkg.spec.artifact_size_bytes,
            manifest_sha256: pkg.spec.manifest_sha256,
            publisher_key_id: pkg.admitted.publisher().as_str().to_owned(),
            trust_set_id: pkg.admitted.trust_set_id().to_owned(),
            trust_sequence: pkg.admitted.trust_sequence(),
            binary_sha256: pkg.binary_sha256,
            binary_size_bytes: pkg.binary.len() as u64,
            mode: INSTALL_MODE,
            uid: ctx.owner.uid,
            gid: ctx.owner.gid,
            install_path: self.install_path().to_string_lossy().into_owned(),
            origin: if kind == InstallKind::Adopt {
                Origin::Adopted
            } else {
                Origin::Installed
            },
            adopted_from,
            plan_id: approval.plan_id().to_owned(),
            plan_digest: *approval.plan_digest(),
            job_id: exec.job_id.clone(),
            exec_id: exec.exec_id.clone(),
            installed_at_unix: chrono::Utc::now().timestamp(),
        };
        let prev = existing.as_ref().map(|(s, b, sha)| PrevState {
            file_id: Some([s.dev, s.ino]),
            binary_sha256: *sha,
            binary_size_bytes: b.len() as u64,
            mode: s.mode,
            had_receipt: receipt_raw.is_some(),
        });
        let mut j = JournalBody {
            schema: JOURNAL_SCHEMA,
            txn_id: uuid::Uuid::new_v4().to_string(),
            job_id: exec.job_id.clone(),
            exec_id: exec.exec_id.clone(),
            kind,
            phase: Phase::Prepared,
            recovery_attempts: 0,
            new_receipt: new_body.clone(),
            prev,
            note: None,
        };

        // Before anything on the install path changes, any failure just
        // abandons the attempt.
        if let Err(e) =
            self.run_prepare(ctx, pkg, &mut j, existing.as_ref(), receipt_raw.as_deref())
        {
            return Err(self.abandon(ctx, &mut j, e));
        }

        // Past this point the install path may have changed.
        let swapped = self.run_swap_and_commit(ctx, pkg, &mut j, existing.as_ref(), &new_body);
        match swapped {
            Ok(rep) => Ok(rep),
            Err(Stop::Pre(e)) => Err(self.abandon(ctx, &mut j, e)),
            Err(Stop::Frozen(e)) => Err(e),
            Err(Stop::Crash) => Err(InstallError::Interrupted),
            Err(Stop::Failed(e)) => match self.rollback(ctx, &mut j) {
                Ok(()) => Err(InstallError::RolledBack {
                    cause: e.to_string(),
                }),
                Err(RollbackStop::Crash) => Err(InstallError::Interrupted),
                Err(RollbackStop::Failed(why)) => Err(self.freeze(ctx, &mut j, &why, &e)),
            },
        }
    }

    fn run_prepare(
        &self,
        ctx: &Ctx,
        pkg: &VerifiedPackage,
        j: &mut JournalBody,
        existing: Option<&(Stat, Vec<u8>, ArtifactSha256)>,
        receipt_raw: Option<&[u8]>,
    ) -> R<()> {
        self.persist(ctx, j, Phase::Prepared)?;
        self.clean_staging(ctx)?;

        // Stage: write, flush, read back, and ask the staged copy for its version.
        self.hit(Point::StageWrite)?;
        ctx.staging
            .write_new(STAGED, &pkg.binary, INSTALL_MODE, ctx.owner)
            .map_err(|e| io_err("stage executable", e))?;
        ctx.staging.fsync().map_err(|e| io_err("stage sync", e))?;
        let (staged, st) = ctx
            .staging
            .read_file(STAGED, MAX_STREAM_BYTES)
            .map_err(|e| io_err("stage verify", e))?;
        if digest(&staged) != pkg.binary_sha256
            || st.mode != INSTALL_MODE
            || st.uid != ctx.owner.uid
            || st.nlink != 1
        {
            return Err(InstallError::Inconsistent(
                "the staged executable does not match the verified bytes".into(),
            ));
        }
        self.check_version(&self.staged_path(STAGED), pkg.version())?;
        self.persist(ctx, j, Phase::Staged)?;

        // Back up the previous executable and receipt as a verified pair.
        if let Some((_, bytes, sha)) = existing {
            self.hit(Point::BackupWrite)?;
            self.put_backup(ctx, BACKUP_BIN, bytes)?;
            match receipt_raw {
                Some(raw) => self.put_backup(ctx, BACKUP_RECEIPT, raw)?,
                None => ctx
                    .backup
                    .remove_if_exists(BACKUP_RECEIPT)
                    .map_err(|e| io_err("clear stale receipt backup", e))?,
            }
            ctx.backup.fsync().map_err(|e| io_err("backup sync", e))?;
            let (back, _) = ctx
                .backup
                .read_file(BACKUP_BIN, MAX_STREAM_BYTES)
                .map_err(|e| io_err("backup verify", e))?;
            if digest(&back) != *sha {
                return Err(InstallError::Inconsistent(
                    "backup verification failed".into(),
                ));
            }
            self.persist(ctx, j, Phase::BackedUp)?;
        }
        Ok(())
    }

    fn put_backup(&self, ctx: &Ctx, name: &str, bytes: &[u8]) -> R<()> {
        let tmp = format!("{name}.tmp");
        ctx.backup
            .remove_if_exists(&tmp)
            .map_err(|e| io_err("backup", e))?;
        ctx.backup
            .write_new(&tmp, bytes, 0o600, ctx.owner)
            .map_err(|e| io_err("backup write", e))?;
        ctx.backup
            .rename_to(&tmp, &ctx.backup, name, RenameMode::Replace)
            .map_err(|e| io_err("backup commit", e))
    }

    fn clean_staging(&self, ctx: &Ctx) -> R<()> {
        for n in [STAGED, RESTORE, DISCARD] {
            ctx.staging
                .remove_if_exists(n)
                .map_err(|e| io_err("clean staging", e))?;
        }
        Ok(())
    }

    fn check_version(&self, path: &std::path::Path, version: &str) -> R<()> {
        let line = self.probe.version_line(path).map_err(InstallError::Probe)?;
        let want = expected_line(version);
        if line != want {
            return Err(InstallError::VersionMismatch {
                expected: want,
                found: line,
            });
        }
        Ok(())
    }

    fn run_swap_and_commit(
        &self,
        ctx: &Ctx,
        pkg: &VerifiedPackage,
        j: &mut JournalBody,
        existing: Option<&(Stat, Vec<u8>, ArtifactSha256)>,
        new_body: &ReceiptBody,
    ) -> Result<InstallReport, Stop> {
        let swap_mode = if existing.is_some() {
            RenameMode::Swap
        } else {
            RenameMode::NoReplace
        };
        self.persist(ctx, j, Phase::Swapping).map_err(Stop::pre)?;
        self.hit(Point::Swap).map_err(Stop::pre)?;
        // The rename is atomic: if it fails, nothing changed.
        if let Err(e) = ctx
            .staging
            .rename_to(STAGED, &ctx.install, INSTALL_NAME, swap_mode)
        {
            return Err(Stop::Pre(match e.kind() {
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound => {
                    InstallError::Concurrent(format!(
                        "the install path changed after it was inspected ({e}); left untouched"
                    ))
                }
                _ => io_err("swap executable", e),
            }));
        }

        // What we displaced must be exactly what we inspected; otherwise a
        // third party changed it, and their file goes straight back.
        if let Some((stat, _, sha)) = existing {
            let displaced = ctx.staging.read_file(STAGED, MAX_STREAM_BYTES);
            let ok = matches!(&displaced, Ok((b, s)) if s.same_file(stat) && digest(b) == *sha);
            if !ok {
                j.note = Some("foreign displaced path retained; swap-back required".into());
                for dir in [&ctx.staging, &ctx.install] {
                    dir.fsync().map_err(|e| {
                        Stop::Frozen(self.freeze(
                            ctx,
                            j,
                            "cannot flush foreign displaced path",
                            &io_err("sync swap", e),
                        ))
                    })?;
                }
                self.persist(ctx, j, Phase::SwapBackRequired).map_err(|e| {
                    if matches!(e, InstallError::Interrupted) {
                        Stop::Crash
                    } else {
                        Stop::Frozen(self.freeze(ctx, j, "cannot record swap-back intent", &e))
                    }
                })?;
                self.hit(Point::SwapBack).map_err(|e| {
                    if matches!(e, InstallError::Interrupted) {
                        Stop::Crash
                    } else {
                        Stop::Frozen(self.freeze(ctx, j, "swap-back interrupted", &e))
                    }
                })?;
                let back =
                    ctx.staging
                        .rename_to(STAGED, &ctx.install, INSTALL_NAME, RenameMode::Swap);
                return Err(match back {
                    Ok(()) => {
                        for dir in [&ctx.staging, &ctx.install] {
                            if let Err(e) = dir.fsync() {
                                return Err(Stop::Frozen(self.freeze(
                                    ctx,
                                    j,
                                    "cannot flush completed swap-back",
                                    &io_err("sync swap-back", e),
                                )));
                            }
                        }
                        Stop::Pre(InstallError::Concurrent(
                            "the install path was replaced between inspection and swap; the other party's file was put back".into(),
                        ))
                    }
                    Err(e) => Stop::Frozen(self.freeze(
                        ctx,
                        j,
                        "swap-back failed; foreign path retained",
                        &io_err("swap-back", e),
                    )),
                });
            }
        }
        self.persist(ctx, j, Phase::Swapped).map_err(Stop::failed)?;

        let line = self.verify_installed(ctx, pkg).map_err(Stop::failed)?;

        self.hit(Point::ReceiptWrite).map_err(Stop::failed)?;
        let receipt = Receipt::new(new_body.clone());
        self.write_receipt(ctx, &receipt).map_err(Stop::failed)?;
        match self.read_receipt(ctx).map_err(Stop::failed)? {
            RcptState::Valid { receipt: back, .. } if back == receipt => {}
            _ => {
                return Err(Stop::Failed(InstallError::Inconsistent(
                    "receipt read-back differs from what was written".into(),
                )));
            }
        }
        self.persist(ctx, j, Phase::ReceiptWritten)
            .map_err(Stop::failed)?;
        self.persist(ctx, j, Phase::Committed)
            .map_err(Stop::failed)?;
        let _ = self.clean_staging(ctx);
        Ok(InstallReport {
            outcome: match j.kind {
                InstallKind::Fresh => InstallOutcome::Installed,
                InstallKind::Update => InstallOutcome::Updated,
                InstallKind::Adopt => InstallOutcome::Adopted,
            },
            receipt,
            execution: ExecutionObservation::VersionObserved { line },
            recovered: None,
        })
    }

    fn write_receipt(&self, ctx: &Ctx, r: &Receipt) -> R<()> {
        ctx.deploy
            .remove_if_exists(RECEIPT_TMP)
            .map_err(|e| io_err("receipt", e))?;
        ctx.deploy
            .write_new(RECEIPT_TMP, &r.to_bytes(), 0o600, ctx.owner)
            .map_err(|e| io_err("receipt write", e))?;
        ctx.deploy
            .rename_to(RECEIPT_TMP, &ctx.deploy, RECEIPT_NAME, RenameMode::Replace)
            .map_err(|e| io_err("receipt commit", e))?;
        ctx.deploy.fsync().map_err(|e| io_err("receipt sync", e))
    }

    /// Files first (installed_verified), then a bounded `--version` run
    /// (execution observation). Returns the line the CLI printed.
    fn verify_installed(&self, ctx: &Ctx, pkg: &VerifiedPackage) -> R<String> {
        ctx.install.fsync().map_err(|e| io_err("install sync", e))?;
        ctx.staging.fsync().map_err(|e| io_err("staging sync", e))?;
        let (bytes, st) = ctx
            .install
            .read_file(INSTALL_NAME, MAX_STREAM_BYTES)
            .map_err(|e| io_err("verify installed", e))?;
        if digest(&bytes) != pkg.binary_sha256
            || st.mode != INSTALL_MODE
            || st.uid != ctx.owner.uid
            || st.gid != ctx.owner.gid
            || st.nlink != 1
        {
            return Err(InstallError::Inconsistent(
                "the installed executable does not match the verified bytes, owner or mode".into(),
            ));
        }
        self.check_version(&self.install_path(), pkg.version())?;
        Ok(expected_line(pkg.version()))
    }

    // ----- failure handling ------------------------------------------------

    /// Mark a pre-change failure and return it unchanged.
    fn abandon(&self, ctx: &Ctx, j: &mut JournalBody, e: InstallError) -> InstallError {
        if matches!(e, InstallError::Interrupted) {
            return e;
        }
        let _ = self.clean_staging(ctx);
        j.note = Some(e.to_string());
        let _ = self.persist(ctx, j, Phase::Abandoned);
        e
    }

    /// Record an unrecoverable state. The journal is frozen: later runs
    /// refuse to act until an operator inspects the retained backups.
    fn freeze(
        &self,
        ctx: &Ctx,
        j: &mut JournalBody,
        why: &str,
        cause: &InstallError,
    ) -> InstallError {
        j.note = Some(format!("{why} (after: {cause})"));
        j.phase = Phase::RecoveryRequired;
        let _ = self.write_journal(ctx, j);
        InstallError::RecoveryRequired(format!("{why} (after: {cause}); backups retained"))
    }

    fn rollback(&self, ctx: &Ctx, j: &mut JournalBody) -> Result<(), RollbackStop> {
        // Best effort: if the journal itself cannot be written we still restore.
        if let Err(InstallError::Interrupted) = self.persist(ctx, j, Phase::RollingBack) {
            return Err(RollbackStop::Crash);
        }
        let r = self.restore_state(ctx, j);
        match r {
            Ok(()) => {
                let _ = self.clean_staging(ctx);
                match self.persist(ctx, j, Phase::RolledBack) {
                    Err(InstallError::Interrupted) => Err(RollbackStop::Crash),
                    _ => Ok(()),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn restore_state(&self, ctx: &Ctx, j: &JournalBody) -> Result<(), RollbackStop> {
        let new_sha = j.new_receipt.binary_sha256;
        let cur = match self.read_exe_loose(ctx) {
            Ok(c) => c,
            Err(e) => {
                return Err(RollbackStop::Failed(format!(
                    "cannot inspect the executable: {e}"
                )));
            }
        };
        match (&j.prev, cur) {
            (None, None) => {}
            (None, Some((_, sha))) if sha == new_sha => {
                // Move ours aside, confirm it is ours, then remove it.
                ctx.install
                    .rename_to(INSTALL_NAME, &ctx.staging, DISCARD, RenameMode::NoReplace)
                    .map_err(|e| {
                        RollbackStop::Failed(format!("cannot remove the new executable: {e}"))
                    })?;
                let moved = ctx.staging.read_file(DISCARD, MAX_STREAM_BYTES);
                if !matches!(&moved, Ok((b, _)) if digest(b) == new_sha) {
                    let _ = ctx.staging.rename_to(
                        DISCARD,
                        &ctx.install,
                        INSTALL_NAME,
                        RenameMode::NoReplace,
                    );
                    return Err(RollbackStop::Failed(
                        "the install path changed during rollback; left as found".into(),
                    ));
                }
                ctx.staging.unlink(DISCARD).map_err(|e| {
                    RollbackStop::Failed(format!("cannot delete the new executable: {e}"))
                })?;
            }
            (None, Some(_)) => {
                return Err(RollbackStop::Failed(
                    "the install path holds a file that is not ours".into(),
                ));
            }
            (Some(p), Some((_, sha))) if sha == p.binary_sha256 => {}
            (Some(p), cur) => {
                let present = cur.as_ref().map(|(_, s)| *s);
                if present.is_some_and(|s| s != new_sha) {
                    return Err(RollbackStop::Failed(
                        "the executable is neither the previous nor the new version".into(),
                    ));
                }
                self.restore_binary(ctx, p, present.is_some(), new_sha)?;
            }
        }
        self.restore_receipt(ctx, j)?;
        ctx.install
            .fsync()
            .and_then(|_| ctx.deploy.fsync())
            .map_err(|e| RollbackStop::Failed(format!("cannot flush the restore: {e}")))?;
        // Prove the restored state before declaring it.
        match (&j.prev, self.read_exe_loose(ctx)) {
            (None, Ok(None)) => Ok(()),
            (Some(p), Ok(Some((_, sha)))) if sha == p.binary_sha256 => Ok(()),
            _ => Err(RollbackStop::Failed(
                "restored state failed verification".into(),
            )),
        }
    }

    fn restore_binary(
        &self,
        ctx: &Ctx,
        p: &PrevState,
        exists: bool,
        new_sha: ArtifactSha256,
    ) -> Result<(), RollbackStop> {
        let fail = |m: String| RollbackStop::Failed(m);
        let (bytes, _) = ctx
            .backup
            .read_file(BACKUP_BIN, MAX_STREAM_BYTES)
            .map_err(|e| fail(format!("the retained backup is unreadable: {e}")))?;
        if digest(&bytes) != p.binary_sha256 {
            return Err(fail(
                "the retained backup does not match the journal".into(),
            ));
        }
        match self.hit(Point::RestoreWrite) {
            Err(InstallError::Interrupted) => return Err(RollbackStop::Crash),
            Err(e) => return Err(fail(e.to_string())),
            Ok(()) => {}
        }
        ctx.staging
            .remove_if_exists(RESTORE)
            .and_then(|_| ctx.staging.write_new(RESTORE, &bytes, p.mode, ctx.owner))
            .map_err(|e| fail(format!("cannot stage the restore: {e}")))?;
        match self.hit(Point::RestoreSwap) {
            Err(InstallError::Interrupted) => return Err(RollbackStop::Crash),
            Err(e) => return Err(fail(e.to_string())),
            Ok(()) => {}
        }
        if exists {
            ctx.staging
                .rename_to(RESTORE, &ctx.install, INSTALL_NAME, RenameMode::Swap)
                .map_err(|e| fail(format!("cannot swap the previous executable back: {e}")))?;
            // What came out must be ours; if not, put the other party's file back.
            let out = ctx.staging.read_file(RESTORE, MAX_STREAM_BYTES);
            if !matches!(&out, Ok((b, _)) if digest(b) == new_sha) {
                let _ =
                    ctx.staging
                        .rename_to(RESTORE, &ctx.install, INSTALL_NAME, RenameMode::Swap);
                return Err(fail(
                    "the install path changed during rollback; left as found".into(),
                ));
            }
            ctx.staging
                .unlink(RESTORE)
                .map_err(|e| fail(format!("cannot clean the restore area: {e}")))
        } else {
            ctx.staging
                .rename_to(RESTORE, &ctx.install, INSTALL_NAME, RenameMode::NoReplace)
                .map_err(|e| fail(format!("cannot place the previous executable: {e}")))
        }
    }

    fn restore_receipt(&self, ctx: &Ctx, j: &JournalBody) -> Result<(), RollbackStop> {
        let fail = |m: String| RollbackStop::Failed(m);
        let cur = self
            .read_receipt(ctx)
            .map_err(|e| fail(format!("cannot inspect the receipt: {e}")))?;
        let ours = |r: &Receipt| r.body == j.new_receipt;
        let had = j.prev.as_ref().is_some_and(|p| p.had_receipt);
        if !had {
            return match cur {
                RcptState::Missing => Ok(()),
                RcptState::Valid { receipt, .. } if ours(&receipt) => ctx
                    .deploy
                    .unlink(RECEIPT_NAME)
                    .map_err(|e| fail(format!("cannot remove the new receipt: {e}"))),
                _ => Err(fail("the receipt is not ours; left as found".into())),
            };
        }
        let (prev_raw, _) = ctx
            .backup
            .read_file(BACKUP_RECEIPT, MAX_STATE_FILE_BYTES)
            .map_err(|e| fail(format!("the retained receipt backup is unreadable: {e}")))?;
        let prev = Receipt::parse(&prev_raw)
            .map_err(|e| fail(format!("the retained receipt backup is corrupt: {e}")))?;
        match cur {
            RcptState::Valid { raw, .. } if raw == prev_raw => return Ok(()),
            RcptState::Valid { receipt, .. } if ours(&receipt) => {}
            RcptState::Missing => {}
            _ => return Err(fail("the receipt is not ours; left as found".into())),
        }
        let _ = prev;
        match self.hit(Point::RestoreReceipt) {
            Err(InstallError::Interrupted) => return Err(RollbackStop::Crash),
            Err(e) => return Err(fail(e.to_string())),
            Ok(()) => {}
        }
        ctx.deploy
            .remove_if_exists(RECEIPT_TMP)
            .and_then(|_| {
                ctx.deploy
                    .write_new(RECEIPT_TMP, &prev_raw, 0o600, ctx.owner)
            })
            .and_then(|_| {
                ctx.deploy
                    .rename_to(RECEIPT_TMP, &ctx.deploy, RECEIPT_NAME, RenameMode::Replace)
            })
            .map_err(|e| fail(format!("cannot restore the receipt: {e}")))
    }

    /// Like [`Self::read_exe`] but only reports the content (rollback must
    /// work even when ownership has drifted).
    fn read_exe_loose(&self, ctx: &Ctx) -> R<Option<(Stat, ArtifactSha256)>> {
        match ctx
            .install
            .stat(INSTALL_NAME)
            .map_err(|e| io_err("stat cli", e))?
        {
            None => Ok(None),
            Some(s) if s.kind != Kind::Regular => Err(InstallError::ForeignPath(
                "the install path is not a regular file".into(),
            )),
            Some(_) => {
                let (b, st) = ctx
                    .install
                    .read_file(INSTALL_NAME, MAX_STREAM_BYTES)
                    .map_err(|e| io_err("read cli", e))?;
                Ok(Some((st, digest(&b))))
            }
        }
    }

    // ----- recovery --------------------------------------------------------

    /// Resolve whatever an earlier, interrupted run left behind. Returns a
    /// note when it acted. Never retries more than `MAX_RECOVERY_ATTEMPTS`
    /// times and never touches a frozen journal.
    fn recover(&self, ctx: &Ctx) -> R<Option<String>> {
        let Some(mut j) = self.read_journal(ctx)? else {
            return Ok(None);
        };
        match j.phase {
            Phase::Committed | Phase::RolledBack | Phase::Abandoned => return Ok(None),
            Phase::RecoveryRequired => {
                return Err(InstallError::RecoveryRequired(format!(
                    "an earlier install is frozen: {}",
                    j.note.as_deref().unwrap_or("no detail")
                )));
            }
            _ => {}
        }
        let cause = InstallError::Interrupted;
        if j.recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
            return Err(self.freeze(ctx, &mut j, "recovery attempts exhausted", &cause));
        }
        j.recovery_attempts += 1;
        self.write_journal(ctx, &j)?;

        if j.phase == Phase::SwapBackRequired {
            return Err(self.freeze(
                ctx,
                &mut j,
                "interrupted swap-back; foreign path retained in staging",
                &cause,
            ));
        }
        // Swapping is durable before the exchange, but a crash can precede
        // displaced-file validation. Never delete an unverified displaced path.
        if j.phase == Phase::Swapping {
            let staged = ctx.staging.read_file(STAGED, MAX_STREAM_BYTES);
            let current = ctx.install.stat(INSTALL_NAME);
            let known = match (&j.prev, &staged, &current) {
                (Some(prev), Ok((bytes, st)), Ok(cur)) => {
                    let original = |s: &Stat| prev.file_id == Some([s.dev, s.ino]);
                    (original(st) && digest(bytes) == prev.binary_sha256)
                        || (cur.as_ref().is_some_and(original)
                            && digest(bytes) == j.new_receipt.binary_sha256)
                }
                (None, Ok((bytes, _)), Ok(None)) => digest(bytes) == j.new_receipt.binary_sha256,
                (None, Err(_), _) => ctx.staging.stat(STAGED).is_ok_and(|s| s.is_none()),
                _ => false,
            };
            if !known {
                return Err(self.freeze(
                    ctx,
                    &mut j,
                    "unverified displaced path retained in staging",
                    &cause,
                ));
            }
        }
        if !j.phase.may_have_changed_install() {
            self.clean_staging(ctx)?;
            j.note = Some("interrupted before the install path was touched".into());
            self.persist(ctx, &mut j, Phase::Abandoned)?;
            return Ok(Some(
                "an interrupted install that had not changed anything was abandoned".into(),
            ));
        }
        if j.phase == Phase::RollingBack {
            return match self.rollback(ctx, &mut j) {
                Ok(()) => Ok(Some("an interrupted rollback was completed".into())),
                Err(RollbackStop::Crash) => Err(InstallError::Interrupted),
                Err(RollbackStop::Failed(why)) => Err(self.freeze(ctx, &mut j, &why, &cause)),
            };
        }

        let cur = match self.read_exe_loose(ctx) {
            Ok(c) => c,
            Err(e) => return Err(self.freeze(ctx, &mut j, &e.to_string(), &cause)),
        };
        let new_sha = j.new_receipt.binary_sha256;
        let is_prev = match (&j.prev, &cur) {
            (None, None) => true,
            (Some(p), Some((_, s))) => *s == p.binary_sha256,
            _ => false,
        };
        let is_new = matches!(&cur, Some((_, s)) if *s == new_sha);
        let rcpt = self.read_receipt(ctx)?;
        if is_prev && !is_new {
            if matches!(&rcpt, RcptState::Valid { receipt, .. } if receipt.body == j.new_receipt) {
                return Err(self.freeze(
                    ctx,
                    &mut j,
                    "the receipt is new but the executable is not",
                    &cause,
                ));
            }
            self.clean_staging(ctx)?;
            j.note = Some("the swap had not happened".into());
            self.persist(ctx, &mut j, Phase::Abandoned)?;
            return Ok(Some(
                "an interrupted install was abandoned; the swap had not happened".into(),
            ));
        }
        if !is_new {
            return Err(self.freeze(
                ctx,
                &mut j,
                "the executable matches neither the previous nor the new version",
                &cause,
            ));
        }
        // Roll forward: the new executable is in place. Re-verify it and
        // finish the receipt from the journalled body, or restore on failure.
        let forward = self.roll_forward(ctx, &j, &rcpt);
        match forward {
            Ok(()) => {
                self.persist(ctx, &mut j, Phase::Committed)?;
                let _ = self.clean_staging(ctx);
                Ok(Some("an interrupted install was completed".into()))
            }
            Err(InstallError::Interrupted) => Err(InstallError::Interrupted),
            Err(e) => match self.rollback(ctx, &mut j) {
                Ok(()) => Ok(Some(format!(
                    "an interrupted install could not be completed ({e}) and was rolled back"
                ))),
                Err(RollbackStop::Crash) => Err(InstallError::Interrupted),
                Err(RollbackStop::Failed(why)) => Err(self.freeze(ctx, &mut j, &why, &e)),
            },
        }
    }

    fn roll_forward(&self, ctx: &Ctx, j: &JournalBody, rcpt: &RcptState) -> R<()> {
        let b = &j.new_receipt;
        let (bytes, st) = ctx
            .install
            .read_file(INSTALL_NAME, MAX_STREAM_BYTES)
            .map_err(|e| io_err("verify installed", e))?;
        if digest(&bytes) != b.binary_sha256
            || st.mode != INSTALL_MODE
            || st.uid != ctx.owner.uid
            || st.gid != ctx.owner.gid
            || st.nlink != 1
        {
            return Err(InstallError::Inconsistent(
                "the installed executable does not match the journal".into(),
            ));
        }
        self.check_version(&self.install_path(), b.identity.version().as_str())?;
        let receipt = Receipt::new(b.clone());
        match rcpt {
            RcptState::Valid { receipt: r, .. } if *r == receipt => Ok(()),
            RcptState::Valid { raw, .. } => {
                // Only the previous receipt may be replaced.
                let prev_ok = ctx
                    .backup
                    .read_file(BACKUP_RECEIPT, MAX_STATE_FILE_BYTES)
                    .is_ok_and(|(p, _)| &p == raw);
                if !prev_ok {
                    return Err(InstallError::Inconsistent(
                        "an unexpected receipt is present".into(),
                    ));
                }
                self.hit(Point::ReceiptWrite)?;
                self.write_receipt(ctx, &receipt)
            }
            RcptState::Missing => {
                self.hit(Point::ReceiptWrite)?;
                self.write_receipt(ctx, &receipt)
            }
            RcptState::Corrupt(m) => Err(InstallError::ReceiptCorrupt(m.clone())),
        }
    }
}

/// Why a mid-transaction step stopped.
enum Stop {
    /// Failed with the install path provably unchanged.
    Pre(InstallError),
    /// Failed after the install path may have changed: roll back.
    Failed(InstallError),
    Frozen(InstallError),
    Crash,
}

impl Stop {
    fn pre(e: InstallError) -> Stop {
        if matches!(e, InstallError::Interrupted) {
            Stop::Crash
        } else {
            Stop::Pre(e)
        }
    }
    fn failed(e: InstallError) -> Stop {
        if matches!(e, InstallError::Interrupted) {
            Stop::Crash
        } else {
            Stop::Failed(e)
        }
    }
}

enum RollbackStop {
    Crash,
    Failed(String),
}

fn require_private_dir(d: &Dir, owner: Owner, what: &str) -> R<()> {
    let st = d.stat_self().map_err(|e| io_err(what, e))?;
    if st.uid != owner.uid || st.mode & 0o077 != 0 {
        return Err(InstallError::UnsafeRoot(format!(
            "{what} must be private to the installer principal"
        )));
    }
    Ok(())
}

fn check_adoption_flag(pkg: &VerifiedPackage, approval: &Approval) -> R<()> {
    // Adoption must be asked for by the signed operation *and* approved with
    // a recorded observation; either alone is refused.
    if pkg.spec.adoption_observation_sha256 != approval.adoption().map(ExistingObservation::digest)
    {
        return Err(InstallError::Approval(
            "adoption observation digest differs from approved operation".into(),
        ));
    }
    match (pkg.spec.adopt_existing_cli, approval.adoption().is_some()) {
        (true, false) => Err(InstallError::Approval(
            "adopt_existing_cli requires an approved observation of the existing executable".into(),
        )),
        (false, true) => Err(InstallError::Approval(
            "an adoption observation was approved but the operation does not request adoption"
                .into(),
        )),
        _ => Ok(()),
    }
}
