mod cmd;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::debug;

const DEFAULT_NATS: &str = "nats://127.0.0.1:4222";

#[derive(Parser, Debug)]
#[command(
    name = "kanade",
    about = "Admin CLI for the kanade endpoint management system",
    version
)]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_NATS, env = "KANADE_NATS_URL")]
    server: String,

    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand, Debug)]
enum SubCmd {
    /// Run a script on a target PC and wait for the result.
    Run(cmd::run::RunArgs),
    /// Wait for one heartbeat from the target PC.
    Ping(cmd::ping::PingArgs),
    /// Manage JetStream streams + KV buckets.
    Jetstream(cmd::jetstream::JetstreamArgs),
    /// Mark a command id as REVOKED so agents skip it (spec §2.6 Layer 2).
    Revoke(cmd::revoke::RevokeArgs),
    /// Re-mark a previously revoked command id as ACTIVE.
    Unrevoke(cmd::revoke::UnrevokeArgs),
    /// Publish kill.{job_id} so agents running the job terminate (spec §2.6 Layer 3).
    Kill(cmd::kill::KillArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kanade=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let client = async_nats::connect(&cli.server)
        .await
        .with_context(|| format!("connect to NATS at {}", cli.server))?;
    debug!("connected to NATS");

    match cli.command {
        SubCmd::Run(args) => cmd::run::execute(client, args).await,
        SubCmd::Ping(args) => cmd::ping::execute(client, args).await,
        SubCmd::Jetstream(args) => cmd::jetstream::execute(client, args).await,
        SubCmd::Revoke(args) => cmd::revoke::revoke(client, args).await,
        SubCmd::Unrevoke(args) => cmd::revoke::unrevoke(client, args).await,
        SubCmd::Kill(args) => cmd::kill::execute(client, args).await,
    }
}
