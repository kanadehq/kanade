//! Per-user CLI configuration.
//!
//! The kanade CLI historically had no config file at all (env vars +
//! flags only); this is its first one, introduced to control the
//! kaishin-powered self-update behaviour. Location:
//!
//! - Windows: `%APPDATA%\kanade\config.toml`
//! - Unix:    `~/.config/kanade/config.toml`
//!
//! ```toml
//! [update]
//! mode = "notify"        # off | notify | install   (default: notify)
//! check_interval = "24h" # optional; kaishin humantime, default 24h
//! ```
//!
//! Loading is lenient by design: a missing file is seeded with a
//! commented starter (best-effort — a read-only profile must not
//! break the CLI) so the knobs are discoverable by just opening it; a
//! malformed file WARNs and falls back to defaults — the CLI's real
//! work must never be blocked by update-check plumbing. Unknown keys
//! are ignored so future fields don't break older binaries.

use serde::Deserialize;
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Default, Deserialize)]
pub struct CliConfig {
    #[serde(default)]
    pub update: UpdateConfig,
}

/// What the CLI does about new releases on ordinary invocations
/// (`kanade self-update` always works regardless).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Never even check.
    Off,
    /// Check in the background (throttled) and print a banner when a
    /// newer release exists. The default.
    Notify,
    /// Silently download + swap the binary in the background; the new
    /// version takes effect on the next invocation.
    Install,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfig {
    #[serde(default = "default_mode")]
    pub mode: UpdateMode,
    /// kaishin humantime interval between background checks (e.g.
    /// `12h`, `7d`). `None` = kaishin's default (24h).
    #[serde(default)]
    pub check_interval: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            mode: default_mode(),
            check_interval: None,
        }
    }
}

fn default_mode() -> UpdateMode {
    UpdateMode::Notify
}

/// `<config_dir>/kanade/config.toml`, or `None` when the OS config dir
/// can't be resolved.
pub fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("kanade").join("config.toml"))
}

/// Commented starter seeded on first run — values spell out the
/// defaults, so writing it changes nothing until the user edits it.
const TEMPLATE: &str = r#"# kanade CLI configuration

[update]
# What to do about new releases on ordinary invocations:
#   off     — never check
#   notify  — check in the background, print a banner when newer (default)
#   install — silently update in the background (next run uses it)
# `KANADE_NO_AUTOUPDATE=1` disables everything regardless of this file.
mode = "notify"

# Interval between background checks (humantime). Default: 24h.
# check_interval = "24h"
"#;

/// Load the config. Missing file → seed the commented [`TEMPLATE`]
/// (best-effort) and return defaults; malformed file → WARN + defaults.
pub fn load() -> CliConfig {
    let Some(p) = path() else {
        return CliConfig::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            warn!(error = %e, path = %p.display(), "malformed CLI config — using defaults");
            CliConfig::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First run: make the knobs discoverable by opening the
            // file. Failures (read-only profile, odd ACLs) are silent —
            // the defaults apply either way.
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&p, TEMPLATE);
            CliConfig::default()
        }
        Err(_) => CliConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seeded starter must parse and spell out exactly the
    /// defaults — writing it on first run changes no behaviour.
    #[test]
    fn template_parses_to_defaults() {
        let cfg: CliConfig = toml::from_str(TEMPLATE).expect("template parses");
        assert_eq!(cfg.update.mode, UpdateMode::Notify);
        assert!(cfg.update.check_interval.is_none());
    }
}
