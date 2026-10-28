//! Receipt-aware, non-atomic pkg state machine. A journal records intent before
//! calling installer. Cache/journal operations use the CLI foundation's dirfd
//! primitives; no app rename, raw rollback, receipt deletion or user-data write.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kanade_shared::artifact::{
    ArtifactIntegrity, ArtifactRole, ArtifactSha256, DistroAbi, PackageFormat, TargetArch, TargetOs,
};
use kanade_shared::package_admission::{AdmissionRequest, CompatibilityPolicy, admit_package};
use kanade_shared::publisher_trust::{PublisherTrustStore, WallClock};
use kanade_shared::typed_operation::TypedOperation;
use serde::{Deserialize, Serialize};

use super::structure::{self, APP, MAX_BYTES, Payload, require};
use super::system::{InstallerExit, LaunchLease, System};
use super::{Error, Result};
use crate::native_install::fs::{Dir, FileLock, Kind, Owner, RenameMode};
use crate::native_install::model::{Approval, ExecContext, HostInfo, Roots};

const JOURNAL: &str = "client.pkg.journal.json";
const RECEIPT: &str = "client.pkg.receipt.json";
pub const LOCK: &str = "client.pkg.install.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Locked,
    BeforeInstaller,
    Reconcile,
    BeforeRollback,
}
/// Must reload CURRENT approval/revocation/expiry/target evidence on every
/// call, never return a cached Approval. It grants neither package trust nor
/// transport/replay authorization. Recovery of the prior operation needs its
/// own currently approved plan; DeployClientSpec has no implicit recovery or
/// adoption authorization field.
pub trait Authorization {
    fn current(
        &self,
        operation: &TypedOperation,
        exec: &ExecContext,
        boundary: Boundary,
    ) -> Result<Approval>;
}

pub struct Request<'a> {
    pub operation: &'a TypedOperation,
    pub manifest: &'a [u8],
    pub pkg: &'a [u8],
    pub host: &'a HostInfo,
    pub exec: &'a ExecContext,
    pub approval: &'a Approval,
    pub target: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Installing,
    InstallerReturned,
    RollingBack,
    Committed,
    RolledBack,
    Abandoned,
    RecoveryRequired,
}
impl Phase {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::RolledBack | Self::Abandoned | Self::RecoveryRequired
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    InstalledVerified,
    NoOpVerified,
}
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub version: String,
    /// Never means a GUI was launched, eligible, or connected to KLP.
    pub gui_activation: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    operation: TypedOperation,
    manifest: Vec<u8>,
    tree: ArtifactSha256,
    plan_id: String,
    plan_digest: ArtifactSha256,
    job_id: String,
    exec_id: String,
    target: String,
}
impl Record {
    fn version(&self) -> &str {
        self.operation.artifact_identity().version().as_str()
    }
    fn cache_name(&self) -> String {
        format!("{}.pkg", String::from(*self.operation.artifact_sha256()))
    }
    fn exec(&self) -> ExecContext {
        ExecContext {
            job_id: self.job_id.clone(),
            exec_id: self.exec_id.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    phase: Phase,
    next: Record,
    previous: Option<Record>,
    recovery_attempts: u32,
    installer_attempts: u32,
    recovery_plan: Option<(String, ArtifactSha256)>,
    evidence: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    body: T,
    sha256: ArtifactSha256,
}
fn encode<T: Serialize>(body: &T) -> Vec<u8> {
    let sha256 = ArtifactSha256::of(&serde_json::to_vec(body).unwrap());
    serde_json::to_vec(&Envelope { body, sha256 }).unwrap()
}
fn decode<T: for<'a> Deserialize<'a> + Serialize>(bytes: &[u8]) -> Result<T> {
    let data: Envelope<T> = serde_json::from_slice(bytes)
        .map_err(|e| Error::RecoveryRequired(format!("state decoding: {e}")))?;
    require(
        ArtifactSha256::of(&serde_json::to_vec(&data.body).unwrap()) == data.sha256,
        "state checksum",
    )
    .map_err(|e| Error::RecoveryRequired(e.to_string()))?;
    Ok(data.body)
}

pub struct Installer<'a> {
    pub roots: Roots,
    pub system: &'a dyn System,
    pub authorization: &'a dyn Authorization,
    pub trust: &'a PublisherTrustStore,
    pub clock: &'a dyn WallClock,
    pub lock_wait: Duration,
}
struct Context {
    deploy: Dir,
    cache: Dir,
    apps: Dir,
    cache_path: PathBuf,
    owner: Owner,
}

impl Installer<'_> {
    /// Explicit local restart reconciliation. Does not require an unexpired
    /// copy of the original request in order to record that current approval
    /// or trust was revoked. Never starts a new installer transaction.
    pub fn recover(&self, host: &HostInfo) -> Result<()> {
        let ctx = self.open()?;
        let _lock = FileLock::acquire(
            &ctx.deploy,
            LOCK,
            ctx.owner,
            self.lock_wait.min(Duration::from_secs(60)),
        )?
        .ok_or_else(|| Error::Deferred("managed Client installer lock timeout".into()))?;
        if ctx.deploy.stat(JOURNAL)?.is_none() {
            return Ok(());
        }
        let mut j: Journal = decode(&self.private_read(&ctx.deploy, JOURNAL, 512 * 1024)?)?;
        if j.schema != 1 || j.phase == Phase::RecoveryRequired {
            return Err(Error::RecoveryRequired(
                "unsupported or frozen transaction; evidence retained".into(),
            ));
        }
        if j.phase.terminal() {
            return Ok(());
        }
        self.reconcile(&ctx, &mut j, host)
    }
    /// Offline inspection only: independent publisher admission, fixed package
    /// structure, BOM and OS app-signature verification. Grants no deployment
    /// approval and never reads or writes a system receipt database.
    pub fn inspect(
        &self,
        operation: &TypedOperation,
        manifest: &[u8],
        pkg: &[u8],
        host: &HostInfo,
    ) -> Result<ArtifactSha256> {
        self.admit(operation, manifest, pkg, host)?;
        let ctx = self.open()?;
        let _lock = FileLock::acquire(
            &ctx.deploy,
            LOCK,
            ctx.owner,
            self.lock_wait.min(Duration::from_secs(60)),
        )?
        .ok_or_else(|| Error::Deferred("managed Client installer lock timeout".into()))?;
        self.validate_pkg(&ctx, pkg, operation.artifact_identity().version().as_str())
    }
    fn open(&self) -> Result<Context> {
        self.roots.validate_offline()?;
        self.system.check_target(&self.roots.install_dir)?;
        let owner = self.roots.owner;
        let deploy = Dir::open_absolute(&self.roots.deploy_dir, Some(0o700), owner)?;
        let st = deploy.stat_self()?;
        require(
            st.uid == owner.uid && st.gid == owner.gid && st.mode == 0o700,
            "private deploy root",
        )?;
        let apps = Dir::open_absolute(&self.roots.install_dir, Some(0o755), owner)?;
        let cache = deploy.open_dir("client-pkgs", Some(0o700), owner)?;
        let st = cache.stat_self()?;
        require(
            st.uid == owner.uid && st.gid == owner.gid && st.mode == 0o700,
            "private pkg cache",
        )?;
        Ok(Context {
            deploy,
            cache,
            apps,
            cache_path: self.roots.deploy_dir.join("client-pkgs"),
            owner,
        })
    }
    fn now(&self) -> Result<i64> {
        i64::try_from(self.clock.unix_now())
            .map_err(|_| Error::Authorization("clock overflow".into()))
    }
    fn authorize(&self, r: &Record, boundary: Boundary, pinned: bool) -> Result<Approval> {
        let approval = self
            .authorization
            .current(&r.operation, &r.exec(), boundary)?;
        approval.validate(&r.operation, &r.exec(), self.now()?)?;
        require(
            approval.target() == r.target
                && (!pinned
                    || (approval.plan_id() == r.plan_id
                        && *approval.plan_digest() == r.plan_digest)),
            "current approval differs from immutable plan/target",
        )
        .map_err(|e| Error::Authorization(e.to_string()))?;
        Ok(approval)
    }
    fn admit(
        &self,
        operation: &TypedOperation,
        manifest: &[u8],
        pkg: &[u8],
        host: &HostInfo,
    ) -> Result<()> {
        operation.validate().map_err(Error::Authorization)?;
        require(
            matches!(operation, TypedOperation::DeployClient(_)),
            "deploy_client required",
        )?;
        require(
            operation.package_id() == structure::COMPONENT,
            "typed package_id must equal actual fixed component id",
        )?;
        let id = operation.artifact_identity();
        let version = id.version().as_str();
        require(
            version.split('.').count() == 3
                && version.split('.').all(|s| {
                    !s.is_empty()
                        && s.bytes().all(|b| b.is_ascii_digit())
                        && (s == "0" || !s.starts_with('0'))
                }),
            "numeric package version required",
        )?;
        require(
            (id.role(), id.os(), id.arch(), id.distro_abi(), id.format())
                == (
                    ArtifactRole::Client,
                    TargetOs::Macos,
                    TargetArch::Aarch64,
                    DistroAbi::Darwin14,
                    PackageFormat::Pkg,
                ),
            "macOS arm64 darwin14 Client pkg required",
        )?;
        require(
            host.os_version
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|v| v >= 14),
            "macOS 14+ required",
        )?;
        require(
            pkg.len() <= MAX_BYTES && manifest.len() <= 128 * 1024,
            "input size limits",
        )?;
        let policy = CompatibilityPolicy::new(
            TargetOs::Macos,
            TargetArch::Aarch64,
            host.os_version.clone(),
            host.capabilities.clone(),
        )
        .map_err(|e| Error::Authorization(e.to_string()))?;
        let admitted = admit_package(
            self.trust,
            &AdmissionRequest {
                manifest_bytes: manifest,
                artifact_bytes: pkg,
                expected_identity: id,
                expected_integrity: &ArtifactIntegrity {
                    sha256: *operation.artifact_sha256(),
                    size_bytes: operation.artifact_size_bytes(),
                },
                expected_package_id: operation.package_id(),
                policy: &policy,
            },
        )
        .map_err(|e| Error::Authorization(e.to_string()))?;
        operation
            .check_admission(&admitted)
            .map_err(Error::Authorization)
    }
    fn private_read(&self, dir: &Dir, name: &str, max: u64) -> Result<Vec<u8>> {
        let (bytes, st) = dir.read_file(name, max)?;
        require(
            st.uid == self.roots.owner.uid
                && st.gid == self.roots.owner.gid
                && st.mode == 0o600
                && st.nlink == 1,
            "private regular state/pkg file",
        )?;
        Ok(bytes)
    }
    fn write_state<T: Serialize>(&self, ctx: &Context, name: &str, value: &T) -> Result<()> {
        // A unique O_EXCL temp avoids accepting a planted/stale temporary file.
        let tmp = format!("{}.tmp", uuid::Uuid::new_v4());
        ctx.deploy
            .write_new(&tmp, &encode(value), 0o600, ctx.owner)?;
        ctx.deploy
            .rename_to(&tmp, &ctx.deploy, name, RenameMode::Replace)?;
        ctx.deploy.fsync()?;
        Ok(())
    }
    fn journal(&self, ctx: &Context, j: &mut Journal, phase: Phase, evidence: &str) -> Result<()> {
        j.phase = phase;
        if !evidence.is_empty() {
            j.evidence.push(evidence.into());
        }
        self.write_state(ctx, JOURNAL, j)?;
        self.system.checkpoint(phase)
    }
    fn freeze<T>(&self, ctx: &Context, j: &mut Journal, why: impl std::fmt::Display) -> Result<T> {
        let why = why.to_string();
        if let Err(e) = self.journal(ctx, j, Phase::RecoveryRequired, &why) {
            return Err(Error::RecoveryRequired(format!(
                "{why}; cannot persist recovery evidence: {e}"
            )));
        }
        Err(Error::RecoveryRequired(why))
    }
    fn cache(&self, ctx: &Context, r: &Record, pkg: &[u8]) -> Result<()> {
        let name = r.cache_name();
        if ctx.cache.stat(&name)?.is_some() {
            require(
                self.private_read(&ctx.cache, &name, MAX_BYTES as u64)? == pkg,
                "cached pkg differs",
            )?;
        } else {
            ctx.cache.write_new(&name, pkg, 0o600, ctx.owner)?;
            ctx.cache.fsync()?;
        }
        Ok(())
    }
    fn cached(&self, ctx: &Context, r: &Record, host: &HostInfo) -> Result<Vec<u8>> {
        let pkg = self.private_read(&ctx.cache, &r.cache_name(), MAX_BYTES as u64)?;
        self.admit(&r.operation, &r.manifest, &pkg, host)?;
        Ok(pkg)
    }
    fn path_still_pinned(&self, ctx: &Context) -> Result<()> {
        let cache = Dir::open_absolute(&ctx.cache_path, None, ctx.owner)?;
        let apps = Dir::open_absolute(&self.roots.install_dir, None, ctx.owner)?;
        require(
            cache.stat_self()?.same_file(&ctx.cache.stat_self()?)
                && apps.stat_self()?.same_file(&ctx.apps.stat_self()?),
            "directory path changed while locked",
        )
    }
    fn validate_pkg(&self, ctx: &Context, bytes: &[u8], version: &str) -> Result<ArtifactSha256> {
        let parts = structure::xar(bytes)?;
        structure::distribution(&parts["Distribution"], version)?;
        structure::package_info(&parts[&format!("{}/PackageInfo", structure::PART)], version)?;
        let payload = structure::payload(&parts[&format!("{}/Payload", structure::PART)])?;
        structure::info_plist(
            &payload[&format!("{APP}/Contents/Info.plist")].data,
            version,
        )?;
        // Entire structure is safe before writing even to the private scratch.
        let name = format!("verify-{}", uuid::Uuid::new_v4());
        let scratch = ctx.cache.open_dir(&name, Some(0o700), ctx.owner)?;
        scratch.write_new(
            "Bom",
            &parts[&format!("{}/Bom", structure::PART)],
            0o600,
            ctx.owner,
        )?;
        for (path, entry) in &payload {
            if path.is_empty() {
                continue;
            }
            let mut parent = scratch.open_dir(APP, Some(0o755), ctx.owner)?;
            if path == APP {
                continue;
            }
            let rel = path.strip_prefix(&format!("{APP}/")).unwrap();
            let components: Vec<_> = rel.split('/').collect();
            for component in &components[..components.len() - 1] {
                parent = parent.open_dir(component, Some(0o755), ctx.owner)?;
            }
            let leaf = components.last().unwrap();
            if entry.mode & 0o170000 == 0o40000 {
                parent.open_dir(leaf, Some(0o755), ctx.owner)?;
            } else {
                parent.write_new(leaf, &entry.data, entry.mode & 0o777, ctx.owner)?;
            }
        }
        self.path_still_pinned(ctx)?;
        let path = ctx.cache_path.join(&name);
        structure::bom_payload(&self.system.bom(&path.join("Bom"))?, &payload)?;
        self.system.signature(&path.join(APP))?;
        // Re-read scratch via no-follow descriptors after OS validation.
        self.verify_tree_at(
            &scratch.open_dir(APP, None, ctx.owner)?,
            &path.join(APP),
            &payload,
            ctx.owner,
        )?;
        tree_digest(&payload)
    }
    fn verify_tree_at(&self, app: &Dir, path: &Path, want: &Payload, owner: Owner) -> Result<()> {
        fn walk(
            dir: &Dir,
            path: &Path,
            prefix: &str,
            want: &Payload,
            owner: Owner,
            seen: &mut BTreeMap<String, u32>,
        ) -> Result<()> {
            let st = dir.stat_self()?;
            require(
                st.uid == owner.uid && st.gid == owner.gid && st.mode == 0o755,
                "bundle directory owner/mode",
            )?;
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let leaf = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| Error::Invalid("bundle filename encoding".into()))?;
                let name = format!("{prefix}/{leaf}");
                let expected = want
                    .get(&name)
                    .ok_or_else(|| Error::Invalid("extra bundle path".into()))?;
                let st = dir
                    .stat(&leaf)?
                    .ok_or_else(|| Error::Invalid("bundle path vanished".into()))?;
                require(
                    st.uid == owner.uid && st.gid == owner.gid && st.mode == expected.mode & 0o7777,
                    "bundle owner/mode",
                )?;
                if expected.mode & 0o170000 == 0o40000 {
                    require(st.kind == Kind::Directory, "bundle directory replaced")?;
                    walk(
                        &dir.open_dir(&leaf, None, owner)?,
                        &path.join(&leaf),
                        &name,
                        want,
                        owner,
                        seen,
                    )?;
                } else {
                    let (bytes, opened) = dir.read_file(&leaf, MAX_BYTES as u64)?;
                    require(
                        st.same_file(&opened) && opened.nlink == 1 && bytes == expected.data,
                        "bundle bytes/inode/links",
                    )?;
                }
                require(
                    seen.insert(name, expected.mode).is_none(),
                    "duplicate bundle path",
                )?;
            }
            Ok(())
        }
        let mut seen = BTreeMap::from([(String::new(), 0o40755), (APP.into(), 0o40755)]);
        walk(app, path, APP, want, owner, &mut seen)?;
        require(seen == structure::expected_modes(), "missing bundle paths")
    }
    fn installed(&self, ctx: &Context, r: &Record, host: &HostInfo) -> Result<()> {
        let pkg = self.cached(ctx, r, host)?;
        let parts = structure::xar(&pkg)?;
        let payload = structure::payload(&parts[&format!("{}/Payload", structure::PART)])?;
        require(tree_digest(&payload)? == r.tree, "managed tree receipt")?;
        let receipt = self
            .system
            .receipt()?
            .ok_or_else(|| Error::Invalid("OS receipt missing".into()))?;
        receipt.verify(r.version())?;
        let app = ctx.apps.open_dir(APP, None, ctx.owner)?;
        self.verify_tree_at(&app, &self.roots.install_dir.join(APP), &payload, ctx.owner)?;
        self.system.signature(&self.roots.install_dir.join(APP))?;
        require(
            ctx.apps
                .stat(APP)?
                .is_some_and(|st| app.stat_self().is_ok_and(|opened| st.same_file(&opened))),
            "installed app directory changed during validation",
        )?;
        self.path_still_pinned(ctx)
    }
    fn absent(&self, ctx: &Context) -> Result<()> {
        if ctx.apps.stat(APP)?.is_some() || self.system.receipt()?.is_some() {
            Err(Error::Unmanaged)
        } else {
            Ok(())
        }
    }
    fn previous_matches(&self, ctx: &Context, j: &Journal, host: &HostInfo) -> Result<()> {
        if let Some(prev) = &j.previous {
            self.installed(ctx, prev, host)
        } else {
            self.absent(ctx)
        }
    }
    pub fn install(&self, req: &Request<'_>) -> Result<Report> {
        req.exec.validate()?;
        req.approval
            .validate(req.operation, req.exec, self.now()?)?;
        require(
            req.approval.target() == req.target,
            "approved target mismatch",
        )?;
        self.admit(req.operation, req.manifest, req.pkg, req.host)?;
        let ctx = self.open()?;
        let _lock = FileLock::acquire(
            &ctx.deploy,
            LOCK,
            ctx.owner,
            self.lock_wait.min(Duration::from_secs(60)),
        )?
        .ok_or_else(|| Error::Deferred("managed Client installer lock timeout".into()))?;
        let mut next = Record {
            operation: req.operation.clone(),
            manifest: req.manifest.to_vec(),
            tree: ArtifactSha256::of(b"unvalidated"),
            plan_id: req.approval.plan_id().into(),
            plan_digest: *req.approval.plan_digest(),
            job_id: req.exec.job_id.clone(),
            exec_id: req.exec.exec_id.clone(),
            target: req.target.into(),
        };
        self.authorize(&next, Boundary::Locked, true)?;
        let mut attempts = 0;
        if ctx.deploy.stat(JOURNAL)?.is_some() {
            let mut j: Journal = decode(&self.private_read(&ctx.deploy, JOURNAL, 512 * 1024)?)?;
            require(j.schema == 1, "journal schema")
                .map_err(|e| Error::RecoveryRequired(e.to_string()))?;
            if j.phase == Phase::RecoveryRequired {
                return Err(Error::RecoveryRequired(
                    "transaction frozen; evidence retained".into(),
                ));
            }
            if !j.phase.terminal() {
                self.reconcile(&ctx, &mut j, req.host)?;
            }
            if j.next.operation == next.operation
                && j.next.exec_id == next.exec_id
                && j.next.job_id == next.job_id
                && j.next.plan_digest == next.plan_digest
            {
                if j.phase == Phase::RolledBack {
                    return Err(Error::RolledBack);
                }
                attempts = j.installer_attempts;
                require(
                    attempts < 3,
                    "installer attempt budget exhausted for this approved execution",
                )?;
            }
        }
        next.tree = self.validate_pkg(&ctx, req.pkg, next.version())?;
        self.cache(&ctx, &next, req.pkg)?;
        let previous = if ctx.deploy.stat(RECEIPT)?.is_some() {
            let prev: Record = decode(&self.private_read(&ctx.deploy, RECEIPT, 256 * 1024)?)?;
            self.installed(&ctx, &prev, req.host)
                .map_err(|e| Error::RecoveryRequired(format!("managed receipt divergence: {e}")))?;
            if prev.operation == next.operation && prev.tree == next.tree {
                self.authorize(&next, Boundary::BeforeInstaller, true)?;
                self.admit(req.operation, req.manifest, req.pkg, req.host)?;
                return Ok(report(Outcome::NoOpVerified, next.version()));
            }
            // Re-validate and retain the previous package before attempting an update.
            let pkg = self.cached(&ctx, &prev, req.host)?;
            require(
                self.validate_pkg(&ctx, &pkg, prev.version())? == prev.tree,
                "prior pkg structure changed",
            )?;
            Some(prev)
        } else {
            self.absent(&ctx)?;
            None
        };
        let lease = self.system.coordinate()?;
        lease.recheck()?;
        let mut j = Journal {
            schema: 1,
            phase: Phase::Prepared,
            next,
            previous,
            recovery_attempts: 0,
            installer_attempts: attempts + 1,
            recovery_plan: None,
            evidence: vec![],
        };
        self.journal(&ctx, &mut j, Phase::Prepared, "")?;
        // This is a mutation intent, NOT a claim of OS transaction atomicity.
        self.journal(
            &ctx,
            &mut j,
            Phase::Installing,
            "installer invocation pending; OS transaction may outlive caller",
        )?;
        self.previous_matches(&ctx, &j, req.host)?;
        self.boundary(
            &ctx,
            &j.next,
            req.host,
            Boundary::BeforeInstaller,
            lease.as_ref(),
            true,
        )?;
        self.previous_matches(&ctx, &j, req.host)?;
        self.authorize(&j.next, Boundary::BeforeInstaller, true)?;
        self.cached(&ctx, &j.next, req.host)?;
        self.path_still_pinned(&ctx)?;
        lease.recheck()?;
        let exit = match self
            .system
            .install(&ctx.cache_path.join(j.next.cache_name()))
        {
            Ok(exit) => exit,
            Err(e) => return self.freeze(&ctx, &mut j, e),
        };
        match exit {
            InstallerExit::Uncertain(why) => self.freeze(&ctx, &mut j, why),
            InstallerExit::Succeeded => {
                self.journal(
                    &ctx,
                    &mut j,
                    Phase::InstallerReturned,
                    "installer exited successfully; receipt/files still unverified",
                )?;
                match self.installed(&ctx, &j.next, req.host) {
                    Ok(()) => {
                        if let Err(e) = self.authorize(&j.next, Boundary::Reconcile, true) {
                            return self.freeze(&ctx, &mut j, e);
                        }
                        if let Err(e) = self.write_state(&ctx, RECEIPT, &j.next) {
                            return self.freeze(&ctx, &mut j, e);
                        }
                        self.journal(&ctx, &mut j, Phase::Committed, "")?;
                        Ok(report(Outcome::InstalledVerified, j.next.version()))
                    }
                    Err(e) => self.rollback(&ctx, &mut j, req.host, lease.as_ref(), &e.to_string()),
                }
            }
            InstallerExit::FailedQuiescent(why) => {
                self.rollback(&ctx, &mut j, req.host, lease.as_ref(), &why)
            }
        }
    }
    fn boundary(
        &self,
        ctx: &Context,
        r: &Record,
        host: &HostInfo,
        boundary: Boundary,
        lease: &dyn LaunchLease,
        pinned: bool,
    ) -> Result<Approval> {
        lease.recheck()?;
        self.path_still_pinned(ctx)?;
        let pkg = self.cached(ctx, r, host)?;
        require(
            self.validate_pkg(ctx, &pkg, r.version())? == r.tree,
            "staged pkg content changed",
        )?;
        let approval = self.authorize(r, boundary, pinned)?;
        // Trust can change during OS validation; admit again at the final edge.
        self.cached(ctx, r, host)?;
        lease.recheck()?;
        Ok(approval)
    }
    fn rollback(
        &self,
        ctx: &Context,
        j: &mut Journal,
        host: &HostInfo,
        lease: &dyn LaunchLease,
        why: &str,
    ) -> Result<Report> {
        // Only a known old or fully validated new state is safe to replace.
        // Partial files/receipt divergence could be external ownership changes.
        if self.previous_matches(ctx, j, host).is_ok() {
            self.journal(ctx, j, Phase::Abandoned, why)?;
            return Err(Error::Invalid(format!(
                "installer failed without observed changes: {why}"
            )));
        }
        if let Err(e) = self.installed(ctx, &j.next, host) {
            return self.freeze(ctx, j, format!("ambiguous failed install: {why}; {e}"));
        }
        let Some(prev) = j.previous.clone() else {
            return self.freeze(
                ctx,
                j,
                "fresh install failed; no prior pkg, never delete OS receipts/app to undo",
            );
        };
        if j.recovery_attempts >= 1 {
            return self.freeze(ctx, j, "rollback attempt limit");
        }
        if let Err(e) = self.authorize(&j.next, Boundary::BeforeRollback, true) {
            return self.freeze(ctx, j, e);
        }
        let approval = match self.boundary(ctx, &prev, host, Boundary::BeforeRollback, lease, false)
        {
            Ok(a) => a,
            Err(e) => return self.freeze(ctx, j, e),
        };
        j.recovery_plan = Some((approval.plan_id().into(), *approval.plan_digest()));
        j.recovery_attempts += 1;
        self.journal(ctx, j, Phase::RollingBack, why)?;
        // Recheck CURRENT recovery approval and trust after durable intent.
        let current = match self.boundary(ctx, &prev, host, Boundary::BeforeRollback, lease, false)
        {
            Ok(a) => a,
            Err(e) => return self.freeze(ctx, j, e),
        };
        if j.recovery_plan.as_ref() != Some(&(current.plan_id().into(), *current.plan_digest())) {
            return self.freeze(ctx, j, "recovery approval changed");
        }
        let final_check = (|| {
            self.installed(ctx, &j.next, host)?;
            self.authorize(&j.next, Boundary::BeforeRollback, true)?;
            let current = self.authorize(&prev, Boundary::BeforeRollback, false)?;
            require(
                j.recovery_plan.as_ref()
                    == Some(&(current.plan_id().into(), *current.plan_digest())),
                "recovery approval changed at invocation",
            )?;
            self.cached(ctx, &prev, host)?;
            self.path_still_pinned(ctx)?;
            lease.recheck()
        })();
        if let Err(e) = final_check {
            return self.freeze(ctx, j, e);
        }
        match self.system.install(&ctx.cache_path.join(prev.cache_name())) {
            Ok(InstallerExit::Succeeded) => {}
            other => return self.freeze(ctx, j, format!("native rollback failed: {other:?}")),
        }
        if let Err(e) = self.installed(ctx, &prev, host) {
            return self.freeze(ctx, j, format!("rollback verification failed: {e}"));
        }
        if let Err(e) = self.write_state(ctx, RECEIPT, &prev) {
            return self.freeze(ctx, j, e);
        }
        self.journal(
            ctx,
            j,
            Phase::RolledBack,
            "prior package restored through OS installer; GUI not restarted",
        )?;
        Err(Error::RolledBack)
    }
    fn reconcile(&self, ctx: &Context, j: &mut Journal, host: &HostInfo) -> Result<()> {
        if let Err(e) = self.authorize(&j.next, Boundary::Reconcile, true) {
            return self.freeze(ctx, j, e);
        }
        if let Err(e) = self.cached(ctx, &j.next, host) {
            return self.freeze(ctx, j, e);
        }
        if j.phase == Phase::Prepared {
            if let Err(e) = self.previous_matches(ctx, j, host) {
                return self.freeze(ctx, j, e);
            }
            self.journal(
                ctx,
                j,
                Phase::Abandoned,
                "interrupted before installer intent; previous state verified",
            )?;
            return Ok(());
        }
        if let Err(e) = self.system.recovery_quiescent() {
            return self.freeze(ctx, j, e);
        }
        if self.installed(ctx, &j.next, host).is_ok() && j.phase != Phase::RollingBack {
            if let Err(e) = self.authorize(&j.next, Boundary::Reconcile, true) {
                return self.freeze(ctx, j, e);
            }
            if let Err(e) = self.cached(ctx, &j.next, host) {
                return self.freeze(ctx, j, e);
            }
            if let Err(e) = self.write_state(ctx, RECEIPT, &j.next) {
                return self.freeze(ctx, j, e);
            }
            self.journal(
                ctx,
                j,
                Phase::Committed,
                "reconciled OS receipt and full bundle after interruption; GUI unverified",
            )?;
            return Ok(());
        }
        if self.previous_matches(ctx, j, host).is_ok() {
            if let Some(prev) = &j.previous {
                self.write_state(ctx, RECEIPT, prev)?;
            }
            let phase = if j.phase == Phase::RollingBack {
                Phase::RolledBack
            } else {
                Phase::Abandoned
            };
            self.journal(
                ctx,
                j,
                phase,
                "reconciled previous state; no installer retry during recovery",
            )?;
            return Ok(());
        }
        self.freeze(ctx,j,"interrupted installer/rollback left ambiguous receipt/files; no overlapping OS transaction or automatic retry")
    }
}
fn tree_digest(payload: &Payload) -> Result<ArtifactSha256> {
    let body: BTreeMap<_, _> = payload
        .iter()
        .map(|(n, e)| (n, (e.mode, ArtifactSha256::of(&e.data))))
        .collect();
    Ok(ArtifactSha256::of(
        &serde_json::to_vec(&body).map_err(|e| Error::Invalid(e.to_string()))?,
    ))
}
fn report(outcome: Outcome, version: &str) -> Report {
    Report {
        outcome,
        version: version.into(),
        gui_activation: "unsupported_not_attempted",
    }
}
