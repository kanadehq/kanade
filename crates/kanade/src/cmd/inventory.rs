//! `kanade inventory <pc_id>` — ask one agent to collect WMI inventory
//! NOW (out of band of its configured cadence) and publish it to the
//! INVENTORY stream.
//!
//! Useful for "I just installed the agent — see it in the UI right
//! now" and for debugging WMI / repository / permission issues
//! without waiting up to `inventory_interval + inventory_jitter` for
//! the next scheduled cycle.

use anyhow::{Context, Result};
use clap::Args;
use kanade_shared::subject;

#[derive(Args, Debug)]
pub struct InventoryArgs {
    /// PC id of the agent to query (must be online).
    pub pc_id: String,

    /// Reply timeout. Defaults to 30s — WMI on a slow / busy host
    /// can take a few seconds for the full set of queries.
    #[arg(long, default_value = "30s")]
    pub timeout: humantime::Duration,
}

pub async fn execute(client: async_nats::Client, args: InventoryArgs) -> Result<()> {
    let subj = subject::inventory_request(&args.pc_id);
    let reply = tokio::time::timeout(
        args.timeout.into(),
        client.request(subj.clone(), bytes::Bytes::new()),
    )
    .await
    .with_context(|| format!("timeout ({}) waiting for {}", args.timeout, args.pc_id))?
    .with_context(|| format!("request {subj}"))?;

    let body = String::from_utf8_lossy(&reply.payload);
    println!("{pc}: {body}", pc = args.pc_id);
    if !body.starts_with("ok") {
        anyhow::bail!(
            "inventory collection failed on {} — see agent log for details",
            args.pc_id
        );
    }
    Ok(())
}
