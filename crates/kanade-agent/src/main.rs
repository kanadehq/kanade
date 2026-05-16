mod commands;
mod config_supervisor;
mod groups;
mod heartbeat;
mod inventory;
mod process;
mod self_update;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use kanade_shared::config::load_agent_config;
use kanade_shared::{default_paths, subject};
use tracing::info;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "kanade-agent",
    about = "Windows endpoint management agent (kanade)",
    version
)]
struct Cli {
    /// Path to agent.toml. When unset, the agent looks at
    /// $KANADE_AGENT_CONFIG, then `<config_dir>/agent.toml` (see
    /// kanade_shared::default_paths::config_dir).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kanade_agent=debug".into()),
        )
        .init();

    cleanup_stale_upgrade_artifacts();

    let cli = Cli::parse();
    let cfg_path =
        default_paths::find_config(cli.config.as_deref(), "KANADE_AGENT_CONFIG", "agent.toml")?;
    let cfg =
        load_agent_config(&cfg_path).with_context(|| format!("load config from {cfg_path:?}"))?;
    info!(
        pc_id = %cfg.agent.id,
        nats_url = %cfg.agent.nats_url,
        version = AGENT_VERSION,
        "starting kanade-agent",
    );

    let client = async_nats::connect(&cfg.agent.nats_url)
        .await
        .with_context(|| format!("connect to NATS at {}", cfg.agent.nats_url))?;
    info!("connected to NATS");

    let cmd_all = client.subscribe(subject::COMMANDS_ALL).await?;
    let cmd_self = client
        .subscribe(subject::commands_pc(&cfg.agent.id))
        .await?;
    info!(
        commands_all = subject::COMMANDS_ALL,
        commands_self = %subject::commands_pc(&cfg.agent.id),
        "subscribed",
    );

    let pc_id = cfg.agent.id.clone();

    // Sprint 6: every fleet-wide knob (heartbeat cadence, inventory
    // cadence / jitter / enabled, target_version) is now sourced
    // from the agent_config KV bucket and watched live. The
    // supervisor publishes the resolved EffectiveConfig on a watch
    // channel; heartbeat / inventory / self_update subscribe.
    let cfg_rx = config_supervisor::spawn(client.clone(), pc_id.clone());

    // Sprint 6: cfg.inventory is parsed for back-compat but the
    // runtime sources cadence / jitter / enabled from the
    // agent_config KV bucket via cfg_rx. Warn the operator when
    // the local toml carries non-default values so they know to
    // migrate via `kanade config set inventory_interval=... ` etc.
    let inv_defaults = kanade_shared::config::InventorySection::default();
    if cfg.inventory.hw_interval != inv_defaults.hw_interval
        || cfg.inventory.jitter != inv_defaults.jitter
        || cfg.inventory.enabled != inv_defaults.enabled
    {
        tracing::warn!(
            local_inventory = ?cfg.inventory,
            "agent.toml::[inventory] is deprecated — values now come from the agent_config KV bucket; this section is logged-and-ignored. Use `kanade config set inventory_interval=...` (and friends) to migrate. The field will be removed in v0.4.0.",
        );
    }

    tokio::spawn(heartbeat::heartbeat_loop(
        client.clone(),
        pc_id.clone(),
        AGENT_VERSION.to_string(),
        cfg_rx.clone(),
    ));
    tokio::spawn(inventory::inventory_loop(
        client.clone(),
        pc_id.clone(),
        cfg_rx.clone(),
    ));
    tokio::spawn(self_update::run(
        client.clone(),
        AGENT_VERSION.to_string(),
        cfg_rx.clone(),
    ));

    // Group membership: Sprint 5 moves this from agent.toml (per-box
    // local config) to a server-managed KV bucket. The manager reads
    // `agent_groups.{pc_id}` from JetStream KV, spawns one
    // `commands.group.<name>` subscriber per current group, and reacts
    // to KV updates by adding / dropping subscriptions live.
    if !cfg.agent.groups.is_empty() {
        tracing::warn!(
            local_groups = ?cfg.agent.groups,
            "agent.toml::[agent] groups is deprecated; use `kanade agent groups set` instead — local value is ignored",
        );
    }
    tokio::spawn(groups::manage(client.clone(), pc_id.clone()));

    let _ = tokio::join!(
        commands::command_loop(client.clone(), pc_id.clone(), cmd_all),
        commands::command_loop(client.clone(), pc_id.clone(), cmd_self),
    );

    Ok(())
}

/// Remove `<exe>.old` / `<exe>.new` left over from the previous
/// self-update cycle. `.old` is the previous-version exe, no longer
/// loaded; `.new` would only exist if a swap was interrupted before
/// the final rename (the in-place exe is still valid in that case).
/// Either way, removal here keeps the install dir tidy and stops
/// stale binaries from accumulating across upgrade cycles.
fn cleanup_stale_upgrade_artifacts() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = current.parent() else {
        return;
    };
    let Some(exe_name) = current.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for suffix in ["old", "new"] {
        let path = exe_dir.join(format!("{exe_name}.{suffix}"));
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(_) => tracing::info!(?path, suffix, "removed stale upgrade artifact"),
            Err(e) => {
                tracing::warn!(?path, suffix, error = %e, "couldn't remove stale upgrade artifact")
            }
        }
    }
}
