//! Break-glass command-signing key (#1165).
//!
//! The one signing key kanade deliberately mints **in the CLI**, which is
//! otherwise the place this design says must never hold one. The exception is
//! not a weakening of that rule — it follows from what the key is for.
//!
//! `kanade run` publishes commands straight to `commands.pc.<id>` with no
//! backend involvement, and that is the documented recovery route for a
//! backend that is down. At stage 3, when agents reject unsigned commands, that
//! route breaks unless something can sign for it — and the thing that can sign
//! for it must work when the backend does not, so it cannot be the backend's
//! key. Four ways out were considered on #1165; three were rejected:
//!
//! * give the CLI the **backend's** key — puts the fleet's crown jewel on every
//!   operator laptop, which is the property the whole feature exists to avoid;
//! * route `kanade run` **through the backend** — the recovery path would then
//!   depend on the thing it recovers from;
//! * **drop the path** — viable, and worth revisiting if break-glass turns out
//!   to be unused, but the reversible choice is to keep it.
//!
//! So: a separate key, deliberately guarded, with a short freshness bound and
//! an audit record on every use. Its whole purpose requires it to exist outside
//! any running system, which is why it is generated here — a private key that
//! must reach an offline medium has to leave the process that made it.
//!
//! # What this command does not do
//!
//! It does not store the private key. Not in the registry (that would put it on
//! an operator machine, the rejected option arriving by a side door), not in a
//! file, not in the config. It prints it once. Between incidents it belongs on
//! an offline medium or in a password manager, and the procedure for retrieving
//! it needs rehearsing — an unrehearsed break-glass procedure is one that fails
//! when it is used.

use anyhow::Result;
use clap::{Args, Subcommand};
use kanade_shared::signing;

/// Default freshness bound for a newly minted break-glass key.
///
/// Long enough for a human to retrieve the key from wherever it rests, compose
/// a command and send it. Short enough that a captured command cannot be
/// replayed later: `kanade run` sets `deadline_at: None`, and the agent's
/// replay dedup is an in-memory cache that is **empty on first boot**, so on a
/// machine that reboots into a JetStream replay this bound is the only thing
/// standing between it and a week-old emergency command.
const DEFAULT_MAX_AGE_MINS: u64 = 15;

#[derive(Args, Debug)]
pub struct CommandKeyArgs {
    #[command(subcommand)]
    pub command: CommandKeyCmd,
}

#[derive(Subcommand, Debug)]
pub enum CommandKeyCmd {
    /// Mint a break-glass signing key. Prints the private key ONCE and stores
    /// it nowhere.
    BreakGlass(BreakGlassArgs),
}

#[derive(Args, Debug)]
pub struct BreakGlassArgs {
    /// Key id agents will match against. Defaults to a timestamp, so a
    /// re-issue produces a distinguishable second entry — two different keys
    /// must never share an id.
    ///
    /// Stamped to the minute rather than the day: break-glass keys are stored
    /// nowhere, so unlike the backend's generator there is nothing here to
    /// check a new id against, and a second incident on the same afternoon is
    /// an ordinary thing to have.
    #[arg(long)]
    pub kid: Option<String>,
    /// Reject a signature older than this many minutes.
    #[arg(long, default_value_t = DEFAULT_MAX_AGE_MINS)]
    pub max_age_mins: u64,
}

pub fn execute(args: CommandKeyArgs) -> Result<()> {
    match args.command {
        CommandKeyCmd::BreakGlass(a) => break_glass(a),
    }
}

/// Decide the `kid`.
///
/// Pure so the default is testable. **Cannot** do what the backend's
/// `resolve_kid` does — refuse an id that collides with the key already in
/// use — because a break-glass key is deliberately stored nowhere, so there is
/// no "currently in use" to compare against. What is available is making the
/// default unlikely to collide in the first place; a duplicate that arrives
/// anyway (a hand-edited array, a re-typed entry) is caught agent-side by
/// `parse_keyring`, which refuses a ring containing the same id twice rather
/// than letting one key silently overwrite the other.
fn resolve_kid(requested: Option<&str>, stamp: &str) -> Result<String, String> {
    let kid = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("break-glass-{stamp}"));
    // A blank id would be accepted by nothing: `Kanade-Sig-Kid: ""` matches no
    // keyring entry, so every provisioned agent would report an unknown key.
    if kid.trim().is_empty() {
        return Err("the key id is empty".into());
    }
    Ok(kid)
}

fn break_glass(args: BreakGlassArgs) -> Result<()> {
    if args.max_age_mins == 0 {
        anyhow::bail!(
            "--max-age-mins 0 would reject every signature the instant it is made. \
             Pick a window a human can act inside."
        );
    }
    let kid = resolve_kid(
        args.kid.as_deref(),
        &chrono::Utc::now().format("%Y%m%d-%H%M").to_string(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    let key = signing::generate_keypair().map_err(|e| anyhow::anyhow!(e))?;
    let max_age = std::time::Duration::from_secs(args.max_age_mins * 60);
    let entry =
        signing::break_glass_keyring_entry(&kid, &key.verifying_key(), "break-glass", max_age);

    // Printed, never written. See the module doc: a break-glass key that rests
    // on the machine that generated it is the rejected "CLI holds a signing
    // key" option with extra steps.
    println!("Break-glass signing key minted. It is NOT stored anywhere.");
    println!();
    println!("kid:         {kid}");
    println!("max age:     {} minutes", args.max_age_mins);
    println!();
    println!("PRIVATE KEY — shown once. Move it to an offline medium or a password manager now:");
    println!("  {}", signing::encode_secret(&key));
    println!();
    println!("Add this entry to every agent's HKLM\\SOFTWARE\\kanade\\agent\\CommandKeys array,");
    println!("alongside the backend's entry (the array is REPLACED, not merged — include both):");
    println!("{}", serde_json::to_string_pretty(&entry)?);
    println!();
    println!(
        "Distribute it in the SAME pass as the backend key, not a later one. A machine that is \
         offline when break-glass is added keeps a backend-only ring, and at stage 3 it cannot be \
         given the key afterwards — the command carrying it would be rejected by the very gap it \
         is meant to fix."
    );
    println!();
    println!(
        "To use it during an incident, set both halves and run `kanade run` as usual:\n  \
         {}=<the private key above>\n  {}={kid}",
        super::run::ENV_BREAK_GLASS_KEY,
        super::run::ENV_BREAK_GLASS_KID
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kid_defaults_to_a_timestamp_including_the_time_of_day() {
        // Stamped to the minute, not the day. Two invocations on one afternoon
        // is an ordinary thing to have — a second incident, or a re-issue after
        // a botched transcription — and a day-only stamp would hand both keys
        // the same id. An agent's ring is keyed by id, so the second entry
        // would silently replace the first and every command signed by the
        // first would stop verifying with nothing to explain it.
        assert_eq!(
            resolve_kid(None, "20260730-1432").unwrap(),
            "break-glass-20260730-1432"
        );
    }

    #[test]
    fn an_explicit_kid_wins_and_is_trimmed() {
        assert_eq!(
            resolve_kid(Some("  bg-1  "), "20260730-1432").unwrap(),
            "bg-1"
        );
    }

    #[test]
    fn a_blank_explicit_kid_falls_back_rather_than_signing_under_nothing() {
        // `--kid ""` is a typo, not a request for an empty id. Falling back to
        // the stamp is safe; accepting the blank would make every provisioned
        // agent report an unknown key.
        assert_eq!(
            resolve_kid(Some("   "), "20260730-1432").unwrap(),
            "break-glass-20260730-1432"
        );
    }
}
