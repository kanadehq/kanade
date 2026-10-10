//! Unix (Linux / macOS) peer credentials and connect-time admission
//! (SPEC §2.12.4.1).
//!
//! The socket is `0666`, so the only authorization layer is the check
//! done here, *before a single byte is read from the peer*:
//!
//! 1. [`peer_credentials`] reads the kernel-attested `(uid, pid)` of the
//!    connecting process (`SO_PEERCRED` on Linux; `LOCAL_PEERCRED` +
//!    `LOCAL_PEERPID` on macOS).
//! 2. [`admit`] applies the rejection rules over an [`AccountLookup`].
//!    It is a pure function so the whole rule table is unit-tested
//!    against a mock; [`SystemLookup`] is the production lookup.
//! 3. A [`Rejection`] is rendered by [`rejection_frame`] into the single
//!    `id: null` `-32000 Unauthorized` frame the listener writes
//!    before closing.
//!
//! Anything that cannot be determined fails closed.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::PeerIdentity;

/// Lowest interactive UID on Linux (`UID_MIN` of a stock `login.defs`).
#[cfg(target_os = "linux")]
pub const MIN_INTERACTIVE_UID: u32 = 1000;
/// Lowest interactive UID on macOS (first user created by Setup Assistant is 501).
#[cfg(not(target_os = "linux"))]
pub const MIN_INTERACTIVE_UID: u32 = 500;

/// Upper bound for the whole account lookup (NSS / LDAP / opendirectoryd
/// can stall). Exceeding it is rule 5 — a retryable rejection.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Which SPEC §2.12.4.1 rule rejected the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Rule 1 — UID below the interactive floor.
    UidBelowFloor,
    /// Rule 2 — UID 0.
    Root,
    /// Rule 3 — no passwd entry (possibly a transient directory outage).
    NoPasswdEntry,
    /// Rule 4 — login shell is empty / relative / unlisted / `nologin` / `false`.
    NonInteractiveShell,
    /// Rule 5 — credential or account lookup failed (fail closed).
    LookupFailed,
}

impl Rule {
    /// `data.retryable` on the wire. Derived from the rule alone:
    /// permanent account ineligibility is `false`, directory trouble is `true`.
    pub fn retryable(self) -> bool {
        matches!(self, Rule::NoPasswdEntry | Rule::LookupFailed)
    }
}

/// A refused peer: the rule that fired plus a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub rule: Rule,
    pub detail: String,
}

impl Rejection {
    fn new(rule: Rule, detail: impl Into<String>) -> Self {
        Self {
            rule,
            detail: detail.into(),
        }
    }

    pub fn retryable(&self) -> bool {
        self.rule.retryable()
    }
}

/// The two passwd fields the admission rules need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub shell: String,
}

/// Account-database access, abstracted so [`admit`] can be tested
/// without a real passwd file. `Err` means the lookup itself failed
/// (rule 5); `Ok(None)` means it succeeded and found nothing (rule 3).
pub trait AccountLookup: Send + Sync {
    fn passwd(&self, uid: u32) -> Result<Option<Account>, String>;
    /// Contents of `/etc/shells`, one entry per element.
    fn login_shells(&self) -> Result<Vec<String>, String>;
    /// Best-effort OS session of `pid` (display only; `None` is fine).
    fn session_of(&self, pid: u32) -> Option<u32>;
}

/// Apply the SPEC §2.12.4.1 rules to a peer and build its [`PeerIdentity`].
pub fn admit(
    uid: u32,
    pid: u32,
    lookup: &dyn AccountLookup,
    min_uid: u32,
) -> Result<PeerIdentity, Rejection> {
    if uid == 0 {
        return Err(Rejection::new(
            Rule::Root,
            "uid 0 (root) is not allowed to use KLP",
        ));
    }
    if uid < min_uid {
        return Err(Rejection::new(
            Rule::UidBelowFloor,
            format!("uid {uid} is below the minimum interactive uid {min_uid}"),
        ));
    }
    let account = match lookup.passwd(uid) {
        Ok(Some(a)) => a,
        Ok(None) => {
            return Err(Rejection::new(
                Rule::NoPasswdEntry,
                format!("uid {uid} has no passwd entry"),
            ));
        }
        Err(e) => {
            return Err(Rejection::new(
                Rule::LookupFailed,
                format!("passwd lookup for uid {uid} failed: {e}"),
            ));
        }
    };
    check_shell(uid, &account.shell, lookup)?;
    Ok(PeerIdentity {
        user_key: format!("uid-{uid}"),
        user_display: account.name,
        pid,
        session: lookup.session_of(pid),
    })
}

fn check_shell(uid: u32, shell: &str, lookup: &dyn AccountLookup) -> Result<(), Rejection> {
    let non_interactive =
        |why: &str| Rejection::new(Rule::NonInteractiveShell, format!("uid {uid}: {why}"));
    if shell.is_empty() {
        return Err(non_interactive("login shell is empty"));
    }
    if !shell.starts_with('/') {
        return Err(non_interactive("login shell is not an absolute path"));
    }
    let base = shell.rsplit('/').next().unwrap_or(shell);
    if base == "nologin" || base == "false" {
        return Err(non_interactive("login shell is non-interactive"));
    }
    let listed = lookup.login_shells().map_err(|e| {
        Rejection::new(
            Rule::LookupFailed,
            format!("cannot read the list of login shells: {e}"),
        )
    })?;
    if !listed.iter().any(|s| s == shell) {
        return Err(non_interactive("login shell is not listed in /etc/shells"));
    }
    Ok(())
}

/// Serialize the rejection as the one frame body SPEC §2.12.4.1 prescribes.
pub fn rejection_frame(rejection: &Rejection) -> Vec<u8> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {
            "code": -32000,
            "message": "Unauthorized",
            "data": {
                "kind": "Unauthorized",
                "retryable": rejection.retryable(),
                "detail": rejection.detail,
            },
        },
    });
    serde_json::to_vec(&body).expect("static JSON shape serializes")
}

/// Run [`admit`] on the blocking pool under [`LOOKUP_TIMEOUT`]; a stalled
/// directory is rule 5 rather than a hung accept task.
pub async fn admit_with_timeout(
    lookup: Arc<dyn AccountLookup>,
    uid: u32,
    pid: u32,
    min_uid: u32,
) -> Result<PeerIdentity, Rejection> {
    let task = tokio::task::spawn_blocking(move || admit(uid, pid, lookup.as_ref(), min_uid));
    match tokio::time::timeout(LOOKUP_TIMEOUT, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => Err(Rejection::new(
            Rule::LookupFailed,
            format!("account lookup task failed: {e}"),
        )),
        Err(_) => Err(Rejection::new(
            Rule::LookupFailed,
            "account lookup timed out",
        )),
    }
}

/// Kernel-attested identity of the process on the other end of `sock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub pid: u32,
}

/// Read `(uid, pid)` of the connected peer. Any failure (syscall error,
/// short or malformed result) is reported as a rule-5 [`Rejection`].
pub fn peer_credentials(sock: &impl AsRawFd) -> Result<PeerCred, Rejection> {
    peer_credentials_impl(sock.as_raw_fd())
        .map_err(|e| Rejection::new(Rule::LookupFailed, format!("peer credentials: {e}")))
}

#[cfg(target_os = "linux")]
fn peer_credentials_impl(fd: std::os::fd::RawFd) -> Result<PeerCred, String> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid for writes of the sizes passed.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(format!("SO_PEERCRED: {}", std::io::Error::last_os_error()));
    }
    if len as usize != std::mem::size_of::<libc::ucred>() || cred.pid <= 0 {
        return Err("SO_PEERCRED returned an unexpected result".into());
    }
    Ok(PeerCred {
        uid: cred.uid,
        pid: cred.pid as u32,
    })
}

#[cfg(target_os = "macos")]
fn peer_credentials_impl(fd: std::os::fd::RawFd) -> Result<PeerCred, String> {
    let mut xuc: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
    // SAFETY: `xuc`/`len` are valid for writes of the sizes passed.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERCRED,
            (&mut xuc as *mut libc::xucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(format!(
            "LOCAL_PEERCRED: {}",
            std::io::Error::last_os_error()
        ));
    }
    if len as usize != std::mem::size_of::<libc::xucred>() || xuc.cr_version != libc::XUCRED_VERSION
    {
        return Err("LOCAL_PEERCRED returned an unexpected result".into());
    }

    let mut pid: libc::pid_t = 0;
    let mut plen = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid`/`plen` are valid for writes of the sizes passed.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut plen,
        )
    };
    if rc != 0 {
        return Err(format!(
            "LOCAL_PEERPID: {}",
            std::io::Error::last_os_error()
        ));
    }
    if plen as usize != std::mem::size_of::<libc::pid_t>() || pid <= 0 {
        return Err("LOCAL_PEERPID returned an unexpected result".into());
    }
    Ok(PeerCred {
        uid: xuc.cr_uid,
        pid: pid as u32,
    })
}

/// Production [`AccountLookup`]: libc `getpwuid_r`, `/etc/shells`, and
/// (Linux) the peer's cgroup for the logind session.
pub struct SystemLookup;

impl AccountLookup for SystemLookup {
    fn passwd(&self, uid: u32) -> Result<Option<Account>, String> {
        getpwuid(uid)
    }

    fn login_shells(&self) -> Result<Vec<String>, String> {
        let text = std::fs::read_to_string("/etc/shells").map_err(|e| e.to_string())?;
        Ok(parse_shells(&text))
    }

    fn session_of(&self, pid: u32) -> Option<u32> {
        #[cfg(target_os = "linux")]
        {
            let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
            parse_logind_session(&text)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            None
        }
    }
}

/// Entries of an `/etc/shells` body: non-blank, non-comment lines, trimmed.
fn parse_shells(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Extract `N` from a `session-N.scope` cgroup path component. Only a
/// purely numeric id counts (logind also issues ids like `c1`); anything
/// else is `None`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_logind_session(cgroup: &str) -> Option<u32> {
    cgroup.lines().flat_map(|l| l.rsplit('/')).find_map(|comp| {
        comp.strip_prefix("session-")
            .and_then(|r| r.strip_suffix(".scope"))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse().ok())
    })
}

/// `getpwuid_r` with `ERANGE` retry. A missing entry is `Ok(None)`;
/// any other errno is `Err`. The shell is returned exactly as stored
/// (no defaulting) so admission judges the real value.
fn getpwuid(uid: u32) -> Result<Option<Account>, String> {
    let mut buf_len: usize = 1024;
    const MAX_BUF: usize = 1 << 20;
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; buf_len];
        // SAFETY: all pointers refer to live locals; `buf` is `buf_len` long.
        let rc = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                &mut pwd,
                buf.as_mut_ptr(),
                buf_len,
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf_len < MAX_BUF {
            buf_len *= 2;
            continue;
        }
        if rc != 0 {
            return match rc {
                libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM => Ok(None),
                _ => Err(std::io::Error::from_raw_os_error(rc).to_string()),
            };
        }
        if result.is_null() {
            return Ok(None);
        }
        // SAFETY: on success the string fields point into `buf` (NUL-terminated).
        let (name, shell) = unsafe {
            let cstr = |p: *const libc::c_char| {
                if p.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            (cstr(pwd.pw_name), cstr(pwd.pw_shell))
        };
        return Ok(Some(Account { name, shell }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockLookup {
        passwd: Result<HashMap<u32, Account>, String>,
        shells: Result<Vec<String>, String>,
        session: Option<u32>,
    }

    impl MockLookup {
        fn with(uid: u32, name: &str, shell: &str) -> Self {
            Self {
                passwd: Ok(HashMap::from([(
                    uid,
                    Account {
                        name: name.into(),
                        shell: shell.into(),
                    },
                )])),
                shells: Ok(vec![
                    "/bin/sh".into(),
                    "/bin/bash".into(),
                    "/bin/zsh".into(),
                ]),
                session: Some(7),
            }
        }
    }

    impl AccountLookup for MockLookup {
        fn passwd(&self, uid: u32) -> Result<Option<Account>, String> {
            self.passwd.clone().map(|m| m.get(&uid).cloned())
        }
        fn login_shells(&self) -> Result<Vec<String>, String> {
            self.shells.clone()
        }
        fn session_of(&self, _pid: u32) -> Option<u32> {
            self.session
        }
    }

    fn reject(uid: u32, lookup: &MockLookup, min: u32) -> Rejection {
        admit(uid, 42, lookup, min).expect_err("must be rejected")
    }

    #[test]
    fn eligible_user_gets_identity() {
        let l = MockLookup::with(1000, "alice", "/bin/bash");
        let id = admit(1000, 42, &l, 1000).unwrap();
        assert_eq!(id.user_key, "uid-1000");
        assert_eq!(id.user_display, "alice");
        assert_eq!(id.pid, 42);
        assert_eq!(id.session, Some(7));
        assert_eq!(id.handshake_session_id(), 1000);
    }

    #[test]
    fn missing_session_does_not_reject() {
        let mut l = MockLookup::with(1000, "alice", "/bin/bash");
        l.session = None;
        assert_eq!(admit(1000, 1, &l, 1000).unwrap().session, None);
    }

    #[test]
    fn uid_floor_boundaries() {
        for (floor, below, at) in [(1000, 999, 1000), (500, 499, 500)] {
            let l = MockLookup::with(below, "svc", "/bin/bash");
            assert_eq!(reject(below, &l, floor).rule, Rule::UidBelowFloor);
            let l = MockLookup::with(at, "u", "/bin/bash");
            assert!(admit(at, 1, &l, floor).is_ok());
        }
    }

    #[test]
    fn floor_detail_matches_spec_example() {
        let l = MockLookup::with(998, "svc", "/bin/bash");
        assert_eq!(
            reject(998, &l, 1000).detail,
            "uid 998 is below the minimum interactive uid 1000"
        );
    }

    #[test]
    fn root_is_rejected_even_with_zero_floor() {
        let l = MockLookup::with(0, "root", "/bin/bash");
        assert_eq!(reject(0, &l, 0).rule, Rule::Root);
    }

    #[test]
    fn no_passwd_entry_is_retryable() {
        let l = MockLookup::with(1000, "alice", "/bin/bash");
        let r = reject(1001, &l, 1000);
        assert_eq!(r.rule, Rule::NoPasswdEntry);
        assert!(r.retryable());
    }

    #[test]
    fn passwd_lookup_error_fails_closed_and_retryable() {
        let mut l = MockLookup::with(1000, "alice", "/bin/bash");
        l.passwd = Err("sssd down".into());
        let r = reject(1000, &l, 1000);
        assert_eq!(r.rule, Rule::LookupFailed);
        assert!(r.retryable());
    }

    #[test]
    fn shell_rules() {
        for shell in [
            "",
            "bash",
            "./bash",
            "/usr/sbin/nologin",
            "/sbin/nologin",
            "/usr/bin/false",
            "/bin/false",
            "/opt/not-listed/sh",
        ] {
            let l = MockLookup::with(1000, "alice", shell);
            let r = reject(1000, &l, 1000);
            assert_eq!(r.rule, Rule::NonInteractiveShell, "shell {shell:?}");
            assert!(!r.retryable());
        }
    }

    #[test]
    fn unreadable_shells_file_fails_closed_and_retryable() {
        let mut l = MockLookup::with(1000, "alice", "/bin/bash");
        l.shells = Err("EACCES".into());
        let r = reject(1000, &l, 1000);
        assert_eq!(r.rule, Rule::LookupFailed);
        assert!(r.retryable());
    }

    #[test]
    fn retryable_classification() {
        assert!(!Rule::UidBelowFloor.retryable());
        assert!(!Rule::Root.retryable());
        assert!(!Rule::NonInteractiveShell.retryable());
        assert!(Rule::NoPasswdEntry.retryable());
        assert!(Rule::LookupFailed.retryable());
    }

    #[test]
    fn rejection_frame_shape() {
        let r = Rejection::new(Rule::NoPasswdEntry, "uid 5000 has no passwd entry");
        let v: serde_json::Value = serde_json::from_slice(&rejection_frame(&r)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {
                    "code": -32000,
                    "message": "Unauthorized",
                    "data": {
                        "kind": "Unauthorized",
                        "retryable": true,
                        "detail": "uid 5000 has no passwd entry",
                    },
                },
            })
        );
        let r = Rejection::new(Rule::Root, "x");
        let v: serde_json::Value = serde_json::from_slice(&rejection_frame(&r)).unwrap();
        assert_eq!(v["error"]["data"]["retryable"], false);
    }

    #[test]
    fn shells_file_parsing() {
        let t = "# comment\n\n/bin/sh\n  /bin/bash  \n#/bin/csh\n";
        assert_eq!(parse_shells(t), vec!["/bin/sh", "/bin/bash"]);
    }

    #[test]
    fn logind_session_parsing() {
        let ok = "0::/user.slice/user-1000.slice/session-12.scope\n";
        assert_eq!(parse_logind_session(ok), Some(12));
        assert_eq!(
            parse_logind_session(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope\n"
            ),
            None
        );
        assert_eq!(parse_logind_session("0::/x/session-c1.scope\n"), None);
        assert_eq!(parse_logind_session(""), None);
    }

    #[test]
    fn real_peer_credentials_match_self() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let cred = peer_credentials(&a).unwrap();
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        // socketpair peers report the creating process on Linux; macOS
        // LOCAL_PEERPID of a socketpair is the creator too.
        assert_eq!(cred.pid, std::process::id());
    }

    #[test]
    fn real_getpwuid_finds_current_user_or_none() {
        // Must not error out on this host's account database.
        let uid = unsafe { libc::geteuid() };
        assert!(getpwuid(uid).is_ok());
        assert_eq!(getpwuid(u32::MAX - 7).unwrap(), None);
    }
}
