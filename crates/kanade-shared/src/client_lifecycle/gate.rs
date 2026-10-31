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
pub struct Reader(Gate);
pub struct Writer {
    gate: Gate,
    _serial: File,
    transaction: String,
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
            if created && unsafe { libc::fchmod(dir.as_raw_fd(), 0o755) } != 0 { return Err(io::Error::last_os_error()); }
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
        Ok(Reader(self))
    }
    fn state(&self) -> io::Result<Option<State>> {
        let f = match open_file(&self.path.join("closed.json"), self.owner, false, false) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        f.take(4097).read_to_end(&mut bytes)?;
        let s: State = serde_json::from_slice(&bytes).map_err(|_| denied())?;
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
    pub fn recheck(&self) -> io::Result<()> {
        self.0.check_open()
    }
}
impl Writer {
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
        if unsafe { libc::fchmod(f.as_raw_fd(), 0o644) } != 0 { return Err(io::Error::last_os_error()); }
        f.write_all(&serde_json::to_vec(&State {
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
    let opened = OpenOptions::new().read(true).write(write || create)
        .create_new(create).mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(path);
    let f = match opened {
        Ok(f) => f,
        Err(e) if create && e.kind() == io::ErrorKind::AlreadyExists => return open_file(path, owner, false, write),
        Err(e) => return Err(e),
    };
    if create {
        if unsafe { libc::fchmod(f.as_raw_fd(), 0o644) } != 0 { return Err(io::Error::last_os_error()); }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn open(root: &Path) -> Gate {
        Gate::open(root, unsafe { libc::geteuid() }, true).unwrap()
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

/// fsync plus the macOS drive-cache barrier for persistent ownership evidence.
pub fn durable_sync(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 { return Ok(()); }
    file.sync_all()
}
