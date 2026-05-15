use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, KEY_AGENT_TARGET_VERSION, OBJECT_AGENT_RELEASES};
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
}

pub async fn execute(client: async_nats::Client, args: AgentArgs) -> Result<()> {
    match args.sub {
        AgentSub::Publish { binary, version } => publish(client, binary, version).await,
        AgentSub::Current => current(client).await,
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
    kv.put(
        KEY_AGENT_TARGET_VERSION,
        bytes::Bytes::from(version.clone().into_bytes()),
    )
    .await
    .context("KV put target_version")?;
    info!(version, "broadcast agent_config.target_version");

    println!("published: {version}");
    println!("  object_store : {OBJECT_AGENT_RELEASES}/{version}");
    println!("  kv           : {BUCKET_AGENT_CONFIG}.{KEY_AGENT_TARGET_VERSION} = {version}");
    Ok(())
}

async fn current(client: async_nats::Client) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| format!("KV '{BUCKET_AGENT_CONFIG}' missing"))?;
    match kv.get(KEY_AGENT_TARGET_VERSION).await? {
        Some(b) => {
            let v = String::from_utf8_lossy(&b);
            println!("target_version = {v}");
        }
        None => println!("target_version = (unset)"),
    }
    Ok(())
}
