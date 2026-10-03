mod audit;
mod cli_config;
mod cmd;
mod http_client;
#[cfg(test)]
mod test_http;
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
    /// NATS broker URL (not the backend).
    ///
    /// Used by the broker subcommands: `run`, `kill`,
    /// `group` (except `group def`, which is HTTP — see --backend-url),
    /// `script`, `app`, `jetstream` (except `jetstream status`,
    /// which is HTTP). Its credential is NOT a flag — the CLI reads
    /// `HKLM\SOFTWARE\kanade\cli\NatsToken` (Windows; no installer
    /// writes this today — a manual reg add), then
    /// `HKLM\SOFTWARE\kanade\agent\NatsToken`, then
    /// `$KANADE_NATS_TOKEN`, and connects unauthenticated if it finds
    /// none of them.
    #[arg(long, global = true, default_value = DEFAULT_NATS, env = "KANADE_NATS_URL")]
    server: String,

    /// Backend HTTP base URL.
    ///
    /// Used by the HTTP subcommands: `job`, `schedule`, `exec`, `view`,
    /// `query`, `freeze`, `account`, `agent`, `group def`, `config`, `meta`, `ping`,
    /// `revoke`, `unrevoke`, `jetstream status`. They authenticate
    /// WITH `$KANADE_AUTH_TOKEN`, a JWT — a different credential from
    /// the broker token above, which is the usual source of confusion
    /// when one set of subcommands works and the other does not.
    /// `kanade login` also talks to the backend, but PRODUCES that JWT
    /// rather than requiring it.
    #[arg(long, global = true, default_value = DEFAULT_BACKEND, env = "KANADE_BACKEND_URL")]
    backend_url: String,

    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand, Debug)]
enum SubCmd {
    /// Run a script on a target PC directly via NATS and wait for the result.
    Run(cmd::run::RunArgs),
    /// Ask the target PC's agent for a fresh heartbeat (via the backend API).
    Ping(cmd::ping::PingArgs),
    /// Manage JetStream streams + KV buckets (`status` goes through the
    /// backend API; setup / delete / reset are NATS-direct).
    Jetstream(cmd::jetstream::JetstreamArgs),
    /// Mark a command id as REVOKED so agents skip it (spec §2.6 Layer 2).
    /// Goes through the backend API (needs KANADE_AUTH_TOKEN), not NATS.
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
    /// Manage agent releases (publish a new binary, roll it out, query the
    /// target version, tail an agent's log). Goes through the backend API
    /// (needs KANADE_AUTH_TOKEN), not NATS.
    Agent(cmd::agent::AgentArgs),
    /// CRUD the generic app-package Object Store (`OBJECT_APP_PACKAGES`, #207).
    /// Goes through the backend API (needs KANADE_AUTH_TOKEN), not NATS.
    App(cmd::app::AppArgs),
    /// CRUD the manifest-script Object Store (`OBJECT_SCRIPTS`, #211).
    /// Bodies referenced by `execute.script_object` (#213 / #214). Goes
    /// through the backend API (needs KANADE_AUTH_TOKEN), not NATS.
    Script(cmd::script::ScriptArgs),
    /// Manage the layered agent config (global / per-group / per-pc). Goes
    /// through the backend API (needs KANADE_AUTH_TOKEN), not NATS.
    Config(cmd::config::ConfigArgs),
    /// Break-glass command-signing key (#1165). The backend's own key is minted
    /// on the backend host (`kanade-backend command-key-generate`), never here.
    CommandKey(cmd::command_key::CommandKeyArgs),
    /// Manage groups: list fleet-wide, add/remove PC memberships,
    /// list PCs in a given group. Goes through the backend API (needs
    /// KANADE_AUTH_TOKEN), not NATS.
    Group(cmd::group::GroupArgs),
    /// Manage per-PC operator metadata (free-form key/value attributes on
    /// the agent_meta KV bucket). Goes through the backend API (needs
    /// KANADE_AUTH_TOKEN), not NATS.
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
    // HTTP-only subcommands (no NATS connect required).
    if let SubCmd::Exec(args) = command {
        return cmd::exec::execute(&backend_url, args).await;
    } else if let SubCmd::Group(args) = command {
        return cmd::group::execute(&backend_url, args).await;
    } else if let SubCmd::Job(args) = command {
        return cmd::job::execute(&backend_url, args).await;
    } else if let SubCmd::Schedule(args) = command {
        return cmd::schedule::execute(&backend_url, args).await;
    } else if let SubCmd::View(args) = command {
        return cmd::view::execute(&backend_url, args).await;
    } else if let SubCmd::Agent(args) = command {
        return cmd::agent::execute(&backend_url, args).await;
    } else if let SubCmd::App(args) = command {
        return cmd::app::execute(&backend_url, args).await;
    } else if let SubCmd::Script(args) = command {
        return cmd::script::execute(&backend_url, args).await;
    } else if let SubCmd::Freeze(args) = command {
        return cmd::freeze::execute(&backend_url, args).await;
    } else if let SubCmd::Login(args) = command {
        return cmd::login::execute(&backend_url, args).await;
    } else if let SubCmd::Account(args) = command {
        return cmd::account::execute(&backend_url, args).await;
    } else if let SubCmd::Meta(args) = command {
        return cmd::meta::execute(&backend_url, args).await;
    } else if let SubCmd::Config(args) = command {
        return cmd::config::execute(&backend_url, args).await;
    } else if let SubCmd::Query(args) = command {
        return cmd::query::execute(&backend_url, args).await;
    } else if let SubCmd::Ping(args) = command {
        return cmd::ping::execute(&backend_url, args).await;
    } else if let SubCmd::Revoke(args) = command {
        return cmd::revoke::revoke(&backend_url, args).await;
    } else if let SubCmd::Unrevoke(args) = command {
        return cmd::revoke::unrevoke(&backend_url, args).await;
    } else if let SubCmd::Jetstream(cmd::jetstream::JetstreamArgs {
        sub: cmd::jetstream::JetstreamSub::Status,
    }) = command
    {
        return cmd::jetstream::status(&backend_url).await;
    } else if let SubCmd::SelfUpdate(args) = command {
        return cmd::self_update::execute(args).await;
    } else if let SubCmd::CommandKey(args) = command {
        // #1165: needs neither NATS nor the backend, and that is deliberate
        // rather than incidental. This mints the credential for recovering from
        // a dead backend; requiring a broker to produce it would put the
        // recovery tool behind the thing it recovers from.
        return cmd::command_key::execute(args);
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
        SubCmd::Jetstream(args) => cmd::jetstream::execute(client, args).await,
        SubCmd::Kill(args) => cmd::kill::execute(client, args).await,
        SubCmd::Exec(_)
        | SubCmd::Job(_)
        | SubCmd::Schedule(_)
        | SubCmd::View(_)
        | SubCmd::Agent(_)
        | SubCmd::Group(_)
        | SubCmd::Meta(_)
        | SubCmd::App(_)
        | SubCmd::Script(_)
        | SubCmd::Config(_)
        | SubCmd::Freeze(_)
        | SubCmd::Ping(_)
        | SubCmd::Revoke(_)
        | SubCmd::Unrevoke(_)
        | SubCmd::Login(_)
        | SubCmd::Account(_)
        | SubCmd::Query(_)
        | SubCmd::SelfUpdate(_)
        | SubCmd::CommandKey(_) => {
            unreachable!("handled above")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    /// `app` / `script` must be served without ever touching NATS: with a
    /// broker address nothing listens on, a NATS-first dispatch would fail
    /// to connect before sending any HTTP request.
    #[tokio::test]
    async fn app_and_script_dispatch_without_connecting_to_nats() {
        let dead_nats = "nats://127.0.0.1:1".to_string();
        let (base, log) = fake_backend(vec![(200, "[]"), (200, "[]")]).await;
        let app = SubCmd::App(cmd::app::AppArgs {
            sub: cmd::app::AppSub::List,
        });
        dispatch(dead_nats.clone(), base.clone(), app)
            .await
            .unwrap();
        let script = SubCmd::Script(cmd::script::ScriptArgs {
            sub: cmd::script::ScriptSub::List,
        });
        dispatch(dead_nats, base, script).await.unwrap();
        let got = seen(&log);
        assert_eq!(got[0].target, "/api/app-packages");
        assert_eq!(got[1].target, "/api/script-objects");
    }

    /// `meta` is HTTP-only too: every operation must succeed against a
    /// broker address nothing listens on.
    #[tokio::test]
    async fn meta_dispatches_without_connecting_to_nats() {
        let dead_nats = "nats://127.0.0.1:1".to_string();
        let ok = (200, r#"{"meta":{"entries":[]},"changed":true}"#);
        let (base, log) = fake_backend(vec![
            (200, r#"{"entries":[]}"#),
            ok,
            ok,
            (200, r#"{"entries":[]}"#),
        ])
        .await;
        let pc_id = || "PC-01".to_string();
        for sub in [
            cmd::meta::MetaSub::Get { pc_id: pc_id() },
            cmd::meta::MetaSub::Set {
                pc_id: pc_id(),
                key: "k".into(),
                value: "v".into(),
            },
            cmd::meta::MetaSub::Rm {
                pc_id: pc_id(),
                key: "k".into(),
            },
            cmd::meta::MetaSub::Clear { pc_id: pc_id() },
        ] {
            let cmd = SubCmd::Meta(cmd::meta::MetaArgs { sub });
            dispatch(dead_nats.clone(), base.clone(), cmd)
                .await
                .unwrap();
        }
        assert_eq!(seen(&log).len(), 4);
    }

    /// `agent` is HTTP-only: every subcommand must succeed against a broker
    /// address nothing listens on.
    #[tokio::test]
    async fn agent_dispatches_without_connecting_to_nats() {
        let dead_nats = "nats://127.0.0.1:1".to_string();
        let (base, log) = fake_backend(vec![
            (200, r#"{"target_version":"1.0.0"}"#),
            (200, "log"),
            (
                200,
                r#"{"version":"1.0.0","scope_key":"global","scope_label":"global","jitter":null}"#,
            ),
            (
                200,
                r#"{"version":"1.0.0","key":"1.0.0-linux-x86_64","platform":"linux-x86_64","size":20,"digest":null}"#,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("agent");
        let mut elf = vec![0u8; 20];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        std::fs::write(&bin, &elf).unwrap();
        for sub in [
            cmd::agent::AgentSub::Current,
            cmd::agent::AgentSub::Logs {
                pc_id: "PC-01".into(),
                tail: 5,
            },
            cmd::agent::AgentSub::Rollout(cmd::agent::RolloutArgs {
                version: "1.0.0".into(),
                global: true,
                group: None,
                pc: None,
                jitter: None,
            }),
            cmd::agent::AgentSub::Publish {
                binary: bin.clone(),
                version: Some("1.0.0".into()),
            },
        ] {
            let cmd = SubCmd::Agent(cmd::agent::AgentArgs { sub });
            dispatch(dead_nats.clone(), base.clone(), cmd)
                .await
                .unwrap();
        }
        assert_eq!(seen(&log).len(), 4);
    }
}
