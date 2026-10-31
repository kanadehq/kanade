//! Fixed OS commands. No shell, PATH search, GUI prompt, or process termination
//! API. The native coordination adapter refuses because a common launch gate
//! has not yet been implemented; observing an empty process list is inadequate.
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::structure::{self, BUNDLE, COMPONENT, require};
use super::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsReceipt {
    pub package_id: String,
    pub version: String,
    pub install_location: String,
    pub volume: String,
    pub files: Vec<String>,
}
impl OsReceipt {
    pub(super) fn verify(&self, version: &str) -> Result<()> {
        let mut files = self.files.clone();
        // pkgutil exposes AppleDouble carrier names from the receipt BOM even
        // though installer applies them as xattrs, not as bundle files.
        for name in &self.files {
            let (head, leaf) = name
                .rsplit_once('/')
                .map_or(("", name.as_str()), |(a, b)| (a, b));
            if let Some(leaf) = leaf.strip_prefix("._") {
                let target = if head.is_empty() {
                    leaf.to_owned()
                } else {
                    format!("{head}/{leaf}")
                };
                require(
                    self.files.contains(&target),
                    "orphan receipt AppleDouble entry",
                )?;
                files.retain(|f| f != name);
            }
        }
        require(
            self.files
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == self.files.len(),
            "duplicate receipt file",
        )?;
        files.sort();
        let mut expected: Vec<_> = structure::expected_modes()
            .keys()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect();
        expected.sort();
        require(
            self.package_id == COMPONENT
                && self.version == version
                && self.install_location == "Applications"
                && self.volume == "/"
                && files == expected,
            "OS receipt identity/location/files",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerExit {
    Succeeded,
    /// Adapter positively proved that the OS transaction has ended.
    FailedQuiescent(String),
    /// Timeout/output cap/unknown worker lifetime: never layer rollback on it.
    Uncertain(String),
}

/// Held across install AND validation/rollback. Implementations must inhibit
/// *all* managed launch paths, preserve Quit/opt-out, and prove absence of
/// active instances by executable path, PID/start time and UID. No stop API is
/// exposed: uncoordinated or active Client processes cause a deferral.
pub trait LaunchLease {
    fn recheck(&self) -> Result<()>;
    fn mutation_intent(&self) -> Result<()> {
        self.recheck()
    }
    fn mutation_recorded(&self) -> Result<Option<bool>> {
        Ok(None)
    }
    fn native(&self) -> Option<&super::lifecycle::NativeLease<'_>> {
        None
    }
    fn finish(self: Box<Self>) -> Result<super::result::GuiActivation> {
        Ok(super::result::GuiActivation::UnsupportedNotAttempted)
    }
}

/// Trusted OS boundary; deterministic tests implement it against isolated
/// filesystems. Methods must be bounded and must never evaluate shell text.
pub trait System {
    fn check_roots(&self, roots: &crate::native_install::model::Roots) -> Result<()> {
        roots.validate_offline()?;
        self.check_target(&roots.install_dir)
    }
    fn check_target(&self, applications: &Path) -> Result<()>;
    fn bom(&self, path: &Path) -> Result<String>;
    fn signature(&self, app: &Path) -> Result<()>;
    fn receipt(&self) -> Result<Option<OsReceipt>>;
    fn coordinate(&self, transaction: &str, recovery: bool) -> Result<Box<dyn LaunchLease + '_>>;
    fn resume(&self, _transaction: &str) -> Result<Option<Box<dyn LaunchLease + '_>>> {
        Ok(None)
    }
    fn install(&self, pkg: &Path, lease: &dyn LaunchLease) -> Result<InstallerExit>;
    /// Restart recovery must establish OS transaction quiescence independently
    /// of the former installer's PID. Native support is deliberately absent.
    fn recovery_quiescent(&self, installer_returned: bool) -> Result<()>;
    fn checkpoint(&self, _: super::transaction::Phase) -> Result<()> {
        Ok(())
    }
}

/// Read-only validation uses fixed macOS executables. Installation remains
/// unreachable with this adapter until trustworthy launch coordination exists.
/// The target is an isolated temporary image root, never the operator's `/`.
pub struct NativeSystem {
    pub(super) target: PathBuf,
}
impl NativeSystem {
    /// Local authenticated executor boundary only; no CLI/remote route calls
    /// this constructor. It grants neither deployment approval nor admission.
    pub fn managed_host() -> Result<Self> {
        require(
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && unsafe { libc::geteuid() } == 0,
            "managed Client installer requires native macOS root",
        )?;
        Ok(Self {
            target: PathBuf::from("/"),
        })
    }
    pub fn for_image(target: PathBuf) -> Result<Self> {
        let root = target.canonicalize()?;
        let temp = std::env::temp_dir().canonicalize()?;
        require(
            root.starts_with(&temp) && root != temp && root == target,
            "native adapter requires canonical temporary image root",
        )?;
        Ok(Self { target })
    }
}

#[derive(Debug)]
struct Output {
    stdout: String,
    stderr: String,
    success: bool,
}
// A single loop drains both streams without blocking, including after the
// direct child exits. On failure the installer worker lifetime remains unknown.
fn run(program: &'static str, args: &[&std::ffi::OsStr], timeout: Duration) -> Result<Output> {
    require(cfg!(target_os = "macos"), "native pkg tools require macOS")?;
    run_bounded(program, args, timeout)
}

// Kept separate from the macOS admission above so tests exercise the exact
// runner with harmless Unix tools, without enabling native pkg execution.
fn run_bounded(program: &str, args: &[&std::ffi::OsStr], timeout: Duration) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        // SAFETY: both pipe descriptors remain owned throughout the read loop.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Deferred("cannot bound command output".into()));
        }
    }
    let mut out = [Vec::new(), Vec::new()];
    let mut eof = [false, false];
    let start = Instant::now();
    let mut status = None;
    let result = 'command: loop {
        if start.elapsed() >= timeout {
            break Err(Error::Deferred(format!(
                "{program}: timeout; OS transaction lifetime unknown"
            )));
        }
        for (i, reader) in [&mut stdout as &mut dyn Read, &mut stderr as &mut dyn Read]
            .into_iter()
            .enumerate()
        {
            let mut b = [0; 4096];
            match reader.read(&mut b) {
                Ok(0) => eof[i] = true,
                Ok(n) => out[i].extend_from_slice(&b[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e) => break 'command Err(Error::Io(e)),
            }
        }
        if out.iter().map(Vec::len).sum::<usize>() > 128 * 1024 {
            break Err(Error::Deferred(
                "OS command output limit; transaction lifetime unknown".into(),
            ));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(s) => status = s,
                Err(e) => break Err(Error::Io(e)),
            }
        }
        if eof.iter().all(|b| *b)
            && let Some(s) = status
        {
            break Ok(Output {
                stdout: String::from_utf8_lossy(&out[0]).into_owned(),
                stderr: String::from_utf8_lossy(&out[1]).into_owned(),
                success: s.success(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

impl System for NativeSystem {
    fn check_roots(&self, roots: &crate::native_install::model::Roots) -> Result<()> {
        if self.target == Path::new("/") {
            require(
                roots.install_dir == Path::new("/Applications")
                    && roots.deploy_dir == Path::new("/Library/Application Support/Kanade/deploy")
                    && roots.owner == crate::native_install::fs::Owner::ROOT_WHEEL,
                "fixed managed Client layout required",
            )?;
            kanade_shared::client_lifecycle::gate::Gate::open(&self.launch_gate(), 0, true)?;
        } else {
            roots.validate_offline()?;
        }
        self.check_target(&roots.install_dir)
    }
    fn check_target(&self, applications: &Path) -> Result<()> {
        require(
            applications == self.target.join("Applications")
                && self.target.canonicalize()? == self.target,
            "native installer and handler roots differ",
        )
    }
    fn bom(&self, path: &Path) -> Result<String> {
        require(path.is_absolute(), "BOM path")?;
        let out = run(
            "/usr/bin/lsbom",
            &["-p".as_ref(), "fmsUGc".as_ref(), path.as_os_str()],
            Duration::from_secs(10),
        )?;
        require(out.success, "lsbom failed")?;
        Ok(out.stdout)
    }
    fn signature(&self, app: &Path) -> Result<()> {
        require(app.is_absolute(), "app path")?;
        let out = run(
            "/usr/bin/codesign",
            &[
                "--verify".as_ref(),
                "--deep".as_ref(),
                "--strict".as_ref(),
                app.as_os_str(),
            ],
            Duration::from_secs(20),
        )?;
        require(out.success, "codesign strict verification failed")?;
        let out = run(
            "/usr/bin/codesign",
            &["-d".as_ref(), "--verbose=4".as_ref(), app.as_os_str()],
            Duration::from_secs(10),
        )?;
        require(
            out.success
                && out.stderr.lines().any(|s| s == "Signature=adhoc")
                && out
                    .stderr
                    .lines()
                    .any(|s| s == format!("Identifier={BUNDLE}")),
            "ad-hoc signature identity",
        )
    }
    fn receipt(&self) -> Result<Option<OsReceipt>> {
        // --pkgs gives a positive absence observation. A failed info lookup is
        // not treated as absence (database corruption/permissions may cause it).
        let base = ["--volume".as_ref(), self.target.as_os_str()];
        let out = run(
            "/usr/sbin/pkgutil",
            &[base[0], base[1], "--pkgs".as_ref()],
            Duration::from_secs(10),
        )?;
        require(out.success, "pkgutil database listing failed")?;
        if !out.stdout.lines().any(|s| s == COMPONENT) {
            return Ok(None);
        }
        let out = run(
            "/usr/sbin/pkgutil",
            &[
                base[0],
                base[1],
                "--pkg-info-plist".as_ref(),
                COMPONENT.as_ref(),
            ],
            Duration::from_secs(10),
        )?;
        require(out.success, "pkgutil info failed")?;
        let text = out.stdout.replace("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">","");
        require(
            !text.contains("<!ENTITY") && !text.contains("<!DOCTYPE"),
            "receipt XML declaration",
        )?;
        let doc = roxmltree::Document::parse(&text).map_err(|e| Error::Invalid(e.to_string()))?;
        let get = |key| -> Result<String> {
            let mut found = doc
                .descendants()
                .filter(|n| n.has_tag_name("key") && n.text() == Some(key));
            let n = found
                .next()
                .and_then(|n| n.next_sibling_element())
                .ok_or_else(|| Error::Invalid("receipt field missing".into()))?;
            require(
                found.next().is_none() && n.has_tag_name("string"),
                "receipt field ambiguous",
            )?;
            Ok(n.text().unwrap_or("").to_owned())
        };
        let files = run(
            "/usr/sbin/pkgutil",
            &[base[0], base[1], "--files".as_ref(), COMPONENT.as_ref()],
            Duration::from_secs(10),
        )?;
        require(files.success, "pkgutil files failed")?;
        Ok(Some(OsReceipt {
            package_id: get("pkgid")?,
            version: get("pkg-version")?,
            install_location: get("install-location")?,
            volume: get("volume")?,
            files: files.stdout.lines().map(str::to_owned).collect(),
        }))
    }
    fn coordinate(&self, transaction: &str, recovery: bool) -> Result<Box<dyn LaunchLease + '_>> {
        self.coordinate_native(transaction, recovery)
    }
    fn resume(&self, transaction: &str) -> Result<Option<Box<dyn LaunchLease + '_>>> {
        use kanade_shared::client_lifecycle::gate::Gate;
        let gate = match Gate::open(&self.launch_gate(), 0, false) {
            Ok(g) => g,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match gate.closed_transaction()? {
            None => Ok(None),
            Some(owner) if owner == transaction => {
                self.coordinate_native(transaction, true).map(Some)
            }
            _ => Err(Error::RecoveryRequired(
                "launch gate owner differs from durable journal".into(),
            )),
        }
    }
    fn install(&self, pkg: &Path, lease: &dyn LaunchLease) -> Result<InstallerExit> {
        require(pkg.is_absolute(), "installer pkg path")?;
        let native = lease.native().ok_or_else(|| {
            Error::Deferred("native installer requires its acquired transaction lease".into())
        })?;
        require(
            std::ptr::eq(native.system, self),
            "foreign transaction lease",
        )?;
        lease.mutation_intent()?;
        let parts = structure::xar(&std::fs::read(pkg)?)?;
        let payload = structure::payload(&parts[&format!("{}/Payload", structure::PART)])?;
        structure::lifecycle_participant(
            &payload[&format!("{}/Contents/Info.plist", structure::APP)].data,
        )?;
        match run(
            "/usr/sbin/installer",
            &[
                "-pkg".as_ref(),
                pkg.as_os_str(),
                "-target".as_ref(),
                self.target.as_os_str(),
            ],
            Duration::from_secs(300),
        ) {
            Ok(out) if out.success => Ok(InstallerExit::Succeeded),
            Ok(out) => Ok(InstallerExit::Uncertain(format!(
                "installer failure; worker quiescence unknown: {}",
                out.stderr
            ))),
            Err(e) => Ok(InstallerExit::Uncertain(e.to_string())),
        }
    }
    fn recovery_quiescent(&self, installer_returned: bool) -> Result<()> {
        if installer_returned {
            // A durable InstallerReturned journal proves our successful child
            // returned. Still exclude any currently observed installer before
            // interpreting its receipt; unknown intent never takes this path.
            let snapshot =
                super::processes::ProcessView::snapshot(&super::processes::NativeProcessView {
                    client_exe: self.executable(),
                })?;
            if !snapshot.installer_active && snapshot.ambiguous.is_empty() {
                return Ok(());
            }
        }
        Err(Error::RecoveryRequired(
            "interrupted OS installer transaction quiescence cannot be proven".into(),
        ))
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;

    #[test]
    fn real_runner_bounds_time_and_combined_output() {
        let start = Instant::now();
        let timeout = run_bounded("/bin/sleep", &["30".as_ref()], Duration::from_millis(100));
        assert!(matches!(timeout, Err(Error::Deferred(ref s)) if s.contains("timeout")));
        assert!(start.elapsed() < Duration::from_secs(5));
        let start = Instant::now();
        let output = run_bounded("/usr/bin/yes", &[], Duration::from_secs(10));
        assert!(matches!(output, Err(Error::Deferred(ref s)) if s.contains("output limit")));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn real_runner_clears_environment_and_preserves_literal_argv() {
        let env = run_bounded("/usr/bin/env", &[], Duration::from_secs(5)).unwrap();
        assert!(env.success);
        assert!(env.stdout.is_empty());
        assert!(env.stderr.is_empty());
        let literal = "a b;$(id)\n*.pkg";
        let output = run_bounded(
            "/usr/bin/printf",
            &["%s".as_ref(), literal.as_ref()],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, literal);
        assert!(output.stderr.is_empty());
        assert!(
            !run_bounded("/usr/bin/false", &[], Duration::from_secs(5))
                .unwrap()
                .success
        );
        assert!(matches!(
            run_bounded("/nonexistent/tool", &[], Duration::from_secs(1)),
            Err(Error::Io(_))
        ));
    }
}
