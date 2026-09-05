use std::time::Duration;

use anyhow::Result;
use clap::Args;
use futures::StreamExt;
use kanade_shared::{Heartbeat, subject};
use tracing::info;

#[derive(Args, Debug)]
pub struct PingArgs {
    /// Target PC, as the agent registered itself — its OS hostname,
    /// VERBATIM (NATS subjects are case-sensitive, and casing is not
    /// uniform across a fleet). Waits for the agent's NEXT periodic
    /// heartbeat rather than polling a last-known value, so a healthy
    /// agent still takes up to one heartbeat interval to answer — this
    /// is a liveness probe, not a status lookup.
    pub pc_id: String,
    /// Seconds to wait for that heartbeat before giving up.
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
