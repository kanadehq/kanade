//! Data contracts of the macOS admin CLI installer: fixed layout, receipt,
//! transaction journal, approval, and the narrow result record.
//!
//! The receipt and journal are plain JSON with a SHA-256 over the canonical
//! body, so a truncated or hand-edited file is detected rather than trusted.

use std::path::PathBuf;

use kanade_shared::artifact::{ArtifactIdentity, ArtifactSha256};
use kanade_shared::deploy_plan::{
    DispatchActor, GateInput, PlanRecord, RequestedScope, TargetFacts, evaluate,
};
use kanade_shared::manifest::Manifest;
use kanade_shared::typed_operation::TypedOperation;
use serde::{Deserialize, Serialize};

use super::archive::ArchiveError;
use super::macho::MachOError;

/// Fixed install name of the CLI. Never taken from the archive or a plan.
pub const INSTALL_NAME: &str = "kanade";
/// Mode of the installed executable (`root:wheel 0755` in production).
pub const INSTALL_MODE: u32 = 0o755;
/// Fixed production locations (SPEC §2.13.6.4, macOS Admin CLI).
pub const PRODUCTION_INSTALL_DIR: &str = "/usr/local/bin";
pub const PRODUCTION_DEPLOY_DIR: &str = "/Library/Application Support/Kanade/deploy";
/// Largest receipt/journal file we will read.
pub const MAX_STATE_FILE_BYTES: u64 = 256 * 1024;

/// Offline roots only. Production paths are policy constants, not a live API.
#[derive(Debug, Clone)]
pub struct Roots {
    pub install_dir: PathBuf,
    pub deploy_dir: PathBuf,
    #[cfg(unix)]
    pub owner: super::fs::Owner,
}

impl Roots {
    pub fn validate_offline(&self) -> Result<(), InstallError> {
        let temp = std::env::temp_dir()
            .canonicalize()
            .map_err(|e| InstallError::io("temp root", e))?;
        let shared = std::path::Path::new("/tmp").canonicalize().ok();
        for path in [&self.install_dir, &self.deploy_dir] {
            if !path.is_absolute()
                || path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(InstallError::UnsafeRoot(
                    "offline roots must be absolute temporary paths".into(),
                ));
            }
            // Resolve the existing ancestor without creating anything. The fd walk
            // still rejects symlinks when the transaction opens the directories.
            let mut ancestor = path.as_path();
            while !ancestor.exists() {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| InstallError::UnsafeRoot("no existing ancestor".into()))?;
            }
            let resolved = ancestor
                .canonicalize()
                .map_err(|e| InstallError::io("offline root", e))?;
            let inside = |base: &std::path::Path| resolved.starts_with(base) && resolved != base;
            if !inside(&temp) && !shared.as_deref().is_some_and(inside) {
                return Err(InstallError::UnsafeRoot(
                    "offline installer requires a temporary subdirectory".into(),
                ));
            }
        }
        Ok(())
    }
}

/// What the current host reports, so admission can check compatibility.
#[derive(Debug, Clone)]
pub struct HostInfo {
    /// Dotted macOS version, e.g. `14.5`.
    pub os_version: String,
    pub capabilities: Vec<String>,
}

/// Ids of the job/exec being served, recorded in the receipt.
#[derive(Debug, Clone)]
pub struct ExecContext {
    pub job_id: String,
    pub exec_id: String,
}

impl ExecContext {
    pub fn validate(&self) -> Result<(), InstallError> {
        for v in [&self.job_id, &self.exec_id] {
            if v.is_empty()
                || v.len() > 128
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            {
                return Err(InstallError::Approval(
                    "job/exec id is not a plain id".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Facts about a pre-existing, unmanaged executable that an operator approved
/// for adoption. Compared exactly against what is observed at apply time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExistingObservation {
    pub sha256: ArtifactSha256,
    pub version: String,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl ExistingObservation {
    pub fn digest(&self) -> ArtifactSha256 {
        let mut bytes = b"kanade/cli-adoption/v1\0".to_vec();
        bytes.extend(kanade_shared::deploy_plan::canonical_json(
            &serde_json::to_value(self).expect("observation serializes"),
        ));
        ArtifactSha256::of(&bytes)
    }
}

/// Operator approval of exactly this deployment — a prerequisite that is
/// distinct from package admission and never derived from it.
#[derive(Debug, Clone)]
pub struct Approval {
    plan_id: String,
    plan_digest: ArtifactSha256,
    adoption: Option<ExistingObservation>,
    manifest: Manifest,
    plan: Option<PlanRecord>,
    actor: DispatchActor,
    scope: RequestedScope,
    targets: Vec<TargetFacts>,
    signer: Option<String>,
    now: i64,
    target: String,
}

impl Approval {
    /// Capture offline gate inputs; install re-evaluates them under the lock.
    /// This is not a transport authorization or a production dispatch entry.
    pub fn new(
        gate: &GateInput<'_>,
        target: &str,
        adoption: Option<ExistingObservation>,
    ) -> Result<Self, InstallError> {
        let authorized = evaluate(gate).map_err(|e| InstallError::Approval(e.to_string()))?;
        if !authorized.targets.iter().any(|t| t == target) {
            return Err(InstallError::Approval("target is not approved".into()));
        }
        Ok(Self {
            plan_id: authorized.plan_id,
            plan_digest: authorized.plan_digest,
            adoption,
            manifest: gate.manifest.clone(),
            plan: gate.plan.cloned(),
            actor: gate.actor.clone(),
            scope: gate.scope.clone(),
            targets: gate.targets.to_vec(),
            signer: gate.signer.map(str::to_owned),
            now: gate.now,
            target: target.into(),
        })
    }

    pub fn validate(
        &self,
        operation: &TypedOperation,
        exec: &ExecContext,
    ) -> Result<(), InstallError> {
        let authorized = evaluate(&GateInput {
            manifest: &self.manifest,
            plan: self.plan.as_ref(),
            actor: &self.actor,
            scope: &self.scope,
            targets: &self.targets,
            signer: self.signer.as_deref(),
            now: self.now,
        })
        .map_err(|e| InstallError::Approval(e.to_string()))?;
        if !authorized.targets.contains(&self.target)
            || self.manifest.id != exec.job_id
            || self
                .manifest
                .execute
                .typed()
                .is_none_or(|t| &t.operation != operation)
        {
            return Err(InstallError::Approval(
                "operation, job or target differs from the approved plan".into(),
            ));
        }
        Ok(())
    }

    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }
    pub fn plan_digest(&self) -> &ArtifactSha256 {
        &self.plan_digest
    }
    pub fn adoption(&self) -> Option<&ExistingObservation> {
        self.adoption.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Installed,
    Adopted,
}

/// What the receipt records about one managed installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptBody {
    pub schema: u32,
    pub identity: ArtifactIdentity,
    pub package_id: String,
    pub artifact_sha256: ArtifactSha256,
    pub artifact_size_bytes: u64,
    pub manifest_sha256: ArtifactSha256,
    pub publisher_key_id: String,
    pub trust_set_id: String,
    pub trust_sequence: u64,
    pub binary_sha256: ArtifactSha256,
    pub binary_size_bytes: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub install_path: String,
    pub origin: Origin,
    /// Observation approved for an adoption, kept as audit evidence.
    pub adopted_from: Option<ExistingObservation>,
    pub plan_id: String,
    pub plan_digest: ArtifactSha256,
    pub job_id: String,
    pub exec_id: String,
    pub installed_at_unix: i64,
}

pub const RECEIPT_SCHEMA: u32 = 1;

fn body_digest<T: Serialize>(body: &T) -> ArtifactSha256 {
    ArtifactSha256::of(&serde_json::to_vec(body).expect("plain structs serialize"))
}

/// A receipt as stored: body plus its own digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub body: ReceiptBody,
    pub body_sha256: ArtifactSha256,
}

impl Receipt {
    pub fn new(body: ReceiptBody) -> Self {
        let body_sha256 = body_digest(&body);
        Self { body, body_sha256 }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec_pretty(self).expect("plain structs serialize");
        v.push(b'\n');
        v
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let r: Receipt = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if r.body.schema != RECEIPT_SCHEMA {
            return Err(format!("unsupported receipt schema {}", r.body.schema));
        }
        if body_digest(&r.body) != r.body_sha256 {
            return Err("receipt digest does not match its body".into());
        }
        Ok(r)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    Fresh,
    Update,
    Adopt,
}

/// Persisted transaction phases. A phase is written *before* the step it
/// names begins mutating (`Swapping`) or *after* the step completed
/// (everything else), so the journal never claims more than the disk holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Inputs verified, intent recorded; nothing on the install path touched.
    Prepared,
    /// New executable staged, flushed, verified and probed.
    Staged,
    /// Previous executable and receipt copied, flushed and verified.
    BackedUp,
    /// About to rename onto the install path (may or may not have happened).
    Swapping,
    /// Executable replaced on the install path; receipt not yet replaced.
    Swapped,
    /// New receipt in place.
    ReceiptWritten,
    /// Restoring the previous state.
    RollingBack,
    // Terminal phases.
    Committed,
    RolledBack,
    /// Failed or interrupted before the install path was touched.
    Abandoned,
    /// Unknown or failed restore: frozen, backups retained, no retries.
    RecoveryRequired,
}

impl Phase {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Committed | Phase::RolledBack | Phase::Abandoned | Phase::RecoveryRequired
        )
    }
    /// Phases from which the install path may already differ from `prev`.
    pub fn may_have_changed_install(self) -> bool {
        matches!(
            self,
            Phase::Swapping | Phase::Swapped | Phase::ReceiptWritten | Phase::RollingBack
        )
    }
}

/// The state being replaced, enough to restore it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrevState {
    pub binary_sha256: ArtifactSha256,
    pub binary_size_bytes: u64,
    pub mode: u32,
    pub had_receipt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalBody {
    pub schema: u32,
    pub txn_id: String,
    pub job_id: String,
    pub exec_id: String,
    pub kind: InstallKind,
    pub phase: Phase,
    /// How many times recovery has acted on this transaction.
    pub recovery_attempts: u32,
    pub new_receipt: ReceiptBody,
    pub prev: Option<PrevState>,
    pub note: Option<String>,
}

pub const JOURNAL_SCHEMA: u32 = 1;
/// Recovery acts on one interrupted transaction at most this many times.
pub const MAX_RECOVERY_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalFile {
    pub body: JournalBody,
    pub body_sha256: ArtifactSha256,
}

impl JournalFile {
    pub fn new(body: JournalBody) -> Self {
        let body_sha256 = body_digest(&body);
        Self { body, body_sha256 }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("plain structs serialize")
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let j: JournalFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if j.body.schema != JOURNAL_SCHEMA {
            return Err(format!("unsupported journal schema {}", j.body.schema));
        }
        if body_digest(&j.body) != j.body_sha256 {
            return Err("journal digest does not match its body".into());
        }
        Ok(j)
    }
}

/// The outcome class, for callers deciding what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A precondition failed; nothing was changed. Retry only with new input.
    Rejected,
    /// The install path was changed and has been restored.
    RolledBack,
    /// Operator action is required; the installer will not act again.
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    #[error("unsupported target for this handler: {0}")]
    Unsupported(String),
    #[error("admission refused: {0}")]
    Admission(String),
    #[error("approval refused: {0}")]
    Approval(String),
    #[error("archive refused: {0}")]
    Archive(#[from] ArchiveError),
    #[error("executable refused: {0}")]
    MachO(#[from] MachOError),
    #[error("executable reports {found:?}, expected {expected:?}")]
    VersionMismatch { expected: String, found: String },
    #[error("version probe failed: {0}")]
    Probe(String),
    #[error("install location is unsafe: {0}")]
    UnsafeRoot(String),
    #[error("timed out waiting for another installer")]
    LockTimeout,
    #[error(
        "an unmanaged executable already exists at the install path and adoption was not approved"
    )]
    UnmanagedExisting(Box<ExistingObservation>),
    #[error("existing executable does not match the approved adoption: {0}")]
    AdoptionMismatch(String),
    #[error("install path is owned by something else: {0}")]
    ForeignPath(String),
    #[error("managed install drifted from its receipt: {0}")]
    Drift(String),
    #[error("receipt/executable state is inconsistent: {0}")]
    Inconsistent(String),
    #[error("receipt is corrupt: {0}")]
    ReceiptCorrupt(String),
    #[error("journal is corrupt: {0}")]
    JournalCorrupt(String),
    #[error("install path changed concurrently: {0}")]
    Concurrent(String),
    #[error("i/o failure during {step}: {message}")]
    Io { step: String, message: String },
    #[error("install failed and the previous state was restored: {cause}")]
    RolledBack { cause: String },
    #[error("recovery required: {0}")]
    RecoveryRequired(String),
    /// Only produced by a fault-injecting test harness.
    #[error("installer interrupted")]
    Interrupted,
}

impl InstallError {
    pub fn class(&self) -> FailureClass {
        match self {
            InstallError::RolledBack { .. } => FailureClass::RolledBack,
            InstallError::RecoveryRequired(_)
            | InstallError::ReceiptCorrupt(_)
            | InstallError::JournalCorrupt(_)
            | InstallError::Inconsistent(_) => FailureClass::RecoveryRequired,
            _ => FailureClass::Rejected,
        }
    }

    /// Stable machine code for the result record.
    pub fn code(&self) -> &'static str {
        match self {
            InstallError::Unsupported(_) => "unsupported",
            InstallError::Admission(_) => "admission_refused",
            InstallError::Approval(_) => "approval_refused",
            InstallError::Archive(_) => "archive_refused",
            InstallError::MachO(_) => "executable_refused",
            InstallError::VersionMismatch { .. } => "version_mismatch",
            InstallError::Probe(_) => "probe_failed",
            InstallError::UnsafeRoot(_) => "unsafe_root",
            InstallError::LockTimeout => "lock_timeout",
            InstallError::UnmanagedExisting(_) => "unmanaged_existing",
            InstallError::AdoptionMismatch(_) => "adoption_mismatch",
            InstallError::ForeignPath(_) => "foreign_path",
            InstallError::Drift(_) => "drift",
            InstallError::Inconsistent(_) => "inconsistent_state",
            InstallError::ReceiptCorrupt(_) => "receipt_corrupt",
            InstallError::JournalCorrupt(_) => "journal_corrupt",
            InstallError::Concurrent(_) => "concurrent_modification",
            InstallError::Io { .. } => "io_error",
            InstallError::RolledBack { .. } => "rolled_back",
            InstallError::RecoveryRequired(_) => "recovery_required",
            InstallError::Interrupted => "interrupted",
        }
    }

    pub fn io(step: &str, e: impl std::fmt::Display) -> Self {
        InstallError::Io {
            step: step.into(),
            message: e.to_string(),
        }
    }
}

/// What a run observed by *executing* the installed CLI. Kept apart from
/// installation verification: `installed_verified` is about files and the
/// receipt; this is about a bounded `--version` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecutionObservation {
    /// The installed CLI was not run (e.g. a no-op).
    NotObserved,
    /// `--version` ran from the installed path and printed this line.
    VersionObserved { line: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOutcome {
    Installed,
    Updated,
    Adopted,
    NoOp,
}

/// A successful run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub outcome: InstallOutcome,
    pub receipt: Receipt,
    pub execution: ExecutionObservation,
    /// Set when an interrupted earlier transaction was resolved first.
    pub recovered: Option<String>,
}

/// Narrow, serialisable result contract for this slice. `installed_verified`
/// is the only success claim about the installation; the execution
/// observation is reported separately and is never a health verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultRecord {
    pub status: ResultStatus,
    pub code: String,
    pub detail: String,
    pub installed_verified: bool,
    pub version: Option<String>,
    pub binary_sha256: Option<String>,
    pub execution: ExecutionObservation,
    pub recovered: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    InstalledVerified,
    NoOp,
    Rejected,
    RolledBack,
    RecoveryRequired,
}

impl ResultRecord {
    pub fn from_result(r: &Result<InstallReport, InstallError>) -> Self {
        match r {
            Ok(rep) => ResultRecord {
                status: if rep.outcome == InstallOutcome::NoOp {
                    ResultStatus::NoOp
                } else {
                    ResultStatus::InstalledVerified
                },
                code: match rep.outcome {
                    InstallOutcome::Installed => "installed",
                    InstallOutcome::Updated => "updated",
                    InstallOutcome::Adopted => "adopted",
                    InstallOutcome::NoOp => "no_op",
                }
                .into(),
                detail: String::new(),
                installed_verified: true,
                version: Some(rep.receipt.body.identity.version().as_str().to_owned()),
                binary_sha256: Some(rep.receipt.body.binary_sha256.into()),
                execution: rep.execution.clone(),
                recovered: rep.recovered.clone(),
            },
            Err(e) => ResultRecord {
                status: match e.class() {
                    FailureClass::Rejected => ResultStatus::Rejected,
                    FailureClass::RolledBack => ResultStatus::RolledBack,
                    FailureClass::RecoveryRequired => ResultStatus::RecoveryRequired,
                },
                code: e.code().into(),
                detail: e.to_string(),
                installed_verified: false,
                version: None,
                binary_sha256: None,
                execution: ExecutionObservation::NotObserved,
                recovered: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_ids_are_plain() {
        let ok = ExecContext {
            job_id: "job-1".into(),
            exec_id: "e:1.2_3".into(),
        };
        assert!(ok.validate().is_ok());
        for bad in ["", "a b", "a/b", "../x", &"x".repeat(129)] {
            let c = ExecContext {
                job_id: bad.into(),
                exec_id: "e".into(),
            };
            assert!(c.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn failure_classes_are_precise() {
        assert_eq!(InstallError::LockTimeout.class(), FailureClass::Rejected);
        assert_eq!(
            InstallError::RolledBack { cause: "x".into() }.class(),
            FailureClass::RolledBack
        );
        assert_eq!(
            InstallError::ReceiptCorrupt("x".into()).class(),
            FailureClass::RecoveryRequired
        );
    }
}
