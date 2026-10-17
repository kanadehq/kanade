//! Pixel-free, bounded onboarding in the same executable and GUI launch route as capture.
//! A durable intent precedes GO: a crash at any later point must never re-prompt.
use std::io::{BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};

unsafe extern "C" {
    fn kanade_console_user(buffer: *mut std::ffi::c_char, length: usize) -> i64;
    fn kanade_signing_identity(buffer: *mut std::ffi::c_char, length: usize) -> bool;
    fn kanade_screen_permission_present() -> bool;
    fn kanade_screen_permission_request() -> bool;
}

pub(crate) fn console_uid() -> Option<u32> {
    let mut name = [0u8; 1024];
    // SAFETY: writable buffer with exact capacity; bridge releases its CF objects.
    let uid = unsafe { kanade_console_user(name.as_mut_ptr().cast(), name.len()) };
    let name = std::ffi::CStr::from_bytes_until_nul(&name)
        .ok()?
        .to_str()
        .ok()?;
    interactive_uid(uid, name)
}

fn interactive_uid(uid: i64, name: &str) -> Option<u32> {
    if name.is_empty() || matches!(name, "loginwindow" | "_mbsetupuser") {
        return None;
    }
    u32::try_from(uid).ok().filter(|uid| *uid != 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Session {
    uid: u32,
    // Ownership change time distinguishes login generations where available.
    // Missing metadata intentionally coalesces generations instead of prompting more.
    generation: Option<(i64, i64)>,
}

fn console() -> Option<Session> {
    let uid = console_uid()?;
    let metadata = std::fs::metadata("/dev/console").ok();
    Some(Session {
        uid,
        generation: metadata
            .filter(|m| m.uid() == uid)
            .map(|m| (m.ctime(), m.ctime_nsec())),
    })
}

fn needs_check(current: Option<Session>, completed: &mut Option<Session>) -> Option<Session> {
    if current.is_none() {
        *completed = None;
    }
    current.filter(|session| Some(*session) != *completed)
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    attempts: u8,
    retry_after: u64,
    requested: bool,
}

impl History {
    fn eligible(&self, now: u64) -> bool {
        !self.requested && self.attempts < 3 && now >= self.retry_after
    }

    fn launching(&mut self, now: u64) {
        self.attempts += 1;
        self.retry_after = now.saturating_add(60 * 10u64.pow(u32::from(self.attempts - 1)));
    }
}

fn load(path: &Path) -> Result<History> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(History::default()),
        Err(e) => Err(e.into()),
    }
}

fn save(path: &Path, history: &History) -> Result<()> {
    let parent = path.parent().context("history has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(history)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temp);
    result
}

/// Dropping the supervisor at agent shutdown cancels its current helper too.
pub(crate) struct Supervisor(tokio::task::JoinHandle<()>);
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn start() -> Supervisor {
    Supervisor(tokio::spawn(async {
        // Only the installed root daemon on supported capture hosts onboards.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 || !crate::remote_macos::supported() {
            return;
        }
        let mut completed = None;
        loop {
            if let Some(session) = needs_check(console(), &mut completed) {
                match attempt(session).await {
                    Ok(done) => {
                        if done {
                            completed = Some(session);
                        }
                    }
                    Err(e) => {
                        // Corrupt/unwritable state fails closed for this session.
                        warn!(error = %e, "Screen Recording onboarding suppressed; check System Settings > Privacy & Security > Screen Recording");
                        completed = Some(session);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }))
}

struct Group(u32);
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: this is the private process group created for our child.
        unsafe { libc::killpg(self.0 as i32, libc::SIGKILL) };
    }
}

async fn attempt(session: Session) -> Result<bool> {
    let exe = std::env::current_exe()?;
    // Stable team + identifier where signed; never include version or cdhash.
    let mut identity = [0u8; 1024];
    // SAFETY: buffer is writable for exactly the supplied capacity.
    let signed = unsafe { kanade_signing_identity(identity.as_mut_ptr().cast(), identity.len()) };
    let mut digest = Sha256::new();
    digest.update(exe.as_os_str().as_encoded_bytes());
    if signed {
        digest.update(identity);
    }
    let key: String = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let path: PathBuf = kanade_shared::default_paths::data_dir()
        .join("screen-permission")
        .join(format!("{}-{key}.json", session.uid));
    std::fs::create_dir_all(path.parent().context("history has no parent")?)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("lock"))?;
    // SAFETY: valid owned descriptor; closing it releases this nonblocking lock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(false);
    }
    let mut history = load(&path)?;
    if history.requested || history.attempts >= 3 {
        info!(
            uid = session.uid,
            "Screen Recording onboarding already attempted; if viewing lacks permission, enable the installed agent in System Settings > Privacy & Security > Screen Recording, then reconnect (restart the agent if necessary)"
        );
        return Ok(true);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if !history.eligible(now) {
        return Ok(false);
    }
    history.launching(now);
    save(&path, &history)?;
    let uid = session.uid.to_string();
    let command = crate::process_as_user_macos::session_command_for_uid(
        kanade_shared::wire::RunAs::User,
        session.uid,
        exe.to_str().context("non UTF-8 executable")?,
        &["--screen-permission-check", &uid],
        None,
    );
    let mut command = match command {
        Ok(command) => command,
        Err(e) => {
            warn!(error = %e, "permission helper launch deferred");
            return Ok(false);
        }
    };
    let child = command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            warn!(error = %e, "permission helper spawn failed");
            return Ok(false);
        }
    };
    let group = Group(child.id().context("missing child pid")?);
    let mut input = child.stdin.take().context("missing stdin")?;
    let mut output = BufReader::new(child.stdout.take().context("missing stdout")?);
    let exchange = async {
        let requested = authorize(
            &mut output,
            &mut input,
            || console() == Some(session),
            || {
                history.requested = true;
                save(&path, &history)
            },
        )
        .await?;
        let status = child.wait().await?;
        if requested {
            info!(uid = session.uid, %status, "Screen Recording request finished; false/unknown does not prove denial. Use System Settings to enable the installed agent; reconnect to create a fresh capture child, or restart the agent if needed");
        } else {
            info!(
                uid = session.uid,
                "Screen Recording already granted; no request made"
            );
        }
        Ok(true)
    };
    let monitor = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if console() != Some(session) {
                break;
            }
        }
    };
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(30), exchange) => {
            match result {
                Ok(result) => result,
                Err(e) => Err(anyhow::anyhow!("permission helper timed out: {e}")),
            }
        }
        () = monitor => Err(anyhow::anyhow!("console session ended or changed")),
    };
    drop(group);
    let _ = child.wait().await;
    // Before GO all failures are retryable, with the persisted finite budget.
    match result {
        Ok(done) => Ok(done),
        Err(e) => {
            warn!(error = %e, "Screen Recording helper stopped; if permission is missing, enable the installed agent in System Settings > Privacy & Security > Screen Recording, then reconnect or restart the agent");
            Ok(history.requested)
        }
    }
}

/// Synchronous early CLI branch: no config, network, sentinel, capture or pixels.
pub(crate) fn helper(uid: u32) -> Result<()> {
    // Independent hard deadline, including an abrupt parent death during a
    // synchronous OS permission dialog. This focused process has no agent work.
    // SAFETY: this early CLI branch owns the process and has no other threads.
    unsafe {
        libc::signal(libc::SIGALRM, libc::SIG_DFL);
        libc::alarm(35);
    }
    let valid = || {
        // SAFETY: geteuid has no preconditions.
        uid != 0 && unsafe { libc::geteuid() } == uid && console_uid() == Some(uid)
    };
    permission_check(
        valid,
        // SAFETY: permission-only APIs have no pointer arguments.
        || unsafe { kanade_screen_permission_present() },
        || unsafe { kanade_screen_permission_request() },
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    )
}

fn permission_check(
    valid: impl Fn() -> bool,
    present: impl Fn() -> bool,
    request: impl Fn() -> bool,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<()> {
    if !valid() {
        bail!("permission check requires the expected console user");
    }
    if present() {
        writeln!(output, "GRANTED")?;
        output.flush()?;
        return Ok(());
    }
    writeln!(output, "READY")?;
    output.flush()?;
    let mut go = String::new();
    input.take(4).read_line(&mut go)?;
    if go != "GO\n" || !valid() {
        bail!("permission request not authorized or user changed");
    }
    if !present() {
        let _ = request();
    }
    Ok(())
}

async fn authorize(
    output: &mut (impl tokio::io::AsyncBufRead + Unpin),
    input: &mut (impl tokio::io::AsyncWrite + Unpin),
    current: impl Fn() -> bool,
    mut commit: impl FnMut() -> Result<()>,
) -> Result<bool> {
    let mut line = String::new();
    tokio::io::AsyncReadExt::take(output, 16)
        .read_line(&mut line)
        .await?;
    match line.as_str() {
        "GRANTED\n" => Ok(false),
        "READY\n" => {
            if !current() {
                bail!("console session changed");
            }
            commit()?;
            input.write_all(b"GO\n").await?;
            input.shutdown().await?;
            Ok(true)
        }
        _ => bail!("invalid or missing permission helper reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn session(uid: u32, generation: i64) -> Session {
        Session {
            uid,
            generation: Some((generation, 0)),
        }
    }

    #[test]
    fn console_detection_excludes_loginwindow_setup_root_and_unavailable() {
        assert_eq!(interactive_uid(-1, ""), None);
        assert_eq!(interactive_uid(0, "root"), None);
        assert_eq!(interactive_uid(501, "loginwindow"), None);
        assert_eq!(interactive_uid(501, "_mbsetupuser"), None);
        assert_eq!(interactive_uid(501, "test-user"), Some(501));
    }

    #[test]
    fn login_startup_switch_logout_and_unknown_generation() {
        let mut completed = None;
        assert_eq!(needs_check(None, &mut completed), None);
        let first = session(501, 1);
        // The first observation handles an already signed-in user, too.
        assert_eq!(needs_check(Some(first), &mut completed), Some(first));
        completed = Some(first);
        assert_eq!(needs_check(Some(first), &mut completed), None);
        let switched = session(502, 2);
        assert_eq!(needs_check(Some(switched), &mut completed), Some(switched));
        let relogin = session(501, 3);
        assert_eq!(needs_check(Some(relogin), &mut completed), Some(relogin));
        assert_eq!(needs_check(None, &mut completed), None);
        assert_eq!(needs_check(Some(first), &mut completed), Some(first));
        let unknown = Session {
            uid: 501,
            generation: None,
        };
        completed = Some(unknown);
        assert_eq!(needs_check(Some(unknown), &mut completed), None);
    }

    #[test]
    fn permission_present_never_reads_authorization_or_requests() {
        let requests = Cell::new(0);
        let mut output = Vec::new();
        permission_check(
            || true,
            || true,
            || {
                requests.set(1);
                true
            },
            &b""[..],
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"GRANTED\n");
        assert_eq!(requests.get(), 0);
    }

    #[test]
    fn refusal_is_one_request_and_lost_authorization_or_switch_never_requests() {
        let requests = Cell::new(0);
        let request = || {
            requests.set(requests.get() + 1);
            false
        };
        let mut output = Vec::new();
        permission_check(|| true, || false, request, &b"GO\n"[..], &mut output).unwrap();
        assert_eq!(requests.get(), 1);
        assert_eq!(output, b"READY\n");
        for input in [&b""[..], &b"NO\n"[..]] {
            assert!(permission_check(|| true, || false, request, input, Vec::new()).is_err());
        }
        let validations = Cell::new(0);
        assert!(
            permission_check(
                || {
                    validations.set(validations.get() + 1);
                    validations.get() == 1
                },
                || false,
                request,
                &b"GO\n"[..],
                Vec::new()
            )
            .is_err()
        );
        assert_eq!(requests.get(), 1);
        assert!(permission_check(|| false, || false, request, &b"GO\n"[..], Vec::new()).is_err());
        assert_eq!(requests.get(), 1);
    }

    #[test]
    fn grant_between_preflights_avoids_request() {
        let checks = Cell::new(0);
        permission_check(
            || true,
            || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
            || panic!("already granted"),
            &b"GO\n"[..],
            Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn finite_failure_backoff_survives_service_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut history = load(&path).unwrap();
        let mut now = 100;
        for delay in [60, 600, 6000] {
            assert!(history.eligible(now));
            history.launching(now);
            save(&path, &history).unwrap();
            history = load(&path).unwrap();
            assert_eq!(history.retry_after, now + delay);
            assert!(!history.eligible(now + delay - 1));
            now += delay;
        }
        assert!(!history.eligible(now + 100_000));
    }

    #[tokio::test]
    async fn intent_is_durable_before_go_and_survives_denial_or_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut history = History::default();
        let mut output = &b"READY\n"[..];
        let mut input = Vec::new();
        authorize(
            &mut output,
            &mut input,
            || true,
            || {
                history.requested = true;
                save(&path, &history)
            },
        )
        .await
        .unwrap();
        assert_eq!(input, b"GO\n");
        // No child result is necessary: refusal, timeout, broken GO pipe and
        // daemon death after the durable intent all suppress further requests.
        assert!(!load(&path).unwrap().eligible(u64::MAX));
    }

    #[tokio::test]
    async fn present_failure_corruption_and_switch_never_send_go() {
        for reply in [&b"GRANTED\n"[..], &b""[..], &b"invalid\n"[..]] {
            let mut output = reply;
            let mut input = Vec::new();
            let result = authorize(
                &mut output,
                &mut input,
                || true,
                || panic!("must not persist request"),
            )
            .await;
            assert_eq!(result.is_ok(), reply == b"GRANTED\n");
            assert!(input.is_empty());
        }
        let mut input = Vec::new();
        assert!(
            authorize(
                &mut &b"READY\n"[..],
                &mut input,
                || false,
                || panic!("switched user")
            )
            .await
            .is_err()
        );
        assert!(input.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"bad history").unwrap();
        assert!(
            authorize(
                &mut &b"READY\n"[..],
                &mut input,
                || true,
                || load(&path).map(|_| ())
            )
            .await
            .is_err()
        );
        assert!(input.is_empty());
        assert!(
            authorize(
                &mut &b"READY\n"[..],
                &mut input,
                || true,
                || bail!("store failed")
            )
            .await
            .is_err()
        );
        assert!(input.is_empty());
    }

    #[tokio::test]
    async fn pending_preflight_is_bounded_without_request_intent() {
        let (_writer, reader) = tokio::io::duplex(64);
        let mut reader = BufReader::new(reader);
        let mut input = Vec::new();
        let committed = Cell::new(false);
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            authorize(
                &mut reader,
                &mut input,
                || true,
                || {
                    committed.set(true);
                    Ok(())
                },
            ),
        )
        .await;
        assert!(result.is_err());
        assert!(!committed.get());
        assert!(input.is_empty());
    }
}
