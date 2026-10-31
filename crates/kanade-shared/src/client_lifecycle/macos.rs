//! Kernel audit-token process identity and bounded, local helper protocol.
use super::{Instance, Presence};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
pub const EXECUTABLE: &str = "/Applications/Kanade Client.app/Contents/MacOS/kanade-client";
unsafe extern "C" {
    fn kl_peer(fd: i32, token: *mut [u32; 8]) -> i32;
    fn kl_self(token: *mut [u32; 8]) -> i32;
    fn kl_image(
        token: *mut [u32; 8],
        path: *const libc::c_char,
        device: *mut u64,
        inode: *mut u64,
    ) -> i32;
    fn kl_alive(token: *mut [u32; 8]) -> i32;
    fn kl_signal(token: *mut [u32; 8], signal: i32) -> i32;
    fn kl_user_context() -> i32;
    fn kl_presence_init();
    fn kl_presence_invalidate();
    fn kl_presence_poll() -> i32;
    fn kl_no_acl(fd: i32) -> i32;
}
pub fn no_acl(f: &impl AsRawFd) -> io::Result<()> {
    check(unsafe { kl_no_acl(f.as_raw_fd()) })
}
pub fn peer(s: &impl AsRawFd) -> io::Result<[u32; 8]> {
    let mut t = [0; 8];
    check(unsafe { kl_peer(s.as_raw_fd(), &mut t) })?;
    Ok(t)
}
pub fn current() -> io::Result<[u32; 8]> {
    let mut t = [0; 8];
    check(unsafe { kl_self(&mut t) })?;
    Ok(t)
}
pub fn identity(mut token: [u32; 8], path: &Path) -> io::Result<Instance> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let (mut device, mut inode) = (0, 0);
    check(unsafe { kl_image(&mut token, path.as_ptr(), &mut device, &mut inode) })?;
    Ok(Instance {
        token,
        device,
        inode,
    })
}
pub fn installed(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let (mut device, mut inode) = (0, 0);
    check(unsafe { kl_image(std::ptr::null_mut(), path.as_ptr(), &mut device, &mut inode) })
}
pub fn invalidate_presence() {
    unsafe { kl_presence_invalidate() }
}
pub fn alive(mut instance: Instance) -> bool {
    unsafe { kl_alive(&mut instance.token) != 0 }
}
pub fn signal(mut instance: Instance, path: &Path, signal: i32) -> io::Result<()> {
    if identity(instance.token, path)? != instance {
        return Err(denied());
    }
    check(unsafe { kl_signal(&mut instance.token, signal) })
}
pub fn init_presence() {
    unsafe { kl_presence_init() }
}
pub fn poll_presence() -> bool {
    unsafe { kl_presence_poll() == 1 }
}
pub fn endpoint(pid: u32) -> PathBuf {
    PathBuf::from(format!("/private/tmp/kanade-client-presence-{pid}.sock"))
}
fn check(rc: i32) -> io::Result<()> {
    if rc == 0 { Ok(()) } else { Err(denied()) }
}
pub fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unverified Client process/session",
    )
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Observe,
    Stop,
    Restore,
    Release,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub presence: Presence,
    pub launched: bool,
}
pub fn configure(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))
}
pub fn send<T: Serialize>(s: &mut UnixStream, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 4096 {
        return Err(denied());
    }
    s.write_all(&(bytes.len() as u32).to_be_bytes())?;
    s.write_all(&bytes)
}
pub fn receive<T: for<'a> Deserialize<'a>>(s: &mut UnixStream) -> io::Result<T> {
    let mut n = [0; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if n > 4096 {
        return Err(denied());
    }
    let mut bytes = vec![0; n];
    s.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub fn user_context() -> io::Result<()> {
    check(unsafe { kl_user_context() })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ChildGuard(std::process::Child);
    impl std::ops::Deref for ChildGuard {
        type Target = std::process::Child;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl std::ops::DerefMut for ChildGuard {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn native_audit_tokens_bound_signals_to_a_test_owned_child() {
        const CHILD: &str = "KANADE_LIFECYCLE_TEST_SOCKET";
        if let Some(path) = std::env::var_os(CHILD) {
            let mut stream = UnixStream::connect(path).unwrap();
            send(&mut stream, &current().unwrap()).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let endpoint = temp.path().join("child.sock");
        let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Only execute a private copy in a synthetic bundle; no installed
        // Client, helper or arbitrary host process is ever a signal target.
        let executable = temp
            .path()
            .join("Synthetic.app/Contents/MacOS/lifecycle-test");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut child = ChildGuard(std::process::Command::new(&executable)
            .args(["--exact", "client_lifecycle::macos::tests::native_audit_tokens_bound_signals_to_a_test_owned_child", "--nocapture"])
            .env_clear().env(CHILD, &endpoint).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()).spawn().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let connection = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break None,
            }
        };
        let Some(mut stream) = connection else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test child did not connect");
        };
        configure(&stream).unwrap();
        let token: [u32; 8] = receive(&mut stream).unwrap();
        assert_eq!(peer(&stream).unwrap(), token);
        assert_eq!(token[5], child.id());
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&executable).unwrap();
        assert_eq!(token[1], metadata.uid());
        assert_eq!(metadata.nlink(), 1);
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let count = unsafe {
            libc::proc_pidpath(
                child.id() as i32,
                path.as_mut_ptr().cast(),
                path.len() as u32,
            )
        };
        assert!(count > 0);
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CStr::from_bytes_until_nul(&path).unwrap();
        assert_eq!(
            Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())),
            executable.canonicalize().unwrap()
        );
        // Always reap this exclusively test-owned subprocess, even on failure.
        let mut wrong = token;
        wrong[7] = wrong[7].wrapping_add(1);
        let rejected = unsafe { kl_signal(&mut wrong, 0) } != 0;
        let signalled = unsafe { kl_signal(&mut token.clone(), libc::SIGTERM) } == 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(rejected);
        assert!(signalled);
        assert!(!status.success());
        assert!(unsafe { kl_signal(&mut token.clone(), 0) } != 0);
    }
}
