use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use kanade_shared::{Command, ExecResult, Heartbeat, Shell, subject};
use tracing::{debug, info};
use uuid::Uuid;

const DEFAULT_NATS: &str = "nats://127.0.0.1:4222";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

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
    Run {
        pc_id: String,
        #[arg(long, default_value = "powershell")]
        shell: String,
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// Script body (use `--` before the script to bypass clap flag parsing).
        script: Vec<String>,
    },
    /// Wait for one heartbeat from the target PC.
    Ping {
        pc_id: String,
        #[arg(long, default_value_t = 45)]
        wait: u64,
    },
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
        SubCmd::Run {
            pc_id,
            shell,
            timeout,
            script,
        } => run_command(client, pc_id, shell, timeout, script).await,
        SubCmd::Ping { pc_id, wait } => ping(client, pc_id, wait).await,
    }
}

async fn run_command(
    client: async_nats::Client,
    pc_id: String,
    shell: String,
    timeout: u64,
    script: Vec<String>,
) -> Result<()> {
    if script.is_empty() {
        anyhow::bail!("script is empty (did you forget `--`?)");
    }
    let script = script.join(" ");
    let request_id = Uuid::new_v4().to_string();
    let shell = match shell.as_str() {
        "powershell" | "ps" | "pwsh" => Shell::Powershell,
        "cmd" => Shell::Cmd,
        other => anyhow::bail!("unknown shell {other:?} (use powershell or cmd)"),
    };
    let cmd = Command {
        id: "adhoc-run".to_string(),
        version: "0.0.0".to_string(),
        request_id: request_id.clone(),
        job_id: None,
        shell,
        script,
        timeout_secs: timeout,
        jitter_secs: None,
    };

    let result_subj = subject::results(&request_id);
    let mut sub = client.subscribe(result_subj.clone()).await?;

    let payload = serde_json::to_vec(&cmd)?;
    client
        .publish(subject::commands_pc(&pc_id), payload.into())
        .await?;
    client.flush().await?;
    info!(pc_id = %pc_id, request_id = %request_id, "sent command, waiting for result");

    let wait = Duration::from_secs(timeout + 10);
    let msg = tokio::time::timeout(wait, sub.next())
        .await
        .map_err(|_| anyhow::anyhow!("timeout waiting for result on {result_subj}"))?
        .ok_or_else(|| anyhow::anyhow!("result subscription closed"))?;
    let result: ExecResult = serde_json::from_slice(&msg.payload)?;

    println!("pc_id     : {}", result.pc_id);
    println!("exit_code : {}", result.exit_code);
    println!("started   : {}", result.started_at);
    println!("finished  : {}", result.finished_at);
    println!("--- stdout ---");
    print!("{}", result.stdout);
    if !result.stdout.ends_with('\n') {
        println!();
    }
    if !result.stderr.is_empty() {
        println!("--- stderr ---");
        print!("{}", result.stderr);
        if !result.stderr.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

async fn ping(client: async_nats::Client, pc_id: String, wait: u64) -> Result<()> {
    let subj = subject::heartbeat(&pc_id);
    let mut sub = client.subscribe(subj.clone()).await?;
    info!(subject = %subj, wait, "waiting for heartbeat");
    let msg = tokio::time::timeout(Duration::from_secs(wait), sub.next())
        .await
        .map_err(|_| anyhow::anyhow!("no heartbeat from {pc_id} within {wait}s"))?
        .ok_or_else(|| anyhow::anyhow!("heartbeat subscription closed"))?;
    let hb: Heartbeat = serde_json::from_slice(&msg.payload)?;
    println!("pc_id         : {}", hb.pc_id);
    println!("at            : {}", hb.at);
    println!("agent_version : {}", hb.agent_version);
    Ok(())
}
