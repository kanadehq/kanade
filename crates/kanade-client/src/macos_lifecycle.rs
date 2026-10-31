//! Pre-GUI participation and independent user-context session-presence worker.
use kanade_shared::client_lifecycle::{
    Presence,
    gate::{self, Gate, Reader},
    macos::{self, Request, Response},
};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static CONTROL: OnceLock<Mutex<UnixStream>> = OnceLock::new();
static LEASE: OnceLock<Reader> = OnceLock::new();
pub fn allowed() -> bool {
    LEASE.get().is_none_or(|lease| lease.recheck().is_ok())
}
/// Returns false for an internal command or a suppressed launch.
pub fn prepare() -> io::Result<bool> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--lifecycle-presence") {
        worker()?;
        return Ok(false);
    }
    let installed = std::env::current_exe()? == Path::new(macos::EXECUTABLE);
    if !installed {
        return Ok(true);
    } // Developer builds are not managed instances.
    let token = macos::current()?;
    macos::identity(token, Path::new(macos::EXECUTABLE))?;
    if args.iter().any(|a| a == "--disable-autostart") {
        crate::macos_storage::autostart_disabled(Some(true))?;
        return Ok(false);
    }
    if args.iter().any(|a| a == "--enable-autostart") {
        crate::macos_storage::autostart_disabled(Some(false))?;
        return Ok(false);
    }
    let lease = Gate::open(Path::new(gate::DIRECTORY), 0, false)?.reader()?;
    let automatic = args.iter().any(|a| a == "--tray");
    if automatic
        && (crate::macos_storage::autostart_disabled(None)?
            || crate::macos_storage::session_quit(token[6], None)?)
    {
        return Ok(false);
    }
    if !automatic {
        crate::macos_storage::session_quit(token[6], Some(false))?;
    }
    let (control, child) = UnixStream::pair()?;
    // Inherited socket records the creating GUI's kernel audit token. Worker
    // verifies its original parent, UID/session and fixed installed image.
    let input = unsafe { std::fs::File::from_raw_fd(child.into_raw_fd()) };
    Command::new(macos::EXECUTABLE)
        .arg("--lifecycle-presence")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    CONTROL
        .set(Mutex::new(control))
        .map_err(|_| macos::denied())?;
    LEASE.set(lease).map_err(|_| macos::denied())?;
    if !allowed() {
        return Ok(false);
    }
    Ok(true)
}
/// Called after the app handle exists, before the first reveal.
pub fn watch(app: tauri::AppHandle) {
    if let Some(control) = CONTROL.get() {
        let mut pipe = match control.lock().unwrap().try_clone() {
            Ok(p) => p,
            Err(_) => {
                app.exit(75);
                return;
            }
        };
        std::thread::spawn(move || {
            let mut b = [0];
            if pipe.read_exact(&mut b).is_ok() && b[0] == b'S' {
                app.exit(75);
            }
        });
    }
}
/// An OS/user Quit is recorded synchronously before ExitRequested completes.
/// The internal update exit uses code 75 and never writes a Quit marker.
pub fn explicit_quit() -> io::Result<()> {
    if LEASE.get().is_none() {
        return Ok(());
    }
    let token = macos::current()?;
    crate::macos_storage::session_quit(token[6], Some(true))?;
    if let Some(control) = CONTROL.get() {
        control.lock().unwrap().write_all(b"Q")?;
    }
    Ok(())
}
fn state(
    gui: kanade_shared::client_lifecycle::Instance,
    helper: kanade_shared::client_lifecycle::Instance,
    generation: u64,
    sequence: u64,
) -> Presence {
    Presence {
        gui,
        helper,
        generation,
        sequence,
        local_active_unlocked: macos::poll_presence(),
        quit_generation: u64::from(
            crate::macos_storage::session_quit(gui.session(), None).unwrap_or(true),
        ),
        opted_out: crate::macos_storage::autostart_disabled(None).unwrap_or(true),
    }
}
fn worker() -> io::Result<()> {
    if std::env::current_exe()? != Path::new(macos::EXECUTABLE) {
        return Err(macos::denied());
    }
    let fd = unsafe { libc::dup(0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut parent = unsafe { UnixStream::from_raw_fd(fd) };
    let gui = macos::identity(macos::peer(&parent)?, Path::new(macos::EXECUTABLE))?;
    let helper = macos::identity(macos::current()?, Path::new(macos::EXECUTABLE))?;
    if gui.pid() != unsafe { libc::getppid() as u32 }
        || gui.uid() != helper.uid()
        || gui.session() != helper.session()
        || gui.uid() == 0
    {
        return Err(macos::denied());
    }
    parent.set_nonblocking(true)?;
    let endpoint = macos::endpoint(helper.pid());
    // Never unlink a pre-existing path to steal an identity or split listeners.
    let listener = UnixListener::bind(&endpoint)?;
    listener.set_nonblocking(true)?;
    macos::init_presence();
    let generation = u64::from(helper.token[7]);
    let mut sequence = 0;
    let mut stopped = false;
    let mut deadline = None;
    let mut connection: Option<UnixStream> = None;
    let mut before = None;
    let mut last_poll = Instant::now();
    let result = (|| {
        loop {
            if last_poll.elapsed() > Duration::from_secs(2) {
                macos::invalidate_presence();
            }
            let mut p = state(gui, helper, generation, sequence);
            sequence += 1;
            // A suspended/starved helper loses its unlock evidence.
            if last_poll.elapsed() > Duration::from_secs(2) {
                p.local_active_unlocked = false;
            }
            last_poll = Instant::now();
            let mut b = [0];
            match parent.read(&mut b) {
                Ok(1) if b[0] == b'Q' => return Ok(()),
                Ok(0) if !stopped => return Ok(()),
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => return Err(e),
                _ => {}
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(());
            }
            if connection.is_none() {
                match listener.accept() {
                    Ok((s, _)) => {
                        if macos::peer(&s)?[1] != 0 {
                            continue;
                        }
                        macos::configure(&s)?;
                        connection = Some(s);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let s = connection.as_mut().unwrap();
            // MSG_PEEK avoids blocking the presence monitor on an idle root
            // connection. A missing/truncated command invalidates this worker.
            let mut byte = [0u8];
            let ready = unsafe {
                libc::recv(
                    s.as_raw_fd(),
                    byte.as_mut_ptr().cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if ready < 0 {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if ready == 0 {
                return Ok(());
            }
            let request: Request = macos::receive(s)?;
            let mut launched = false;
            match request {
                Request::Observe => {}
                Request::Stop => {
                    if stopped || !macos::alive(gui) {
                        return Err(macos::denied());
                    }
                    // Preserve the stop-before set even if the UI exits before
                    // root receives this response. Never overwrite a later Quit.
                    before = Some(p);
                    stopped = true;
                    deadline = Some(Instant::now() + Duration::from_secs(600));
                    parent.write_all(b"S")?;
                }
                Request::Restore => {
                    p = state(gui, helper, generation, sequence);
                    sequence += 1;
                    if !macos::alive(gui) && before.is_some_and(|b| b.may_restore(&p)) {
                        launched = restore(gui.session())?;
                    }
                    macos::send(
                        s,
                        &Response {
                            presence: p,
                            launched,
                        },
                    )?;
                    return Ok(());
                }
                Request::Release => return Ok(()),
            }
            macos::send(
                s,
                &Response {
                    presence: p,
                    launched,
                },
            )?;
        }
    })();
    let _ = std::fs::remove_file(endpoint);
    result
}
fn restore(session: u32) -> io::Result<bool> {
    // This worker already has the target UID/GID/groups and bootstrap context;
    // it was never root. No launchctl-asuser credential assumption is needed.
    macos::user_context()?;
    let token = macos::current()?;
    if token[1] == 0
        || token[6] != session
        || !macos::poll_presence()
        || crate::macos_storage::autostart_disabled(None)?
        || crate::macos_storage::session_quit(session, None)?
    {
        return Ok(false);
    }
    macos::installed(Path::new(macos::EXECUTABLE))?;
    let lease = Gate::open(Path::new(gate::DIRECTORY), 0, false)?.reader()?;
    let home = crate::macos_storage::account_home()?;
    let mut tmp = vec![0u8; 4096];
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            tmp.as_mut_ptr().cast(),
            tmp.len(),
        )
    };
    if n == 0 || n > tmp.len() {
        return Err(macos::denied());
    }
    let tmp = std::ffi::CStr::from_bytes_until_nul(&tmp).map_err(|_| macos::denied())?;
    use std::os::unix::ffi::OsStrExt;
    let tmp = std::path::Path::new(std::ffi::OsStr::from_bytes(tmp.to_bytes()));
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(tmp)?;
    if !tmp.is_absolute() || !m.is_dir() || m.uid() != token[1] || m.mode() & 0o077 != 0 {
        return Err(macos::denied());
    }
    lease.recheck()?;
    if !macos::poll_presence()
        || crate::macos_storage::session_quit(session, None)?
        || crate::macos_storage::autostart_disabled(None)?
    {
        return Ok(false);
    }
    Command::new(macos::EXECUTABLE)
        .arg("--tray")
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", tmp)
        .env("PATH", "/usr/bin:/bin")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(true)
}
