//! Real no-follow filesystem/lock/trust/plan code, deterministic installer,
//! receipt, clock and launch-gate adapters. No test installs into the host OS.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use flate2::{
    Compression,
    write::{GzEncoder, ZlibEncoder},
};
use kanade_shared::artifact::*;
use kanade_shared::deploy_plan::*;
use kanade_shared::manifest::Manifest;
use kanade_shared::package_manifest::{ManifestClaims, PublisherSigningKey, sign_manifest};
use kanade_shared::publisher_trust::*;
use kanade_shared::typed_operation::{DeployClientSpec, OptOutPolicy, TypedOperation};
use kanade_shared::wire::*;

use super::structure::{self, APP, BUNDLE, COMPONENT, EXE, PART};
use super::system::*;
use super::transaction::Boundary;
use super::transaction::*;
use super::{Error, Result};
use crate::native_install::fs::{Dir, FileLock, Owner};
use crate::native_install::model::{Approval, ExecContext, HostInfo, Roots};

const T0: u64 = 1_800_000_000;
const DIST: &str = include_str!("fixtures/Distribution.xml");
const INFO: &str = include_str!("fixtures/PackageInfo.xml");
#[derive(Default)]
struct Clock(AtomicU64);
impl WallClock for Clock {
    fn unix_now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}
fn compressed(bytes: &[u8], gzip: bool) -> Vec<u8> {
    if gzip {
        let mut out = GzEncoder::new(Vec::new(), Compression::fast());
        out.write_all(bytes).unwrap();
        out.finish().unwrap()
    } else {
        let mut out = ZlibEncoder::new(Vec::new(), Compression::fast());
        out.write_all(bytes).unwrap();
        out.finish().unwrap()
    }
}
fn cpio(entries: &BTreeMap<String, structure::Entry>) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (name, e)) in entries.iter().enumerate() {
        let name = if name.is_empty() {
            ".".into()
        } else {
            format!("./{name}")
        };
        out.extend_from_slice(
            format!(
                "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
                0,
                i,
                e.mode,
                0,
                0,
                1,
                0,
                0,
                name.len() + 1,
                e.data.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend(&e.data);
    }
    out.extend_from_slice(
        format!(
            "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
            0, 0, 0, 0, 0, 1, 0, 0, 11, 0
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"TRAILER!!!\0");
    out
}
fn plist(version: &str) -> Vec<u8> {
    let vals = [
        ("CFBundleIdentifier", BUNDLE),
        ("CFBundleName", "Kanade Client"),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleExecutable", "kanade-client"),
        ("CFBundleVersion", version),
        ("CFBundleShortVersionString", version),
        ("LSMinimumSystemVersion", "14.0"),
    ];
    format!(
        "<plist><dict>{}</dict></plist>",
        vals.into_iter()
            .map(|(k, v)| format!("<key>{k}</key><string>{v}</string>"))
            .collect::<String>()
    )
    .into_bytes()
}
fn payload(version: &str) -> structure::Payload {
    structure::expected_modes()
        .into_iter()
        .map(|(name, mode)| {
            let data = if name == format!("{APP}/{EXE}") {
                crate::native_install::macho::test_support::arm64(version.as_bytes())
            } else if name == format!("{APP}/Contents/Info.plist") {
                plist(version)
            } else if mode & 0o170000 == 0o100000 {
                b"resource".to_vec()
            } else {
                vec![]
            };
            (name, structure::Entry { mode, data })
        })
        .collect()
}
fn members(version: &str) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (
            "Distribution".into(),
            DIST.replace("0.97.0", version).into_bytes(),
        ),
        (
            format!("{PART}/PackageInfo"),
            INFO.replace("0.97.0", version).into_bytes(),
        ),
        (format!("{PART}/Bom"), b"fake BOM".to_vec()),
        (
            format!("{PART}/Payload"),
            compressed(&cpio(&payload(version)), true),
        ),
    ])
}
fn xar(members: &BTreeMap<String, Vec<u8>>, signed: bool) -> Vec<u8> {
    let mut heap: Vec<u8> = Vec::new();
    let mut record = |name: &str, data: &[u8]| {
        let offset = heap.len();
        heap.extend(data);
        format!(
            "<file><name>{name}</name><type>file</type><data><offset>{offset}</offset><length>{}</length><size>{}</size><encoding style=\"application/octet-stream\"/></data></file>",
            data.len(),
            data.len()
        )
    };
    let dist = record("Distribution", &members["Distribution"]);
    let children: String = members
        .iter()
        .filter(|(n, _)| n.starts_with(&format!("{PART}/")))
        .map(|(n, d)| record(n.rsplit('/').next().unwrap(), d))
        .collect();
    let xml = format!(
        "<xar><toc>{}{dist}<file><name>{PART}</name><type>directory</type>{children}</file></toc></xar>",
        if signed { "<signature/>" } else { "" }
    );
    let toc = compressed(xml.as_bytes(), false);
    let mut out = b"xar!\0\x1c\0\x01".to_vec();
    out.extend((toc.len() as u64).to_be_bytes());
    out.extend((xml.len() as u64).to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out.extend(toc);
    out.extend(heap);
    out
}
fn bom(path: &Path) -> String {
    structure::expected_modes()
        .iter()
        .map(|(n, mode)| {
            let (size, crc) = if mode & 0o170000 == 0o40000 {
                (String::new(), String::new())
            } else {
                let bytes = std::fs::read(path.parent().unwrap().join(n)).unwrap();
                (
                    bytes.len().to_string(),
                    structure::cksum(&bytes).to_string(),
                )
            };
            format!(
                "{}\t{mode:o}\t{size}\troot\twheel\t{crc}\n",
                if n.is_empty() {
                    ".".into()
                } else {
                    format!("./{n}")
                }
            )
        })
        .collect()
}

struct Gate {
    manifest: Manifest,
    plan: PlanRecord,
    facts: TargetFacts,
    actor: DispatchActor,
    scope: RequestedScope,
}
impl Gate {
    fn new(op: &TypedOperation, now: u64) -> Self {
        let mut manifest: Manifest = serde_json::from_value(serde_json::json!({
            "id":"client-job", "version":"1.0.0", "require_approval":true,
            "execute":{"kind":"typed_operation","payload_version":1,"timeout":"10m","run_as":"system","operation":op},
            "deploy_authorization":{"plan_id":"client-plan","plan_digest":String::from(ArtifactSha256::of(b"placeholder"))}
        })).unwrap();
        let content = PlanContent {
            record_version: PLAN_RECORD_VERSION,
            plan_id: "client-plan".into(),
            job_id: manifest.id.clone(),
            job_manifest_digest: job_manifest_digest(&manifest).unwrap(),
            role: ArtifactRole::Client,
            operation: op.clone(),
            artifact: PlanArtifact {
                identity: op.artifact_identity().clone(),
                integrity: ArtifactIntegrity {
                    sha256: *op.artifact_sha256(),
                    size_bytes: op.artifact_size_bytes(),
                },
                manifest_sha256: *op.manifest_sha256(),
            },
            targets: vec!["test-target".into()],
            not_before: now as i64 - 1,
            expires_at: now as i64 + 300,
            execution_policy: ExecutionPolicy::SingleUsePerTarget,
        };
        manifest.deploy_authorization.as_mut().unwrap().plan_digest = content.digest();
        Self {
            plan: PlanRecord {
                stored_digest: content.digest(),
                content,
                state: PlanState::Approved,
                approved_by: Some("test-operator".into()),
                revoked_by: None,
                applied_targets: vec![],
            },
            manifest,
            facts: TargetFacts {
                pc_id: "test-target".into(),
                registered: true,
                current_agent_version: Some("1.0.0".into()),
                current_os_family: Some("macos".into()),
                current_agent_arch: Some("aarch64".into()),
                evidence: CapabilityEvidence {
                    capabilities: Some(DeployCapabilities::from_handlers(&[DeployHandler::new(
                        TYPED_DEPLOY_CONTRACT_V1,
                        OPERATION_DEPLOY_CLIENT,
                        TargetOs::Macos,
                        TargetArch::Aarch64,
                    )])),
                    agent_version: Some("1.0.0".into()),
                    protocols: Some(vec![PROTOCOL_V2.into()]),
                    keys: Some(vec!["test-signer".into()]),
                    os_family: Some("macos".into()),
                    agent_arch: Some("aarch64".into()),
                    reported_at_ms: Some(now as i64 * 1000),
                    received_at: Some(now as i64),
                },
            },
            actor: DispatchActor::Operator(Principal {
                subject: "test-operator".into(),
                operator: true,
                exec_feature: true,
            }),
            scope: RequestedScope {
                pcs: vec!["test-target".into()],
                ..Default::default()
            },
        }
    }
    fn approval(&self, now: u64) -> Result<Approval> {
        Ok(Approval::new(
            &GateInput {
                manifest: &self.manifest,
                plan: Some(&self.plan),
                actor: &self.actor,
                scope: &self.scope,
                targets: std::slice::from_ref(&self.facts),
                signer: Some("test-signer"),
                now: now as i64,
            },
            "test-target",
            None,
        )?)
    }
}
struct Auth {
    gates: RefCell<Vec<Gate>>,
    clock: Arc<Clock>,
    refuse: Cell<Option<Boundary>>,
    calls: RefCell<Vec<Boundary>>,
}
impl Authorization for Auth {
    fn current(&self, op: &TypedOperation, _: &ExecContext, b: Boundary) -> Result<Approval> {
        self.calls.borrow_mut().push(b);
        if self.refuse.get() == Some(b) {
            return Err(Error::Authorization("current plan revoked".into()));
        }
        self.gates
            .borrow()
            .iter()
            .find(|g| g.plan.content.operation == *op)
            .ok_or_else(|| Error::Authorization("no current plan".into()))?
            .approval(self.clock.unix_now())
    }
}
type CheckpointHook = Box<dyn Fn(Phase)>;
struct Fake {
    apps: PathBuf,
    receipt: RefCell<Option<OsReceipt>>,
    calls: Cell<usize>,
    exits: RefCell<VecDeque<InstallerExit>>,
    coordinate: Cell<bool>,
    lease_safe: Cell<bool>,
    quiescent: Cell<bool>,
    crash: Cell<Option<Phase>>,
    bad_bom: Cell<bool>,
    bad_signature: Cell<bool>,
    mutate: Cell<bool>,
    wrong_receipt: Cell<bool>,
    after_install: RefCell<Option<Box<dyn Fn()>>>,
    checkpoint_hook: RefCell<Option<CheckpointHook>>,
}
struct Lease<'a>(&'a Cell<bool>);
impl LaunchLease for Lease<'_> {
    fn recheck(&self) -> Result<()> {
        if self.0.get() {
            Ok(())
        } else {
            Err(Error::Deferred(
                "managed executable PID/start/UID or launch gate cannot be proven safe".into(),
            ))
        }
    }
}
impl System for Fake {
    fn check_target(&self, applications: &Path) -> Result<()> {
        structure::require(applications == self.apps, "fake adapter target mismatch")
    }
    fn bom(&self, path: &Path) -> Result<String> {
        Ok(if self.bad_bom.get() {
            bom(path).replace("root", "other")
        } else {
            bom(path)
        })
    }
    fn signature(&self, _: &Path) -> Result<()> {
        if self.bad_signature.get() {
            Err(Error::Invalid("unsigned/wrong signature".into()))
        } else {
            Ok(())
        }
    }
    fn receipt(&self) -> Result<Option<OsReceipt>> {
        Ok(self.receipt.borrow().clone())
    }
    fn coordinate(&self) -> Result<Box<dyn LaunchLease + '_>> {
        if self.coordinate.get() {
            Ok(Box::new(Lease(&self.lease_safe)))
        } else {
            Err(Error::Deferred(
                "process/session coordination unavailable".into(),
            ))
        }
    }
    fn recovery_quiescent(&self) -> Result<()> {
        if self.quiescent.get() {
            Ok(())
        } else {
            Err(Error::RecoveryRequired(
                "OS worker may still be running".into(),
            ))
        }
    }
    fn checkpoint(&self, p: Phase) -> Result<()> {
        if let Some(hook) = self.checkpoint_hook.borrow().as_ref() {
            hook(p);
        }
        if self.crash.get() == Some(p) {
            self.crash.set(None);
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    }
    fn install(&self, pkg: &Path) -> Result<InstallerExit> {
        self.calls.set(self.calls.get() + 1);
        if self.mutate.get() {
            let parts = structure::xar(&std::fs::read(pkg)?)?;
            let entries = structure::payload(&parts[&format!("{PART}/Payload")])?;
            let app = self.apps.join(APP);
            if app.exists() {
                std::fs::remove_dir_all(&app)?;
            }
            for (name, e) in &entries {
                if name.is_empty() {
                    continue;
                }
                let path = self.apps.join(name);
                if e.mode & 0o170000 == 0o40000 {
                    std::fs::create_dir_all(&path)?;
                } else {
                    std::fs::write(&path, &e.data)?;
                }
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(e.mode & 0o777))?;
            }
            let doc = roxmltree::Document::parse(
                std::str::from_utf8(&parts[&format!("{PART}/PackageInfo")]).unwrap(),
            )
            .unwrap();
            let version = doc.root_element().attribute("version").unwrap().to_owned();
            *self.receipt.borrow_mut() = Some(OsReceipt {
                package_id: COMPONENT.into(),
                version: if self.wrong_receipt.get() {
                    "9.9.9".into()
                } else {
                    version
                },
                install_location: "Applications".into(),
                volume: "/".into(),
                files: structure::expected_modes()
                    .keys()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect(),
            });
        }
        if let Some(f) = self.after_install.borrow().as_ref() {
            f();
        }
        Ok(self
            .exits
            .borrow_mut()
            .pop_front()
            .unwrap_or(InstallerExit::Succeeded))
    }
}
struct Pkg {
    op: TypedOperation,
    manifest: Vec<u8>,
    pkg: Vec<u8>,
    approval: Approval,
}
struct Fx {
    _temp: tempfile::TempDir,
    roots: Roots,
    clock: Arc<Clock>,
    trust: Arc<PublisherTrustStore>,
    root: PublisherSigningKey,
    publisher: PublisherSigningKey,
    auth: Auth,
    sys: Fake,
    host: HostInfo,
    exec: ExecContext,
}
impl Fx {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let clock = Arc::new(Clock(AtomicU64::new(T0)));
        let trust = Arc::new(PublisherTrustStore::open(base.join("trust"), clock.clone()));
        let root = PublisherSigningKey::from_seed([11; 32]);
        let publisher = PublisherSigningKey::from_seed([12; 32]);
        trust
            .provision_root(&TrustRoot::from_signing_key(&root).to_root_json())
            .unwrap();
        let apps = base.join("Applications");
        let fx = Self {
            _temp: temp,
            roots: Roots {
                install_dir: apps.clone(),
                deploy_dir: base.join("deploy"),
                owner: Owner::current(),
            },
            clock: clock.clone(),
            trust,
            root,
            publisher,
            auth: Auth {
                gates: RefCell::new(vec![]),
                clock,
                refuse: Cell::new(None),
                calls: RefCell::new(vec![]),
            },
            sys: Fake {
                apps,
                receipt: RefCell::new(None),
                calls: Cell::new(0),
                exits: RefCell::new(VecDeque::new()),
                coordinate: Cell::new(true),
                lease_safe: Cell::new(true),
                quiescent: Cell::new(true),
                crash: Cell::new(None),
                bad_bom: Cell::new(false),
                bad_signature: Cell::new(false),
                mutate: Cell::new(true),
                wrong_receipt: Cell::new(false),
                after_install: RefCell::new(None),
                checkpoint_hook: RefCell::new(None),
            },
            host: HostInfo {
                os_version: "14.5".into(),
                capabilities: vec![],
            },
            exec: ExecContext {
                job_id: "client-job".into(),
                exec_id: "client-exec".into(),
            },
        };
        fx.trust_state(false, 1);
        fx
    }
    fn trust_state(&self, revoked: bool, sequence: u64) {
        let entries = if revoked {
            vec![]
        } else {
            vec![PublisherEntry::new(self.publisher.trusted_key(), T0 - 100, T0 + 86400).unwrap()]
        };
        let rev = if revoked {
            vec![self.publisher.trusted_key().key_id().as_str().into()]
        } else {
            vec![]
        };
        self.trust
            .apply(&sign_trust_state(
                &TrustStateClaims::new("test-trust".into(), sequence, T0, T0 + 86400, entries, rev)
                    .unwrap(),
                &self.root,
            ))
            .unwrap();
    }
    fn pkg(&self, version: &str) -> Pkg {
        self.pkg_bytes(version, xar(&members(version), false))
    }
    fn pkg_bytes(&self, version: &str, pkg: Vec<u8>) -> Pkg {
        let id = ArtifactIdentity::new(
            ArtifactRole::Client,
            version.parse().unwrap(),
            TargetOs::Macos,
            TargetArch::Aarch64,
            DistroAbi::Darwin14,
            PackageFormat::Pkg,
        )
        .unwrap();
        let claims = ManifestClaims::new(
            id.clone(),
            COMPONENT.into(),
            "14.0".into(),
            vec![],
            ArtifactIntegrity::of(&pkg),
        )
        .unwrap();
        let manifest = sign_manifest(claims, &self.publisher).to_canonical_bytes();
        let op = TypedOperation::DeployClient(DeployClientSpec {
            package_object_key: id.app_package_key().unwrap(),
            artifact_identity: id,
            artifact_sha256: ArtifactSha256::of(&pkg),
            artifact_size_bytes: pkg.len() as u64,
            package_id: COMPONENT.into(),
            manifest_sha256: ArtifactSha256::of(&manifest),
            opt_out_policy: OptOutPolicy::Respect,
            restart_gui_session_only: true,
        });
        let gate = Gate::new(&op, T0);
        let approval = gate.approval(T0).unwrap();
        self.auth.gates.borrow_mut().push(gate);
        Pkg {
            op,
            manifest,
            pkg,
            approval,
        }
    }
    fn installer(&self) -> Installer<'_> {
        Installer {
            roots: self.roots.clone(),
            system: &self.sys,
            authorization: &self.auth,
            trust: &self.trust,
            clock: self.clock.as_ref(),
            lock_wait: Duration::from_millis(20),
        }
    }
    fn request<'a>(&'a self, p: &'a Pkg) -> Request<'a> {
        Request {
            operation: &p.op,
            manifest: &p.manifest,
            pkg: &p.pkg,
            host: &self.host,
            exec: &self.exec,
            approval: &p.approval,
            target: "test-target",
        }
    }
    fn run(&self, p: &Pkg) -> Result<Report> {
        self.installer().install(&self.request(p))
    }
    fn journal_phase(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(self.roots.deploy_dir.join("client.pkg.journal.json")).unwrap(),
        )
        .unwrap()["body"]["phase"]
            .as_str()
            .unwrap()
            .into()
    }
}

#[test]
fn install_update_and_verified_noop_never_activate_gui() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let q = f.pkg("0.98.0");
    let r = f.run(&p).unwrap();
    assert_eq!(r.outcome, Outcome::InstalledVerified);
    assert_eq!(
        r.gui_activation,
        super::result::GuiActivation::UnsupportedNotAttempted
    );
    let record = super::result::ResultRecord::from_result(&Ok(r));
    assert!(record.installed_verified);
    assert_eq!(record.version.as_deref(), Some("0.97.0"));
    let noop = f.run(&p);
    assert_eq!(noop.as_ref().unwrap().outcome, Outcome::NoOpVerified);
    let record = super::result::ResultRecord::from_result(&noop);
    assert!(record.installed_verified);
    assert_eq!(record.status, super::result::ResultStatus::NoOpVerified);
    assert_eq!(f.sys.calls.get(), 1);
    assert_eq!(f.run(&q).unwrap().version, "0.98.0");
    assert_eq!(f.sys.calls.get(), 2);
    assert_eq!(f.journal_phase(), "committed");
    assert!(
        f.roots
            .deploy_dir
            .join("client-pkgs")
            .join(format!("{}.pkg", String::from(*p.op.artifact_sha256())))
            .exists()
    );
}
#[test]
fn current_plan_revocation_and_expiry_are_not_hidden_by_copied_approval() {
    for boundary in [Boundary::Locked, Boundary::BeforeInstaller] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        f.auth.refuse.set(Some(boundary));
        assert!(f.run(&p).is_err());
        assert_eq!(f.sys.calls.get(), 0);
    }
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.clock.0.store(T0 + 301, Ordering::SeqCst);
    assert!(f.run(&p).is_err());
    assert_eq!(f.sys.calls.get(), 0);
}
#[test]
fn independent_publisher_and_exact_bytes_metadata_binding() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.trust_state(true, 2);
    assert!(f.run(&p).is_err());
    assert_eq!(f.sys.calls.get(), 0);
    for field in ["digest", "manifest", "object", "package_id"] {
        let f = Fx::new();
        let mut p = f.pkg("0.97.0");
        match field {
            "digest" => p.pkg.push(1),
            "manifest" => p.manifest.push(0),
            "object" => {
                let TypedOperation::DeployClient(s) = &mut p.op else {
                    unreachable!()
                };
                s.package_object_key = "wrong".into();
            }
            _ => {
                let TypedOperation::DeployClient(s) = &mut p.op else {
                    unreachable!()
                };
                s.package_id = "wrong".into();
            }
        }
        assert!(f.run(&p).is_err(), "{field}");
        assert_eq!(f.sys.calls.get(), 0);
    }
}
#[test]
fn structurally_wrong_but_publisher_signed_packages_are_refused() {
    for kind in [
        "signed_pkg",
        "scripts",
        "distribution",
        "relocation",
        "version",
        "payload",
        "arch",
        "minimum_os",
        "link",
    ] {
        let f = Fx::new();
        let mut m = members("0.97.0");
        match kind {
            "scripts" => {
                m.insert(format!("{PART}/Scripts"), b"bad".to_vec());
            }
            "distribution" => {
                m.insert(
                    "Distribution".into(),
                    DIST.replace(
                        "customize=\"never\"",
                        "customize=\"never\" installation-check=\"evil()\"",
                    )
                    .into_bytes(),
                );
            }
            "relocation" => {
                m.insert(
                    format!("{PART}/PackageInfo"),
                    INFO.replace("relocatable=\"false\"", "relocatable=\"true\"")
                        .into_bytes(),
                );
            }
            "version" => {
                m.insert(
                    format!("{PART}/PackageInfo"),
                    INFO.replace("0.97.0", "0.96.0").into_bytes(),
                );
            }
            "payload" | "arch" | "minimum_os" | "link" => {
                let mut entries = payload("0.97.0");
                match kind {
                    "payload" => {
                        entries.insert(
                            "evil".into(),
                            structure::Entry {
                                mode: 0o100644,
                                data: b"bad".to_vec(),
                            },
                        );
                    }
                    "arch" => entries.get_mut(&format!("{APP}/{EXE}")).unwrap().data[4..8]
                        .copy_from_slice(&0x01000007u32.to_le_bytes()),
                    "minimum_os" => entries.get_mut(&format!("{APP}/{EXE}")).unwrap().data[44..48]
                        .copy_from_slice(&(15u32 << 16).to_le_bytes()),
                    _ => entries.get_mut(&format!("{APP}/{EXE}")).unwrap().mode = 0o120777,
                }
                m.insert(format!("{PART}/Payload"), compressed(&cpio(&entries), true));
            }
            _ => {}
        }
        let p = f.pkg_bytes("0.97.0", xar(&m, kind == "signed_pkg"));
        assert!(f.run(&p).is_err(), "{kind}");
        assert_eq!(f.sys.calls.get(), 0);
    }
}
#[test]
fn wrong_bom_and_unsigned_app_are_refused_before_installer() {
    for signature in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        if signature {
            f.sys.bad_signature.set(true);
        } else {
            f.sys.bad_bom.set(true);
        }
        assert!(f.run(&p).is_err());
        assert_eq!(f.sys.calls.get(), 0);
    }
}
#[test]
fn unmanaged_app_or_receipt_never_implies_adoption() {
    for app in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        if app {
            std::fs::create_dir_all(f.roots.install_dir.join(APP)).unwrap();
        } else {
            *f.sys.receipt.borrow_mut() = Some(OsReceipt {
                package_id: COMPONENT.into(),
                version: "0.96.0".into(),
                install_location: "Applications".into(),
                volume: "/".into(),
                files: vec![],
            });
        }
        assert!(matches!(f.run(&p), Err(Error::Unmanaged)));
        assert_eq!(f.sys.calls.get(), 0);
    }
}
#[test]
fn receipt_or_bundle_drift_blocks_even_same_version_noop() {
    for receipt in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        f.run(&p).unwrap();
        if receipt {
            f.sys.receipt.borrow_mut().as_mut().unwrap().version = "0.96.0".into();
        } else {
            std::fs::write(f.roots.install_dir.join(APP).join(EXE), b"changed").unwrap();
        }
        assert!(matches!(f.run(&p), Err(Error::RecoveryRequired(_))));
        assert_eq!(f.sys.calls.get(), 1);
    }
}
#[test]
fn coordination_unavailable_or_active_client_defers_update_without_termination() {
    for unavailable in [true, false] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        let q = f.pkg("0.98.0");
        f.run(&p).unwrap();
        if unavailable {
            f.sys.coordinate.set(false);
        } else {
            f.sys.lease_safe.set(false);
        }
        assert!(matches!(f.run(&q), Err(Error::Deferred(_))));
        assert_eq!(f.sys.calls.get(), 1);
    }
}
#[test]
fn uncertain_os_worker_never_gets_overlapping_rollback_or_retry() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let q = f.pkg("0.98.0");
    f.run(&p).unwrap();
    f.sys
        .exits
        .borrow_mut()
        .push_back(InstallerExit::Uncertain("timeout".into()));
    assert!(matches!(f.run(&q), Err(Error::RecoveryRequired(_))));
    assert_eq!(f.journal_phase(), "recovery_required");
    assert!(matches!(f.run(&q), Err(Error::RecoveryRequired(_))));
    assert_eq!(f.sys.calls.get(), 2);
}
#[test]
fn rollback_is_previous_verified_pkg_through_installer_and_failure_freezes() {
    for failed in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        let q = f.pkg("0.98.0");
        f.run(&p).unwrap();
        f.sys
            .exits
            .borrow_mut()
            .push_back(InstallerExit::FailedQuiescent(
                "simulated failure after full install".into(),
            ));
        if failed {
            f.sys
                .exits
                .borrow_mut()
                .push_back(InstallerExit::Uncertain("rollback failure".into()));
        }
        let result = f.run(&q);
        if failed {
            assert!(matches!(result, Err(Error::RecoveryRequired(_))));
            assert_eq!(f.journal_phase(), "recovery_required");
            assert!(f.run(&q).is_err());
        } else {
            assert!(matches!(result, Err(Error::RolledBack)));
            assert_eq!(f.sys.receipt.borrow().as_ref().unwrap().version, "0.97.0");
            assert_eq!(f.journal_phase(), "rolled_back");
        }
        assert_eq!(f.sys.calls.get(), 3);
    }
}
#[test]
fn rollback_requires_separate_current_approval_for_previous_operation() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let q = f.pkg("0.98.0");
    f.run(&p).unwrap();
    f.auth
        .gates
        .borrow_mut()
        .retain(|g| g.plan.content.operation == q.op);
    f.sys
        .exits
        .borrow_mut()
        .push_back(InstallerExit::FailedQuiescent("failed".into()));
    assert!(matches!(f.run(&q), Err(Error::RecoveryRequired(_))));
    assert_eq!(f.sys.calls.get(), 2);
}
#[test]
fn fresh_failure_and_receipt_divergence_retain_evidence_without_deleting_bundle() {
    for divergence in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        f.sys.wrong_receipt.set(divergence);
        if !divergence {
            f.sys
                .exits
                .borrow_mut()
                .push_back(InstallerExit::FailedQuiescent("fresh failure".into()));
        }
        assert!(matches!(f.run(&p), Err(Error::RecoveryRequired(_))));
        assert!(f.roots.install_dir.join(APP).exists());
        assert_eq!(f.sys.calls.get(), 1);
    }
}
#[test]
fn interrupted_phases_reconcile_files_and_receipts_without_reinstalling() {
    for phase in [
        Phase::Prepared,
        Phase::Installing,
        Phase::InstallerReturned,
        Phase::Committed,
    ] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        f.sys.crash.set(Some(phase));
        assert!(matches!(f.run(&p), Err(Error::Interrupted)));
        let calls = f.sys.calls.get();
        let report = f.run(&p).unwrap();
        assert_eq!(f.sys.calls.get(), 1);
        assert_eq!(
            report.outcome,
            if calls == 1 {
                Outcome::NoOpVerified
            } else {
                Outcome::InstalledVerified
            }
        );
    }
}
#[test]
fn interrupted_rollback_reconciles_old_state_but_never_repeats_rollback() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let q = f.pkg("0.98.0");
    f.run(&p).unwrap();
    f.sys
        .exits
        .borrow_mut()
        .push_back(InstallerExit::FailedQuiescent("failed".into()));
    f.sys.crash.set(Some(Phase::RollingBack));
    assert!(matches!(f.run(&q), Err(Error::Interrupted)));
    assert_eq!(f.sys.calls.get(), 2);
    assert!(matches!(f.run(&q), Err(Error::RecoveryRequired(_))));
    assert_eq!(f.sys.calls.get(), 2);
}
#[test]
fn restart_with_unknown_worker_or_changed_receipt_becomes_recovery_required() {
    for worker in [false, true] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        f.sys.crash.set(Some(Phase::InstallerReturned));
        assert!(f.run(&p).is_err());
        if worker {
            f.sys.quiescent.set(false);
        } else {
            f.sys.receipt.borrow_mut().as_mut().unwrap().version = "9.9.9".into();
        }
        assert!(matches!(f.run(&p), Err(Error::RecoveryRequired(_))));
        assert_eq!(f.sys.calls.get(), 1);
    }
}
#[test]
fn symlink_ancestors_and_foreign_cache_and_bounded_lock_are_refused() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    std::os::unix::fs::symlink(&f.roots.install_dir, &f.roots.deploy_dir).unwrap();
    assert!(f.run(&p).is_err());
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let d = Dir::open_absolute(&f.roots.deploy_dir, Some(0o700), Owner::current()).unwrap();
    let _held = FileLock::acquire(&d, LOCK, Owner::current(), Duration::ZERO)
        .unwrap()
        .unwrap();
    assert!(matches!(f.run(&p), Err(Error::Deferred(_))));
    assert_eq!(f.sys.calls.get(), 0);
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.run(&p).unwrap();
    let cache = f
        .roots
        .deploy_dir
        .join("client-pkgs")
        .join(format!("{}.pkg", String::from(*p.op.artifact_sha256())));
    std::fs::remove_file(&cache).unwrap();
    std::os::unix::fs::symlink("missing", cache).unwrap();
    assert!(f.run(&p).is_err());
    assert_eq!(f.sys.calls.get(), 1);
}
#[test]
fn malformed_bounded_archives_and_plist_duplicates_are_rejected() {
    for data in [vec![], b"xar!".to_vec(), vec![0; 100]] {
        assert!(structure::xar(&data).is_err());
    }
    let mut p = payload("0.97.0");
    p.get_mut(&format!("{APP}/Contents/Info.plist"))
        .unwrap()
        .data = plist("0.97.0").repeat(2);
    assert!(
        structure::info_plist(&p[&format!("{APP}/Contents/Info.plist")].data, "0.97.0").is_err()
    );
    assert!(structure::payload(&compressed(&vec![0; 1024], true)).is_err());
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn real_productbuild_fixture_shared_with_python_passes_native_readonly_validation() {
    let f = Fx::new();
    let fixture = f._temp.path().join("fixture");
    std::fs::create_dir(&fixture).unwrap();
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/ci");
    let output=std::process::Command::new("python3").args(["-B","-I","-c", "import sys,pathlib;sys.path.insert(0,sys.argv[1]);import test_client_macos_pkg as t;base=pathlib.Path(sys.argv[2]);app=t.make_app(base);pkg,_=t.m.build(app,base/'out',production=False);print(pkg);print(t.VERSION)"]).arg(scripts).arg(&fixture).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.lines();
    let path = PathBuf::from(lines.next().unwrap());
    let version = lines.next().unwrap();
    let bytes = std::fs::read(path).unwrap();
    let pkg = f.pkg_bytes(version, bytes.clone());
    let native = NativeSystem::for_image(f._temp.path().canonicalize().unwrap()).unwrap();
    let mut installer = f.installer();
    installer.system = &native;
    installer
        .inspect(&pkg.op, &pkg.manifest, &pkg.pkg, &f.host)
        .unwrap();
    for resource in [
        "Contents/Resources/icon.icns",
        "Contents/_CodeSignature/CodeResources",
    ] {
        let mut parts = structure::xar(&bytes).unwrap();
        let mut payload = structure::payload(&parts[&format!("{PART}/Payload")]).unwrap();
        payload
            .get_mut(&format!("{APP}/{resource}"))
            .unwrap()
            .data
            .extend_from_slice(b"tampered");
        parts.insert(format!("{PART}/Payload"), compressed(&cpio(&payload), true));
        let bad = f.pkg_bytes(version, xar(&parts, false));
        let err = installer
            .inspect(&bad.op, &bad.manifest, &bad.pkg, &f.host)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("BOM payload size/checksum mismatch"),
            "{err}"
        );
    }
    assert!(matches!(native.coordinate(), Err(Error::Deferred(_))));
    assert_eq!(f.sys.calls.get(), 0);
}

#[test]
fn publisher_revocation_at_installer_and_recovery_boundaries_is_rechecked() {
    for phase in [Phase::Installing, Phase::RollingBack] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        let q = f.pkg("0.98.0");
        f.run(&p).unwrap();
        if phase == Phase::RollingBack {
            f.sys
                .exits
                .borrow_mut()
                .push_back(InstallerExit::FailedQuiescent("failed".into()));
        }
        let revoked = sign_trust_state(
            &TrustStateClaims::new(
                "test-trust".into(),
                2,
                T0,
                T0 + 86400,
                vec![],
                vec![f.publisher.trusted_key().key_id().as_str().into()],
            )
            .unwrap(),
            &f.root,
        );
        let trust = f.trust.clone();
        *f.sys.checkpoint_hook.borrow_mut() = Some(Box::new(move |p| {
            if p == phase {
                trust.apply(&revoked).unwrap();
            }
        }));
        assert!(f.run(&q).is_err());
        assert_eq!(
            f.sys.calls.get(),
            if phase == Phase::Installing { 1 } else { 2 }
        );
        if phase == Phase::RollingBack {
            assert_eq!(f.journal_phase(), "recovery_required");
        }
    }
}
#[test]
fn launch_path_and_staged_pkg_races_are_refused_before_installer() {
    for mode in ["directory", "pkg"] {
        let f = Fx::new();
        let p = f.pkg("0.97.0");
        let roots = f.roots.clone();
        let digest = String::from(*p.op.artifact_sha256());
        *f.sys.checkpoint_hook.borrow_mut() = Some(Box::new(move |phase| {
            if phase != Phase::Installing {
                return;
            }
            if mode == "directory" {
                std::fs::rename(
                    &roots.install_dir,
                    roots.install_dir.with_extension("displaced"),
                )
                .unwrap();
                std::fs::create_dir(&roots.install_dir).unwrap();
            } else {
                let pkg = roots
                    .deploy_dir
                    .join("client-pkgs")
                    .join(format!("{digest}.pkg"));
                std::fs::remove_file(&pkg).unwrap();
                std::os::unix::fs::symlink("missing", pkg).unwrap();
            }
        }));
        assert!(f.run(&p).is_err());
        assert_eq!(f.sys.calls.get(), 0);
    }
}
#[test]
fn post_installer_plan_expiry_and_restart_revocation_freeze_evidence() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    let clock = f.clock.clone();
    *f.sys.after_install.borrow_mut() =
        Some(Box::new(move || clock.0.store(T0 + 301, Ordering::SeqCst)));
    assert!(matches!(f.run(&p), Err(Error::RecoveryRequired(_))));
    assert_eq!(f.sys.calls.get(), 1);
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.sys.crash.set(Some(Phase::InstallerReturned));
    assert!(f.run(&p).is_err());
    f.trust_state(true, 2);
    assert!(matches!(
        f.installer().recover(&f.host),
        Err(Error::RecoveryRequired(_))
    ));
    assert_eq!(f.journal_phase(), "recovery_required");
    assert_eq!(f.sys.calls.get(), 1);
}
#[test]
fn retries_are_bounded_and_noop_does_not_need_a_gui_lease() {
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.sys.mutate.set(false);
    for _ in 0..3 {
        f.sys
            .exits
            .borrow_mut()
            .push_back(InstallerExit::FailedQuiescent("no mutation".into()));
        assert!(f.run(&p).is_err());
    }
    assert!(f.run(&p).is_err());
    assert_eq!(f.sys.calls.get(), 3);
    let f = Fx::new();
    let p = f.pkg("0.97.0");
    f.run(&p).unwrap();
    f.sys.coordinate.set(false);
    assert_eq!(f.run(&p).unwrap().outcome, Outcome::NoOpVerified);
    assert_eq!(f.sys.calls.get(), 1);
}
#[test]
fn real_metadata_fixtures_and_carrier_bom_are_validated_with_no_install() {
    structure::distribution(DIST.as_bytes(), "0.97.0").unwrap();
    structure::package_info(INFO.as_bytes(), "0.97.0").unwrap();
    structure::bom(include_str!("fixtures/Bom.txt")).unwrap();
    for bad in [
        "install-location=\"/Applications\"",
        "identifier=\"com.yukimemi.kanade-client.pkg\"",
    ] {
        assert!(
            structure::package_info(INFO.replace(bad, "unexpected=\"bad\"").as_bytes(), "0.97.0")
                .is_err()
        );
    }
}
#[test]
fn signed_manifest_package_id_must_equal_real_component_id() {
    let f = Fx::new();
    let mut p = f.pkg("0.97.0");
    let claims = ManifestClaims::new(
        p.op.artifact_identity().clone(),
        "com.example.other".into(),
        "14.0".into(),
        vec![],
        ArtifactIntegrity::of(&p.pkg),
    )
    .unwrap();
    p.manifest = sign_manifest(claims, &f.publisher).to_canonical_bytes();
    let TypedOperation::DeployClient(spec) = &mut p.op else {
        unreachable!()
    };
    spec.package_id = "com.example.other".into();
    spec.manifest_sha256 = ArtifactSha256::of(&p.manifest);
    assert!(
        f.installer()
            .inspect(&p.op, &p.manifest, &p.pkg, &f.host)
            .is_err()
    );
    assert_eq!(f.sys.calls.get(), 0);
}

#[test]
fn bom_content_checks_pin_posix_cksum_and_reject_size_or_checksum_divergence() {
    assert_eq!(structure::cksum(b"icns"), 4266274611);
    assert_eq!(structure::cksum(b"notices\n"), 1413671364);
    let payload = payload("0.97.0");
    let text: String = payload
        .iter()
        .map(|(n, e)| {
            let (size, crc) = if e.mode & 0o170000 == 0o40000 {
                (String::new(), String::new())
            } else {
                (
                    e.data.len().to_string(),
                    structure::cksum(&e.data).to_string(),
                )
            };
            format!(
                "{}\t{:o}\t{size}\troot\twheel\t{crc}\n",
                if n.is_empty() {
                    ".".into()
                } else {
                    format!("./{n}")
                },
                e.mode
            )
        })
        .collect();
    structure::bom_payload(&text, &payload).unwrap();
    assert!(structure::bom_payload(&text.replace("\troot\t", "0\troot\t"), &payload).is_err());
    let bad = text
        .lines()
        .map(|l| {
            let mut f: Vec<_> = l.split('\t').collect();
            if f[1] == "100644" {
                f[5] = "0";
            }
            f.join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(structure::bom_payload(&bad, &payload).is_err());
}
