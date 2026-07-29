pub mod account;
pub mod agent;
pub mod app;
pub mod bulk;

/// Recover a version label when VERSIONINFO extraction off a binary
/// fails (non-PE binary, missing VERSIONINFO resource).
///
/// #270: in an interactive shell the operator just wants to type the
/// version inline rather than re-run the whole `publish` command with
/// `--version`. In a pipe / CI a silent prompt would hang the job, so
/// the non-interactive path returns `Ok(None)` and lets the caller
/// fail fast with its own context-specific guidance.
///
/// Returns `Ok(Some(label))` when the operator answered, `Ok(None)`
/// when stdin is not a TTY (caller should `bail!`), and `Err` when the
/// operator was prompted but entered nothing (explicit abort).
pub(crate) async fn prompt_version_if_interactive(
    binary: std::path::PathBuf,
) -> anyhow::Result<Option<String>> {
    use anyhow::Context;

    // `read_line` is blocking stdin I/O. The callers are `async fn`s
    // holding a live NATS client, so run the read off the runtime's
    // worker threads via spawn_blocking — otherwise a slow operator
    // could starve tasks sharing that thread (e.g. NATS keep-alives).
    // The returned label is validated by the caller via
    // `validate_segment`, so no key-shape checks are duplicated here.
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        use std::io::{IsTerminal, Write};

        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }

        eprint!("VERSIONINFO not found in {binary:?}. Enter version label: ");
        std::io::stderr().flush().ok();

        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .context("read version label from stdin")?;
        let entered = buf.trim().to_string();
        if entered.is_empty() {
            anyhow::bail!("no version entered — aborting");
        }
        Ok(Some(entered))
    })
    .await
    .context("version prompt task")?
}

/// Mirror of the backend's object-key segment validators
/// (`api::app_packages::validate_segment` / `api::script_objects::`)
/// so the CLI rejects the same shapes the HTTP endpoint would — fails
/// fast on a typo before round-tripping to the bucket, avoiding a
/// confusing "upload succeeded; download 400s" loop. Keeping a CLI-side
/// copy is fine: the constraint set is small, stable, and lives outside
/// the wire crate today.
///
/// Shared by `app publish`, `agent publish` (#270 — the new interactive
/// prompt can feed an unvalidated label here) and `script` so all three
/// validate identically.
pub(crate) fn validate_segment(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must be non-empty");
    }
    if value.contains('/') {
        anyhow::bail!("{label} must not contain '/'");
    }
    for c in value.chars() {
        if !c.is_ascii() {
            anyhow::bail!("{label} must be ASCII-printable (rejected non-ASCII {c:?})");
        }
        if c.is_ascii_control() {
            anyhow::bail!("{label} must not contain control characters");
        }
        if c == '"' || c == '\\' {
            anyhow::bail!("{label} must not contain '\"' or '\\\\'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #270: under `cargo test` stdin is not a TTY, so the helper must
    /// take the non-interactive path and return `Ok(None)` (the caller
    /// then fails fast) — it must never block reading stdin in CI.
    #[tokio::test]
    async fn prompt_version_non_interactive_returns_none() {
        let got = prompt_version_if_interactive(std::path::PathBuf::from("vendor.msi"))
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn validate_segment_accepts_plain_labels() {
        assert!(validate_segment("version", "1.2.3").is_ok());
        assert!(validate_segment("name", "kanade-client").is_ok());
    }

    /// #270: an interactively-typed version must not be able to inject a
    /// key separator or control/non-ASCII bytes into the object-store key.
    #[test]
    fn validate_segment_rejects_bad_labels() {
        assert!(validate_segment("version", "").is_err(), "empty");
        assert!(validate_segment("version", "bad/version").is_err(), "slash");
        assert!(validate_segment("version", "v1\n").is_err(), "control");
        assert!(validate_segment("version", "café").is_err(), "non-ascii");
        assert!(validate_segment("version", "a\"b").is_err(), "quote");
    }
}

pub mod command_key;
pub mod config;
pub mod exec;
pub mod freeze;
pub mod group;
pub mod jetstream;
pub mod job;
pub mod kill;
pub mod login;
pub mod meta;
pub mod ping;
pub mod provenance;
pub mod publish_verify;
pub mod query;
pub mod revoke;
pub mod run;
pub mod schedule;
pub mod script;
pub mod self_update;
pub mod view;
