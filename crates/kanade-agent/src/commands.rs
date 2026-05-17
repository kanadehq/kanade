use anyhow::Result;
use async_nats::jetstream::kv::Store;
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_REVOKED};
use kanade_shared::wire::Command;
use kanade_shared::{ExecResult, subject};
use tracing::{error, info, warn};

use crate::process::{ExecOutcome, run_command_with_kill};

pub async fn command_loop(
    client: async_nats::Client,
    pc_id: String,
    mut sub: async_nats::Subscriber,
) {
    let jetstream = async_nats::jetstream::new(client.clone());
    let script_current = jetstream.get_key_value(BUCKET_SCRIPT_CURRENT).await.ok();
    let script_status = jetstream.get_key_value(BUCKET_SCRIPT_STATUS).await.ok();
    if script_current.is_none() {
        warn!(
            bucket = BUCKET_SCRIPT_CURRENT,
            "KV bucket missing — version-pinning skipped (run `kanade jetstream setup`)"
        );
    }
    if script_status.is_none() {
        warn!(
            bucket = BUCKET_SCRIPT_STATUS,
            "KV bucket missing — revoke check skipped (run `kanade jetstream setup`)"
        );
    }

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
        let cur = script_current.clone();
        let sta = script_status.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_command(client, pc_id, cmd, cur, sta).await {
                error!(error = %e, "command handler failed");
            }
        });
    }
}

async fn handle_command(
    client: async_nats::Client,
    pc_id: String,
    cmd: Command,
    script_current: Option<Store>,
    script_status: Option<Store>,
) -> Result<()> {
    // Spec §2.6 Layer 2: version-pinning + revoke check
    if let Some(cur) = &script_current
        && let Ok(Some(entry)) = cur.get(&cmd.id).await
    {
        let expected = String::from_utf8_lossy(&entry).to_string();
        if expected != cmd.version {
            warn!(
                cmd_id = %cmd.id,
                expected = %expected,
                got = %cmd.version,
                request_id = %cmd.request_id,
                "skip stale command (version mismatch)",
            );
            return Ok(());
        }
    }
    if let Some(sta) = &script_status
        && let Ok(Some(entry)) = sta.get(&cmd.id).await
    {
        let status = String::from_utf8_lossy(&entry).to_string();
        if status == SCRIPT_STATUS_REVOKED {
            warn!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                "skip revoked command",
            );
            return Ok(());
        }
    }

    info!(
        cmd_id = %cmd.id,
        request_id = %cmd.request_id,
        version = %cmd.version,
        job_id = ?cmd.job_id,
        "executing command",
    );
    let started_at = chrono::Utc::now();
    let outcome = run_command_with_kill(&client, &cmd).await?;
    let finished_at = chrono::Utc::now();

    let (exit_code, stdout, stderr, status_note) = match outcome {
        ExecOutcome::Completed {
            exit_code,
            stdout,
            stderr,
        } => (exit_code, stdout, stderr, None),
        ExecOutcome::Killed { stdout, stderr } => {
            let jid = cmd.job_id.as_deref().unwrap_or("?");
            (
                -1,
                stdout,
                stderr,
                Some(format!("killed by remote signal (kill.{jid})")),
            )
        }
        ExecOutcome::Timeout { stdout, stderr } => (
            -1,
            stdout,
            stderr,
            Some(format!("timeout after {}s", cmd.timeout_secs)),
        ),
    };
    let stderr = match status_note {
        Some(note) if stderr.is_empty() => note,
        Some(note) => format!("{stderr}\n{note}"),
        None => stderr,
    };

    let result = ExecResult {
        request_id: cmd.request_id.clone(),
        pc_id: pc_id.clone(),
        exit_code,
        stdout,
        stderr,
        started_at,
        finished_at,
        // Forward `Command.id` (the manifest's id, e.g. "inventory-hw"),
        // NOT `Command.job_id` (a per-deploy UUID). The backend's
        // results projector uses this to look up the manifest's
        // `inventory:` hint and upsert `inventory_facts` rows.
        manifest_id: Some(cmd.id.clone()),
    };
    let payload = serde_json::to_vec(&result)?;
    client
        .publish(subject::results(&cmd.request_id), payload.into())
        .await?;
    info!(request_id = %cmd.request_id, exit_code, "published result");
    Ok(())
}
