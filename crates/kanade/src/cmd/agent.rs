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
    Publish {
        /// Path to the new agent binary (e.g. `target/release/kanade-agent.exe`).
        binary: PathBuf,
        /// Semver-ish version string. Stored as the object name in
        /// the Object Store. When omitted, the CLI runs `<binary>
        /// --version` and parses the second whitespace-separated
        /// token; pass explicitly to publish under a label
        /// different from what the binary reports (e.g. release
        /// candidates, rollback aliases) or when cross-arch
        /// publishing prevents executing the binary locally.
        #[arg(long)]
        version: Option<String>,
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
        AgentSub::Publish { binary, version } => publish(client, binary, version).await,
        AgentSub::Rollout(args) => rollout(client, args).await,
        AgentSub::Current => current(client).await,
        AgentSub::Logs { pc_id, tail } => logs(client, pc_id, tail).await,
    }
}

async fn publish(
    client: async_nats::Client,
    binary: PathBuf,
    version: Option<String>,
) -> Result<()> {
    let version = match version {
        Some(v) => v,
        None => probe_binary_version(&binary).with_context(|| {
            format!(
                "auto-detect version from {binary:?} via `--version` — \
                 pass --version explicitly if the binary can't be executed here \
                 (e.g. cross-arch publish from Linux/macOS)"
            )
        })?,
    };

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

    println!("published: {version}");
    println!("  object_store : {OBJECT_AGENT_RELEASES}/{version}");
    println!();
    println!("Next: target a scope with `kanade agent rollout`:");
    println!("  kanade agent rollout {version} --group canary --jitter 5m   # try on canary first");
    println!("  kanade agent rollout {version} --global --jitter 30m        # fleet-wide");
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

    let js = async_nats::jetstream::new(client);

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
            "version '{}' not found in {OBJECT_AGENT_RELEASES} — run \
                 `kanade agent publish <binary> --version {}` first",
            args.version, args.version
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
    Ok(())
}

/// Run `<binary> --version` and pull the version token out of its
/// stdout. kanade-agent's clap setup emits `kanade-agent <version>`
/// on `--version`, so we take the second whitespace-separated word.
/// Fails when the binary doesn't run on this host (cross-arch),
/// exits non-zero, or prints an unexpected shape.
fn probe_binary_version(binary: &std::path::Path) -> Result<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("run `{} --version`", binary.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{} --version` exited {} (stderr: {})",
            binary.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let line = String::from_utf8(out.stdout)
        .context("`--version` stdout is not UTF-8")?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let token = line
        .split_whitespace()
        .nth(1)
        .with_context(|| format!("unexpected --version output shape: {line:?}"))?;
    Ok(token.to_owned())
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
