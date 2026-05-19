use anyhow::Result;
use clap::Args;
use kanade_shared::subject;
use tracing::info;

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Exec id to terminate (formerly named `job_id` pre-v0.29; the
    /// positional arg is unchanged because it's a positional). Agents
    /// running a Command whose `exec_id` matches will kill the child
    /// process on receipt (spec §2.6 Layer 3).
    pub exec_id: String,
}

pub async fn execute(client: async_nats::Client, args: KillArgs) -> Result<()> {
    client
        .publish(subject::kill(&args.exec_id), bytes::Bytes::new())
        .await?;
    client.flush().await?;
    info!(exec_id = %args.exec_id, "kill signal published");
    println!("kill signal published to kill.{}", args.exec_id);
    Ok(())
}
