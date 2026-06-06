use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, KEY_AGENT_CONFIG_GLOBAL, OBJECT_AGENT_RELEASES, agent_config_group_key,
    agent_config_pc_key,
};
use kanade_shared::subject;
use kanade_shared::wire::{ConfigScope, LogsRequest};
use tokio::fs;
use tracing::info;

#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub sub: AgentSub,
}

#[derive(Subcommand, Debug)]
pub enum AgentSub {
    /// Upload a new agent binary to the agent_releases Object Store.
    /// No KV is touched — agents only start downloading once a
    /// follow-up `kanade agent rollout` flips `target_version` on
    /// some scope (global / group / pc). Two-step on purpose, so a
    /// typo doesn't fan a half-baked binary out to the whole fleet.
    ///
    /// v0.13.1+: the Object Store key is auto-extracted from the
    /// binary's embedded VERSIONINFO resource — no `--version`
    /// flag, no chance of a label/binary mismatch. Cross-arch
    /// publish works too (the extractor is pure-Rust `pelite`,
    /// no spawn).
    Publish {
        /// Path to the new agent binary (e.g. `target/release/kanade-agent.exe`).
        binary: PathBuf,
    },
    /// Flip `target_version` (and optionally `target_version_jitter`)
    /// on one scope of the layered agent_config bucket. Verifies the
    /// binary exists in the Object Store first — fail-fast on typos.
    ///
    /// Pick exactly one scope:
    ///   --global             roll out fleet-wide
    ///   --group <name>       canary / wave / dept overlay
    ///   --pc    <pc_id>      single-host pin
    Rollout(RolloutArgs),
    /// Print the currently broadcast target_version.
    Current,
    /// Tail the agent's log file (`logs.fetch.<pc_id>` request /
    /// reply). The agent reads its local rolling log file and
    /// returns the last N lines as UTF-8.
    Logs {
        /// PC id of the agent to query (must be online).
        pc_id: String,
        /// Trailing line count. Defaults to 500.
        #[arg(long, default_value_t = 500)]
        tail: u32,
    },
}

#[derive(Args, Debug)]
pub struct RolloutArgs {
    /// Version label to point the chosen scope at. Must match an
    /// object already in the agent_releases Object Store (i.e. a
    /// previous `kanade agent publish` round).
    pub version: String,

    /// Roll out to the global scope (`agent_config.global`). Mutually
    /// exclusive with `--group` / `--pc`.
    #[arg(long, conflicts_with_all = ["group", "pc"])]
    pub global: bool,

    /// Roll out to a single group (`agent_config.groups.<name>`).
    #[arg(long, value_name = "NAME")]
    pub group: Option<String>,

    /// Roll out to a single PC (`agent_config.pcs.<pc_id>`).
    #[arg(long, value_name = "PC_ID")]
    pub pc: Option<String>,

    /// Optional override for `target_version_jitter` on the same
    /// scope (humantime, e.g. `30m`). Recommended ≥ a few minutes
    /// for fleet-wide rollouts so 3000 agents don't synchronise
    /// their downloads. Omit to leave the existing value alone.
    #[arg(long, value_name = "DURATION")]
    pub jitter: Option<String>,
}

pub async fn execute(client: async_nats::Client, args: AgentArgs) -> Result<()> {
    match args.sub {
        AgentSub::Publish { binary } => publish(client, binary).await,
        AgentSub::Rollout(args) => rollout(client, args).await,
        AgentSub::Current => current(client).await,
        AgentSub::Logs { pc_id, tail } => logs(client, pc_id, tail).await,
    }
}

async fn publish(client: async_nats::Client, binary: PathBuf) -> Result<()> {
    let bytes = fs::read(&binary)
        .await
        .with_context(|| format!("read {binary:?}"))?;

    // v0.13.1+: extract version from the binary's embedded
    // VERSIONINFO resource (pelite, no spawn, cross-arch safe). The
    // operator never types a label — the binary IS its label.
    let version = kanade_shared::exe_version::extract_pe_version(&bytes).with_context(|| {
        format!(
            "couldn't extract VERSIONINFO from {binary:?} — is it a Windows PE built \
             with `winres` (kanade ≥ v0.13.1)? Older binaries need to be re-published \
             from a current build."
        )
    })?;

    info!(version, size = bytes.len(), "uploading new agent binary");

    let js = async_nats::jetstream::new(client.clone());
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

    // #277: same JetStream read-after-write window as `app publish`.
    // Block until a `get(key)` returns the same bytes we just put,
    // so downstream consumers (`kanade agent rollout` triggers an
    // agent self-update path that fetches from this very key) don't
    // race against the upstream race.
    super::publish_verify::verify_readback(
        &store,
        version.as_str(),
        meta.digest.as_deref(),
        meta.size,
    )
    .await
    .context("publish read-back verify")?;

    println!("published: {version}");
    println!("  object_store : {OBJECT_AGENT_RELEASES}/{version}");
    println!();
    println!("Next: target a scope with `kanade agent rollout`:");
    println!("  kanade agent rollout {version} --group canary --jitter 5m   # try on canary first");
    println!("  kanade agent rollout {version} --global --jitter 30m        # fleet-wide");

    crate::audit::record(
        &client,
        "agent_publish",
        Some(version.as_str()),
        serde_json::json!({ "size": meta.size, "digest": meta.digest }),
    )
    .await;
    Ok(())
}

async fn rollout(client: async_nats::Client, args: RolloutArgs) -> Result<()> {
    let (key, label) = match (args.global, args.group.as_deref(), args.pc.as_deref()) {
        (true, None, None) => (KEY_AGENT_CONFIG_GLOBAL.to_string(), "global".to_string()),
        (false, Some(g), None) => (agent_config_group_key(g), format!("group:{g}")),
        (false, None, Some(p)) => (agent_config_pc_key(p), format!("pc:{p}")),
        (false, None, None) => bail!(
            "must pick a scope: --global / --group <name> / --pc <pc_id>. \
             Refusing to rollout — explicit scope keeps a forgotten flag from \
             fanning a release out to every agent."
        ),
        _ => bail!("--global / --group / --pc are mutually exclusive"),
    };

    let js = async_nats::jetstream::new(client.clone());

    // Fail-fast on a version that doesn't have a binary uploaded
    // yet — saves the operator from finding out at agent-side via a
    // "self-update fetch failed" log line per host.
    let store = js
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .with_context(|| {
            format!("object store '{OBJECT_AGENT_RELEASES}' missing — run `kanade jetstream setup`")
        })?;
    store.info(&args.version).await.with_context(|| {
        format!(
            "version '{}' not found in {OBJECT_AGENT_RELEASES} — \
             run `kanade agent publish <binary>` first (the version is \
             auto-extracted from the binary's VERSIONINFO)",
            args.version
        )
    })?;

    let kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| {
            format!("KV '{BUCKET_AGENT_CONFIG}' missing — run `kanade jetstream setup`")
        })?;

    let mut scope = match kv.get(&key).await? {
        Some(b) => serde_json::from_slice::<ConfigScope>(&b)
            .with_context(|| format!("decode existing {BUCKET_AGENT_CONFIG}.{key}"))?,
        None => ConfigScope::default(),
    };
    scope.target_version = Some(args.version.clone());
    if let Some(j) = args.jitter.as_deref() {
        scope.target_version_jitter = Some(j.to_owned());
    }
    let payload = serde_json::to_vec(&scope).context("encode ConfigScope")?;
    kv.put(key.as_str(), payload.into())
        .await
        .context("KV put scope ConfigScope")?;

    info!(
        scope = %label,
        version = %args.version,
        jitter = ?args.jitter,
        "rollout: target_version flipped",
    );

    println!("rolled out: {} -> {}", label, args.version);
    println!(
        "  kv           : {BUCKET_AGENT_CONFIG}.{key}.target_version = {}",
        args.version
    );
    if let Some(j) = args.jitter.as_deref() {
        println!("  kv           : {BUCKET_AGENT_CONFIG}.{key}.target_version_jitter = {j}");
    } else {
        println!(
            "  jitter       : (unchanged — explicit `--jitter <duration>` recommended for fleets ≥ 100)"
        );
    }

    crate::audit::record(
        &client,
        "agent_rollout",
        Some(&key),
        serde_json::json!({
            "version": args.version,
            "scope_label": label,
            "jitter": args.jitter,
        }),
    )
    .await;
    Ok(())
}

async fn logs(client: async_nats::Client, pc_id: String, tail: u32) -> Result<()> {
    let req = LogsRequest { tail_lines: tail };
    let payload = serde_json::to_vec(&req).context("encode LogsRequest")?;
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.request(subject::logs_fetch(&pc_id), payload.into()),
    )
    .await
    .with_context(|| format!("timeout waiting for {pc_id} (10s)"))?
    .with_context(|| format!("request logs.fetch.{pc_id}"))?;

    // Reply is raw UTF-8 log bytes — pass straight through to stdout.
    use std::io::Write;
    std::io::stdout().write_all(&reply.payload).ok();
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
