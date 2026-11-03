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
#[derive(Clone, Copy, Debug)]
pub enum Entrypoint {
    BeforeWindowReveal,
    SecondInstance,
    ShowMainWindow,
    DeepLink,
    EmergencyFallback,
}

/// Explicit inputs make policy evaluation independent of process globals.
/// Evaluation never clears Quit or changes persistent opt-out.
pub fn evaluate_cold_start(
    directory: &Path,
    owner: u32,
    paths: &crate::paths::ClientPaths,
    session: u32,
    intent: crate::startup::StartupIntent,
) -> io::Result<Reader> {
    let lease = if directory == Path::new(gate::DIRECTORY) {
        Reader::startup(directory, owner, Path::new("/Library"))?
    } else {
        Gate::open(directory, owner, false)?.reader()?
    };
    evaluate_runtime_entrypoint(
        &lease,
        paths,
        session,
        intent,
        Entrypoint::BeforeWindowReveal,
    )?;
    Ok(lease)
}
pub fn evaluate_runtime_entrypoint(
    lease: &Reader,
    paths: &crate::paths::ClientPaths,
    session: u32,
    intent: crate::startup::StartupIntent,
    _entrypoint: Entrypoint,
) -> io::Result<()> {
    lease.recheck()?;
    if session == 0 {
        return Err(macos::denied());
    }
    if intent.tray
        && (crate::macos_storage::autostart_disabled_at(paths)?
            || crate::macos_storage::session_quit_at(paths, session)?)
    {
        return Err(macos::denied());
    }
    Ok(())
}
pub fn allowed(entrypoint: Entrypoint, intent: crate::startup::StartupIntent) -> bool {
    let Some(lease) = LEASE.get() else {
        return true;
    };
    (|| {
        evaluate_runtime_entrypoint(
            lease,
            &crate::macos_storage::paths()?,
            macos::current()?[6],
            intent,
            entrypoint,
        )
    })()
    .is_ok()
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
    let intent = crate::startup::StartupIntent::from_args(&args);
    let lease = evaluate_cold_start(
        Path::new(gate::DIRECTORY),
        0,
        &crate::macos_storage::paths()?,
        token[6],
        intent,
    )?;
    let automatic = intent.tray;
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
    if LEASE.get().unwrap().recheck().is_err() {
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
        // The persisted veto is authoritative even after coordinator/helper
        // disconnect. Notification failure must never trap an explicit Quit.
        notify_quit(control);
    }
    Ok(())
}
fn notify_quit(control: &Mutex<UnixStream>) {
    if let Ok(mut stream) = control.lock() {
        let _ = stream.write_all(b"Q");
    }
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
    restore_command(&home, tmp).spawn()?;
    Ok(true)
}
fn restore_command(home: &Path, tmp: &Path) -> Command {
    let mut command = Command::new(macos::EXECUTABLE);
    command
        .arg("--tray")
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", tmp)
        .env("PATH", "/usr/bin:/bin")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paths::ClientPaths, startup::StartupIntent};

    #[test]
    fn quit_notification_tolerates_disconnected_helper() {
        let (control, helper) = UnixStream::pair().unwrap();
        drop(helper);
        notify_quit(&Mutex::new(control));
    }
    #[test]
    fn restore_uses_fixed_hidden_argv_and_only_safe_environment() {
        let command = restore_command(
            Path::new("/test-account-home"),
            Path::new("/test-account-temp"),
        );
        assert_eq!(command.get_program(), macos::EXECUTABLE);
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--tray"]);
        let vars: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(vars.len(), 3);
        assert_eq!(
            vars[std::ffi::OsStr::new("HOME")],
            Some(std::ffi::OsStr::new("/test-account-home"))
        );
        assert_eq!(
            vars[std::ffi::OsStr::new("TMPDIR")],
            Some(std::ffi::OsStr::new("/test-account-temp"))
        );
        assert_eq!(
            vars[std::ffi::OsStr::new("PATH")],
            Some(std::ffi::OsStr::new("/usr/bin:/bin"))
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    }
    #[test]
    fn entrypoint_matrix_rechecks_gate_and_keeps_quit_separate_from_optout() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kanade-lifecycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let owner = unsafe { libc::geteuid() };
        let paths = ClientPaths {
            uid: owner,
            config: root.join("unused-config"),
            log: root.join("unused-log"),
            cache: root.join("unused-cache"),
            window_size: root.join("Kanade/Client/window-size.json"),
        };
        let directory = root.join("gate");
        let open = || Gate::open(&directory, owner, true).unwrap();
        let lease = open().reader().unwrap();
        for quit in [false, true] {
            for optout in [false, true] {
                crate::macos_storage::lifecycle_marker_at(
                    &paths,
                    &crate::macos_storage::quit_marker_name(9).unwrap(),
                    Some(quit),
                )
                .unwrap();
                crate::macos_storage::lifecycle_marker_at(
                    &paths,
                    "autostart-disabled",
                    Some(optout),
                )
                .unwrap();
                for automatic in [false, true] {
                    let args = if automatic {
                        vec!["--tray".into()]
                    } else {
                        vec![]
                    };
                    let intent = StartupIntent::from_args(&args);
                    assert_eq!(
                        evaluate_cold_start(&directory, owner, &paths, 9, intent).is_ok(),
                        !automatic || !(quit || optout)
                    );
                    for entry in [
                        Entrypoint::BeforeWindowReveal,
                        Entrypoint::SecondInstance,
                        Entrypoint::ShowMainWindow,
                        Entrypoint::DeepLink,
                        Entrypoint::EmergencyFallback,
                    ] {
                        assert_eq!(
                            evaluate_runtime_entrypoint(&lease, &paths, 9, intent, entry).is_ok(),
                            !automatic || !(quit || optout)
                        );
                        let writer = open().writer("update").unwrap();
                        assert!(evaluate_cold_start(&directory, owner, &paths, 9, intent).is_err());
                        assert!(
                            evaluate_runtime_entrypoint(&lease, &paths, 9, intent, entry).is_err()
                        );
                        writer.finish().unwrap();
                    }
                    // Pure evaluation never clears the per-session veto or opt-out.
                    assert_eq!(
                        crate::macos_storage::session_quit_at(&paths, 9).unwrap(),
                        quit
                    );
                    assert_eq!(
                        crate::macos_storage::autostart_disabled_at(&paths).unwrap(),
                        optout
                    );
                }
                // Other audit sessions do not inherit this session's Quit.
                assert!(!crate::macos_storage::session_quit_at(&paths, 10).unwrap());
            }
        }
        crate::macos_storage::lifecycle_marker_at(&paths, "autostart-disabled", Some(false))
            .unwrap();
        let marker = paths
            .window_size
            .parent()
            .unwrap()
            .join(crate::macos_storage::quit_marker_name(9).unwrap());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        value["body"] = serde_json::json!([0]);
        std::fs::write(marker, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(crate::macos_storage::session_quit_at(&paths, 9).is_err());
        assert!(
            evaluate_cold_start(
                &directory,
                owner,
                &paths,
                9,
                StartupIntent::from_args(&["--tray".into()])
            )
            .is_err()
        );
        drop(lease);
        std::fs::remove_dir_all(root).unwrap();
    }
}
