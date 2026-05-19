//! Spec §2.11 install layout — OS-aware default paths for config /
//! mutable data / logs, plus the [`find_config`] fallback chain that
//! every binary uses to locate its config file.
//!
//! Layout
//!
//! ```text
//! Windows                                    Linux
//! C:\Program Files\Kanade\                   /usr/local/bin/
//!   ↑ binaries                                 ↑ binaries
//!
//! C:\ProgramData\Kanade\config\              /etc/kanade/
//!   ├─ agent.toml                              ├─ agent.toml
//!   └─ backend.toml                            └─ backend.toml
//!
//! C:\ProgramData\Kanade\data\                /var/lib/kanade/
//!   ├─ state.db        (agent)                 ├─ state.db
//!   ├─ outbox/         (agent)                 ├─ outbox/
//!   ├─ staging/        (agent self-update)     ├─ staging/
//!   ├─ backend.db      (backend)               ├─ backend.db
//!   ├─ certs/                                  ├─ certs/
//!   └─ nats/           (JetStream data)        └─ nats/
//!
//! C:\ProgramData\Kanade\logs\                /var/log/kanade/
//!   ├─ agent.log                               ├─ agent.log
//!   ├─ backend.log                             ├─ backend.log
//!   └─ nats-server.log                         └─ nats-server.log
//! ```

use std::path::{Path, PathBuf};

/// `%ProgramData%\Kanade\config\` on Windows, `/etc/kanade/` on Linux.
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        program_data().join("Kanade").join("config")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/etc/kanade")
    }
}

/// Where the agent stores its outbox, state.db, local_completions.json,
/// staging area, etc. Defaults to:
///   * Windows: `%ProgramData%\Kanade\data\`
///   * Linux:   `/var/lib/kanade/`
///
/// Honors `KANADE_AGENT_DATA_DIR` (non-empty value wins) so a single
/// host can run multiple isolated agents — the dev-fleet target uses
/// this to fan out agents into `target/dev-data/agents/{pc_id}/`
/// without two agents stomping on each other's outbox files.
pub fn data_dir() -> PathBuf {
    if let Some(os_path) = std::env::var_os("KANADE_AGENT_DATA_DIR").filter(|s| !s.is_empty()) {
        // Promote relative env paths to absolute against the current
        // cwd so the agent's outbox / state.db stay put even if a
        // later component changes its working directory (the
        // self_update flow notably does this on Windows). Honours the
        // `dirs_are_os_appropriate` invariant that the rest of the
        // codebase assumes about data_dir's return value.
        let path = PathBuf::from(&os_path);
        if path.is_absolute() {
            return path;
        }
        return std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or_else(|_| PathBuf::from(os_path));
    }
    #[cfg(target_os = "windows")]
    {
        program_data().join("Kanade").join("data")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/var/lib/kanade")
    }
}

/// `%ProgramData%\Kanade\logs\` on Windows, `/var/log/kanade/` on Linux.
pub fn log_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        program_data().join("Kanade").join("logs")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/var/log/kanade")
    }
}

#[cfg(target_os = "windows")]
fn program_data() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// Resolve the config file path through the fallback chain:
///
/// 1. `flag` (e.g. `--config <path>`) — honored verbatim, even when the
///    file does not exist (caller's choice).
/// 2. `env_var` value (e.g. `KANADE_AGENT_CONFIG`) — honored verbatim
///    when set to a non-empty string.
/// 3. OS-standard location `<config_dir>/<basename>` — only used when
///    the file is actually present.
///
/// Returns an error when none of the above produced a usable path; the
/// message lists every option an operator can take to fix it.
pub fn find_config(flag: Option<&Path>, env_var: &str, basename: &str) -> anyhow::Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    if let Ok(raw) = std::env::var(env_var)
        && !raw.is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let std_path = config_dir().join(basename);
    if std_path.exists() {
        return Ok(std_path);
    }
    Err(anyhow::anyhow!(
        "config not found — pass `--config <path>`, set `{env_var}`, or place `{basename}` at `{}`",
        std_path.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Unique env var per test so the global-environment writes don't race.
    /// (Cargo runs tests within a binary on a single thread by default for
    /// unit tests, but be explicit to avoid surprises.)
    fn unique_env(test_name: &str) -> String {
        format!("KANADE_TEST_CFG_{test_name}_{}", std::process::id())
    }

    /// Tests that touch the *real* `KANADE_AGENT_DATA_DIR` env var
    /// (or call `data_dir()` whose return value depends on it) must
    /// take this lock so they serialize against each other. Cargo
    /// test parallelises across tests within a binary by default, and
    /// macOS CI in particular cranks that parallelism high enough to
    /// reliably catch this kind of cross-test pollution.
    static DATA_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_data_dir_env() -> std::sync::MutexGuard<'static, ()> {
        DATA_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn find_config_prefers_flag_over_env_and_default() {
        let env = unique_env("flag_wins");
        // SAFETY: single-threaded test, env var owned by us.
        unsafe {
            std::env::set_var(&env, "env-path.toml");
        }
        let flag = PathBuf::from("flag-path.toml");
        let got = find_config(Some(&flag), &env, "agent.toml").expect("ok");
        assert_eq!(got, flag);
        unsafe { std::env::remove_var(&env) };
    }

    #[test]
    fn find_config_uses_env_when_flag_missing() {
        let env = unique_env("env_wins");
        unsafe {
            std::env::set_var(&env, "env-path.toml");
        }
        let got = find_config(None, &env, "agent.toml").expect("ok");
        assert_eq!(got, PathBuf::from("env-path.toml"));
        unsafe { std::env::remove_var(&env) };
    }

    #[test]
    fn find_config_skips_empty_env() {
        let env = unique_env("empty_env");
        unsafe {
            std::env::set_var(&env, "");
        }
        // No flag, env is empty, and the OS-standard path almost certainly
        // doesn't exist on the test machine — should return an error.
        let r = find_config(None, &env, "non-existent-kanade-test.toml");
        assert!(r.is_err(), "expected error, got {:?}", r);
        unsafe { std::env::remove_var(&env) };
    }

    #[test]
    fn find_config_errors_with_helpful_message() {
        let env = unique_env("missing");
        let r = find_config(None, &env, "definitely-not-here-kanade-test.toml");
        let err = r.expect_err("should error");
        let msg = format!("{err}");
        assert!(msg.contains("--config"), "msg: {msg}");
        assert!(msg.contains(&env), "msg: {msg}");
        assert!(
            msg.contains("definitely-not-here-kanade-test.toml"),
            "msg: {msg}"
        );
    }

    /// Gemini #54 review: when the env override is set to a relative
    /// path (e.g. the dev-fleet tasks use
    /// `target/dev-data/agents/dev-pc-1`), data_dir() must still
    /// return an absolute path — anchored against the cwd at call
    /// time. Otherwise a later component that chdir's (the
    /// self_update flow does, notably) would point the outbox at a
    /// surprise location.
    #[test]
    fn data_dir_promotes_relative_env_to_absolute() {
        let _g = lock_data_dir_env();
        unsafe {
            std::env::set_var("KANADE_AGENT_DATA_DIR", "target/dev-data/agents/dev-pc-x");
        }
        let p = data_dir();
        assert!(p.is_absolute(), "expected absolute path, got {p:?}");
        assert!(
            p.ends_with("target/dev-data/agents/dev-pc-x")
                || p.ends_with(r"target\dev-data\agents\dev-pc-x"),
            "trailing components should match the env value, got {p:?}",
        );
        unsafe {
            std::env::remove_var("KANADE_AGENT_DATA_DIR");
        }
    }

    #[test]
    fn dirs_are_os_appropriate() {
        let _g = lock_data_dir_env();
        let cfg = config_dir();
        let data = data_dir();
        let logs = log_dir();
        // Sanity: all three are absolute paths.
        assert!(cfg.is_absolute(), "config_dir = {cfg:?}");
        assert!(data.is_absolute(), "data_dir = {data:?}");
        assert!(logs.is_absolute(), "log_dir = {logs:?}");
        // And distinct from each other.
        assert_ne!(cfg, data);
        assert_ne!(data, logs);
        assert_ne!(cfg, logs);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_dirs_root_at_program_data_kanade() {
        let _g = lock_data_dir_env();
        let cfg = config_dir();
        let data = data_dir();
        let logs = log_dir();
        assert!(cfg.ends_with("Kanade\\config"), "{cfg:?}");
        assert!(data.ends_with("Kanade\\data"), "{data:?}");
        assert!(logs.ends_with("Kanade\\logs"), "{logs:?}");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_dirs_match_fhs_conventions() {
        let _g = lock_data_dir_env();
        assert_eq!(config_dir(), PathBuf::from("/etc/kanade"));
        assert_eq!(data_dir(), PathBuf::from("/var/lib/kanade"));
        assert_eq!(log_dir(), PathBuf::from("/var/log/kanade"));
    }
}
