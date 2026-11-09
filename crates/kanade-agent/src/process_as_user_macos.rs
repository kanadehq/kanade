//! macOS spawn path for `run_as: user` / `run_as: system_gui` — the
//! counterpart of the Windows WTS token dance in `process_as_user.rs`.
//!
//! The agent is a root LaunchDaemon, so a plain spawn lands in the system
//! bootstrap namespace: no Keychain, no WindowServer, no `open`. Instead
//! the job's host is wrapped as
//!
//! ```text
//! run_as: user        /bin/launchctl asuser <uid> /usr/bin/sudo -n -u #<target-uid> -H -- \
//!                         /usr/bin/env -i <user env> <program> <args...>
//! run_as: system_gui  /bin/launchctl asuser <uid> /usr/bin/env -i <root env> <program> <args...>
//! ```
//!
//! `launchctl asuser` moves the chain into the console user's GUI
//! bootstrap (it `exec`s, so it is the direct child); `sudo` drops to the
//! user's identity (uid, gid, supplementary groups); `env -i` replaces the
//! root daemon's environment with an explicit one, so nothing the agent
//! inherited (the NATS token in particular) reaches a user process. Every
//! value is its own argv element — no shell ever re-parses them.
//!
//! The caller (`process::run_command_with_kill`) owns everything after
//! the builder: piped stdout/stderr, the live tail, the own-session spawn
//! and the process-group kill on timeout / operator kill. So this module
//! only decides *who*, *where* and *with which environment* — including
//! the PATH ([`job_path`]) that `run_as: system` jobs get too.

#![cfg(target_os = "macos")]

use std::ffi::{CStr, c_char};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kanade_shared::wire::RunAs;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tracing::{info, warn};

const LAUNCHCTL: &str = "/bin/launchctl";
const SUDO: &str = "/usr/bin/sudo";
const ENV: &str = "/usr/bin/env";

/// Prepended ahead of the `path_helper` entries: Homebrew on Apple Silicon
/// (`/opt/homebrew`) and the Microsoft `pwsh` package / Intel-era
/// Homebrew (`/usr/local`). A LaunchDaemon's own PATH never has them.
const PATH_PREFIX: [&str; 3] = ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"];

/// Stock `/etc/paths`, used only when the real file can't be read so a job
/// still finds `sh` and friends.
const FALLBACK_ETC_PATHS: &str = "/usr/local/bin\n/usr/bin\n/bin\n/usr/sbin\n/sbin\n";

const FALLBACK_LANG: &str = "en_US.UTF-8";

/// Root-owned, world-traversable home for staged PowerShell launchers —
/// the macOS analog of `%ProgramData%\Kanade\agent-scripts`. Not the agent
/// data dir: that one is 0700 and the `run_as: user` child could not read
/// the script it is asked to run.
const STAGING_CATEGORY: &str = "/Library/Application Support/Kanade/agent-scripts";

/// A resolved passwd entry.
#[derive(Debug)]
struct Account {
    uid: u32,
    name: String,
    home: String,
    shell: String,
}

/// Build the `launchctl asuser` command for a `run_as: user` /
/// `run_as: system_gui` job whose host is `program args...`.
///
/// No console user (login window, or `/dev/console` unreadable) is an
/// error, exactly like the Windows path's "no active console session":
/// the job is not run and the error surfaces from `run_command_with_kill`.
pub(crate) async fn session_command(
    run_as: RunAs,
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<ProcessCommand> {
    debug_assert!(matches!(run_as, RunAs::User | RunAs::SystemGui));
    let console_owner = std::fs::metadata("/dev/console").map(|m| m.uid()).ok();
    let Some(console_uid) = interactive_uid(console_owner) else {
        bail!(
            "no console user logged in (/dev/console is owned by root or unreadable) — \
             run_as: user / system_gui needs a logged-in user"
        );
    };
    session_command_for_uid(run_as, console_uid, program, args, cwd).await
}

/// Pin the target discovered by the caller; never retarget to a newly switched user.
pub(crate) async fn session_command_for_uid(
    run_as: RunAs,
    console_uid: u32,
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<ProcessCommand> {
    if crate::screen_permission_macos::console_uid() != Some(console_uid) {
        bail!("console user changed before session launch");
    }
    // `system_gui` keeps the agent's own identity (root) — only the
    // bootstrap namespace moves to the user's GUI session.
    let target_uid = match run_as {
        RunAs::User => console_uid,
        _ => current_euid(),
    };
    let target = lookup_account(target_uid)?;
    let path = job_path();
    let lang = std::env::var("LANG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| FALLBACK_LANG.to_string());
    let env = job_env(&target, &lang, &path, None);
    let temp = resolve_temp_dir(run_as, console_uid, &target, &env).await?;
    let env = job_env(&target, &lang, &path, Some(&temp));
    if crate::screen_permission_macos::console_uid() != Some(console_uid) {
        bail!("console user changed while resolving session temporary directory");
    }
    let argv = launch_argv(run_as, console_uid, &target, &env, program, args);

    let mut builder = ProcessCommand::new(LAUNCHCTL);
    builder.env_clear().args(&argv);
    if let Some(dir) = launch_cwd(run_as, cwd, &target.home) {
        builder.current_dir(dir);
    }
    info!(
        run_as = ?run_as,
        console_uid,
        account = %target.name,
        program,
        "launching in the console user's GUI session via launchctl asuser",
    );
    Ok(builder)
}

/// Expand a leading `~` in a `run_as: system` cwd against the agent's own
/// home (root's `/var/root`), matching what the Windows system path does
/// with its own token. Lookup failure keeps the raw value, with a warning.
pub(crate) fn expand_agent_cwd(raw: &str) -> String {
    if split_tilde(raw).is_none() {
        return raw.to_string();
    }
    match lookup_account(current_euid()) {
        Ok(account) => expand_tilde(raw, &account.home),
        Err(e) => {
            warn!(error = %e, raw_cwd = %raw, "cwd expansion failed; using raw value");
            raw.to_string()
        }
    }
}

/// Create this process's script staging dir (`<category>/<uuid>`), 0755
/// all the way down so a `run_as: user` child can read the launcher it is
/// handed. Only root can write under `/Library/Application Support`, so a
/// non-root agent (dev runs, `cargo test`) stages under `$TMPDIR` instead.
pub(crate) fn create_staging_dir(uuid: &str) -> Result<PathBuf> {
    let dir = if current_euid() == 0 {
        let category = Path::new(STAGING_CATEGORY);
        // `mode` caps the creation mode (it is still umask-filtered), so no
        // directory is ever more permissive than 0755, not even briefly.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(category)
            .with_context(|| format!("create_dir_all {}", category.display()))?;
        for d in [category.parent().unwrap_or(category), category] {
            set_mode(d, 0o755)?;
        }
        category.join(uuid)
    } else {
        std::env::temp_dir().join(format!("kanade-agent-{uuid}"))
    };
    // Non-clobber: the UUID is unguessable, so nobody can pre-create it.
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(&dir)
        .with_context(|| format!("create_dir {}", dir.display()))?;
    set_mode(&dir, 0o755)?;
    Ok(dir)
}

/// `chmod` to exactly `mode`, overriding whatever the daemon's umask did.
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// The console owner is the interactive user; uid 0 means the login window
/// holds the console, i.e. nobody is logged in. Unknown owner ⇒ nobody.
fn interactive_uid(console_owner: Option<u32>) -> Option<u32> {
    console_owner.filter(|&uid| uid != 0)
}

fn current_euid() -> u32 {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// `getpwuid_r` for `uid`, with the buffer grown on `ERANGE`.
fn lookup_account(uid: u32) -> Result<Account> {
    const MAX_BUF: usize = 1 << 20;
    let mut buf: Vec<c_char> = vec![0; 4096];
    loop {
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call; `buf.len()` is the
        // true capacity of `buf`.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc))
                .with_context(|| format!("getpwuid_r({uid})"));
        }
        if result.is_null() {
            bail!("uid {uid} has no passwd entry");
        }
        // SAFETY: a non-null `result` means `pwd` was filled in; its string
        // fields point into `buf`, which outlives these reads.
        let pwd = unsafe { pwd.assume_init_ref() };
        let name = passwd_field(pwd.pw_name, uid, "pw_name")?;
        if name.is_empty() {
            bail!("uid {uid} has an empty passwd name");
        }
        let home = passwd_field(pwd.pw_dir, uid, "pw_dir")?;
        let shell = passwd_field(pwd.pw_shell, uid, "pw_shell")?;
        // login(1) semantics for blank fields.
        return Ok(Account {
            uid,
            name,
            home: if home.is_empty() { "/".into() } else { home },
            shell: if shell.is_empty() {
                "/bin/sh".into()
            } else {
                shell
            },
        });
    }
}

fn passwd_field(ptr: *const c_char, uid: u32, field: &str) -> Result<String> {
    if ptr.is_null() {
        return Ok(String::new());
    }
    // SAFETY: getpwuid_r's fields are NUL-terminated strings inside `buf`.
    let s = unsafe { CStr::from_ptr(ptr) };
    s.to_str()
        .map(str::to_owned)
        .with_context(|| format!("uid {uid}: {field} is not UTF-8"))
}

/// PATH for every macOS job (system ones too): [`PATH_PREFIX`], then
/// `/etc/paths`, then every file in `/etc/paths.d` in name order —
/// `path_helper(8)` semantics.
pub(crate) fn job_path() -> String {
    let etc_paths = std::fs::read_to_string("/etc/paths").unwrap_or_else(|e| {
        warn!(error = %e, "read /etc/paths failed; using the stock entries");
        FALLBACK_ETC_PATHS.to_string()
    });
    let mut files: Vec<PathBuf> = std::fs::read_dir("/etc/paths.d")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let paths_d: Vec<String> = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    build_path(&etc_paths, &paths_d)
}

/// Join [`PATH_PREFIX`] + one entry per non-blank line of `etc_paths` and
/// each `paths_d` file, keeping the first occurrence of a duplicate.
fn build_path(etc_paths: &str, paths_d: &[String]) -> String {
    let files = std::iter::once(etc_paths).chain(paths_d.iter().map(String::as_str));
    let mut entries: Vec<&str> = Vec::new();
    for entry in PATH_PREFIX
        .into_iter()
        .chain(files.flat_map(str::lines).map(str::trim))
    {
        if !entry.is_empty() && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(":")
}

/// The complete environment a session job starts with (`env -i`).
fn job_env(account: &Account, lang: &str, path: &str, temp: Option<&Path>) -> Vec<String> {
    let mut env = vec![
        format!("HOME={}", account.home),
        format!("USER={}", account.name),
        format!("LOGNAME={}", account.name),
        format!("SHELL={}", account.shell),
        format!("LANG={lang}"),
        format!("PATH={path}"),
    ];
    if let Some(temp) = temp {
        env.push(format!("TMPDIR={}", temp.display()));
    }
    env
}

const TEMP_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
const TEMP_OUTPUT_LIMIT: u64 = 4096;

/// Resolve under the same bootstrap and credentials as the host, before
/// PowerShell/.NET initializes its process-wide temporary-directory cache.
async fn resolve_temp_dir(
    run_as: RunAs,
    console_uid: u32,
    target: &Account,
    env: &[String],
) -> Result<PathBuf> {
    let output = session_probe(
        run_as,
        console_uid,
        target,
        env,
        "/usr/bin/getconf",
        &["DARWIN_USER_TEMP_DIR"],
    )
    .await
    .context("resolve session temporary directory")?;
    let path = parse_temp_dir(&output)?;
    // lstat each component, including the leaf, before canonicalization:
    // canonicalize alone would silently accept a redirected leaf symlink.
    validate_temp_dir(&path, target.uid)?;
    let canonical =
        std::fs::canonicalize(&path).context("canonicalize session temporary directory")?;
    validate_temp_dir(&canonical, target.uid)?;
    let path_arg = canonical
        .to_str()
        .context("temporary directory is not UTF-8")?;
    session_probe(
        run_as,
        console_uid,
        target,
        env,
        "/bin/sh",
        &[
            "-c",
            "umask 077; p=$(/usr/bin/mktemp \"$1/.kanade-temp-probe.XXXXXXXX\") || exit 1; /bin/rm -f -- \"$p\"",
            "kanade-temp-probe",
            path_arg,
        ],
    )
    .await
    .context("create/remove temporary file as session identity")?;
    validate_temp_dir(&canonical, target.uid)?;
    Ok(canonical)
}

fn parse_temp_dir(output: &[u8]) -> Result<PathBuf> {
    if output.len() as u64 > TEMP_OUTPUT_LIMIT {
        bail!("temporary directory response exceeds limit");
    }
    let value = std::str::from_utf8(output).context("temporary directory is not UTF-8")?;
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.is_empty() || value.chars().any(char::is_control) {
        bail!("invalid temporary directory response");
    }
    let path = Path::new(value);
    if !path.is_absolute()
        || value.split('/').any(|part| matches!(part, "." | ".."))
        || path == Path::new("/")
    {
        bail!("temporary directory must be an absolute non-root path without dot components");
    }
    // /var is a system alias on macOS. No other symlink is accepted.
    Ok(match path.strip_prefix("/var") {
        Ok(rest) => Path::new("/private/var").join(rest),
        Err(_) => path.to_path_buf(),
    })
}

fn validate_temp_dir(path: &Path, uid: u32) -> Result<()> {
    let mut component_path = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            bail!("invalid temporary directory component");
        }
        component_path.push(component);
        let metadata = std::fs::symlink_metadata(&component_path)
            .context("inspect session temporary directory component")?;
        let leaf = component_path == path;
        if !metadata.is_dir()
            || (leaf && metadata.uid() != uid)
            || (!leaf && metadata.uid() != 0 && metadata.uid() != uid)
            || metadata.mode() & 0o022 != 0
            || (leaf && metadata.mode() & 0o077 != 0)
        {
            bail!("unsafe session temporary directory ownership, type or permissions");
        }
    }
    Ok(())
}

/// A short, bounded child with no inherited secrets and no diagnostic output
/// mixed into the host's streams. The group guard also covers future cancellation.
async fn session_probe(
    run_as: RunAs,
    console_uid: u32,
    target: &Account,
    env: &[String],
    program: &str,
    args: &[&str],
) -> Result<Vec<u8>> {
    let mut command = ProcessCommand::new(LAUNCHCTL);
    command.args(launch_argv(run_as, console_uid, target, env, program, args));
    command.env_clear().current_dir("/");
    bounded_probe(command, TEMP_RESOLVE_TIMEOUT).await
}

struct ProbeGroup(u32);

impl Drop for ProbeGroup {
    fn drop(&mut self) {
        // SAFETY: the child owns this new process group; never signal our group.
        unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
    }
}

async fn bounded_probe(mut command: ProcessCommand, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .context("spawn session temporary directory probe")?;
    let group = ProbeGroup(child.id().context("probe has no pid")?);
    let stdout = child.stdout.take().context("probe has no stdout")?;
    let mut output = Vec::new();
    let result = tokio::time::timeout(timeout, async {
        stdout
            .take(TEMP_OUTPUT_LIMIT + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() as u64 > TEMP_OUTPUT_LIMIT {
            bail!("session temporary directory probe output exceeds limit");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("session temporary directory probe failed: {status}");
        }
        Ok(())
    })
    .await;
    // Kill descendants even if they have closed stdout and the leader exited.
    drop(group);
    let _ = child.wait().await;
    result.context("session temporary directory probe timed out")??;
    Ok(output)
}

/// Arguments to `/bin/launchctl` (argv\[1..\]). `target` is whose identity
/// the host runs under: the console user for `user`, the agent (root) for
/// `system_gui`.
fn launch_argv(
    run_as: RunAs,
    console_uid: u32,
    target: &Account,
    env: &[String],
    program: &str,
    args: &[&str],
) -> Vec<String> {
    let mut argv = vec!["asuser".to_string(), console_uid.to_string()];
    if run_as == RunAs::User {
        let numeric_uid = format!("#{}", target.uid);
        argv.extend(
            [SUDO, "-n", "-u", &numeric_uid, "-H", "--"]
                .into_iter()
                .map(String::from),
        );
    }
    argv.extend([ENV.to_string(), "-i".to_string()]);
    argv.extend(env.iter().cloned());
    argv.push(program.to_string());
    argv.extend(args.iter().map(|a| a.to_string()));
    argv
}

/// Working directory for the chain. An explicit cwd gets `~` expanded
/// against the target's home and is otherwise passed through (a missing
/// directory fails the spawn, as on the system path). Unset: a user job
/// starts in the user's home — the agent's own cwd is its 0700 data dir,
/// which the user cannot enter — while `system_gui` inherits the agent's
/// cwd like the system path.
fn launch_cwd(run_as: RunAs, raw: Option<&str>, target_home: &str) -> Option<PathBuf> {
    match raw.filter(|s| !s.is_empty()) {
        Some(dir) => Some(PathBuf::from(expand_tilde(dir, target_home))),
        None if run_as == RunAs::User && Path::new(target_home).is_dir() => {
            Some(PathBuf::from(target_home))
        }
        None => None,
    }
}

/// `~` → `""`, `~/x` → `"x"`; anything else (including `~name`) → `None`.
fn split_tilde(raw: &str) -> Option<&str> {
    if raw == "~" {
        Some("")
    } else {
        raw.strip_prefix("~/")
    }
}

fn expand_tilde(raw: &str, home: &str) -> String {
    match split_tilde(raw) {
        None => raw.to_string(),
        Some("") => home.to_string(),
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Account {
        Account {
            uid: 501,
            name: "alice".into(),
            home: "/Users/alice".into(),
            shell: "/bin/zsh".into(),
        }
    }

    #[test]
    fn login_window_or_unknown_console_owner_means_no_user() {
        assert_eq!(interactive_uid(Some(0)), None);
        assert_eq!(interactive_uid(None), None);
        assert_eq!(interactive_uid(Some(501)), Some(501));
    }

    #[test]
    fn path_follows_path_helper_order_and_dedupes() {
        let etc_paths = "/usr/local/bin\n/usr/bin\n\n  /bin  \n/usr/sbin\n/sbin\n";
        let paths_d = vec![
            "/var/run/cryptex/usr/bin\n".to_string(),
            "/opt/homebrew/bin\n/usr/bin\n".to_string(),
            "/Library/Apple/usr/bin".to_string(),
        ];
        assert_eq!(
            build_path(etc_paths, &paths_d),
            "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:\
             /usr/sbin:/sbin:/var/run/cryptex/usr/bin:/Library/Apple/usr/bin"
        );
    }

    #[test]
    fn path_without_etc_paths_entries_is_just_the_prefix() {
        assert_eq!(
            build_path("", &[]),
            "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin"
        );
    }

    #[test]
    fn user_argv_drops_to_the_user_then_replaces_the_environment() {
        let env = job_env(
            &user(),
            "ja_JP.UTF-8",
            "/usr/bin:/bin",
            Some(Path::new("/private/var/folders/user/T")),
        );
        let argv = launch_argv(
            RunAs::User,
            501,
            &user(),
            &env,
            "pwsh",
            &["-NoProfile", "-File", "/staged/launcher.ps1"],
        );
        assert_eq!(
            argv,
            [
                "asuser",
                "501",
                "/usr/bin/sudo",
                "-n",
                "-u",
                "#501",
                "-H",
                "--",
                "/usr/bin/env",
                "-i",
                "HOME=/Users/alice",
                "USER=alice",
                "LOGNAME=alice",
                "SHELL=/bin/zsh",
                "LANG=ja_JP.UTF-8",
                "PATH=/usr/bin:/bin",
                "TMPDIR=/private/var/folders/user/T",
                "pwsh",
                "-NoProfile",
                "-File",
                "/staged/launcher.ps1",
            ]
        );
    }

    #[test]
    fn system_gui_argv_joins_the_user_session_but_stays_root() {
        let root = Account {
            uid: 0,
            name: "root".into(),
            home: "/var/root".into(),
            shell: "/bin/sh".into(),
        };
        let env = job_env(
            &root,
            "en_US.UTF-8",
            "/usr/bin",
            Some(Path::new("/private/var/folders/root/T")),
        );
        let argv = launch_argv(RunAs::SystemGui, 501, &root, &env, "sh", &["-c", "id"]);
        assert_eq!(
            argv,
            [
                "asuser",
                "501",
                "/usr/bin/env",
                "-i",
                "HOME=/var/root",
                "USER=root",
                "LOGNAME=root",
                "SHELL=/bin/sh",
                "LANG=en_US.UTF-8",
                "PATH=/usr/bin",
                "TMPDIR=/private/var/folders/root/T",
                "sh",
                "-c",
                "id",
            ]
        );
        assert!(!argv.iter().any(|a| a == "/usr/bin/sudo"));
    }

    #[test]
    fn values_are_single_argv_elements_never_reparsed() {
        let odd = Account {
            uid: 502,
            name: "bob".into(),
            home: "/Users/bob smith/$(id)".into(),
            shell: "/bin/zsh".into(),
        };
        let script = "echo \"$HOME\"; rm -rf ~ `id` $(whoami)";
        let env = job_env(&odd, "en_US.UTF-8", "/usr/bin", None);
        let argv = launch_argv(RunAs::User, 502, &odd, &env, "sh", &["-c", script]);
        assert!(argv.contains(&"HOME=/Users/bob smith/$(id)".to_string()));
        assert_eq!(argv[argv.len() - 3..], ["sh", "-c", script]);
    }

    #[test]
    fn temp_response_only_accepts_one_absolute_path_and_normalizes_var() {
        assert_eq!(
            parse_temp_dir(b"/var/folders/test/T/\n").unwrap(),
            Path::new("/private/var/folders/test/T")
        );
        for invalid in [
            "", "\n", "/", "relative", "/a/../b", "/a/./b", "/a\n\n", "/a\r\n", "/a\0b",
        ] {
            assert!(parse_temp_dir(invalid.as_bytes()).is_err(), "{invalid:?}");
        }
        assert!(parse_temp_dir(&[b'/'; 4097]).is_err());
        assert!(parse_temp_dir(b"/\xff").is_err());
    }

    #[test]
    fn temp_directory_rejects_wrong_owner_open_modes_and_links() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap();
        set_mode(&path, 0o700).unwrap();
        validate_temp_dir(&path, current_euid()).unwrap();
        assert!(validate_temp_dir(&path, current_euid() + 1).is_err());
        for mode in [0o755, 0o770, 0o777] {
            set_mode(&path, mode).unwrap();
            assert!(validate_temp_dir(&path, current_euid()).is_err());
        }
        set_mode(&path, 0o700).unwrap();
        let leaf = path.join("leaf");
        std::os::unix::fs::symlink(&path, &leaf).unwrap();
        assert!(validate_temp_dir(&leaf, current_euid()).is_err());
        assert!(validate_temp_dir(&leaf.join("child"), current_euid()).is_err());
        std::fs::remove_file(&leaf).unwrap();
        std::fs::write(&leaf, b"file").unwrap();
        assert!(validate_temp_dir(&leaf, current_euid()).is_err());
    }

    #[test]
    fn resolved_temp_is_the_only_addition_to_the_environment_allowlist() {
        let env = job_env(
            &user(),
            "en_US.UTF-8",
            "/usr/bin",
            Some(Path::new("/private/var/folders/example/T")),
        );
        assert_eq!(
            env.iter()
                .map(|v| v.split('=').next().unwrap())
                .collect::<Vec<_>>(),
            ["HOME", "USER", "LOGNAME", "SHELL", "LANG", "PATH", "TMPDIR"]
        );
        let argv = launch_argv(RunAs::User, 501, &user(), &env, "pwsh", &["a b", "$(id)"]);
        assert!(argv.contains(&"#501".into()));
        assert_eq!(argv[argv.len() - 3..], ["pwsh", "a b", "$(id)"]);
    }

    #[tokio::test]
    async fn probe_bounds_output_and_preserves_failure_status() {
        for script in ["exit 7", "while :; do printf 'xxxxxxxxxxxxxxxx'; done"] {
            let mut command = ProcessCommand::new("/bin/sh");
            command.args(["-c", script]);
            assert!(
                bounded_probe(command, Duration::from_secs(1))
                    .await
                    .is_err()
            );
        }
        let mut command = ProcessCommand::new("/bin/sh");
        command.args(["-c", "printf /private/var/folders/test/T"]);
        assert_eq!(
            bounded_probe(command, Duration::from_secs(1))
                .await
                .unwrap(),
            b"/private/var/folders/test/T"
        );
    }

    #[tokio::test]
    async fn probe_timeout_kills_and_reaps_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("pid");
        let mut command = ProcessCommand::new("/bin/sh");
        command.args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "probe"]);
        command.arg(&pid_file);
        assert!(
            bounded_probe(command, Duration::from_millis(300))
                .await
                .is_err()
        );
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only tests existence. wait() must already have reaped it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn cancelling_probe_kills_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("pid");
        let mut command = ProcessCommand::new("/bin/sh");
        command.args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 30", "probe"]);
        command.arg(&pid_file);
        let task = tokio::spawn(bounded_probe(command, Duration::from_secs(10)));
        let pid: i32 = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = contents.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Tokio reaps kill_on_drop children asynchronously on cancellation.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // SAFETY: signal 0 only tests process existence.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH)
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn tilde_expands_against_the_given_home() {
        assert_eq!(expand_tilde("~", "/Users/alice"), "/Users/alice");
        assert_eq!(
            expand_tilde("~/src/x", "/Users/alice"),
            "/Users/alice/src/x"
        );
        assert_eq!(expand_tilde("~/src", "/Users/alice/"), "/Users/alice/src");
        assert_eq!(expand_tilde("~/x", "/"), "/x");
        // Only the caller's own `~` — no `~name`, no Windows separator.
        assert_eq!(expand_tilde("~bob/x", "/Users/alice"), "~bob/x");
        assert_eq!(expand_tilde("~\\x", "/Users/alice"), "~\\x");
        assert_eq!(expand_tilde("/tmp/~", "/Users/alice"), "/tmp/~");
    }

    #[test]
    fn explicit_cwd_wins_and_system_gui_inherits_when_unset() {
        assert_eq!(
            launch_cwd(RunAs::SystemGui, Some("~/work"), "/var/root"),
            Some(PathBuf::from("/var/root/work"))
        );
        assert_eq!(
            launch_cwd(RunAs::User, Some("/does/not/exist"), "/"),
            Some(PathBuf::from("/does/not/exist"))
        );
        assert_eq!(launch_cwd(RunAs::SystemGui, None, "/var/root"), None);
        assert_eq!(
            launch_cwd(RunAs::User, Some(""), "/"),
            Some(PathBuf::from("/"))
        );
    }
}
