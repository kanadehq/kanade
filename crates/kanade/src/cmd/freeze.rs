//! `kanade freeze` — fleet-wide change-freeze (#418 Phase 5).
//!
//! A single fleet-global "stop all automated change" switch. While a
//! freeze is active, the backend scheduler and every agent's local
//! scheduler skip every fire. Set it during an incident or a planned
//! change-freeze; clear it to thaw.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::Freeze;

#[derive(Args, Debug)]
pub struct FreezeArgs {
    #[command(subcommand)]
    pub sub: FreezeSub,
}

#[derive(Subcommand, Debug)]
pub enum FreezeSub {
    /// Show the current fleet freeze (or report the fleet is live).
    Status,
    /// Freeze the fleet. With no bounds it freezes indefinitely until
    /// `kanade freeze clear`; `--from` / `--until` schedule a window.
    Set {
        /// Frozen from this instant (RFC3339 `2026-12-20T00:00:00+09:00`
        /// or bare `2026-12-20`). Omit for no lower bound (frozen from
        /// the beginning of time — i.e. effective immediately).
        #[arg(long)]
        from: Option<String>,
        /// Thawed from this instant on, exclusive. Omit for an
        /// open-ended freeze (manual clear).
        #[arg(long)]
        until: Option<String>,
        /// Operator note shown on the freeze-skip log + SPA banner
        /// ("year-end change freeze", "INC-1234").
        #[arg(long)]
        reason: Option<String>,
        /// Evaluate bare-date bounds in UTC instead of host-local.
        #[arg(long)]
        utc: bool,
    },
    /// Clear the fleet freeze (thaw). Idempotent — a no-op when the
    /// fleet isn't frozen.
    Clear,
}

pub async fn execute(backend_url: &str, args: FreezeArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        FreezeSub::Status => status(base).await,
        FreezeSub::Set {
            from,
            until,
            reason,
            utc,
        } => set(base, from, until, reason, utc).await,
        FreezeSub::Clear => clear(base).await,
    }
}

async fn status(base: &str) -> Result<()> {
    let url = format!("{base}/api/freeze");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("freeze status failed: {status} — {body}");
    }
    let freeze: Option<Freeze> = resp.json().await?;
    match freeze {
        None => println!("fleet is NOT frozen (no freeze configured — schedules fire normally)"),
        Some(f) => {
            // A freeze can be CONFIGURED but not currently active — a
            // future `from` or a past `until` means it isn't gating
            // fires right now (coderabbit #472). Report the live state,
            // not merely "a freeze row exists".
            if f.is_active(chrono::Utc::now()) {
                println!("fleet is FROZEN now — all schedule fires are skipped");
            } else {
                println!(
                    "freeze is CONFIGURED but NOT active right now (outside its window — schedules fire normally)"
                );
            }
            match (&f.from, &f.until) {
                (None, None) => println!("  window : indefinite (until `kanade freeze clear`)"),
                (from, until) => println!(
                    "  window : {} .. {} ({:?})",
                    from.as_deref().unwrap_or("-∞"),
                    until.as_deref().unwrap_or("+∞"),
                    f.tz,
                ),
            }
            if let Some(r) = &f.reason {
                println!("  reason : {r}");
            }
        }
    }
    Ok(())
}

async fn set(
    base: &str,
    from: Option<String>,
    until: Option<String>,
    reason: Option<String>,
    utc: bool,
) -> Result<()> {
    use kanade_shared::manifest::ScheduleTz;
    let freeze = Freeze {
        from,
        until,
        reason,
        tz: if utc {
            ScheduleTz::Utc
        } else {
            ScheduleTz::Local
        },
    };
    // Client-side validation first so a bad bound errors at the
    // operator's shell, not as the backend's 400 (same rationale as
    // `kanade schedule create`).
    freeze
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid freeze: {e}"))?;
    let url = format!("{base}/api/freeze");
    let resp = crate::http_client::authed_client()?
        .put(&url)
        .json(&freeze)
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("freeze set rejected: {status} — {body}");
    }
    println!("fleet change-freeze set. Run `kanade freeze status` to confirm.");
    Ok(())
}

async fn clear(base: &str) -> Result<()> {
    let url = format!("{base}/api/freeze");
    let resp = crate::http_client::authed_client()?
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("freeze clear rejected: {status} — {body}");
    }
    println!("fleet change-freeze cleared (thawed).");
    Ok(())
}
