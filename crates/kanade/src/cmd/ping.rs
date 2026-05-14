use std::time::Duration;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use kanade_shared::{Heartbeat, subject};
use tracing::info;

#[derive(Args, Debug)]
pub struct PingArgs {
    pub pc_id: String,
    #[arg(long, default_value_t = 45)]
    pub wait: u64,
}

pub async fn execute(client: async_nats::Client, args: PingArgs) -> Result<()> {
    let subj = subject::heartbeat(&args.pc_id);
    let mut sub = client.subscribe(subj.clone()).await?;
    info!(subject = %subj, wait = args.wait, "waiting for heartbeat");
    let msg = tokio::time::timeout(Duration::from_secs(args.wait), sub.next())
        .await
        .map_err(|_| anyhow::anyhow!("no heartbeat from {} within {}s", args.pc_id, args.wait))?
        .ok_or_else(|| anyhow::anyhow!("heartbeat subscription closed"))?;
    let hb: Heartbeat = serde_json::from_slice(&msg.payload)?;
    println!("pc_id         : {}", hb.pc_id);
    println!("at            : {}", hb.at);
    println!("agent_version : {}", hb.agent_version);
    Ok(())
}
