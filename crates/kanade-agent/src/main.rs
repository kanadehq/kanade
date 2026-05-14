mod commands;
mod heartbeat;
mod process;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use kanade_shared::config::load_agent_config;
use kanade_shared::subject;
use tracing::info;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "kanade-agent",
    about = "Windows endpoint management agent (kanade)",
    version
)]
struct Cli {
    #[arg(long, default_value = "agent.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kanade_agent=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = load_agent_config(&cli.config)
        .with_context(|| format!("load config from {:?}", cli.config))?;
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
    tokio::spawn(heartbeat::heartbeat_loop(
        client.clone(),
        pc_id.clone(),
        AGENT_VERSION.to_string(),
    ));

    let _ = tokio::join!(
        commands::command_loop(client.clone(), pc_id.clone(), cmd_all),
        commands::command_loop(client.clone(), pc_id.clone(), cmd_self),
    );

    Ok(())
}
