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

/// `%ProgramData%\Kanade\data\` on Windows, `/var/lib/kanade/` on Linux.
pub fn data_dir() -> PathBuf {
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
