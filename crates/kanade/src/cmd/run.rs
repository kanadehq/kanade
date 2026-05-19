use std::time::Duration;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use kanade_shared::wire::{Command, Shell};
use kanade_shared::{ExecResult, subject};
use tracing::info;
use uuid::Uuid;

const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Args, Debug)]
pub struct RunArgs {
    pub pc_id: String,
    #[arg(long, default_value = "powershell")]
    pub shell: String,
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Optional job_id; when set, `kanade kill <job_id>` can terminate the run.
    #[arg(long)]
    pub job_id: Option<String>,
    /// Script body (use `--` before the script to bypass clap flag parsing).
    pub script: Vec<String>,
}

pub async fn execute(client: async_nats::Client, args: RunArgs) -> Result<()> {
    if args.script.is_empty() {
        anyhow::bail!("script is empty (did you forget `--`?)");
    }
    let script = args.script.join(" ");
    let request_id = Uuid::new_v4().to_string();
    let shell = match args.shell.as_str() {
        "powershell" | "ps" | "pwsh" => Shell::Powershell,
        "cmd" => Shell::Cmd,
        other => anyhow::bail!("unknown shell {other:?} (use powershell or cmd)"),
    };
    let cmd = Command {
        id: "adhoc-run".to_string(),
        version: "0.0.0".to_string(),
        request_id: request_id.clone(),
        job_id: args.job_id.clone(),
        shell,
        script,
        timeout_secs: args.timeout,
        jitter_secs: None,
        // `kanade run` is one-shot / inline; use Job + `kanade exec`
        // for the run_as: user / system_gui / cwd flows.
        run_as: kanade_shared::wire::RunAs::System,
        cwd: None,
        // Ad-hoc inline run; no scheduled tick → no deadline.
        deadline_at: None,
        // v0.26: no Manifest behind this ad-hoc run, so use the
        // back-compat default (`Cached`).
        staleness: kanade_shared::wire::Staleness::Cached,
    };

    let result_subj = subject::results(&request_id);
    let mut sub = client.subscribe(result_subj.clone()).await?;

    let payload = serde_json::to_vec(&cmd)?;
    client
        .publish(subject::commands_pc(&args.pc_id), payload.into())
        .await?;
    client.flush().await?;
    info!(
        pc_id = %args.pc_id,
        request_id = %request_id,
        job_id = ?args.job_id,
        "sent command, waiting for result",
    );

    let wait = Duration::from_secs(args.timeout + 10);
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
