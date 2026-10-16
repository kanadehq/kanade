//! Descriptor-relative macOS storage. Never trust inherited HOME or follow links.
use crate::paths::{
    ClientPaths, PrivateEntry, PrivateStorageFacts, UnixPlatform, UserAccount, XdgOverrides,
    private_storage_access,
};
use crate::window_size::{self, WindowSize};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Component, Path, PathBuf};

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unverified private client storage",
    )
}

pub fn paths() -> io::Result<ClientPaths> {
    // getpwuid_r copies account data into our buffer; no process-global passwd pointer.
    let uid = unsafe { libc::geteuid() };
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 65536];
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if result.is_null() {
        return Err(denied());
    }
    let record = unsafe { record.assume_init() };
    if record.pw_uid != uid || record.pw_dir.is_null() {
        return Err(denied());
    }
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes(),
    ));
    ClientPaths::resolve(
        UnixPlatform::MacOs,
        &UserAccount { uid, home },
        &XdgOverrides::default(),
    )
    .map_err(|_| denied())
}

fn open_at(parent: &File, name: &CStr, flags: i32, mode: u32) -> io::Result<File> {
    // The parent descriptor pins traversal; O_NOFOLLOW rejects swaps to symlinks.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn validate(file: &File, paths: &ClientPaths, entry: PrivateEntry) -> io::Result<()> {
    let meta = file.metadata()?;
    let kind_ok = match entry {
        PrivateEntry::Directory => meta.is_dir(),
        PrivateEntry::File => meta.is_file() && meta.nlink() == 1,
    };
    if kind_ok
        && private_storage_access(
            paths,
            entry,
            &PrivateStorageFacts {
                effective_uid: Some(unsafe { libc::geteuid() }),
                owner_uid: Some(meta.uid()),
                permissions: Some(meta.mode() & 0o7777),
                no_symlinks_verified: true,
            },
        )
    {
        Ok(())
    } else {
        Err(denied())
    }
}
fn directory(paths: &ClientPaths, path: &Path) -> io::Result<File> {
    let mut dir = File::open("/")?;
    let mut private = false;
    for part in path.components() {
        let Component::Normal(name) = part else {
            if part == Component::RootDir {
                continue;
            }
            return Err(denied());
        };
        private |= name == "Kanade";
        let name = CString::new(name.as_bytes()).map_err(|_| denied())?;
        if private {
            let code = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) };
            if code != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error());
            }
        }
        dir = open_at(&dir, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        if private {
            validate(&dir, paths, PrivateEntry::Directory)?;
        }
    }
    if !private {
        return Err(denied());
    }
    Ok(dir)
}
fn state_file(paths: &ClientPaths, write: bool) -> io::Result<File> {
    let dir = directory(paths, paths.window_size.parent().ok_or_else(denied)?)?;
    let name = CString::new(window_size::FILE_NAME).unwrap();
    let file = open_at(
        &dir,
        &name,
        if write {
            libc::O_WRONLY | libc::O_CREAT | libc::O_NONBLOCK
        } else {
            libc::O_RDONLY | libc::O_NONBLOCK
        },
        0o600,
    )?;
    validate(&file, paths, PrivateEntry::File)?;
    Ok(file)
}
/// A private per-user log; failure is nonfatal and leaves tracing on stderr.
pub fn open_log() -> io::Result<File> {
    let paths = paths()?;
    let dir = directory(&paths, paths.log.parent().ok_or_else(denied)?)?;
    let name = CString::new("client.log").unwrap();
    let file = open_at(
        &dir,
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_NONBLOCK,
        0o600,
    )?;
    validate(&file, &paths, PrivateEntry::File)?;
    Ok(file)
}

pub fn load_size(min: (f64, f64), max: Option<(f64, f64)>) -> io::Result<Option<WindowSize>> {
    let paths = paths()?;
    load_from(&paths, min, max)
}
fn load_from(
    paths: &ClientPaths,
    min: (f64, f64),
    max: Option<(f64, f64)>,
) -> io::Result<Option<WindowSize>> {
    let file = match state_file(paths, false) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    Ok(window_size::decode(&bytes, min, max))
}
pub fn save_size(
    width: f64,
    height: f64,
    min: (f64, f64),
    max: Option<(f64, f64)>,
) -> io::Result<()> {
    let Some(bytes) = window_size::encode(width, height, min, max)? else {
        return Ok(());
    };
    save_to(&paths()?, &bytes)
}
fn save_to(paths: &ClientPaths, bytes: &[u8]) -> io::Result<()> {
    // Refuse to replace an existing unsafe entry, including another user's file.
    match state_file(paths, false) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let dir = directory(paths, paths.window_size.parent().ok_or_else(denied)?)?;
    let name = CString::new(window_size::FILE_NAME).unwrap();
    let tmp = CString::new(format!(".window-size-{}.tmp", uuid::Uuid::new_v4())).unwrap();
    let mut file = open_at(
        &dir,
        &tmp,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = (|| {
        validate(&file, paths, PrivateEntry::File)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // Rename relative to the pinned private directory; neither endpoint is followed.
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                tmp.as_ptr(),
                dir.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn private_storage_rejects_links_permissions_and_corruption() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("target/storage-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let paths = ClientPaths {
            uid: unsafe { libc::geteuid() },
            config: root.join("unused"),
            log: root.join("unused"),
            cache: root.join("unused"),
            window_size: root.join("Kanade/Client/window-size.json"),
        };
        let mut f = state_file(&paths, true).unwrap();
        f.write_all(b"corrupt").unwrap();
        assert_eq!(load_from(&paths, (640., 420.), None).unwrap(), None);
        std::fs::set_permissions(&paths.window_size, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(state_file(&paths, true).is_err());
        std::fs::remove_file(&paths.window_size).unwrap();
        symlink(root.join("victim"), &paths.window_size).unwrap();
        assert!(state_file(&paths, true).is_err());
        assert!(!root.join("victim").exists());
        std::fs::remove_file(&paths.window_size).unwrap();
        let bytes = window_size::encode(900., 700., (640., 420.), None)
            .unwrap()
            .unwrap();
        save_to(&paths, &bytes).unwrap();
        assert_eq!(
            load_from(&paths, (640., 420.), None).unwrap(),
            Some(WindowSize {
                width: 900,
                height: 700
            })
        );
        std::fs::hard_link(&paths.window_size, root.join("hardlink")).unwrap();
        assert!(save_to(&paths, &bytes).is_err());
        std::fs::remove_file(root.join("hardlink")).unwrap();
        std::fs::rename(root.join("Kanade/Client"), root.join("real-client")).unwrap();
        symlink(root.join("real-client"), root.join("Kanade/Client")).unwrap();
        assert!(save_to(&paths, &bytes).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
