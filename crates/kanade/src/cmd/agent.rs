use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, KEY_AGENT_CONFIG_GLOBAL, OBJECT_AGENT_RELEASES,
};
use kanade_shared::wire::{AgentGroups, ConfigScope};
use tokio::fs;
use tracing::info;

#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub sub: AgentSub,
}

#[derive(Subcommand, Debug)]
pub enum AgentSub {
    /// Upload a new agent binary to the agent_releases Object Store and
    /// flip agent_config.target_version so every agent picks it up on
    /// its next watch tick (spec §2.10.5).
    Publish {
        /// Path to the new agent binary (e.g. `target/release/kanade-agent.exe`).
        binary: PathBuf,
        /// Semver-ish version string. Stored as the object name and
        /// broadcast in agent_config.target_version.
        #[arg(long)]
        version: String,
    },
    /// Print the currently broadcast target_version.
    Current,
    /// Manage a PC's group memberships via the agent_groups KV bucket.
    Groups(GroupsArgs),
}

#[derive(Args, Debug)]
pub struct GroupsArgs {
    #[command(subcommand)]
    pub sub: GroupsSub,
}

#[derive(Subcommand, Debug)]
pub enum GroupsSub {
    /// Print the groups this PC currently belongs to.
    List { pc_id: String },
    /// Add one group to this PC's membership (idempotent).
    Add { pc_id: String, group: String },
    /// Remove one group from this PC's membership (idempotent).
    Rm { pc_id: String, group: String },
    /// Replace this PC's whole membership list (sorted + deduped on
    /// the server side). Pass zero groups to clear.
    Set {
        pc_id: String,
        #[arg(trailing_var_arg = true)]
        groups: Vec<String>,
    },
}

pub async fn execute(client: async_nats::Client, args: AgentArgs) -> Result<()> {
    match args.sub {
        AgentSub::Publish { binary, version } => publish(client, binary, version).await,
        AgentSub::Current => current(client).await,
        AgentSub::Groups(g) => groups(client, g).await,
    }
}

async fn publish(client: async_nats::Client, binary: PathBuf, version: String) -> Result<()> {
    let bytes = fs::read(&binary)
        .await
        .with_context(|| format!("read {binary:?}"))?;
    info!(version, size = bytes.len(), "uploading new agent binary");

    let js = async_nats::jetstream::new(client);
    let store = js
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_AGENT_RELEASES}' missing — run `kanade jetstream setup`")
        })?;
    // Slice → Cursor for the put() API.
    let mut cursor = std::io::Cursor::new(bytes);
    let meta = store
        .put(version.as_str(), &mut cursor)
        .await
        .context("object_store.put")?;
    info!(version, digest = ?meta.digest, "agent binary uploaded");

    let kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| {
            format!("KV '{BUCKET_AGENT_CONFIG}' missing — run `kanade jetstream setup`")
        })?;
    // Sprint 6: target_version is now a field on the layered
    // `global` ConfigScope, not a standalone key. Read-modify-write
    // so other fields (inventory cadence, heartbeat interval, …)
    // operators have already set survive the publish.
    let mut global = match kv.get(KEY_AGENT_CONFIG_GLOBAL).await? {
        Some(b) => serde_json::from_slice::<ConfigScope>(&b)
            .with_context(|| format!("decode existing {BUCKET_AGENT_CONFIG}.global"))?,
        None => ConfigScope::default(),
    };
    global.target_version = Some(version.clone());
    let payload = serde_json::to_vec(&global).context("encode global ConfigScope")?;
    kv.put(KEY_AGENT_CONFIG_GLOBAL, payload.into())
        .await
        .context("KV put global ConfigScope")?;
    info!(version, "broadcast agent_config.global.target_version");

    println!("published: {version}");
    println!("  object_store : {OBJECT_AGENT_RELEASES}/{version}");
    println!(
        "  kv           : {BUCKET_AGENT_CONFIG}.{KEY_AGENT_CONFIG_GLOBAL}.target_version = {version}"
    );
    Ok(())
}

async fn current(client: async_nats::Client) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| format!("KV '{BUCKET_AGENT_CONFIG}' missing"))?;
    match kv.get(KEY_AGENT_CONFIG_GLOBAL).await? {
        Some(b) => {
            let scope: ConfigScope = serde_json::from_slice(&b)
                .with_context(|| format!("decode {BUCKET_AGENT_CONFIG}.global"))?;
            match scope.target_version {
                Some(v) => println!("global.target_version = {v}"),
                None => println!("global.target_version = (unset)"),
            }
        }
        None => println!("global = (unset)"),
    }
    Ok(())
}

async fn groups(client: async_nats::Client, args: GroupsArgs) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let kv = js
        .get_key_value(BUCKET_AGENT_GROUPS)
        .await
        .with_context(|| {
            format!("KV '{BUCKET_AGENT_GROUPS}' missing — run `kanade jetstream setup`")
        })?;
    match args.sub {
        GroupsSub::List { pc_id } => {
            let g = read_groups(&kv, &pc_id).await?;
            if g.is_empty() {
                println!("{pc_id}: (no groups)");
            } else {
                println!("{pc_id}: {}", g.groups.join(", "));
            }
        }
        GroupsSub::Add { pc_id, group } => {
            let mut g = read_groups(&kv, &pc_id).await?;
            if g.insert(&group) {
                write_groups(&kv, &pc_id, &g).await?;
                println!("{pc_id}: added '{group}' -> [{}]", g.groups.join(", "));
            } else {
                println!("{pc_id}: already has '{group}' (no change)");
            }
        }
        GroupsSub::Rm { pc_id, group } => {
            let mut g = read_groups(&kv, &pc_id).await?;
            if g.remove(&group) {
                write_groups(&kv, &pc_id, &g).await?;
                let after = if g.is_empty() {
                    "(no groups)".to_string()
                } else {
                    g.groups.join(", ")
                };
                println!("{pc_id}: removed '{group}' -> [{after}]");
            } else {
                println!("{pc_id}: not a member of '{group}' (no change)");
            }
        }
        GroupsSub::Set { pc_id, groups } => {
            let normalised = AgentGroups::new(groups);
            write_groups(&kv, &pc_id, &normalised).await?;
            if normalised.is_empty() {
                println!("{pc_id}: cleared all groups");
            } else {
                println!(
                    "{pc_id}: set membership to [{}]",
                    normalised.groups.join(", ")
                );
            }
        }
    }
    Ok(())
}

async fn read_groups(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<AgentGroups> {
    match kv.get(pc_id).await.context("kv get")? {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode agent_groups"),
        None => Ok(AgentGroups::default()),
    }
}

async fn write_groups(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    groups: &AgentGroups,
) -> Result<()> {
    let bytes = serde_json::to_vec(groups).context("encode agent_groups")?;
    kv.put(pc_id, bytes.into()).await.context("kv put")?;
    info!(pc_id, groups = ?groups.groups, "agent_groups updated");
    Ok(())
}
