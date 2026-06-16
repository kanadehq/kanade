use std::time::Duration;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use kanade_shared::wire::{Command, RunAs, Shell};
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
    /// Optional exec_id (formerly `--job-id` pre-v0.29); when set,
    /// `kanade kill <exec_id>` can terminate the run. The CLI still
    /// accepts the historical `--job-id` flag name via clap alias so
    /// existing scripts keep working.
    #[arg(long, alias = "job-id")]
    pub exec_id: Option<String>,
    /// Execution identity: `system` (default), `user`, or
    /// `system_gui` — same values as the manifest's
    /// `execute.run_as`.
    #[arg(long, default_value = "system", value_parser = ["system", "user", "system_gui", "system-gui"])]
    pub run_as: String,
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
    // Keep in sync with `kanade_shared::wire::RunAs` (and the
    // `value_parser` list on the arg above) if the enum grows a
    // variant — clap already rejects anything outside that list,
    // so the bail! arm is unreachable today.
    let run_as = match args.run_as.as_str() {
        "system" => RunAs::System,
        "user" => RunAs::User,
        "system_gui" | "system-gui" => RunAs::SystemGui,
        other => anyhow::bail!("unknown run_as {other:?} (use system, user, or system_gui)"),
    };
    let cmd = Command {
        id: "adhoc-run".to_string(),
        version: "0.0.0".to_string(),
        request_id: request_id.clone(),
        exec_id: args.exec_id.clone(),
        shell,
        script,
        // `kanade run` is always inline — there's no Manifest behind
        // it to carry a script_object reference (#210).
        script_object: None,
        script_object_sha256: None,
        timeout_secs: args.timeout,
        jitter_secs: None,
        // Operator-selectable via `--run-as` (defaults to system,
        // the historical behaviour). cwd customisation still
        // belongs on a registered Job + `kanade exec`.
        run_as,
        cwd: None,
        // Ad-hoc inline run; no scheduled tick → no deadline.
        deadline_at: None,
        // v0.26: no Manifest behind this ad-hoc run, so use the
        // back-compat default (`Cached`).
        staleness: kanade_shared::wire::Staleness::Cached,
        // Issue #246: no Manifest → no emit hint. Stdout flows
        // back via ExecResult unchanged.
        emit: None,
        // #290: ad-hoc inline run is never a check.
        check: None,
        // #219: ad-hoc inline run has no manifest → no collect hint.
        collect: None,
        // #418 Phase 4: ad-hoc run has no schedule → no retry policy.
        retry: None,
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
        exec_id = ?args.exec_id,
        "sent command, waiting for result",
    );

    // Audit at dispatch (not on result): the code ran on the host
    // regardless of whether we hang around for its output. Truncate the
    // script — the audit row should show intent, not be a payload dump.
    crate::audit::record(
        &client,
        "run",
        Some(&args.pc_id),
        serde_json::json!({
            "request_id": request_id,
            "exec_id": args.exec_id,
            "shell": args.shell,
            // The parsed enum (not the raw flag string) so the audit
            // row records the normalized snake_case form even when
            // the operator typed the hyphenated `system-gui` alias.
            "run_as": run_as,
            "script": cmd.script.chars().take(500).collect::<String>(),
        }),
    )
    .await;

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
