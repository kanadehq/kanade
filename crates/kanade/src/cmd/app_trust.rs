//! Managed package-publisher trust administration for `kanade app`
//! (SPEC §2.13.4.3, stage 2b). Everything is local: no backend, NATS or
//! Object Store, and no live key is ever changed automatically.
//!
//! * `trust-root-record` / `trust-state-sign` run where the trust-root private
//!   key is held. The key comes from a hidden prompt or `--secret-stdin`, never
//!   argv/env, and is never printed.
//! * `trust-store-provision` is the explicit administrator step that pins the
//!   trust root on a host. `trust-store-apply` only accepts states signed by
//!   that pinned root. `trust-store-status` prints public identifiers, times
//!   and health only.
//!
//! These commands write to the agent's trust directory (default
//! `<data_dir>/package-trust`, override with `--dir`) and need the privileges
//! that directory requires. No management transport calls them yet.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use kanade_shared::publisher_trust::{
    ApplyOutcome, PublisherTrustStore, TrustRoot, TrustStateClaims, TrustStateDraft,
    sign_trust_state,
};

use super::app_manifest::{read_signing_key, read_small};

#[derive(Args, Debug)]
pub struct SecretArgs {
    /// Read the trust-root private key from one stdin line instead of a hidden prompt.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Args, Debug)]
pub struct StateSignArgs {
    /// Draft JSON: trust_set_id, sequence, issued_at, not_after (unix seconds),
    /// publishers [{key_id, public_key, not_before, not_after}], revoked [key_id].
    #[arg(long)]
    pub draft: PathBuf,
    /// Where to write the signed state. Never overwrites an existing file.
    #[arg(long)]
    pub out: PathBuf,
    /// Read the trust-root private key from one stdin line instead of a hidden prompt.
    #[arg(long)]
    pub secret_stdin: bool,
}

#[derive(Args, Debug)]
pub struct ProvisionArgs {
    /// Trust-root record obtained from the root holder over an authenticated channel.
    #[arg(long)]
    pub root_record: PathBuf,
    /// Trust directory (default: `<data_dir>/package-trust`).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// Signed trust state (output of `trust-state-sign`).
    #[arg(long)]
    pub state: PathBuf,
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

fn store(dir: Option<PathBuf>) -> PublisherTrustStore {
    PublisherTrustStore::open_system(
        dir.unwrap_or_else(|| kanade_shared::default_paths::data_dir().join("package-trust")),
    )
}

pub fn root_record(args: SecretArgs) -> Result<()> {
    let key = read_signing_key(args.secret_stdin)?;
    std::io::stdout().write_all(&TrustRoot::from_signing_key(&key).to_root_json())?;
    Ok(())
}

pub fn state_sign(args: StateSignArgs) -> Result<()> {
    let draft: TrustStateDraft =
        serde_json::from_slice(&read_small(&args.draft)?).context("parse trust state draft")?;
    let claims = TrustStateClaims::from_draft(draft)?;
    let key = read_signing_key(args.secret_stdin)?;
    let bytes = sign_trust_state(&claims, &key);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.out)
        .with_context(|| format!("create {} (refusing to overwrite)", args.out.display()))?;
    if let Err(e) = out.write_all(&bytes).and_then(|()| out.sync_all()) {
        drop(out);
        let _ = std::fs::remove_file(&args.out);
        return Err(e).with_context(|| format!("write {}", args.out.display()));
    }
    println!("Wrote signed trust state {}", args.out.display());
    println!(
        "trust_set_id: {}  sequence: {}",
        claims.trust_set_id(),
        claims.sequence()
    );
    println!("NOTE: signing changes nothing live; apply it with `trust-store-apply` on each host.");
    Ok(())
}

pub fn provision(args: ProvisionArgs) -> Result<()> {
    let root = store(args.dir).provision_root(&read_small(&args.root_record)?)?;
    println!("Pinned trust root {}", root.key_id().as_str());
    println!(
        "NOTE: this host now trusts states signed by that root. Verify the key_id out of band."
    );
    Ok(())
}

pub fn apply(args: ApplyArgs) -> Result<()> {
    let outcome = store(args.dir)
        .apply(&read_small(&args.state)?)
        .context("trust state REJECTED (nothing was changed)")?;
    match outcome {
        ApplyOutcome::Advanced => println!("Applied trust state"),
        ApplyOutcome::Unchanged => println!("Trust state already applied (no change)"),
    }
    Ok(())
}

pub fn status(args: StatusArgs) -> Result<()> {
    let st = store(args.dir).status();
    println!("health:  {}", st.health);
    println!(
        "root:    {}",
        st.root_key_id.as_deref().unwrap_or("(not pinned)")
    );
    match st.state {
        None => println!("state:   (none applied)"),
        Some(s) => {
            println!("state:   set {} sequence {}", s.trust_set_id, s.sequence);
            println!("issued_at: {}  not_after: {}", s.issued_at, s.not_after);
            println!("digest:  sha256:{}", s.digest);
            for (id, nb, na) in &s.publishers {
                println!("active:  {id}  [{nb}, {na})");
            }
            for id in &s.revoked {
                println!("revoked: {id}");
            }
        }
    }
    Ok(())
}
