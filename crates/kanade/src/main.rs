mod cmd;
mod http_client;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::debug;

const DEFAULT_NATS: &str = "nats://127.0.0.1:4222";
const DEFAULT_BACKEND: &str = "http://127.0.0.1:8080";

#[derive(Parser, Debug)]
#[command(
    name = "kanade",
    about = "Admin CLI for the kanade endpoint management system",
    version
)]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_NATS, env = "KANADE_NATS_URL")]
    server: String,

    #[arg(long, global = true, default_value = DEFAULT_BACKEND, env = "KANADE_BACKEND_URL")]
    backend_url: String,

    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand, Debug)]
enum SubCmd {
    /// Run a script on a target PC directly via NATS and wait for the result.
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
    /// Submit a YAML job manifest to the backend's POST /api/exec.
    Exec(cmd::exec::ExecArgs),
    /// CRUD the job catalog (jobs KV). Schedules reference jobs by id.
    Job(cmd::job::JobArgs),
    /// CRUD cron schedules (spec §2.5.3).
    Schedule(cmd::schedule::ScheduleArgs),
    /// Manage agent releases (publish a new binary, query the target version).
    Agent(cmd::agent::AgentArgs),
    /// Manage the layered agent_config KV bucket (global / per-group / per-pc).
    Config(cmd::config::ConfigArgs),
    /// Manage groups: list fleet-wide, add/remove PC memberships,
    /// list PCs in a given group.
    Group(cmd::group::GroupArgs),
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
    let Cli {
        server,
        backend_url,
        command,
    } = cli;

    // HTTP-only subcommands (no NATS connect required).
    if let SubCmd::Exec(args) = command {
        return cmd::exec::execute(&backend_url, args).await;
    } else if let SubCmd::Job(args) = command {
        return cmd::job::execute(&backend_url, args).await;
    } else if let SubCmd::Schedule(args) = command {
        return cmd::schedule::execute(&backend_url, args).await;
    }

    // The remaining subcommands need NATS. Shared helper picks up
    // $KANADE_NATS_TOKEN when set and attaches it as the bearer
    // token.
    let client = kanade_shared::nats_client::connect(&server).await?;
    debug!("connected to NATS");

    match command {
        SubCmd::Run(args) => cmd::run::execute(client, args).await,
        SubCmd::Ping(args) => cmd::ping::execute(client, args).await,
        SubCmd::Jetstream(args) => cmd::jetstream::execute(client, args).await,
        SubCmd::Revoke(args) => cmd::revoke::revoke(client, args).await,
        SubCmd::Unrevoke(args) => cmd::revoke::unrevoke(client, args).await,
        SubCmd::Kill(args) => cmd::kill::execute(client, args).await,
        SubCmd::Agent(args) => cmd::agent::execute(client, args).await,
        SubCmd::Config(args) => cmd::config::execute(client, args).await,
        SubCmd::Group(args) => cmd::group::execute(client, args).await,
        SubCmd::Exec(_) | SubCmd::Job(_) | SubCmd::Schedule(_) => {
            unreachable!("handled above")
        }
    }
}
