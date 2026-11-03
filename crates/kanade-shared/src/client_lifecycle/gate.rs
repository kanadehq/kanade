//! Stable inode flock plus durable closure. Dropping a writer NEVER reopens it.
//! Only the transaction owner, after journal/receipt reconciliation, may finish.
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const DIRECTORY: &str = "/Library/Application Support/Kanade/client-lifecycle";
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Client launch gate closed or unverified",
    )
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    transaction: String,
    mutating: bool,
}

pub struct Gate {
    path: PathBuf,
    file: File,
    owner: u32,
}
pub struct Reader {
    gate: Option<Gate>,
    path: PathBuf,
    owner: u32,
    _bootstrap: Option<File>,
}
pub struct Writer {
    gate: Gate,
    _serial: File,
    transaction: String,
    bootstrap: Option<File>,
}
impl Gate {
    /// Every component is no-follow and administrator owned. Test roots must
    /// explicitly supply the test UID; production always supplies zero.
    pub fn open(path: &Path, owner: u32, create: bool) -> io::Result<Self> {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        if !path.is_absolute() {
            return Err(denied());
        }
        let mut dir = File::open("/")?;
        for part in path.components() {
            let Component::Normal(name) = part else {
                if part == Component::RootDir {
                    continue;
                }
                return Err(denied());
            };
            let name = CString::new(name.as_bytes())?;
            let mut created = false;
            if create {
                let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) };
                created = rc == 0;
                if rc != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error());
                }
            }
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            dir = unsafe { File::from_raw_fd(fd) };
            if created && unsafe { libc::fchmod(dir.as_raw_fd(), 0o755) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let m = dir.metadata()?;
            #[cfg(target_os = "macos")]
            super::macos::no_acl(&dir)?;
            // Root's sticky temporary ancestors are safe for isolated tests.
            if (m.uid() != 0 && m.uid() != owner)
                || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
            {
                return Err(denied());
            }
        }
        let m = dir.metadata()?;
        if m.uid() != owner || m.mode() & 0o022 != 0 {
            return Err(denied());
        }
        let file = open_file(&path.join("lease"), owner, create, false)?;
        Ok(Self {
            path: path.into(),
            file,
            owner,
        })
    }
    pub fn closed_transaction(&self) -> io::Result<Option<String>> {
        self.pinned()?;
        Ok(self.state()?.map(|s| s.transaction))
    }
    pub fn reader(self) -> io::Result<Reader> {
        lock(&self.file, libc::LOCK_SH)?;
        self.check_open()?;
        Ok(Reader {
            path: self.path.clone(),
            owner: self.owner,
            gate: Some(self),
            _bootstrap: None,
        })
    }
    fn state(&self) -> io::Result<Option<State>> {
        let f = match open_file(&self.path.join("closed.json"), self.owner, false, false) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        f.take(4097).read_to_end(&mut bytes)?;
        let s: State = super::decode_envelope(&bytes).map_err(|_| denied())?;
        if s.schema != 1 || s.transaction.is_empty() {
            return Err(denied());
        }
        Ok(Some(s))
    }
    pub fn check_open(&self) -> io::Result<()> {
        self.pinned()?;
        if self.state()?.is_some() {
            Err(denied())
        } else {
            Ok(())
        }
    }
    fn pinned(&self) -> io::Result<()> {
        let other = Gate::open(&self.path, self.owner, false)?;
        let a = self.file.metadata()?;
        let b = other.file.metadata()?;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            Ok(())
        } else {
            Err(denied())
        }
    }
    pub fn writer(self, transaction: &str) -> io::Result<Writer> {
        if transaction.is_empty() || transaction.len() > 512 {
            return Err(denied());
        }
        let serial = open_file(&self.path.join("owner"), self.owner, true, true)?;
        lock(&serial, libc::LOCK_EX)?;
        if self.state()?.is_some_and(|s| s.transaction != transaction) {
            return Err(denied());
        }
        let writer = Writer {
            gate: self,
            _serial: serial,
            transaction: transaction.into(),
            bootstrap: None,
        };
        // No installer intent has been recorded yet. A repeated writer must
        // preserve its durable mutating bit, including across reboot.
        if writer.gate.state()?.is_none() {
            writer.persist(false)?;
        }
        Ok(writer)
    }
}
impl Reader {
    /// Payload-only pkg migration: before root has provisioned the durable gate,
    /// pin a pre-existing administrator-owned directory. The writer must also
    /// exclude these readers before recording any installer mutation intent.
    pub fn startup(path: &Path, owner: u32, bootstrap: &Path) -> io::Result<Self> {
        let file = bootstrap_file(bootstrap, owner)?;
        lock(&file, libc::LOCK_SH)?;
        let gate = match Gate::open(path, owner, false) {
            Ok(gate) => Some(gate.reader()?.gate.unwrap()),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(path)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        let reader = Self {
            gate,
            path: path.into(),
            owner,
            _bootstrap: Some(file),
        };
        reader.recheck()?;
        Ok(reader)
    }
    pub fn recheck(&self) -> io::Result<()> {
        if let Some(gate) = &self.gate {
            return gate.check_open();
        }
        match Gate::open(&self.path, self.owner, false) {
            Ok(gate) => gate.check_open(),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&self.path)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
fn bootstrap_file(path: &Path, owner: u32) -> io::Result<File> {
    // Canonical equality excludes links in ancestors as well as the leaf.
    if !path.is_absolute() || path.canonicalize()? != path {
        return Err(denied());
    }
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    if m.uid() != owner || m.mode() & 0o022 != 0 {
        return Err(denied());
    }
    #[cfg(target_os = "macos")]
    super::macos::no_acl(&f)?;
    Ok(f)
}
impl Writer {
    pub fn with_bootstrap(mut self, path: &Path, owner: u32) -> io::Result<Self> {
        self.bootstrap = Some(bootstrap_file(path, owner)?);
        Ok(self)
    }
    fn persist(&self, mutating: bool) -> io::Result<()> {
        self.gate.pinned()?;
        let tmp = self
            .gate
            .path
            .join(format!(".closed-{}", uuid::Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        if unsafe { libc::fchmod(f.as_raw_fd(), 0o644) } != 0 {
            return Err(io::Error::last_os_error());
        }
        f.write_all(&super::encode_envelope(&State {
            schema: 1,
            transaction: self.transaction.clone(),
            mutating,
        })?)?;
        durable_sync(&f)?;
        std::fs::rename(tmp, self.gate.path.join("closed.json"))?;
        durable_sync(&File::open(&self.gate.path)?)
    }
    /// Called only after positively identified GUI processes have stopped.
    pub fn exclusive(&self) -> io::Result<()> {
        self.recheck()?;
        if let Some(file) = &self.bootstrap {
            lock(file, libc::LOCK_EX)?;
        }
        lock(&self.gate.file, libc::LOCK_EX)
    }
    pub fn recheck(&self) -> io::Result<()> {
        self.gate.pinned()?;
        if self
            .gate
            .state()?
            .is_some_and(|s| s.transaction == self.transaction)
        {
            Ok(())
        } else {
            Err(denied())
        }
    }
    pub fn mutation_recorded(&self) -> io::Result<bool> {
        self.recheck()?;
        Ok(self.gate.state()?.ok_or_else(denied)?.mutating)
    }
    pub fn mutation_intent(&self) -> io::Result<()> {
        self.exclusive()?;
        self.persist(true)
    }
    /// No timeout, PID disappearance or changed boot is evidence of completion.
    /// The caller must prove terminal journal AND installed/previous contents.
    pub fn finish(self) -> io::Result<()> {
        self.recheck()?;
        std::fs::remove_file(self.gate.path.join("closed.json"))?;
        durable_sync(&File::open(&self.gate.path)?)
    }
}
fn lock(f: &File, mode: i32) -> io::Result<()> {
    if unsafe { libc::flock(f.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn open_file(path: &Path, owner: u32, create: bool, write: bool) -> io::Result<File> {
    let opened = OpenOptions::new()
        .read(true)
        .write(write || create)
        .create_new(create)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path);
    let f = match opened {
        Ok(f) => f,
        Err(e) if create && e.kind() == io::ErrorKind::AlreadyExists => {
            return open_file(path, owner, false, write);
        }
        Err(e) => return Err(e),
    };
    if create {
        if unsafe { libc::fchmod(f.as_raw_fd(), 0o644) } != 0 {
            return Err(io::Error::last_os_error());
        }
        durable_sync(&f)?;
        durable_sync(&File::open(path.parent().ok_or_else(denied)?)?)?;
    }
    let m = f.metadata()?;
    #[cfg(target_os = "macos")]
    super::macos::no_acl(&f)?;
    if !m.is_file() || m.nlink() != 1 || m.uid() != owner || m.mode() & 0o022 != 0 {
        return Err(denied());
    }
    Ok(f)
}

/// fsync plus the macOS drive-cache barrier for persistent ownership evidence.
pub fn durable_sync(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
        return Ok(());
    }
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn open(root: &Path) -> Gate {
        Gate::open(root, unsafe { libc::geteuid() }, true).unwrap()
    }
    #[test]
    fn payload_only_startup_and_first_provisioning_share_exclusion() {
        let t = tempfile::tempdir().unwrap();
        let bootstrap = t.path().canonicalize().unwrap();
        let root = bootstrap.join("new-gate");
        let owner = unsafe { libc::geteuid() };
        let reader = Reader::startup(&root, owner, &bootstrap).unwrap();
        reader.recheck().unwrap();
        let writer = open(&root)
            .writer("first-update")
            .unwrap()
            .with_bootstrap(&bootstrap, owner)
            .unwrap();
        assert!(reader.recheck().is_err());
        assert!(Reader::startup(&root, owner, &bootstrap).is_err());
        assert!(writer.mutation_intent().is_err());
        drop(reader);
        writer.mutation_intent().unwrap();
        drop(writer);
        // Neither a crash nor a new Client process discards durable exclusion.
        assert!(Reader::startup(&root, owner, &bootstrap).is_err());
        let recovery = open(&root)
            .writer("first-update")
            .unwrap()
            .with_bootstrap(&bootstrap, owner)
            .unwrap();
        recovery.finish().unwrap();
        Reader::startup(&root, owner, &bootstrap).unwrap();
        // A partially provisioned directory never becomes an absent-gate bypass.
        std::fs::remove_file(root.join("lease")).unwrap();
        assert!(Reader::startup(&root, owner, &bootstrap).is_err());
    }
    #[test]
    fn newly_created_directories_are_readable_with_private_umask() {
        const CHILD: &str = "KANADE_GATE_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "client_lifecycle::gate::tests::newly_created_directories_are_readable_with_private_umask"])
                .env(CHILD, "1").output().unwrap();
            // Keep nested harness counts out of the required e2e parser.
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // This process runs only this test; never change the suite's umask.
        unsafe {
            libc::umask(0o077);
        }
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap().join("one/two/gate");
        let gate = open(&root);
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::metadata(root.parent().unwrap()).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(gate.file.metadata().unwrap().mode() & 0o777, 0o644);
        gate.reader().unwrap();
    }
    #[test]
    fn closure_precedes_exclusion_and_crash_never_releases_intent() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap().join("gate");
        let reader = open(&root).reader().unwrap();
        let writer = open(&root).writer("execution-1").unwrap();
        assert!(reader.recheck().is_err());
        assert!(open(&root).reader().is_err());
        assert!(writer.exclusive().is_err());
        assert!(open(&root).writer("execution-1").is_err());
        drop(reader);
        writer.mutation_intent().unwrap();
        drop(writer);
        assert!(open(&root).reader().is_err());
        assert!(open(&root).writer("execution-2").is_err());
        let recovery = open(&root).writer("execution-1").unwrap();
        recovery.exclusive().unwrap();
        recovery.finish().unwrap();
        assert!(open(&root).reader().is_ok());
    }
    #[test]
    fn modified_transaction_cannot_acquire_or_release_gate() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap().join("gate");
        let writer = open(&root).writer("original").unwrap();
        drop(writer);
        let path = root.join("closed.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state["body"]["transaction"] = "forged".into();
        std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(open(&root).reader().is_err());
        assert!(open(&root).writer("forged").is_err());
        assert!(open(&root).writer("original").is_err());
    }
    #[test]
    fn planted_links_and_replaced_lock_inode_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap().join("gate");
        let gate = open(&root);
        std::fs::rename(root.join("lease"), root.join("old")).unwrap();
        let _ = open(&root);
        assert!(gate.reader().is_err());
        std::fs::remove_file(root.join("lease")).unwrap();
        std::os::unix::fs::symlink(root.join("old"), root.join("lease")).unwrap();
        assert!(Gate::open(&root, unsafe { libc::geteuid() }, false).is_err());
    }
}
