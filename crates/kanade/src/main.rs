mod audit;
mod cli_config;
mod cmd;
mod http_client;
mod updater;

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
    /// Publish kill.{exec_id} so agents running the exec terminate (spec §2.6 Layer 3).
    Kill(cmd::kill::KillArgs),
    /// Fire a registered job (`kanade job create` it first) at its declared targets.
    Exec(cmd::exec::ExecArgs),
    /// CRUD the job catalog (jobs KV). Schedules reference jobs by id.
    Job(cmd::job::JobArgs),
    /// CRUD cron schedules (spec §2.5.3).
    Schedule(cmd::schedule::ScheduleArgs),
    /// CRUD Analytics views (#743): declarative cross-cutting dashboards.
    View(cmd::view::ViewArgs),
    /// Fleet-wide change-freeze: stop all schedule fires (#418 Phase 5).
    Freeze(cmd::freeze::FreezeArgs),
    /// Manage agent releases (publish a new binary, query the target version).
    Agent(cmd::agent::AgentArgs),
    /// CRUD the generic app-package Object Store (`OBJECT_APP_PACKAGES`, #207).
    /// Sibling of `agent` — different bucket, same NATS-direct shape.
    App(cmd::app::AppArgs),
    /// CRUD the manifest-script Object Store (`OBJECT_SCRIPTS`, #211).
    /// Bodies referenced by `execute.script_object` (#213 / #214).
    Script(cmd::script::ScriptArgs),
    /// Manage the layered agent_config KV bucket (global / per-group / per-pc).
    Config(cmd::config::ConfigArgs),
    /// Manage groups: list fleet-wide, add/remove PC memberships,
    /// list PCs in a given group.
    Group(cmd::group::GroupArgs),
    /// Manage per-PC operator metadata (free-form key/value attributes on
    /// the agent_meta KV bucket). Sibling of `group`; NATS-direct.
    Meta(cmd::meta::MetaArgs),
    /// Log in with username/password; prints a JWT for KANADE_AUTH_TOKEN.
    Login(cmd::login::LoginArgs),
    /// Admin-only RBAC account management (create / role / disable / …).
    Account(cmd::account::AccountArgs),
    /// Run an ad-hoc read-only SQL query against the projector DB
    /// (admin-only, SELECT/WITH only). Prints a table or `--json`.
    Query(cmd::query::QueryArgs),
    /// Update the kanade CLI itself from GitHub Releases (kaishin).
    /// Background behaviour on ordinary runs is configured in the
    /// per-user config (`[update] mode = off|notify|install`, default
    /// notify); `KANADE_NO_AUTOUPDATE` disables it entirely.
    SelfUpdate(cmd::self_update::SelfUpdateArgs),
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

    // Background update check (notify by default; see cli_config).
    // Skipped for `self-update` itself — it IS the update path.
    let update_handle = updater::maybe_spawn(matches!(command, SubCmd::SelfUpdate(_)));
    let result = dispatch(server, backend_url, command).await;
    updater::finalize(update_handle).await;
    result
}

async fn dispatch(server: String, backend_url: String, command: SubCmd) -> Result<()> {
    // #1032: `kanade group def …` is HTTP manifest CRUD (backend), unlike the
    // rest of `kanade group` which writes `agent_groups` KV over NATS. Route
    // it here before the NATS connect below. A non-`Def` group subcommand
    // doesn't match this pattern and falls through to the NATS path.
    if let SubCmd::Group(cmd::group::GroupArgs {
        sub: cmd::group::GroupSub::Def(def),
    }) = command
    {
        return cmd::group::execute_def(&backend_url, def).await;
    }

    // HTTP-only subcommands (no NATS connect required).
    if let SubCmd::Exec(args) = command {
        return cmd::exec::execute(&backend_url, args).await;
    } else if let SubCmd::Job(args) = command {
        return cmd::job::execute(&backend_url, args).await;
    } else if let SubCmd::Schedule(args) = command {
        return cmd::schedule::execute(&backend_url, args).await;
    } else if let SubCmd::View(args) = command {
        return cmd::view::execute(&backend_url, args).await;
    } else if let SubCmd::Freeze(args) = command {
        return cmd::freeze::execute(&backend_url, args).await;
    } else if let SubCmd::Login(args) = command {
        return cmd::login::execute(&backend_url, args).await;
    } else if let SubCmd::Account(args) = command {
        return cmd::account::execute(&backend_url, args).await;
    } else if let SubCmd::Query(args) = command {
        return cmd::query::execute(&backend_url, args).await;
    } else if let SubCmd::SelfUpdate(args) = command {
        return cmd::self_update::execute(args).await;
    }

    // The remaining subcommands need NATS. The role decides which
    // credential the helper looks for (#1155):
    // `HKLM\SOFTWARE\kanade\cli\NatsToken` when provisioned, otherwise the
    // fleet-wide token every role shared before roles existed, otherwise
    // $KANADE_NATS_TOKEN — which is the branch an operator shell normally
    // takes, since the CLI does not run as LocalSystem.
    let client =
        kanade_shared::nats_client::connect(kanade_shared::nats_client::NatsRole::Cli, &server)
            .await?;
    debug!("connected to NATS");

    match command {
        SubCmd::Run(args) => cmd::run::execute(client, args).await,
        SubCmd::Ping(args) => cmd::ping::execute(client, args).await,
        SubCmd::Jetstream(args) => cmd::jetstream::execute(client, args).await,
        SubCmd::Revoke(args) => cmd::revoke::revoke(client, args).await,
        SubCmd::Unrevoke(args) => cmd::revoke::unrevoke(client, args).await,
        SubCmd::Kill(args) => cmd::kill::execute(client, args).await,
        SubCmd::Agent(args) => cmd::agent::execute(client, args).await,
        SubCmd::App(args) => cmd::app::execute(client, args).await,
        SubCmd::Script(args) => cmd::script::execute(client, args).await,
        SubCmd::Config(args) => cmd::config::execute(client, args).await,
        SubCmd::Group(args) => cmd::group::execute(client, args).await,
        SubCmd::Meta(args) => cmd::meta::execute(client, args).await,
        SubCmd::Exec(_)
        | SubCmd::Job(_)
        | SubCmd::Schedule(_)
        | SubCmd::View(_)
        | SubCmd::Freeze(_)
        | SubCmd::Login(_)
        | SubCmd::Account(_)
        | SubCmd::Query(_)
        | SubCmd::SelfUpdate(_) => {
            unreachable!("handled above")
        }
    }
}
