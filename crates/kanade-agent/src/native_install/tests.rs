//! Transaction tests. Every test uses temporary roots owned by the invoking
//! user and a fake `--version` adapter; the real filesystem primitives
//! (`openat`, flagged renames, `flock`) run against those roots.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanade_shared::artifact::{
    ArtifactIdentity, ArtifactIntegrity, ArtifactRole, ArtifactSha256, DistroAbi, PackageFormat,
    TargetArch, TargetOs,
};
use kanade_shared::deploy_plan::*;
use kanade_shared::manifest::Manifest;
use kanade_shared::package_manifest::{ManifestClaims, PublisherSigningKey, sign_manifest};
use kanade_shared::publisher_trust::{
    PublisherEntry, PublisherTrustStore, TrustRoot, TrustStateClaims, WallClock, sign_trust_state,
};
use kanade_shared::typed_operation::{DeployCliSpec, TypedOperation};
use kanade_shared::wire::{
    DeployCapabilities, DeployHandler, OPERATION_DEPLOY_CLI, PROTOCOL_V2, TYPED_DEPLOY_CONTRACT_V1,
};

use super::admit::InstallInputs;
use super::archive;
use super::archive::test_support as tarball;
use super::fs::{Dir, FileLock, Owner};
use super::macho::test_support as macho_fx;
use super::model::*;
use super::probe::VersionProbe;
use super::txn::*;

const T0: u64 = 1_800_000_000;
const DAY: u64 = 86_400;
const ENOSPC: i32 = 28;

struct Clock;
impl WallClock for Clock {
    fn unix_now(&self) -> u64 {
        T0
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

fn key(seed: u8) -> PublisherSigningKey {
    PublisherSigningKey::from_seed([seed; 32])
}

// ---- fake `--version` adapter ------------------------------------------------

type Pred = Box<dyn Fn(&Path) -> bool + Send + Sync>;

#[derive(Default)]
struct FakeProbe {
    calls: Mutex<Vec<PathBuf>>,
    fail_when: Mutex<Option<Pred>>,
}

impl FakeProbe {
    fn calls(&self) -> Vec<PathBuf> {
        self.calls.lock().unwrap().clone()
    }
    fn fail_when(&self, p: impl Fn(&Path) -> bool + Send + Sync + 'static) {
        *self.fail_when.lock().unwrap() = Some(Box::new(p));
    }
}

struct Shared(Arc<FakeProbe>);

impl VersionProbe for Shared {
    fn version_line(&self, executable: &Path) -> Result<String, String> {
        self.0.calls.lock().unwrap().push(executable.to_owned());
        if let Some(p) = self.0.fail_when.lock().unwrap().as_ref() {
            if p(executable) {
                return Err("fake probe failure".into());
            }
        }
        let bytes = std::fs::read(executable).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&bytes);
        let v = text
            .split("VERSION=")
            .nth(1)
            .and_then(|r| r.split(';').next())
            .ok_or("no version marker")?;
        Ok(format!("kanade {v}"))
    }
}

struct Hook(Box<dyn Fn(Point) -> Fault + Send + Sync>);
impl Faults for Hook {
    fn at(&self, p: Point) -> Fault {
        (self.0)(p)
    }
}

// ---- fixture ---------------------------------------------------------------------

struct Fx {
    _t: tempfile::TempDir,
    base: PathBuf,
    roots: Roots,
    store: PublisherTrustStore,
    publisher: PublisherSigningKey,
    root: PublisherSigningKey,
    host: HostInfo,
    probe: Arc<FakeProbe>,
}

impl Fx {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = PublisherTrustStore::open(base.join("trust"), Arc::new(Clock));
        let root = key(1);
        let publisher = key(2);
        store
            .provision_root(&TrustRoot::from_signing_key(&root).to_root_json())
            .unwrap();
        let fx = Fx {
            roots: Roots {
                install_dir: base.join("usr-local-bin"),
                deploy_dir: base.join("deploy"),
                owner: Owner::current(),
            },
            _t: t,
            base,
            store,
            publisher,
            root,
            host: HostInfo {
                os_version: "14.5".into(),
                capabilities: vec![],
            },
            probe: Arc::new(FakeProbe::default()),
        };
        fx.trust(&[&fx.publisher], &[], 1);
        fx
    }

    fn trust(&self, active: &[&PublisherSigningKey], revoked: &[&PublisherSigningKey], seq: u64) {
        let mut pubs: Vec<_> = active
            .iter()
            .map(|k| PublisherEntry::new(k.trusted_key(), T0 - DAY, T0 + 20 * DAY).unwrap())
            .collect();
        pubs.sort_by(|a, b| a.key().key_id().as_str().cmp(b.key().key_id().as_str()));
        let mut rev: Vec<String> = revoked
            .iter()
            .map(|k| k.trusted_key().key_id().as_str().to_owned())
            .collect();
        rev.sort();
        let c = TrustStateClaims::new("fleet".into(), seq, T0, T0 + 30 * DAY, pubs, rev).unwrap();
        self.store.apply(&sign_trust_state(&c, &self.root)).unwrap();
    }

    fn installer(&self) -> Installer {
        Installer::new(self.roots.clone(), Box::new(Shared(self.probe.clone())))
            .with_clock(Box::new(Clock))
            .with_lock_wait(Duration::from_secs(10))
    }

    fn installer_with(&self, f: impl Fn(Point) -> Fault + Send + Sync + 'static) -> Installer {
        self.installer().with_faults(Box::new(Hook(Box::new(f))))
    }

    fn run(&self, inst: &Installer, pkg: &Pkg) -> Result<InstallReport, InstallError> {
        inst.install(
            &self.store,
            &pkg.inputs(&self.host),
            &pkg.approval(),
            &pkg.exec(),
        )
    }

    fn ok(&self, pkg: &Pkg) -> InstallReport {
        self.run(&self.installer(), pkg).unwrap()
    }

    fn exe_path(&self) -> PathBuf {
        self.roots.install_dir.join(INSTALL_NAME)
    }
    fn exe(&self) -> Option<Vec<u8>> {
        std::fs::read(self.exe_path()).ok()
    }
    fn receipt_raw(&self) -> Option<Vec<u8>> {
        std::fs::read(self.roots.deploy_dir.join(RECEIPT_NAME)).ok()
    }
    fn receipt(&self) -> Option<Receipt> {
        self.receipt_raw().map(|r| Receipt::parse(&r).unwrap())
    }
    fn journal(&self) -> Option<JournalBody> {
        let raw = std::fs::read(self.roots.deploy_dir.join(JOURNAL_NAME)).ok()?;
        Some(JournalFile::parse(&raw).unwrap().body)
    }
    fn staging_entries(&self) -> Vec<String> {
        names(&self.roots.install_dir.join(STAGING_DIR))
    }
    fn backup(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.roots.deploy_dir.join(BACKUP_DIR).join(name)).ok()
    }
    /// Everything on disk the installer owns, for "nothing changed" checks.
    fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for dir in [&self.roots.install_dir, &self.roots.deploy_dir] {
            walk(dir, &mut out);
        }
        out.sort();
        out
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn walk(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let e = e.unwrap();
        let p = e.path();
        let md = std::fs::symlink_metadata(&p).unwrap();
        if md.is_dir() {
            walk(&p, out);
        } else if md.is_file() && !p.ends_with(JOURNAL_NAME) {
            out.push((p.to_string_lossy().into_owned(), std::fs::read(&p).unwrap()));
        }
    }
}

// ---- package builder -------------------------------------------------------------------

struct Pkg {
    version: String,
    binary: Vec<u8>,
    archive: Vec<u8>,
    manifest: Vec<u8>,
    op: TypedOperation,
    adoption: Option<ExistingObservation>,
    approved_op: TypedOperation,
}

struct B<'a> {
    fx: &'a Fx,
    version: &'a str,
    variant: &'a str,
    adopt: Option<ExistingObservation>,
    archive: Option<Vec<u8>>,
    manifest_version: Option<&'a str>,
    signer: Option<PublisherSigningKey>,
    package_id: &'a str,
}

fn identity(version: &str) -> ArtifactIdentity {
    ArtifactIdentity::new(
        ArtifactRole::Cli,
        version.parse().unwrap(),
        TargetOs::Macos,
        TargetArch::Aarch64,
        DistroAbi::Darwin,
        PackageFormat::TarGz,
    )
    .unwrap()
}

fn binary(version: &str, variant: &str) -> Vec<u8> {
    macho_fx::arm64(format!("VERSION={version};{variant}").as_bytes())
}

impl<'a> B<'a> {
    fn new(fx: &'a Fx, version: &'a str) -> Self {
        B {
            fx,
            version,
            variant: "",
            adopt: None,
            archive: None,
            manifest_version: None,
            signer: None,
            package_id: "com.kanade.cli",
        }
    }
    fn variant(mut self, v: &'a str) -> Self {
        self.variant = v;
        self
    }
    fn adopt(mut self, o: ExistingObservation) -> Self {
        self.adopt = Some(o);
        self
    }
    fn archive(mut self, a: Vec<u8>) -> Self {
        self.archive = Some(a);
        self
    }
    fn manifest_version(mut self, v: &'a str) -> Self {
        self.manifest_version = Some(v);
        self
    }
    fn signer(mut self, k: PublisherSigningKey) -> Self {
        self.signer = Some(k);
        self
    }
    fn build(self) -> Pkg {
        let bin = binary(self.version, self.variant);
        let archive = self.archive.unwrap_or_else(|| tarball::good(&bin));
        let signer = self.signer.as_ref().unwrap_or(&self.fx.publisher);
        let claims = ManifestClaims::new(
            identity(self.manifest_version.unwrap_or(self.version)),
            self.package_id.into(),
            "14.0".into(),
            vec![],
            ArtifactIntegrity::of(&archive),
        )
        .unwrap();
        let manifest = sign_manifest(claims, signer).to_canonical_bytes();
        let id = identity(self.version);
        let op = TypedOperation::DeployCli(DeployCliSpec {
            package_object_key: id.app_package_key().unwrap(),
            artifact_identity: id,
            artifact_sha256: ArtifactSha256::of(&archive),
            artifact_size_bytes: archive.len() as u64,
            package_id: "com.kanade.cli".into(),
            manifest_sha256: ArtifactSha256::of(&manifest),
            adopt_existing_cli: self.adopt.is_some(),
            adoption_observation_sha256: self.adopt.as_ref().map(ExistingObservation::digest),
        });
        Pkg {
            version: self.version.into(),
            binary: bin,
            archive,
            manifest,
            approved_op: op.clone(),
            op,
            adoption: self.adopt,
        }
    }
}

impl Pkg {
    fn new(fx: &Fx, version: &str) -> Pkg {
        B::new(fx, version).build()
    }
    fn inputs<'a>(&'a self, host: &'a HostInfo) -> InstallInputs<'a> {
        InstallInputs {
            operation: &self.op,
            manifest_bytes: &self.manifest,
            artifact_bytes: &self.archive,
            host,
        }
    }
    fn approval(&self) -> Approval {
        self.approval_with("host", |_, _, _, _| {}).unwrap()
    }
    fn approval_with(
        &self,
        target: &str,
        change: impl FnOnce(&mut PlanRecord, &mut Manifest, &mut RequestedScope, &mut i64),
    ) -> Result<Approval, InstallError> {
        let mut manifest: Manifest = serde_json::from_value(serde_json::json!({
            "id":"job-1", "version":"1.0.0", "require_approval":true,
            "execute":{"kind":"typed_operation","payload_version":1,"timeout":"1m","run_as":"system","operation":self.approved_op},
            "deploy_authorization":{"plan_id":"plan-1","plan_digest":String::from(ArtifactSha256::of(b"placeholder"))}
        })).unwrap();
        let TypedOperation::DeployCli(spec) = &self.approved_op else {
            unreachable!()
        };
        let content = PlanContent {
            record_version: PLAN_RECORD_VERSION,
            plan_id: "plan-1".into(),
            job_id: manifest.id.clone(),
            job_manifest_digest: job_manifest_digest(&manifest).unwrap(),
            role: ArtifactRole::Cli,
            operation: self.approved_op.clone(),
            artifact: PlanArtifact {
                identity: spec.artifact_identity.clone(),
                integrity: ArtifactIntegrity {
                    sha256: spec.artifact_sha256,
                    size_bytes: spec.artifact_size_bytes,
                },
                manifest_sha256: spec.manifest_sha256,
            },
            targets: vec!["host".into()],
            not_before: T0 as i64 - 1,
            expires_at: T0 as i64 + 300,
            execution_policy: ExecutionPolicy::SingleUsePerTarget,
        };
        manifest.deploy_authorization.as_mut().unwrap().plan_digest = content.digest();
        let mut plan = PlanRecord {
            stored_digest: content.digest(),
            content,
            state: PlanState::Approved,
            approved_by: Some("operator".into()),
            revoked_by: None,
            applied_targets: vec![],
        };
        let facts = TargetFacts {
            pc_id: "host".into(),
            registered: true,
            current_agent_version: Some("1.0.0".into()),
            current_os_family: Some("macos".into()),
            current_agent_arch: Some("aarch64".into()),
            evidence: CapabilityEvidence {
                capabilities: Some(DeployCapabilities::from_handlers(&[DeployHandler::new(
                    TYPED_DEPLOY_CONTRACT_V1,
                    OPERATION_DEPLOY_CLI,
                    TargetOs::Macos,
                    TargetArch::Aarch64,
                )])),
                agent_version: Some("1.0.0".into()),
                protocols: Some(vec![PROTOCOL_V2.into()]),
                keys: Some(vec!["offline-signer".into()]),
                os_family: Some("macos".into()),
                agent_arch: Some("aarch64".into()),
                reported_at_ms: Some(T0 as i64 * 1000),
                received_at: Some(T0 as i64),
            },
        };
        let mut scope = RequestedScope {
            pcs: vec!["host".into()],
            ..Default::default()
        };
        let mut now = T0 as i64;
        change(&mut plan, &mut manifest, &mut scope, &mut now);
        Approval::new(
            &GateInput {
                manifest: &manifest,
                plan: Some(&plan),
                actor: &DispatchActor::Operator(Principal {
                    subject: "operator".into(),
                    operator: true,
                    exec_feature: true,
                }),
                scope: &scope,
                targets: &[facts],
                signer: Some("offline-signer"),
                now,
            },
            target,
            self.adoption.clone(),
        )
    }
    fn exec(&self) -> ExecContext {
        ExecContext {
            job_id: "job-1".into(),
            exec_id: format!("exec-{}", self.version),
        }
    }
}

/// Observation of a hand-placed executable, as an approved plan would record it.
fn observe(bytes: &[u8], version: &str) -> ExistingObservation {
    let o = Owner::current();
    ExistingObservation {
        sha256: ArtifactSha256::of(bytes),
        version: version.into(),
        uid: o.uid,
        gid: o.gid,
        mode: 0o755,
    }
}

fn place_unmanaged(fx: &Fx, bytes: &[u8]) {
    std::fs::create_dir_all(&fx.roots.install_dir).unwrap();
    std::fs::set_permissions(
        &fx.roots.install_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Temporary directories may inherit a different group on macOS. Model
    // the operator's executable with the exact principal the receipt expects.
    let dir = Dir::open_absolute(&fx.roots.install_dir, None, Owner::current()).unwrap();
    dir.write_new(INSTALL_NAME, bytes, 0o755, Owner::current())
        .unwrap();
}

fn calls_touching(fx: &Fx) -> Vec<String> {
    fx.probe
        .calls()
        .iter()
        .map(|p| {
            p.strip_prefix(&fx.roots.install_dir)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

// ======================= happy paths =========================================

#[test]
fn fresh_install_places_verified_cli_receipt_and_journal() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    let rep = fx.ok(&pkg);
    assert_eq!(rep.outcome, InstallOutcome::Installed);
    assert_eq!(fx.exe().unwrap(), pkg.binary);
    let md = std::fs::metadata(fx.exe_path()).unwrap();
    assert_eq!(md.mode() & 0o7777, 0o755);
    assert_eq!(md.nlink(), 1);
    let r = fx.receipt().unwrap();
    assert_eq!(r, rep.receipt);
    assert_eq!(r.body.binary_sha256, ArtifactSha256::of(&pkg.binary));
    assert_eq!(r.body.origin, Origin::Installed);
    assert_eq!(r.body.identity.version().as_str(), "1.0.0");
    assert_eq!(r.body.plan_id, "plan-1");
    assert_eq!(
        std::fs::metadata(fx.roots.deploy_dir.join(RECEIPT_NAME))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(fx.journal().unwrap().phase, Phase::Committed);
    assert!(fx.staging_entries().is_empty());
    assert_eq!(
        rep.execution,
        ExecutionObservation::VersionObserved {
            line: "kanade 1.0.0".into()
        }
    );
    // Only the staged copy and the installed path were ever run.
    assert_eq!(
        calls_touching(&fx),
        vec![format!("{STAGING_DIR}/kanade.new"), "kanade".to_string()]
    );
    let rec = ResultRecord::from_result(&Ok(rep));
    assert_eq!(rec.status, ResultStatus::InstalledVerified);
    assert!(rec.installed_verified);
}

#[test]
fn update_replaces_atomically_and_retains_old_executable_and_receipt() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let old_receipt = fx.receipt_raw().unwrap();
    // A process that already has the old executable open keeps its inode.
    let held = std::fs::File::open(fx.exe_path()).unwrap();
    let old_ino = held.metadata().unwrap().ino();

    let v2 = Pkg::new(&fx, "1.1.0");
    let rep = fx.ok(&v2);
    assert_eq!(rep.outcome, InstallOutcome::Updated);
    assert_eq!(fx.exe().unwrap(), v2.binary);
    assert_ne!(std::fs::metadata(fx.exe_path()).unwrap().ino(), old_ino);
    let mut old = Vec::new();
    std::io::Read::read_to_end(&mut &held, &mut old).unwrap();
    assert_eq!(old, v1.binary, "running process keeps the old inode");
    assert_eq!(fx.backup("kanade.prev").unwrap(), v1.binary);
    assert_eq!(fx.backup("cli.receipt.prev.json").unwrap(), old_receipt);
    assert_eq!(
        fx.receipt().unwrap().body.identity.version().as_str(),
        "1.1.0"
    );
    assert!(fx.staging_entries().is_empty());
}

#[test]
fn same_identity_and_verified_content_is_a_noop() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    fx.ok(&pkg);
    let ino = std::fs::metadata(fx.exe_path()).unwrap().ino();
    let journal = fx.journal().unwrap();
    let calls = fx.probe.calls().len();
    let snap = fx.snapshot();

    let rep = fx.ok(&pkg);
    assert_eq!(rep.outcome, InstallOutcome::NoOp);
    assert_eq!(rep.execution, ExecutionObservation::NotObserved);
    assert_eq!(std::fs::metadata(fx.exe_path()).unwrap().ino(), ino);
    assert_eq!(fx.journal().unwrap(), journal, "no new transaction");
    assert_eq!(fx.probe.calls().len(), calls, "nothing was executed");
    assert_eq!(fx.snapshot(), snap);
    let rec = ResultRecord::from_result(&Ok(rep));
    assert_eq!(rec.status, ResultStatus::NoOp);
    assert!(rec.installed_verified);
    assert_eq!(rec.execution, ExecutionObservation::NotObserved);
}

#[test]
fn explicit_adoption_of_an_unmanaged_cli_is_bound_and_retained() {
    let fx = Fx::new();
    let manual = binary("0.9.0", "manual");
    place_unmanaged(&fx, &manual);
    let pkg = B::new(&fx, "1.0.0")
        .adopt(observe(&manual, "0.9.0"))
        .build();
    let rep = fx.ok(&pkg);
    assert_eq!(rep.outcome, InstallOutcome::Adopted);
    assert_eq!(fx.exe().unwrap(), pkg.binary);
    let r = fx.receipt().unwrap();
    assert_eq!(r.body.origin, Origin::Adopted);
    assert_eq!(r.body.adopted_from.as_ref().unwrap().version, "0.9.0");
    assert_eq!(fx.backup("kanade.prev").unwrap(), manual);
    assert!(fx.backup("cli.receipt.prev.json").is_none());
    // The unmanaged executable was run only after approval, and only by path.
    assert_eq!(
        calls_touching(&fx),
        vec!["kanade", &format!("{STAGING_DIR}/kanade.new"), "kanade"]
    );
}

// ======================= adoption / ownership refusals ==========================

#[test]
fn unmanaged_existing_cli_is_refused_without_adoption_and_never_executed() {
    let fx = Fx::new();
    let manual = binary("0.9.0", "manual");
    place_unmanaged(&fx, &manual);
    let before = fx.snapshot();
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.0.0"))
        .unwrap_err();
    let InstallError::UnmanagedExisting(o) = &err else {
        panic!("{err:?}")
    };
    assert_eq!(o.sha256, ArtifactSha256::of(&manual));
    assert_eq!(err.class(), FailureClass::Rejected);
    assert!(
        fx.probe.calls().is_empty(),
        "unapproved file never executed"
    );
    assert_eq!(fx.exe().unwrap(), manual);
    // Only private scaffolding was created; the operator's file is intact.
    assert_eq!(
        fx.snapshot()
            .into_iter()
            .filter(|(p, _)| p.ends_with("/kanade"))
            .collect::<Vec<_>>(),
        before
            .into_iter()
            .filter(|(p, _)| p.ends_with("/kanade"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn adoption_must_match_the_approved_observation_exactly() {
    let fx = Fx::new();
    let manual = binary("0.9.0", "manual");
    place_unmanaged(&fx, &manual);
    for wrong in [
        observe(&binary("0.9.0", "other"), "0.9.0"),
        observe(&manual, "0.8.0"),
        ExistingObservation {
            mode: 0o700,
            ..observe(&manual, "0.9.0")
        },
        ExistingObservation {
            uid: Owner::current().uid + 1,
            ..observe(&manual, "0.9.0")
        },
    ] {
        let pkg = B::new(&fx, "1.0.0").adopt(wrong).build();
        let err = fx.run(&fx.installer(), &pkg).unwrap_err();
        assert!(matches!(err, InstallError::AdoptionMismatch(_)), "{err:?}");
        assert_eq!(fx.exe().unwrap(), manual);
        assert!(fx.receipt().is_none());
    }
}

#[test]
fn adoption_flag_and_approved_observation_must_agree() {
    let fx = Fx::new();
    let manual = binary("0.9.0", "manual");
    place_unmanaged(&fx, &manual);
    // Operation asks for adoption, approval carries none.
    let mut pkg = B::new(&fx, "1.0.0")
        .adopt(observe(&manual, "0.9.0"))
        .build();
    pkg.adoption = None;
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::Approval(_)), "{err:?}");
    // Approval carries an observation, operation does not ask for adoption.
    let mut pkg = Pkg::new(&fx, "1.0.0");
    pkg.adoption = Some(observe(&manual, "0.9.0"));
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::Approval(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), manual);
}

#[test]
fn adopting_something_that_is_not_an_arm64_cli_never_executes_it() {
    let fx = Fx::new();
    let script = b"#!/bin/sh\necho kanade 0.9.0\n".to_vec();
    place_unmanaged(&fx, &script);
    let pkg = B::new(&fx, "1.0.0")
        .adopt(observe(&script, "0.9.0"))
        .build();
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::AdoptionMismatch(_)), "{err:?}");
    assert!(fx.probe.calls().is_empty());
    assert_eq!(fx.exe().unwrap(), script);
}

#[test]
fn symlinked_hardlinked_and_writable_install_paths_are_not_taken_over() {
    // Homebrew-style: the install path is a symlink into a Cellar.
    let fx = Fx::new();
    std::fs::create_dir_all(&fx.roots.install_dir).unwrap();
    std::fs::set_permissions(
        &fx.roots.install_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let cellar = fx.base.join("Cellar-kanade");
    std::fs::write(&cellar, binary("0.9.0", "brew")).unwrap();
    symlink(&cellar, fx.exe_path()).unwrap();
    let pkg = B::new(&fx, "1.0.0")
        .adopt(observe(&binary("0.9.0", "brew"), "0.9.0"))
        .build();
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::ForeignPath(_)), "{err:?}");
    assert_eq!(std::fs::read(&cellar).unwrap(), binary("0.9.0", "brew"));
    assert!(
        std::fs::symlink_metadata(fx.exe_path())
            .unwrap()
            .is_symlink()
    );

    // Hard link shared with another path.
    std::fs::remove_file(fx.exe_path()).unwrap();
    std::fs::set_permissions(&cellar, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::hard_link(&cellar, fx.exe_path()).unwrap();
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::ForeignPath(_)), "{err:?}");
    assert_eq!(std::fs::read(&cellar).unwrap(), binary("0.9.0", "brew"));

    // Group/world-writable executable.
    std::fs::remove_file(fx.exe_path()).unwrap();
    place_unmanaged(&fx, &binary("0.9.0", "brew"));
    std::fs::set_permissions(fx.exe_path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::ForeignPath(_)), "{err:?}");
}

#[test]
fn a_directory_at_the_install_path_is_foreign() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.exe_path()).unwrap();
    std::fs::set_permissions(
        &fx.roots.install_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.0.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::ForeignPath(_)), "{err:?}");
}

// ======================= admission / tampering ===================================

fn assert_untouched(fx: &Fx) {
    assert!(
        !fx.roots.deploy_dir.exists(),
        "no state created for refused input"
    );
    assert!(!fx.roots.install_dir.exists());
    assert!(fx.probe.calls().is_empty());
}

#[test]
fn tampered_archive_bytes_are_refused_before_any_state_exists() {
    let fx = Fx::new();
    let mut pkg = Pkg::new(&fx, "1.0.0");
    let mid = pkg.archive.len() / 2;
    pkg.archive[mid] ^= 0xff;
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(matches!(err, InstallError::Admission(_)), "{err:?}");
    assert_untouched(&fx);
}

#[test]
fn tampered_manifest_wrong_signer_and_revoked_signer_are_refused() {
    let fx = Fx::new();
    let mut pkg = Pkg::new(&fx, "1.0.0");
    let i = pkg.manifest.len() / 2;
    pkg.manifest[i] ^= 1;
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    // Signed by a key nobody pinned.
    let pkg = B::new(&fx, "1.0.0").signer(key(9)).build();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    // Genuine package, then the publisher is revoked: admission is current.
    let pkg = Pkg::new(&fx, "1.0.0");
    fx.trust(&[&key(3)], &[&fx.publisher], 2);
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    assert_untouched(&fx);
}

#[test]
fn identity_and_digest_bindings_are_enforced_against_the_operation() {
    let fx = Fx::new();
    // Manifest signed for 1.0.0 but the (approved) operation names 1.0.1.
    let pkg = B::new(&fx, "1.0.1").manifest_version("1.0.0").build();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    // Operation pins a different archive digest than the bytes supplied.
    let mut pkg = Pkg::new(&fx, "1.0.0");
    let TypedOperation::DeployCli(spec) = &mut pkg.op else {
        unreachable!()
    };
    spec.artifact_sha256 = ArtifactSha256::of(b"other");
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    // Operation pins a different manifest digest.
    let mut pkg = Pkg::new(&fx, "1.0.0");
    let TypedOperation::DeployCli(spec) = &mut pkg.op else {
        unreachable!()
    };
    spec.manifest_sha256 = ArtifactSha256::of(b"other manifest");
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    // Wrong package id.
    let pkg = B::new(&fx, "1.0.0");
    let pkg = B {
        package_id: "com.kanade.other",
        ..pkg
    }
    .build();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Admission(_)
    ));
    assert_untouched(&fx);
}

#[test]
fn a_genuinely_signed_but_malformed_payload_is_still_refused() {
    let fx = Fx::new();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "x86_64 image",
            tarball::good(&macho_fx::image(0x0100_0007, 2, 14, b"VERSION=1.0.0;")),
        ),
        (
            "extra member",
            tarball::build(&[
                (
                    archive::EXPECTED_MEMBER,
                    b'0',
                    0o755,
                    "",
                    &binary("1.0.0", ""),
                ),
                ("evil", b'0', 0o755, "", b"x"),
            ]),
        ),
        (
            "traversal",
            tarball::build(&[("../kanade", b'0', 0o755, "", &binary("1.0.0", ""))]),
        ),
        (
            "symlink member",
            tarball::build(&[(archive::EXPECTED_MEMBER, b'2', 0o755, "/bin/sh", b"")]),
        ),
        ("not an executable", tarball::good(b"#!/bin/sh\necho hi\n")),
        (
            "requires newer macOS",
            tarball::good(&macho_fx::image(0x0100_000c, 2, 15, b"VERSION=1.0.0;")),
        ),
    ];
    for (name, archive) in cases {
        let pkg = B::new(&fx, "1.0.0").archive(archive).build();
        let err = fx.run(&fx.installer(), &pkg).unwrap_err();
        assert!(
            matches!(err, InstallError::Archive(_) | InstallError::MachO(_)),
            "{name}: {err:?}"
        );
        assert_untouched(&fx);
    }
}

#[test]
fn an_executable_that_reports_the_wrong_version_is_never_placed() {
    let fx = Fx::new();
    // Signed as 1.0.0 but the binary inside identifies itself as 0.0.1.
    let pkg = B::new(&fx, "1.0.0")
        .archive(tarball::good(&binary("0.0.1", "")))
        .build();
    let err = fx.run(&fx.installer(), &pkg).unwrap_err();
    assert!(
        matches!(err, InstallError::VersionMismatch { .. }),
        "{err:?}"
    );
    assert!(fx.exe().is_none());
    assert!(fx.receipt().is_none());
    assert_eq!(fx.journal().unwrap().phase, Phase::Abandoned);
    assert!(fx.staging_entries().is_empty());
}

#[test]
fn unsupported_hosts_and_targets_are_refused() {
    let mut fx = Fx::new();
    fx.host.os_version = "13.6".into();
    assert!(matches!(
        fx.run(&fx.installer(), &Pkg::new(&fx, "1.0.0"))
            .unwrap_err(),
        InstallError::Unsupported(_)
    ));
    fx.host.os_version = "14.0".into();
    // A Linux CLI identity is a valid artifact but not for this handler.
    let id = ArtifactIdentity::new(
        ArtifactRole::Cli,
        "1.0.0".parse().unwrap(),
        TargetOs::Linux,
        TargetArch::Aarch64,
        DistroAbi::Musl,
        PackageFormat::TarGz,
    )
    .unwrap();
    let mut pkg = Pkg::new(&fx, "1.0.0");
    let TypedOperation::DeployCli(spec) = &mut pkg.op else {
        unreachable!()
    };
    spec.package_object_key = id.app_package_key().unwrap();
    spec.artifact_identity = id;
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::Unsupported(_)
    ));
    assert_untouched(&fx);
}

#[test]
fn approval_is_a_separate_prerequisite_bound_to_the_job_plan() {
    let bad = ExecContext {
        job_id: "../x".into(),
        exec_id: "e".into(),
    };
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    let err = fx
        .installer()
        .install(&fx.store, &pkg.inputs(&fx.host), &pkg.approval(), &bad)
        .unwrap_err();
    assert!(matches!(err, InstallError::Approval(_)));
    assert_untouched(&fx);
}

// ======================= races and locking ================================

#[test]
fn a_third_party_replacing_the_cli_before_the_swap_is_never_overwritten() {
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    let third = binary("7.7.7", "third-party");
    let (exe, dir, t2) = (fx.exe_path(), fx.roots.install_dir.clone(), third.clone());
    let inst = fx.installer_with(move |p| {
        if p == Point::Swap {
            let tmp = dir.join("third.tmp");
            std::fs::write(&tmp, &t2).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &exe).unwrap();
        }
        Fault::Pass
    });
    let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
    assert!(matches!(err, InstallError::Concurrent(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), third, "their file was put back");
    assert_eq!(
        fx.receipt().unwrap().body.identity.version().as_str(),
        "1.0.0"
    );
    assert_eq!(fx.journal().unwrap().phase, Phase::Abandoned);
    assert!(fx.staging_entries().is_empty());
}

#[test]
fn a_symlink_planted_at_the_install_path_is_swapped_back_untouched() {
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    let target = fx.base.join("victim");
    std::fs::write(&target, b"victim data").unwrap();
    let (exe, t2) = (fx.exe_path(), target.clone());
    let inst = fx.installer_with(move |p| {
        if p == Point::Swap {
            std::fs::remove_file(&exe).unwrap();
            symlink(&t2, &exe).unwrap();
        }
        Fault::Pass
    });
    let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
    assert!(matches!(err, InstallError::Concurrent(_)), "{err:?}");
    assert!(
        std::fs::symlink_metadata(fx.exe_path())
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"victim data");
}

#[test]
fn a_fresh_install_never_replaces_a_file_created_during_the_run() {
    let fx = Fx::new();
    let third = binary("7.7.7", "third-party");
    let (exe, t2) = (fx.exe_path(), third.clone());
    let inst = fx.installer_with(move |p| {
        if p == Point::Swap {
            std::fs::write(&exe, &t2).unwrap();
        }
        Fault::Pass
    });
    let err = fx.run(&inst, &Pkg::new(&fx, "1.0.0")).unwrap_err();
    assert!(matches!(err, InstallError::Concurrent(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), third);
    assert!(fx.receipt().is_none());
}

#[test]
fn concurrent_installers_serialize_and_the_second_sees_a_noop() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    let outcomes: Vec<InstallOutcome> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|_| s.spawn(|| fx.run(&fx.installer(), &pkg).unwrap().outcome))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == InstallOutcome::Installed)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == InstallOutcome::NoOp)
            .count(),
        3
    );
    assert_eq!(fx.exe().unwrap(), pkg.binary);
}

#[test]
fn lock_wait_is_bounded_and_a_timeout_changes_nothing() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    fx.ok(&pkg);
    let snap = fx.snapshot();
    let deploy = Dir::open_absolute(&fx.roots.deploy_dir, None, Owner::current()).unwrap();
    let _held = FileLock::acquire(&deploy, LOCK_NAME, Owner::current(), Duration::from_secs(1))
        .unwrap()
        .unwrap();
    let inst = fx.installer().with_lock_wait(Duration::from_millis(150));
    let t0 = std::time::Instant::now();
    let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
    assert!(matches!(err, InstallError::LockTimeout), "{err:?}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(fx.snapshot(), snap);
}

// ======================= unsafe roots ===============================================

#[test]
fn unsafe_install_and_state_directories_are_refused() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    // A different principal than the one running: cannot create its files.
    let mut roots = fx.roots.clone();
    roots.owner.uid += 1;
    let inst =
        Installer::new(roots, Box::new(Shared(fx.probe.clone()))).with_clock(Box::new(Clock));
    assert!(matches!(
        fx.run(&inst, &pkg).unwrap_err(),
        InstallError::UnsafeRoot(_)
    ));

    // Group/world-writable install directory.
    std::fs::create_dir_all(&fx.roots.install_dir).unwrap();
    std::fs::set_permissions(
        &fx.roots.install_dir,
        std::fs::Permissions::from_mode(0o775),
    )
    .unwrap();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::UnsafeRoot(_)
    ));
    std::fs::set_permissions(
        &fx.roots.install_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    // Symlinked install directory.
    let real = fx.base.join("real-bin");
    std::fs::create_dir(&real).unwrap();
    let alias = fx.base.join("alias-bin");
    symlink(&real, &alias).unwrap();
    let mut roots = fx.roots.clone();
    roots.install_dir = alias;
    let inst =
        Installer::new(roots, Box::new(Shared(fx.probe.clone()))).with_clock(Box::new(Clock));
    assert!(matches!(
        fx.run(&inst, &pkg).unwrap_err(),
        InstallError::UnsafeRoot(_)
    ));
    assert!(names(&real).is_empty());

    // Non-private state directory and a pre-existing loose staging directory.
    std::fs::create_dir_all(&fx.roots.deploy_dir).unwrap();
    std::fs::set_permissions(&fx.roots.deploy_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::UnsafeRoot(_)
    ));
    std::fs::set_permissions(&fx.roots.deploy_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(fx.roots.install_dir.join(STAGING_DIR)).unwrap();
    std::fs::set_permissions(
        fx.roots.install_dir.join(STAGING_DIR),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg).unwrap_err(),
        InstallError::UnsafeRoot(_)
    ));
    assert!(fx.exe().is_none());
}

// ======================= interruption at every phase ================================

fn crash_after(p: Phase) -> impl Fn(Point) -> Fault + Send + Sync {
    move |q| {
        if q == Point::AfterPhase(p) {
            Fault::Crash
        } else {
            Fault::Pass
        }
    }
}

fn assert_consistent(fx: &Fx, pkg: &Pkg) {
    assert_eq!(fx.exe().unwrap(), pkg.binary);
    let r = fx.receipt().unwrap();
    assert_eq!(r.body.binary_sha256, ArtifactSha256::of(&pkg.binary));
    assert!(fx.journal().unwrap().phase.is_terminal());
    assert!(fx.staging_entries().is_empty());
}

#[test]
fn an_update_interrupted_at_every_persistent_phase_converges() {
    for phase in [
        Phase::Prepared,
        Phase::Staged,
        Phase::BackedUp,
        Phase::Swapping,
        Phase::Swapped,
        Phase::ReceiptWritten,
        Phase::Committed,
    ] {
        let fx = Fx::new();
        let v1 = Pkg::new(&fx, "1.0.0");
        let v2 = Pkg::new(&fx, "1.1.0");
        fx.ok(&v1);
        let err = fx
            .run(&fx.installer_with(crash_after(phase)), &v2)
            .unwrap_err();
        assert!(
            matches!(err, InstallError::Interrupted),
            "{phase:?}: {err:?}"
        );
        assert_eq!(fx.journal().unwrap().phase, phase);
        if phase == Phase::Swapped {
            // The documented non-atomic window: new binary, old receipt.
            assert_eq!(fx.exe().unwrap(), v2.binary);
            assert_eq!(
                fx.receipt().unwrap().body.identity.version().as_str(),
                "1.0.0"
            );
        }

        // "Restart": a new installer picks the same request back up.
        let rep = fx
            .run(&fx.installer(), &v2)
            .unwrap_or_else(|e| panic!("{phase:?}: {e:?}"));
        assert_consistent(&fx, &v2);
        match phase {
            Phase::Committed => {
                assert!(rep.recovered.is_none());
                assert_eq!(rep.outcome, InstallOutcome::NoOp);
            }
            Phase::Swapped | Phase::ReceiptWritten => {
                assert!(rep.recovered.is_some(), "{phase:?}");
                assert_eq!(
                    rep.outcome,
                    InstallOutcome::NoOp,
                    "rolled forward, not redone"
                );
            }
            _ => {
                assert!(rep.recovered.is_some(), "{phase:?}");
                assert_eq!(rep.outcome, InstallOutcome::Updated);
            }
        }
        assert_eq!(fx.backup("kanade.prev").unwrap(), v1.binary);
    }
}

#[test]
fn a_fresh_install_interrupted_at_every_phase_converges() {
    for phase in [
        Phase::Prepared,
        Phase::Staged,
        Phase::Swapping,
        Phase::Swapped,
        Phase::ReceiptWritten,
        Phase::Committed,
    ] {
        let fx = Fx::new();
        let v1 = Pkg::new(&fx, "1.0.0");
        let err = fx
            .run(&fx.installer_with(crash_after(phase)), &v1)
            .unwrap_err();
        assert!(matches!(err, InstallError::Interrupted), "{phase:?}");
        fx.run(&fx.installer(), &v1)
            .unwrap_or_else(|e| panic!("{phase:?}: {e:?}"));
        assert_consistent(&fx, &v1);
        assert!(fx.backup("kanade.prev").is_none());
    }
}

#[test]
fn a_crash_just_before_the_rename_or_the_journal_write_converges() {
    for point in [
        Point::BeforePhase(Phase::Swapping),
        Point::Swap,
        Point::BeforePhase(Phase::Swapped),
        Point::ReceiptWrite,
        Point::BeforePhase(Phase::Committed),
    ] {
        let fx = Fx::new();
        let v1 = Pkg::new(&fx, "1.0.0");
        let v2 = Pkg::new(&fx, "1.1.0");
        fx.ok(&v1);
        let err = fx
            .run(
                &fx.installer_with(move |q| {
                    if q == point {
                        Fault::Crash
                    } else {
                        Fault::Pass
                    }
                }),
                &v2,
            )
            .unwrap_err();
        assert!(matches!(err, InstallError::Interrupted), "{point:?}");
        fx.run(&fx.installer(), &v2)
            .unwrap_or_else(|e| panic!("{point:?}: {e:?}"));
        assert_consistent(&fx, &v2);
    }
}

#[test]
fn recovery_does_not_run_a_stale_interrupted_install_against_new_input() {
    // v2 crashed after the swap; the next request is for v3. The v2 swap is
    // completed (never half-applied), then v3 is an ordinary update.
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    let v2 = Pkg::new(&fx, "1.1.0");
    fx.run(&fx.installer_with(crash_after(Phase::Swapped)), &v2)
        .unwrap_err();
    let v3 = Pkg::new(&fx, "1.2.0");
    let rep = fx.run(&fx.installer(), &v3).unwrap();
    assert_eq!(rep.outcome, InstallOutcome::Updated);
    assert!(rep.recovered.is_some());
    assert_consistent(&fx, &v3);
    assert_eq!(fx.backup("kanade.prev").unwrap(), v2.binary);
}

// ======================= disk full / failed rename ===================================

#[test]
fn disk_full_before_the_swap_leaves_the_install_untouched() {
    for point in [Point::StageWrite, Point::BackupWrite, Point::Swap] {
        let fx = Fx::new();
        let v1 = Pkg::new(&fx, "1.0.0");
        fx.ok(&v1);
        let receipt = fx.receipt_raw().unwrap();
        let err = fx
            .run(
                &fx.installer_with(move |q| {
                    if q == point {
                        Fault::Errno(ENOSPC)
                    } else {
                        Fault::Pass
                    }
                }),
                &Pkg::new(&fx, "1.1.0"),
            )
            .unwrap_err();
        assert!(matches!(err, InstallError::Io { .. }), "{point:?}: {err:?}");
        assert_eq!(err.class(), FailureClass::Rejected);
        assert_eq!(fx.exe().unwrap(), v1.binary);
        assert_eq!(fx.receipt_raw().unwrap(), receipt);
        assert_eq!(fx.journal().unwrap().phase, Phase::Abandoned, "{point:?}");
        assert!(fx.staging_entries().is_empty());
        // The failure is not sticky: the next run completes.
        assert_eq!(
            fx.ok(&Pkg::new(&fx, "1.1.0")).outcome,
            InstallOutcome::Updated
        );
    }
}

#[test]
fn a_real_rename_failure_is_reported_and_changes_nothing() {
    // The staged file vanishing makes the kernel's rename fail for real.
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let staged = fx.roots.install_dir.join(STAGING_DIR).join("kanade.new");
    let inst = fx.installer_with(move |p| {
        if p == Point::Swap {
            std::fs::remove_file(&staged).unwrap();
        }
        Fault::Pass
    });
    let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
    assert!(
        matches!(err, InstallError::Concurrent(_) | InstallError::Io { .. }),
        "{err:?}"
    );
    assert_eq!(fx.exe().unwrap(), v1.binary);
    assert_eq!(fx.journal().unwrap().phase, Phase::Abandoned);
}

#[test]
fn disk_full_while_writing_the_receipt_rolls_the_pair_back() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let receipt = fx.receipt_raw().unwrap();
    let err = fx
        .run(
            &fx.installer_with(|q| {
                if q == Point::ReceiptWrite {
                    Fault::Errno(ENOSPC)
                } else {
                    Fault::Pass
                }
            }),
            &Pkg::new(&fx, "1.1.0"),
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::RolledBack { .. }), "{err:?}");
    assert_eq!(err.class(), FailureClass::RolledBack);
    assert_eq!(fx.exe().unwrap(), v1.binary);
    assert_eq!(fx.receipt_raw().unwrap(), receipt);
    assert_eq!(fx.journal().unwrap().phase, Phase::RolledBack);
    assert!(fx.staging_entries().is_empty());
    assert_eq!(
        ResultRecord::from_result(&Err(err)).status,
        ResultStatus::RolledBack
    );
}

// ======================= verification failure, rollback, rollback failure ============

#[test]
fn post_swap_verification_failure_restores_the_old_pair() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let receipt = fx.receipt_raw().unwrap();
    let exe = fx.exe_path();
    fx.probe.fail_when(move |p| p == exe);
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.1.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::RolledBack { .. }), "{err:?}");
    assert_eq!(fx.exe().unwrap(), v1.binary);
    assert_eq!(fx.receipt_raw().unwrap(), receipt);
    assert_eq!(
        std::fs::metadata(fx.exe_path()).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(fx.journal().unwrap().phase, Phase::RolledBack);
    assert_eq!(
        fx.backup("kanade.prev").unwrap(),
        v1.binary,
        "backup is retained"
    );
}

#[test]
fn a_failed_fresh_install_removes_only_what_it_created() {
    let fx = Fx::new();
    let exe = fx.exe_path();
    fx.probe.fail_when(move |p| p == exe);
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.0.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::RolledBack { .. }), "{err:?}");
    assert!(fx.exe().is_none());
    assert!(fx.receipt().is_none());
    assert_eq!(fx.journal().unwrap().phase, Phase::RolledBack);
    // Operator-owned neighbours in the same directory were never touched.
    assert_eq!(names(&fx.roots.install_dir), vec![STAGING_DIR.to_string()]);
}

#[test]
fn rollback_never_clobbers_a_file_it_did_not_install() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let third = binary("7.7.7", "third-party");
    let (exe, t2) = (fx.exe_path(), third.clone());
    // Verification fails after the swap, and just before the rollback a third
    // party replaces the file.
    let exe2 = exe.clone();
    fx.probe.fail_when(move |p| {
        if p == exe2 {
            let tmp = exe2.with_file_name("third.tmp");
            std::fs::write(&tmp, &t2).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &exe2).unwrap();
            true
        } else {
            false
        }
    });
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.1.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::RecoveryRequired(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), third);
    assert_eq!(fx.backup("kanade.prev").unwrap(), v1.binary);
    assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);
}

#[test]
fn rollback_failure_freezes_with_backups_and_stops_retrying() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let exe = fx.exe_path();
    fx.probe.fail_when(move |p| p == exe);
    let v2 = Pkg::new(&fx, "1.1.0");
    let err = fx
        .run(
            &fx.installer_with(|q| {
                if q == Point::RestoreSwap {
                    Fault::Errno(5)
                } else {
                    Fault::Pass
                }
            }),
            &v2,
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::RecoveryRequired(_)), "{err:?}");
    assert_eq!(err.class(), FailureClass::RecoveryRequired);
    assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);
    assert_eq!(fx.backup("kanade.prev").unwrap(), v1.binary);
    assert!(fx.backup("cli.receipt.prev.json").is_some());

    // Later runs refuse without touching anything, however many times asked.
    let snap = fx.snapshot();
    let calls = fx.probe.calls().len();
    fx.probe.fail_when(|_| false);
    for _ in 0..3 {
        let e = fx.run(&fx.installer(), &v2).unwrap_err();
        assert!(matches!(e, InstallError::RecoveryRequired(_)), "{e:?}");
        let e = fx.run(&fx.installer(), &v1).unwrap_err();
        assert!(matches!(e, InstallError::RecoveryRequired(_)), "{e:?}");
    }
    assert_eq!(fx.snapshot(), snap);
    assert_eq!(fx.probe.calls().len(), calls);
    assert_eq!(
        ResultRecord::from_result(&Err(err)).status,
        ResultStatus::RecoveryRequired
    );
}

#[test]
fn a_missing_backup_makes_a_failed_rollback_recovery_required_not_a_guess() {
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    let exe = fx.exe_path();
    fx.probe.fail_when(move |p| p == exe);
    let backup = fx.roots.deploy_dir.join(BACKUP_DIR).join("kanade.prev");
    let inst = fx.installer_with(move |q| {
        if q == Point::BeforePhase(Phase::Swapped) {
            std::fs::remove_file(&backup).unwrap();
        }
        Fault::Pass
    });
    let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
    assert!(matches!(err, InstallError::RecoveryRequired(_)), "{err:?}");
    assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);
}

// ======================= receipt / journal corruption ==================================

#[test]
fn corrupt_or_loose_receipts_stop_everything_without_changes() {
    let truncated = |p: &Path| std::fs::write(p, b"{\"body\":").unwrap();
    let tampered = |p: &Path| {
        let s = std::fs::read_to_string(p).unwrap();
        std::fs::write(p, s.replace("\"mode\": 493", "\"mode\": 511")).unwrap();
    };
    let loose =
        |p: &Path| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let empty = |p: &Path| std::fs::write(p, b"").unwrap();
    type Damage<'a> = &'a dyn Fn(&Path);
    let cases: [(&str, Damage); 4] = [
        ("truncated", &truncated),
        ("tampered", &tampered),
        ("loose mode", &loose),
        ("empty", &empty),
    ];
    for (name, damage) in cases {
        let fx = Fx::new();
        let v1 = Pkg::new(&fx, "1.0.0");
        fx.ok(&v1);
        damage(&fx.roots.deploy_dir.join(RECEIPT_NAME));
        let snap = fx.snapshot();
        for pkg in [&v1, &Pkg::new(&fx, "1.1.0")] {
            let err = fx.run(&fx.installer(), pkg).unwrap_err();
            assert!(
                matches!(err, InstallError::ReceiptCorrupt(_)),
                "{name}: {err:?}"
            );
            assert_eq!(err.class(), FailureClass::RecoveryRequired);
        }
        assert_eq!(fx.snapshot(), snap, "{name}");
        assert_eq!(fx.exe().unwrap(), v1.binary);
    }
}

#[test]
fn receipt_and_executable_that_disagree_are_not_repaired_silently() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    // Operator edits the managed executable.
    let edited = binary("1.0.0", "edited");
    std::fs::write(fx.exe_path(), &edited).unwrap();
    assert!(matches!(
        fx.run(&fx.installer(), &v1).unwrap_err(),
        InstallError::Drift(_)
    ));
    assert_eq!(fx.exe().unwrap(), edited);
    // Mode drift on content that still matches.
    std::fs::write(fx.exe_path(), &v1.binary).unwrap();
    std::fs::set_permissions(fx.exe_path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        fx.run(&fx.installer(), &v1).unwrap_err(),
        InstallError::Drift(_)
    ));
    // Receipt without an executable.
    std::fs::remove_file(fx.exe_path()).unwrap();
    let err = fx.run(&fx.installer(), &v1).unwrap_err();
    assert!(matches!(err, InstallError::Inconsistent(_)), "{err:?}");
    assert_eq!(err.class(), FailureClass::RecoveryRequired);
    assert!(fx.exe().is_none());
}

#[test]
fn a_corrupt_journal_blocks_further_action() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    std::fs::write(fx.roots.deploy_dir.join(JOURNAL_NAME), b"{ not json").unwrap();
    let snap = fx.snapshot();
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.1.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::JournalCorrupt(_)), "{err:?}");
    assert_eq!(err.class(), FailureClass::RecoveryRequired);
    assert_eq!(fx.snapshot(), snap);
}

#[test]
fn recovery_is_bounded_and_unknown_states_are_frozen() {
    // Interrupted after the swap, but a third party has since changed the file.
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    fx.run(
        &fx.installer_with(crash_after(Phase::Swapped)),
        &Pkg::new(&fx, "1.1.0"),
    )
    .unwrap_err();
    let third = binary("7.7.7", "third-party");
    std::fs::write(fx.exe_path(), &third).unwrap();
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.1.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::RecoveryRequired(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), third);
    assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);

    // A journal whose attempts are exhausted is frozen, not retried.
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    fx.run(
        &fx.installer_with(crash_after(Phase::Swapped)),
        &Pkg::new(&fx, "1.1.0"),
    )
    .unwrap_err();
    let mut j = fx.journal().unwrap();
    j.recovery_attempts = MAX_RECOVERY_ATTEMPTS;
    let bytes = JournalFile::new(j).to_bytes();
    std::fs::write(fx.roots.deploy_dir.join(JOURNAL_NAME), bytes).unwrap();
    let before = fx.exe().unwrap();
    let err = fx
        .run(&fx.installer(), &Pkg::new(&fx, "1.1.0"))
        .unwrap_err();
    assert!(matches!(err, InstallError::RecoveryRequired(_)), "{err:?}");
    assert_eq!(fx.exe().unwrap(), before);
    assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);
}

#[test]
fn an_interrupted_rollback_is_completed_on_restart() {
    let fx = Fx::new();
    let v1 = Pkg::new(&fx, "1.0.0");
    fx.ok(&v1);
    let exe = fx.exe_path();
    fx.probe.fail_when(move |p| p == exe);
    let v2 = Pkg::new(&fx, "1.1.0");
    let err = fx
        .run(
            &fx.installer_with(|q| {
                if q == Point::RestoreSwap {
                    Fault::Crash
                } else {
                    Fault::Pass
                }
            }),
            &v2,
        )
        .unwrap_err();
    assert!(matches!(err, InstallError::Interrupted), "{err:?}");
    assert_eq!(fx.journal().unwrap().phase, Phase::RollingBack);
    fx.probe.fail_when(|_| false);
    let rep = fx.run(&fx.installer(), &v1).unwrap();
    assert!(rep.recovered.is_some());
    assert_eq!(rep.outcome, InstallOutcome::NoOp);
    assert_eq!(fx.exe().unwrap(), v1.binary);
}

// ======================= no process / service side effects ==============================

#[test]
fn only_the_probe_adapter_may_start_a_process_and_nothing_names_services() {
    let sources = [
        ("admit.rs", include_str!("admit.rs")),
        ("archive.rs", include_str!("archive.rs")),
        ("fs.rs", include_str!("fs.rs")),
        ("macho.rs", include_str!("macho.rs")),
        ("model.rs", include_str!("model.rs")),
        ("probe.rs", include_str!("probe.rs")),
        ("txn.rs", include_str!("txn.rs")),
    ];
    for (name, src) in sources {
        // Production code only: everything before the unit tests.
        let prod = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "pkill",
            "killall",
            "launchctl",
            "systemctl",
            "\"sh\"",
            "/bin/sh",
            "bash",
            "osascript",
            "sudo",
            "libc::kill(",
            "libc::system",
            "libc::exec",
        ] {
            assert!(!prod.contains(forbidden), "{name} mentions {forbidden}");
        }
        let spawns = prod.matches("Command::new").count();
        if name == "probe.rs" {
            assert_eq!(spawns, 1);
            assert!(prod.contains(".arg(\"--version\")"));
            assert!(prod.contains(".env_clear()"));
        } else {
            assert_eq!(spawns, 0, "{name} spawns a process");
        }
    }
}

#[test]
fn the_installer_probes_only_staged_and_installed_paths_under_the_install_root() {
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    fx.ok(&Pkg::new(&fx, "1.1.0"));
    for c in fx.probe.calls() {
        let rel = c
            .strip_prefix(&fx.roots.install_dir)
            .expect("inside the install root");
        assert!(
            rel == Path::new("kanade") || rel == Path::new(STAGING_DIR).join("kanade.new"),
            "{rel:?}"
        );
    }
}

#[test]
fn offline_api_rejects_production_roots_before_any_mutation() {
    let fx = Fx::new();
    for install in [true, false] {
        let mut roots = fx.roots.clone();
        if install {
            roots.install_dir = PRODUCTION_INSTALL_DIR.into();
        } else {
            roots.deploy_dir = PRODUCTION_DEPLOY_DIR.into();
        }
        assert!(matches!(
            fx.run(
                &Installer::new(roots, Box::new(Shared(fx.probe.clone())))
                    .with_clock(Box::new(Clock)),
                &Pkg::new(&fx, "1.0.0")
            ),
            Err(InstallError::UnsafeRoot(_))
        ));
    }
    assert_untouched(&fx);
    assert!(fx.probe.calls().is_empty());
}

#[test]
fn approval_reuses_stage_four_gate_for_plan_operation_scope_and_time() {
    let fx = Fx::new();
    let pkg = Pkg::new(&fx, "1.0.0");
    assert!(pkg.approval_with("other-host", |_, _, _, _| {}).is_err());
    assert!(
        pkg.approval_with("host", |p, _, _, _| p.state = PlanState::Revoked)
            .is_err()
    );
    assert!(
        pkg.approval_with("host", |_, m, _, _| m
            .deploy_authorization
            .as_mut()
            .unwrap()
            .plan_digest =
            ArtifactSha256::of(b"other"))
            .is_err()
    );
    assert!(
        pkg.approval_with("host", |p, _, _, _| {
            let TypedOperation::DeployCli(s) = &mut p.content.operation else {
                unreachable!()
            };
            s.package_id = "com.other.cli".into();
        })
        .is_err()
    );
    assert!(
        pkg.approval_with("host", |_, _, scope, _| scope.pcs =
            vec!["other-host".into()])
            .is_err()
    );
    assert!(
        pkg.approval_with("host", |_, _, _, now| *now += 400)
            .is_err()
    );
    let mut exec = pkg.exec();
    exec.job_id = "other-job".into();
    assert!(matches!(
        fx.installer()
            .install(&fx.store, &pkg.inputs(&fx.host), &pkg.approval(), &exec),
        Err(InstallError::Approval(_))
    ));
    assert_untouched(&fx);
}

#[test]
fn adoption_observation_cannot_be_substituted_after_plan_approval() {
    let fx = Fx::new();
    let manual = binary("0.9.0", "manual");
    place_unmanaged(&fx, &manual);
    let mut pkg = B::new(&fx, "1.0.0")
        .adopt(observe(&manual, "0.9.0"))
        .build();
    let before = fx.snapshot();
    pkg.adoption.as_mut().unwrap().version = "0.8.0".into();
    assert!(matches!(
        fx.run(&fx.installer(), &pkg),
        Err(InstallError::Approval(_))
    ));
    assert_eq!(fx.snapshot(), before);
    assert!(fx.probe.calls().is_empty());
    let TypedOperation::DeployCli(spec) = &mut pkg.op else {
        unreachable!()
    };
    spec.adoption_observation_sha256 = None;
    assert!(pkg.op.validate().is_err());
}

#[test]
fn installer_rejects_a_different_valid_operation_than_the_approved_plan() {
    let fx = Fx::new();
    let approved = Pkg::new(&fx, "1.0.0");
    let requested = B::new(&fx, "1.0.0")
        .adopt(observe(&binary("0.9.0", "manual"), "0.9.0"))
        .build();
    assert!(matches!(
        fx.installer().install(
            &fx.store,
            &requested.inputs(&fx.host),
            &approved.approval(),
            &requested.exec()
        ),
        Err(InstallError::Approval(_))
    ));
    assert_untouched(&fx);
    assert!(fx.probe.calls().is_empty());
}

#[test]
fn adoption_observation_changes_the_approved_plan_digest() {
    let fx = Fx::new();
    let first = B::new(&fx, "1.0.0")
        .adopt(observe(&binary("0.9.0", "first"), "0.9.0"))
        .build();
    let second = B::new(&fx, "1.0.0")
        .adopt(observe(&binary("0.9.0", "second"), "0.9.0"))
        .build();
    assert_eq!(first.archive, second.archive);
    assert_ne!(
        first.approval().plan_digest(),
        second.approval().plan_digest()
    );
}

#[test]
fn interrupted_foreign_swap_back_retains_displaced_files_and_freezes() {
    for crash in [Point::SwapBack, Point::BeforePhase(Phase::SwapBackRequired)] {
        for variant in 0..3 {
            let link = variant == 1;
            let fx = Fx::new();
            fx.ok(&Pkg::new(&fx, "1.0.0"));
            let third = if variant == 2 {
                fx.exe().unwrap()
            } else {
                binary("7.7.7", "foreign")
            };
            let victim = fx.base.join("foreign-target");
            std::fs::write(&victim, &third).unwrap();
            let (exe, target, bytes) = (fx.exe_path(), victim.clone(), third.clone());
            let inst = fx.installer_with(move |p| {
                if p == Point::Swap {
                    let tmp = exe.with_extension("foreign");
                    if link {
                        symlink(&target, &tmp).unwrap();
                    } else {
                        std::fs::write(&tmp, &bytes).unwrap();
                    }
                    std::fs::rename(tmp, &exe).unwrap();
                }
                if p == crash {
                    Fault::Crash
                } else {
                    Fault::Pass
                }
            });
            let pkg = Pkg::new(&fx, "1.1.0");
            assert!(matches!(
                fx.run(&inst, &pkg),
                Err(InstallError::Interrupted)
            ));
            let displaced = fx.roots.install_dir.join(STAGING_DIR).join("kanade.new");
            for _ in 0..2 {
                assert!(matches!(
                    fx.run(&fx.installer(), &pkg),
                    Err(InstallError::RecoveryRequired(_))
                ));
                assert_eq!(std::fs::read(&displaced).unwrap(), third);
                assert_eq!(
                    std::fs::symlink_metadata(&displaced).unwrap().is_symlink(),
                    link
                );
                assert_eq!(std::fs::read(&victim).unwrap(), third);
                assert_eq!(fx.journal().unwrap().phase, Phase::RecoveryRequired);
            }
        }
    }
}

#[test]
fn approval_expiring_while_waiting_for_the_lock_is_rejected() {
    use std::sync::atomic::{AtomicU64, Ordering};
    struct AdvancingClock(AtomicU64);
    impl WallClock for AdvancingClock {
        fn unix_now(&self) -> u64 {
            self.0.fetch_add(301, Ordering::SeqCst)
        }
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
    }
    let fx = Fx::new();
    fx.ok(&Pkg::new(&fx, "1.0.0"));
    let snap = fx.snapshot();
    let deploy = Dir::open_absolute(&fx.roots.deploy_dir, None, Owner::current()).unwrap();
    let held = FileLock::acquire(&deploy, LOCK_NAME, Owner::current(), Duration::from_secs(1))
        .unwrap()
        .unwrap();
    std::thread::scope(|s| {
        s.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        let inst = fx
            .installer()
            .with_clock(Box::new(AdvancingClock(AtomicU64::new(T0))));
        let err = fx.run(&inst, &Pkg::new(&fx, "1.1.0")).unwrap_err();
        assert!(matches!(err, InstallError::Approval(_)), "{err:?}");
    });
    assert_eq!(fx.snapshot(), snap);
}
