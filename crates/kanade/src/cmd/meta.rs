//! `kanade meta …` — per-PC operator metadata on the `agent_meta` KV
//! bucket (free-form key/value: the primary user's name / email /
//! department, an ad-hoc note).
//!
//! Sibling of `kanade group` (same NATS-direct shape): reads / writes the
//! bucket straight over JetStream, bypassing the backend HTTP API, gated
//! by the NATS bearer token ($KANADE_NATS_TOKEN). Being NATS-direct, it
//! keeps working during a backend outage.
//!
//! The intended bulk producer is an operator AD-sync job on the (domain-
//! joined) backend host: `kanade query` fetches the roster
//! (`agents.last_logon_user`), resolves each user's directory attributes
//! via ADSI, and calls `meta set` here. That flow overwrites only its own
//! synced keys and leaves hand-entered keys alone, which is why `set` is a
//! per-key upsert (not a whole-set replace).

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::kv::BUCKET_AGENT_META;
use kanade_shared::wire::AgentMeta;
use tracing::info;

#[derive(Args, Debug)]
pub struct MetaArgs {
    #[command(subcommand)]
    pub sub: MetaSub,
}

#[derive(Subcommand, Debug)]
pub enum MetaSub {
    /// Show all key/value attributes for a PC.
    Get { pc_id: String },
    /// Set (upsert) one key on a PC. Overwrites the value if the key
    /// already exists; other keys are left untouched. An empty value
    /// keeps the key with a blank value — use `rm` to drop a key.
    Set {
        pc_id: String,
        key: String,
        value: String,
    },
    /// Remove one key from a PC (idempotent).
    Rm { pc_id: String, key: String },
    /// Clear ALL attributes for a PC.
    Clear { pc_id: String },
}

pub async fn execute(client: async_nats::Client, args: MetaArgs) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let kv = js.get_key_value(BUCKET_AGENT_META).await.with_context(|| {
        format!("KV '{BUCKET_AGENT_META}' missing — run `kanade jetstream setup`")
    })?;

    match args.sub {
        MetaSub::Get { pc_id } => get(&kv, &pc_id).await,
        MetaSub::Set { pc_id, key, value } => set(&kv, &pc_id, key, value).await,
        MetaSub::Rm { pc_id, key } => rm(&kv, &pc_id, &key).await,
        MetaSub::Clear { pc_id } => clear(&kv, &pc_id).await,
    }
}

async fn get(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<()> {
    let m = read_meta(kv, pc_id).await?;
    if m.is_empty() {
        println!("{pc_id}: (no attributes)");
    } else {
        for e in &m.entries {
            println!("{}: {}", e.key, e.value);
        }
    }
    Ok(())
}

async fn set(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    key: String,
    value: String,
) -> Result<()> {
    // Reject an empty key up front — clearer than a confusing "no change"
    // and it saves a NATS round-trip (the upsert would refuse it anyway).
    if key.trim().is_empty() {
        anyhow::bail!("metadata key must not be empty");
    }
    // #505-style CAS read-modify-write — a blind get→put here races a
    // concurrent set/rm for the same PC (two operators, or the AD-sync job
    // fanning out) and silently drops one side's change.
    let mut changed = false;
    let _ = kanade_shared::kv_cas::read_modify_write(kv, pc_id, |m: &mut AgentMeta| {
        changed = m.upsert(&key, &value);
        changed
    })
    .await
    .with_context(|| format!("set meta for {pc_id}"))?;
    if changed {
        info!(pc_id, key = %key, "agent_meta set");
        println!("{pc_id}: set '{}' = '{}'", key.trim(), value.trim());
    } else {
        println!(
            "{pc_id}: '{}' already '{}' (no change)",
            key.trim(),
            value.trim()
        );
    }
    Ok(())
}

async fn rm(kv: &async_nats::jetstream::kv::Store, pc_id: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        anyhow::bail!("metadata key must not be empty");
    }
    let mut changed = false;
    let _ = kanade_shared::kv_cas::read_modify_write(kv, pc_id, |m: &mut AgentMeta| {
        changed = m.remove(key);
        changed
    })
    .await
    .with_context(|| format!("remove meta for {pc_id}"))?;
    if changed {
        info!(pc_id, key = %key, "agent_meta removed");
        println!("{pc_id}: removed '{}'", key.trim());
    } else {
        println!("{pc_id}: no key '{}' (no change)", key.trim());
    }
    Ok(())
}

async fn clear(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<()> {
    write_meta(kv, pc_id, &AgentMeta::default()).await?;
    println!("{pc_id}: cleared all attributes");
    Ok(())
}

async fn read_meta(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<AgentMeta> {
    match kv.get(pc_id).await.context("kv get")? {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode agent_meta"),
        None => Ok(AgentMeta::default()),
    }
}

async fn write_meta(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    meta: &AgentMeta,
) -> Result<()> {
    let bytes = serde_json::to_vec(meta).context("encode agent_meta")?;
    kv.put(pc_id, bytes.into()).await.context("kv put")?;
    info!(pc_id, count = meta.entries.len(), "agent_meta written");
    Ok(())
}
