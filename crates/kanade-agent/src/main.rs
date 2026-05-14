use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use kanade_shared::config::load_agent_config;
use kanade_shared::{Command, ExecResult, Heartbeat, Shell, subject};
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tracing::{error, info, warn};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

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
    let kill_all = client.subscribe("kill.>").await?;
    info!(
        commands_all = subject::COMMANDS_ALL,
        commands_self = %subject::commands_pc(&cfg.agent.id),
        "subscribed",
    );

    let pc_id = cfg.agent.id.clone();
    tokio::spawn(heartbeat_loop(client.clone(), pc_id.clone()));
    tokio::spawn(kill_listener(kill_all));

    let _ = tokio::join!(
        command_loop(client.clone(), pc_id.clone(), cmd_all),
        command_loop(client.clone(), pc_id.clone(), cmd_self),
    );

    Ok(())
}

async fn heartbeat_loop(client: async_nats::Client, pc_id: String) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        interval.tick().await;
        let hb = Heartbeat {
            pc_id: pc_id.clone(),
            at: chrono::Utc::now(),
            agent_version: AGENT_VERSION.to_string(),
        };
        let payload = match serde_json::to_vec(&hb) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "serialize heartbeat");
                continue;
            }
        };
        if let Err(e) = client
            .publish(subject::heartbeat(&pc_id), payload.into())
            .await
        {
            warn!(error = %e, "publish heartbeat");
        }
    }
}

async fn kill_listener(mut sub: async_nats::Subscriber) {
    while let Some(msg) = sub.next().await {
        info!(subject = %msg.subject, "received kill signal (no-op in Sprint 1)");
    }
}

async fn command_loop(client: async_nats::Client, pc_id: String, mut sub: async_nats::Subscriber) {
    while let Some(msg) = sub.next().await {
        let cmd: Command = match serde_json::from_slice(&msg.payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, subject = %msg.subject, "deserialize command");
                continue;
            }
        };
        let client = client.clone();
        let pc_id = pc_id.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(client, pc_id, cmd).await {
                error!(error = %e, "command handler failed");
            }
        });
    }
}

async fn handle_command(client: async_nats::Client, pc_id: String, cmd: Command) -> Result<()> {
    info!(
        cmd_id = %cmd.id,
        request_id = %cmd.request_id,
        version = %cmd.version,
        "executing command",
    );
    let started_at = chrono::Utc::now();
    let timeout = Duration::from_secs(cmd.timeout_secs.max(1));

    let exec = tokio::time::timeout(timeout, run_child(&cmd)).await;

    let (exit_code, stdout, stderr) = match exec {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => (-1, String::new(), format!("error: {e}")),
        Err(_) => (-1, String::new(), format!("timeout after {timeout:?}")),
    };
    let finished_at = chrono::Utc::now();

    let result = ExecResult {
        request_id: cmd.request_id.clone(),
        pc_id: pc_id.clone(),
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
    };
    let payload = serde_json::to_vec(&result)?;
    client
        .publish(subject::results(&cmd.request_id), payload.into())
        .await?;
    info!(request_id = %cmd.request_id, exit_code, "published result");
    Ok(())
}

async fn run_child(cmd: &Command) -> Result<(i32, String, String)> {
    let (program, args): (&str, Vec<&str>) = match cmd.shell {
        Shell::Powershell => (
            "powershell",
            vec!["-NoProfile", "-NonInteractive", "-Command", &cmd.script],
        ),
        Shell::Cmd => ("cmd", vec!["/C", &cmd.script]),
    };
    let mut child = ProcessCommand::new(program)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stdout_handle {
            s.read_to_string(&mut buf).await?;
        }
        Ok::<_, anyhow::Error>(buf)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stderr_handle {
            s.read_to_string(&mut buf).await?;
        }
        Ok::<_, anyhow::Error>(buf)
    });

    let status = child.wait().await?;
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}
