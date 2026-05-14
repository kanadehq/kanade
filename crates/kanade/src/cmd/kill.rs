use anyhow::Result;
use clap::Args;
use kanade_shared::subject;
use tracing::info;

#[derive(Args, Debug)]
pub struct KillArgs {
    /// Job id to terminate. Agents running a Command whose `job_id` matches
    /// will kill the child process on receipt (spec §2.6 Layer 3).
    pub job_id: String,
}

pub async fn execute(client: async_nats::Client, args: KillArgs) -> Result<()> {
    client
        .publish(subject::kill(&args.job_id), bytes::Bytes::new())
        .await?;
    client.flush().await?;
    info!(job_id = %args.job_id, "kill signal published");
    println!("kill signal published to kill.{}", args.job_id);
    Ok(())
}
