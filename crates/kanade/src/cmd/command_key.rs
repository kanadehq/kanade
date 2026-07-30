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
/// Bounds the age of a **signature**, not the life of the key — the key itself
/// never expires, and one retrieved from an offline copy months later works
/// because using it produces a fresh signature at that moment. This is why one
/// break-glass key suffices rather than a rotating pair.
///
/// It has to be short enough that a captured command cannot be replayed later:
/// `kanade run` sets `deadline_at: None`, and the agent's replay dedup is an
/// in-memory cache that is **empty after a reboot** — which an incident makes
/// likely — so on a machine replaying from JetStream this bound is the only
/// thing standing between it and a stale emergency command.
///
/// # Why an hour and not the tighter window this started at
///
/// The check is symmetric (`age > bound || age < -bound`), so this is really a
/// **± clock-skew tolerance** between the signing host and the target. And the
/// machines that need break-glass correlate with the machines whose clocks are
/// wrong: off for months, dead CMOS battery, w32time not yet resynced. A
/// window tight enough to be elegant is one where a *freshly made* signature
/// reads as stale on exactly the host you are trying to rescue.
///
/// Weighed against that, what a replay actually buys an attacker here is
/// narrow: re-running the same script the operator already ran, not arbitrary
/// code. The costs are asymmetric — a too-tight window fails the feature
/// completely at the one moment it exists for, while a looser one widens a
/// bounded, low-value replay. An hour also stays far from the 7-day JetStream
/// retention that motivated having a bound at all.
///
/// Per-key overridable with `--max-age-mins`; the value travels in the keyring
/// entry, so tightening it later is a re-provision rather than a rebuild.
const DEFAULT_MAX_AGE_MINS: u64 = 60;

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

/// The freshness window as a `Duration`.
///
/// Exists so the minutes-to-seconds conversion has exactly one home. A test
/// that redoes the arithmetic cannot catch an error in the arithmetic — it
/// agrees with a `* 6` typo as readily as with `* 60` — so the generator and
/// the test that pins its output must go through the same call.
fn max_age(mins: u64) -> std::time::Duration {
    std::time::Duration::from_secs(mins * 60)
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
    let max_age = max_age(args.max_age_mins);
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
    fn the_default_window_reaches_the_keyring_entry_as_an_hour() {
        // Asserted through the entry the operator actually pastes, and through
        // the same `max_age` the generator calls. Both halves are needed for
        // this to mean anything: an earlier version of this test did its own
        // `* 60`, which agrees with a `* 6` typo in the generator as readily
        // as with the correct code — it pinned the constant while claiming to
        // pin the conversion.
        //
        // Pinned at all because the number is a judgement, not an arbitrary
        // constant: the freshness check is symmetric, so this is the ±
        // clock-skew tolerance between the signing host and the target, and a
        // machine that needs break-glass is disproportionately one whose clock
        // is wrong. Tightening it is a deliberate act with a rehearsal cost.
        let key = signing::generate_keypair().unwrap();
        let entry = signing::break_glass_keyring_entry(
            "bg",
            &key.verifying_key(),
            "break-glass",
            max_age(DEFAULT_MAX_AGE_MINS),
        );
        assert_eq!(entry["max_age_secs"], 3600);
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
