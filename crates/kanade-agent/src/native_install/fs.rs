//! Race-resistant Unix file primitives for the installer.
//!
//! Everything operates relative to an already-opened directory handle with
//! `O_NOFOLLOW`, and every decision about a file is taken from `fstat` of the
//! descriptor that is then read — never from a path that could be swapped
//! between a check and a use. Absolute paths are walked component by
//! component, refusing symlinks and directories another principal could
//! rewrite.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Component, Path};

/// Who must own everything the installer creates or trusts.
/// Production is `root:wheel` (0:0); tests use the invoking user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    pub const ROOT_WHEEL: Owner = Owner { uid: 0, gid: 0 };

    /// The effective user and group of this process (for isolated tests).
    pub fn current() -> Owner {
        // SAFETY: geteuid/getegid have no preconditions and cannot fail.
        unsafe {
            Owner {
                uid: libc::geteuid(),
                gid: libc::getegid(),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// Identity-bearing facts about one file, taken from `fstat`/`fstatat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub size: u64,
}

impl Stat {
    fn from_raw(st: &libc::stat) -> Stat {
        let fmt = st.st_mode as u32 & libc::S_IFMT as u32;
        let kind = match fmt {
            x if x == libc::S_IFREG as u32 => Kind::Regular,
            x if x == libc::S_IFDIR as u32 => Kind::Directory,
            x if x == libc::S_IFLNK as u32 => Kind::Symlink,
            _ => Kind::Other,
        };
        Stat {
            kind,
            uid: st.st_uid,
            gid: st.st_gid,
            mode: st.st_mode as u32 & 0o7777,
            dev: st.st_dev as u64,
            ino: st.st_ino,
            nlink: st.st_nlink as u64,
            size: st.st_size.max(0) as u64,
        }
    }

    pub fn same_file(&self, other: &Stat) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

/// How `rename_at` treats an existing destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    /// POSIX `rename`: atomically replace the destination.
    Replace,
    /// Fail with `EEXIST` rather than replace.
    NoReplace,
    /// Atomically exchange the two names; both must exist.
    Swap,
}

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in file name"))
}

fn component(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a single path component",
        ));
    }
    cstr(name)
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Flush a descriptor to stable storage (`F_FULLFSYNC` on macOS, where a bare
/// `fsync` only reaches the drive cache).
pub fn sync_fd(fd: RawFd) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain fcntl on a descriptor we own.
        if unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    // SAFETY: fsync on a valid descriptor.
    check(unsafe { libc::fsync(fd) }).map(|_| ())
}

/// An open directory; all operations are relative to it.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
}

impl Dir {
    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Open an absolute directory path component by component without ever
    /// following a symlink. Missing components are created with
    /// `create_mode` when given. Every directory on the way must be owned by
    /// root or `owner` and not writable by group/other (sticky directories
    /// are tolerated, since entries in them cannot be renamed by others).
    pub fn open_absolute(path: &Path, create_mode: Option<u32>, owner: Owner) -> io::Result<Dir> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "install root must be absolute",
            ));
        }
        let mut cur = Dir::open_root()?;
        for c in path.components() {
            match c {
                Component::RootDir => {}
                Component::Normal(n) => {
                    let name = n.to_str().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path component")
                    })?;
                    let next = cur.open_dir(name, create_mode, owner)?;
                    next.require_trusted_ancestor(owner)?;
                    cur = next;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "install root contains . or .. components",
                    ));
                }
            }
        }
        Ok(cur)
    }

    fn open_root() -> io::Result<Dir> {
        let root = CString::new("/").expect("static");
        // SAFETY: valid NUL-terminated path; result checked.
        let fd = check(unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: fd is freshly opened and owned by nobody else.
        Ok(Dir {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    fn require_trusted_ancestor(&self, owner: Owner) -> io::Result<()> {
        let st = self.stat_self()?;
        let sticky = st.mode & 0o1000 != 0;
        let writable_by_others = st.mode & 0o022 != 0;
        if (st.uid != 0 && st.uid != owner.uid) || (writable_by_others && !sticky) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory on the install path is not trusted (owner or group/world-writable)",
            ));
        }
        Ok(())
    }

    /// Open (optionally creating) a child directory, never following a link.
    pub fn open_dir(&self, name: &str, create_mode: Option<u32>, owner: Owner) -> io::Result<Dir> {
        let c = component(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: dirfd is valid for the life of self; c is NUL-terminated.
        let mut fd = unsafe { libc::openat(self.raw(), c.as_ptr(), flags) };
        if fd < 0 {
            let err = io::Error::last_os_error();
            match (err.kind(), create_mode) {
                (io::ErrorKind::NotFound, Some(mode)) => {
                    // SAFETY: as above.
                    let made =
                        unsafe { libc::mkdirat(self.raw(), c.as_ptr(), mode as libc::mode_t) };
                    if made < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::AlreadyExists {
                            // Lost a creation race with another installer: use theirs.
                            return self.open_dir(name, None, owner);
                        }
                        return Err(e);
                    }
                    // SAFETY: as above.
                    fd = check(unsafe { libc::openat(self.raw(), c.as_ptr(), flags) })?;
                    // SAFETY: fd just opened.
                    let d = Dir {
                        fd: unsafe { OwnedFd::from_raw_fd(fd) },
                    };
                    if owner.uid == unsafe { libc::geteuid() } {
                        // mkdirat is masked by umask; make the mode exact.
                        check(unsafe { libc::fchmod(d.raw(), mode as libc::mode_t) })?;
                    } else {
                        d.chown(owner)?;
                        check(unsafe { libc::fchmod(d.raw(), mode as libc::mode_t) })?;
                    }
                    return Ok(d);
                }
                (_, _) => {
                    // ELOOP/ENOTDIR for a symlink or non-directory surfaces here.
                    return Err(err);
                }
            }
        }
        // SAFETY: fd just opened.
        Ok(Dir {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn stat_self(&self) -> io::Result<Stat> {
        reject_extended_acl(self.raw())?;
        let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: valid fd and out-pointer.
        check(unsafe { libc::fstat(self.raw(), st.as_mut_ptr()) })?;
        // SAFETY: initialised by fstat on success.
        Ok(Stat::from_raw(&unsafe { st.assume_init() }))
    }

    /// `lstat` semantics: a symlink is reported as a symlink.
    pub fn stat(&self, name: &str) -> io::Result<Option<Stat>> {
        let c = component(name)?;
        let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: valid fd, NUL-terminated name, out-pointer.
        let rc = unsafe {
            libc::fstatat(
                self.raw(),
                c.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // SAFETY: initialised on success.
        Ok(Some(Stat::from_raw(&unsafe { st.assume_init() })))
    }

    /// Open a regular file for reading without following a link, returning
    /// the `fstat` of the *opened* descriptor.
    pub fn open_read(&self, name: &str) -> io::Result<(File, Stat)> {
        let c = component(name)?;
        // SAFETY: valid fd and name.
        let fd = check(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        })?;
        // SAFETY: fd freshly opened.
        let file = unsafe { File::from_raw_fd(fd) };
        let st = fstat_file(&file)?;
        if st.kind != Kind::Regular {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular file",
            ));
        }
        Ok((file, st))
    }

    /// Read a whole regular file, at most `max` bytes, plus its `fstat`.
    pub fn read_file(&self, name: &str, max: u64) -> io::Result<(Vec<u8>, Stat)> {
        let (file, st) = self.open_read(name)?;
        if st.size > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
        }
        let mut buf = Vec::with_capacity(st.size as usize);
        file.take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
        }
        Ok((buf, st))
    }

    /// Create a new file (`O_EXCL`, no link following) with an exact mode.
    pub fn create_new(&self, name: &str, mode: u32, owner: Owner) -> io::Result<File> {
        let c = component(name)?;
        // SAFETY: valid fd and name.
        let fd = check(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })?;
        // SAFETY: fd freshly opened.
        let file = unsafe { File::from_raw_fd(fd) };
        let st = fstat_file(&file)?;
        if st.uid != owner.uid || st.gid != owner.gid {
            // Created by us, but not as the required principal (e.g. group
            // inherited from the directory): fix it, or fail.
            // SAFETY: valid fd.
            check(unsafe { libc::fchown(file.as_raw_fd(), owner.uid, owner.gid) })?;
        }
        // SAFETY: valid fd.
        check(unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })?;
        Ok(file)
    }

    /// Write `bytes` to a brand-new file, flush it, and close it.
    pub fn write_new(&self, name: &str, bytes: &[u8], mode: u32, owner: Owner) -> io::Result<()> {
        let mut f = self.create_new(name, mode, owner)?;
        let res = f.write_all(bytes).and_then(|_| sync_fd(f.as_raw_fd()));
        if res.is_err() {
            drop(f);
            let _ = self.unlink(name);
        }
        res
    }

    pub fn unlink(&self, name: &str) -> io::Result<()> {
        let c = component(name)?;
        // SAFETY: valid fd and name; AT_REMOVEDIR not set, so directories are refused.
        check(unsafe { libc::unlinkat(self.raw(), c.as_ptr(), 0) }).map(|_| ())
    }

    /// Remove `name` if it exists; absence is not an error.
    pub fn remove_if_exists(&self, name: &str) -> io::Result<()> {
        match self.unlink(name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    pub fn fsync(&self) -> io::Result<()> {
        sync_fd(self.raw())
    }

    fn chown(&self, owner: Owner) -> io::Result<()> {
        // SAFETY: valid fd.
        check(unsafe { libc::fchown(self.raw(), owner.uid, owner.gid) }).map(|_| ())
    }

    /// Rename `from` in `self` to `to` in `to_dir`, atomically, with the
    /// requested treatment of an existing destination.
    pub fn rename_to(
        &self,
        from: &str,
        to_dir: &Dir,
        to: &str,
        mode: RenameMode,
    ) -> io::Result<()> {
        let f = component(from)?;
        let t = component(to)?;
        match mode {
            RenameMode::Replace => {
                // SAFETY: valid fds and names.
                check(unsafe { libc::renameat(self.raw(), f.as_ptr(), to_dir.raw(), t.as_ptr()) })
                    .map(|_| ())
            }
            RenameMode::NoReplace | RenameMode::Swap => {
                rename_flagged(self.raw(), &f, to_dir.raw(), &t, mode)
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn rename_flagged(
    from_fd: RawFd,
    from: &CString,
    to_fd: RawFd,
    to: &CString,
    mode: RenameMode,
) -> io::Result<()> {
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    const RENAME_SWAP: libc::c_uint = 0x2;
    const RENAME_EXCL: libc::c_uint = 0x4;
    let flags = if mode == RenameMode::Swap {
        RENAME_SWAP
    } else {
        RENAME_EXCL
    };
    // SAFETY: valid fds and NUL-terminated names.
    check(unsafe { renameatx_np(from_fd, from.as_ptr(), to_fd, to.as_ptr(), flags) }).map(|_| ())
}

#[cfg(target_os = "linux")]
fn rename_flagged(
    from_fd: RawFd,
    from: &CString,
    to_fd: RawFd,
    to: &CString,
    mode: RenameMode,
) -> io::Result<()> {
    let flags: libc::c_uint = if mode == RenameMode::Swap { 2 } else { 1 };
    // SAFETY: valid fds and NUL-terminated names; renameat2 takes exactly these.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_fd,
            from.as_ptr(),
            to_fd,
            to.as_ptr(),
            flags,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_flagged(_: RawFd, _: &CString, _: RawFd, _: &CString, _: RenameMode) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic exclusive/swap rename is not available on this platform",
    ))
}

#[cfg(target_os = "macos")]
fn reject_extended_acl(fd: RawFd) -> io::Result<()> {
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            entry_id: i32,
            entry: *mut *mut libc::c_void,
        ) -> i32;
        fn acl_free(ptr: *mut libc::c_void) -> i32;
    }
    // SAFETY: fd is held open by the caller. ACL storage is freed once, after
    // examining its first entry; preserve errno before acl_free can change it.
    unsafe {
        let acl = acl_get_fd_np(fd, 0x100); // ACL_TYPE_EXTENDED
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let mut entry = std::ptr::null_mut();
        let result = acl_get_entry(acl, 0, &mut entry); // ACL_FIRST_ENTRY
        let error = io::Error::last_os_error();
        acl_free(acl);
        if result == -1 && error.raw_os_error() == Some(libc::EINVAL) {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "extended ACL or ACL inspection failure",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn reject_extended_acl(_: RawFd) -> io::Result<()> {
    Ok(())
}

pub fn fstat_file(f: &File) -> io::Result<Stat> {
    reject_extended_acl(f.as_raw_fd())?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: valid fd and out-pointer.
    check(unsafe { libc::fstat(f.as_raw_fd(), st.as_mut_ptr()) })?;
    // SAFETY: initialised on success.
    Ok(Stat::from_raw(&unsafe { st.assume_init() }))
}

/// An exclusive advisory lock on a file in a private directory. Released when
/// dropped — including when the owning process dies.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Try to take the lock until `deadline`; returns `Ok(None)` on timeout.
    pub fn acquire(
        dir: &Dir,
        name: &str,
        owner: Owner,
        wait: std::time::Duration,
    ) -> io::Result<Option<FileLock>> {
        // Concurrent O_CREAT of the same name can report a spurious ENOENT on
        // some filesystems (observed on APFS); the directory handle is valid,
        // so a short bounded retry is safe.
        let mut tries = 0;
        let file = loop {
            match dir.open_lock_file(name, owner) {
                Ok(f) => break f,
                Err(e) if e.kind() == io::ErrorKind::NotFound && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        };
        let deadline = std::time::Instant::now() + wait;
        loop {
            // SAFETY: valid fd.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(FileLock { _file: file }));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::WouldBlock {
                return Err(e);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Dir {
    fn open_lock_file(&self, name: &str, owner: Owner) -> io::Result<File> {
        let c = component(name)?;
        // SAFETY: valid fd and name. O_NOFOLLOW + fstat checks below stop a
        // planted symlink or foreign file from becoming our lock.
        let fd = check(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })?;
        // SAFETY: freshly opened.
        let file = unsafe { File::from_raw_fd(fd) };
        let st = fstat_file(&file)?;
        if st.kind != Kind::Regular || st.nlink != 1 || st.uid != owner.uid || st.mode & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lock file is not a private regular file owned by the installer principal",
            ));
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, Dir) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let d = Dir::open_absolute(&p, None, Owner::current()).unwrap();
        (t, d)
    }

    #[test]
    fn create_exclusive_read_back_and_exact_mode() {
        let (_t, d) = tmp();
        d.write_new("a", b"hello", 0o755, Owner::current()).unwrap();
        let (bytes, st) = d.read_file("a", 100).unwrap();
        assert_eq!(bytes, b"hello");
        assert_eq!(st.mode, 0o755);
        assert_eq!(st.kind, Kind::Regular);
        assert!(d.create_new("a", 0o600, Owner::current()).is_err());
        assert!(d.read_file("a", 3).is_err(), "size cap enforced");
    }

    #[test]
    fn symlinks_are_never_followed() {
        let (t, d) = tmp();
        std::fs::write(t.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("real", t.path().join("link")).unwrap();
        assert_eq!(d.stat("link").unwrap().unwrap().kind, Kind::Symlink);
        assert!(d.open_read("link").is_err());
        assert!(d.create_new("link", 0o600, Owner::current()).is_err());
        std::os::unix::fs::symlink(".", t.path().join("dirlink")).unwrap();
        assert!(d.open_dir("dirlink", None, Owner::current()).is_err());
    }

    #[test]
    fn absolute_walk_refuses_a_symlinked_ancestor() {
        let (t, _d) = tmp();
        let base = t.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("via")).unwrap();
        assert!(Dir::open_absolute(&base.join("via"), None, Owner::current()).is_err());
        assert!(Dir::open_absolute(&base.join("real"), None, Owner::current()).is_ok());
    }

    #[test]
    fn world_writable_ancestors_are_not_trusted() {
        let (t, _d) = tmp();
        let base = t.path().canonicalize().unwrap();
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();
        assert!(Dir::open_absolute(&open, None, Owner::current()).is_err());
    }

    #[test]
    fn rename_modes_noreplace_replace_and_swap() {
        let (_t, d) = tmp();
        let o = Owner::current();
        d.write_new("a", b"A", 0o600, o).unwrap();
        d.write_new("b", b"B", 0o600, o).unwrap();
        let e = d
            .rename_to("a", &d, "b", RenameMode::NoReplace)
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        d.rename_to("a", &d, "b", RenameMode::Swap).unwrap();
        assert_eq!(d.read_file("a", 9).unwrap().0, b"B");
        assert_eq!(d.read_file("b", 9).unwrap().0, b"A");
        d.rename_to("a", &d, "b", RenameMode::Replace).unwrap();
        assert!(d.stat("a").unwrap().is_none());
        assert_eq!(d.read_file("b", 9).unwrap().0, b"B");
        d.rename_to("b", &d, "c", RenameMode::NoReplace).unwrap();
    }

    #[test]
    fn names_must_be_single_components() {
        let (_t, d) = tmp();
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(d.stat(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lock_is_exclusive_and_wait_is_bounded() {
        let (_t, d) = tmp();
        let o = Owner::current();
        let held = FileLock::acquire(&d, "l", o, std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let t0 = std::time::Instant::now();
        let second = FileLock::acquire(&d, "l", o, std::time::Duration::from_millis(150)).unwrap();
        assert!(second.is_none());
        assert!(t0.elapsed() >= std::time::Duration::from_millis(140));
        drop(held);
        assert!(
            FileLock::acquire(&d, "l", o, std::time::Duration::from_millis(100))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_hardlinked_or_loose_lock_file_is_refused() {
        let (t, d) = tmp();
        let o = Owner::current();
        std::fs::write(t.path().join("l"), b"").unwrap();
        std::fs::set_permissions(
            t.path().join("l"),
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )
        .unwrap();
        assert!(FileLock::acquire(&d, "l", o, std::time::Duration::from_millis(10)).is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod acl_tests {
    use super::*;
    #[test]
    fn extended_acls_on_open_directories_and_files_are_refused() {
        unsafe extern "C" {
            fn acl_init(count: i32) -> *mut libc::c_void;
            fn acl_create_entry(acl: *mut *mut libc::c_void, entry: *mut *mut libc::c_void) -> i32;
            fn acl_set_tag_type(entry: *mut libc::c_void, tag: i32) -> i32;
            fn acl_set_qualifier(entry: *mut libc::c_void, qualifier: *const libc::c_void) -> i32;
            fn acl_get_permset(entry: *mut libc::c_void, perms: *mut *mut libc::c_void) -> i32;
            fn acl_add_perm(perms: *mut libc::c_void, perm: u32) -> i32;
            fn acl_set_fd_np(fd: i32, acl: *mut libc::c_void, kind: i32) -> i32;
            fn acl_free(acl: *mut libc::c_void) -> i32;
            fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> i32;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = Dir::open_absolute(&temp.path().canonicalize().unwrap(), None, Owner::current())
            .unwrap();
        let file = dir.create_new("test", 0o600, Owner::current()).unwrap();
        for fd in [file.as_raw_fd(), dir.raw()] {
            assert!(reject_extended_acl(fd).is_ok());
            // SAFETY: all buffers and returned ACL handles are valid until
            // acl_free; only isolated temporary descriptors are modified.
            unsafe {
                let mut acl = acl_init(1);
                assert!(!acl.is_null());
                let mut entry = std::ptr::null_mut();
                assert_eq!(acl_create_entry(&mut acl, &mut entry), 0);
                assert_eq!(acl_set_tag_type(entry, 1), 0); // ACL_EXTENDED_ALLOW
                let mut uuid = [0u8; 16];
                assert_eq!(mbr_uid_to_uuid(libc::geteuid(), uuid.as_mut_ptr()), 0);
                assert_eq!(acl_set_qualifier(entry, uuid.as_ptr().cast()), 0);
                let mut perms = std::ptr::null_mut();
                assert_eq!(acl_get_permset(entry, &mut perms), 0);
                assert_eq!(acl_add_perm(perms, 1 << 1), 0); // ACL_READ_DATA
                assert_eq!(acl_set_fd_np(fd, acl, 0x100), 0);
                assert_eq!(acl_free(acl), 0);
            }
            assert!(reject_extended_acl(fd).is_err());
        }
        assert!(dir.stat_self().is_err());
        assert!(fstat_file(&file).is_err());
    }
}
