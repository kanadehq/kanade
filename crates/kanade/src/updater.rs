//! kaishin-powered self-update for the kanade CLI.
//!
//! Two entry points:
//!
//! - [`maybe_spawn`] / [`finalize`] — wrapped around every ordinary
//!   invocation in `main`. Behaviour is driven by the per-user config
//!   ([`crate::cli_config`], default `notify`): the throttled
//!   background check overlaps the command's real work as a tokio
//!   task, and at shutdown a banner (notify) or a one-line "updated"
//!   notice (install) is printed to stderr. `KANADE_NO_AUTOUPDATE`
//!   is the env kill-switch (works even with a broken config file).
//! - `kanade self-update` (see `cmd::self_update`) — the explicit,
//!   interactive path; always available regardless of mode.
//!
//! kaishin handles the GitHub Releases lookup, the 24 h throttle, the
//! cross-process advisory lock, the dev-build (target/) no-op, and the
//! atomic binary swap.

use std::path::PathBuf;
use std::time::Duration;

use crate::cli_config::{self, UpdateMode};

/// Bounded shutdown wait for a pending notify check — long enough for
/// a warm GitHub round-trip, short enough to never feel like a hang.
const NOTIFY_DRAIN: Duration = Duration::from_millis(800);
/// Install mode opted in to background swaps, so give the download a
/// real window at shutdown (it only fires once per throttle interval).
const INSTALL_DRAIN: Duration = Duration::from_secs(10);

pub fn options() -> kaishin::KaishinOptions {
    kaishin::KaishinOptions::new(
        "kanadehq",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// Throttle bookkeeping is transient → OS cache dir (mirrors renri),
/// not kaishin's data-dir default.
fn cache_state_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("kanade").join("last_update_check.json"))
}

type CheckResult = anyhow::Result<Option<kaishin::LatestRelease>>;

/// Handle for an in-flight or cached background update check, drained
/// by [`finalize`] at shutdown.
pub enum UpdateHandle {
    /// A newer version is already known from a previous run's cache —
    /// no network this run, just print the banner at shutdown.
    CachedAvailable {
        checker: kaishin::Checker,
        latest: kaishin::LatestRelease,
    },
    /// A notify-mode check is running as a spawned task.
    Pending {
        checker: kaishin::Checker,
        handle: tokio::task::JoinHandle<CheckResult>,
        cached_latest: Option<kaishin::LatestRelease>,
    },
    /// An install-mode silent update is running as a spawned task.
    Installing {
        handle: tokio::task::JoinHandle<CheckResult>,
    },
}

/// Spawn the background check/install per the resolved mode. `skip`
/// silences it for invocations where it would be noise (the
/// `self-update` subcommand itself).
pub fn maybe_spawn(skip: bool) -> Option<UpdateHandle> {
    if skip {
        return None;
    }
    // Env kill-switch first — must work even when the config is broken.
    if std::env::var_os("KANADE_NO_AUTOUPDATE").is_some() {
        return None;
    }
    let cfg = cli_config::load().update;
    if cfg.mode == UpdateMode::Off {
        return None;
    }

    let mut checker = kaishin::Checker::new(env!("CARGO_PKG_NAME"), options());
    if let Some(p) = cache_state_path() {
        checker = checker.state_path(p);
    }
    if let Some(iv) = cfg
        .check_interval
        .as_deref()
        .and_then(|s| kaishin::parse_interval(s).ok())
    {
        checker = checker.interval(iv);
    }

    match cfg.mode {
        UpdateMode::Off => None,
        UpdateMode::Install => {
            let c = checker.clone();
            Some(UpdateHandle::Installing {
                handle: tokio::spawn(async move { c.auto_update().await }),
            })
        }
        UpdateMode::Notify => {
            if checker.should_check() {
                let c = checker.clone();
                let cached_latest = checker.cached_update();
                Some(UpdateHandle::Pending {
                    checker,
                    handle: tokio::spawn(async move { c.check_and_save().await }),
                    cached_latest,
                })
            } else {
                checker
                    .cached_update()
                    .map(|latest| UpdateHandle::CachedAvailable { checker, latest })
            }
        }
    }
}

/// Drain the background check at shutdown (bounded — never holds the
/// CLI's exit hostage) and print the banner / installed notice.
pub async fn finalize(handle: Option<UpdateHandle>) {
    match handle {
        None => {}
        Some(UpdateHandle::CachedAvailable { checker, latest }) => {
            eprintln!("\n{}", checker.format_banner(&latest));
        }
        Some(UpdateHandle::Pending {
            checker,
            handle,
            cached_latest,
        }) => {
            let latest = match tokio::time::timeout(NOTIFY_DRAIN, handle).await {
                // A completed fetch is authoritative either way:
                // Ok(None) means "up to date", which must SUPPRESS a
                // stale pre-check cache entry, not fall back to it.
                Ok(Ok(Ok(latest))) => latest,
                // Timeout / join error / fetch error: fall back to the
                // cache — the result (if any) was persisted for next run.
                _ => cached_latest,
            };
            if let Some(latest) = latest {
                eprintln!("\n{}", checker.format_banner(&latest));
            }
        }
        Some(UpdateHandle::Installing { handle }) => {
            if let Ok(Ok(Ok(Some(latest)))) = tokio::time::timeout(INSTALL_DRAIN, handle).await {
                eprintln!(
                    "\nkanade updated to {} — takes effect on the next invocation.",
                    latest.tag_name
                );
            }
            // Anything else (throttled, no release, dev build, lock held,
            // timeout) is silent by design.
        }
    }
}
