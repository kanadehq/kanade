use anyhow::Result;
use async_nats::jetstream;
use clap::Args;
use kanade_shared::kv::{BUCKET_SCRIPT_STATUS, SCRIPT_STATUS_ACTIVE, SCRIPT_STATUS_REVOKED};
use tracing::info;

#[derive(Args, Debug)]
pub struct RevokeArgs {
    /// Command id to revoke (matches Command.id from the YAML manifest).
    pub cmd_id: String,
}

#[derive(Args, Debug)]
pub struct UnrevokeArgs {
    pub cmd_id: String,
}

pub async fn revoke(client: async_nats::Client, args: RevokeArgs) -> Result<()> {
    set_status(&client, &args.cmd_id, SCRIPT_STATUS_REVOKED).await?;
    info!(cmd_id = %args.cmd_id, "revoked");
    println!("revoked: {} → {}", args.cmd_id, SCRIPT_STATUS_REVOKED);
    crate::audit::record(&client, "revoke", Some(&args.cmd_id), serde_json::json!({})).await;
    Ok(())
}

pub async fn unrevoke(client: async_nats::Client, args: UnrevokeArgs) -> Result<()> {
    set_status(&client, &args.cmd_id, SCRIPT_STATUS_ACTIVE).await?;
    info!(cmd_id = %args.cmd_id, "unrevoked");
    println!("unrevoked: {} → {}", args.cmd_id, SCRIPT_STATUS_ACTIVE);
    crate::audit::record(
        &client,
        "unrevoke",
        Some(&args.cmd_id),
        serde_json::json!({}),
    )
    .await;
    Ok(())
}

async fn set_status(client: &async_nats::Client, cmd_id: &str, value: &str) -> Result<()> {
    let js = jetstream::new(client.clone());
    let store = js.get_key_value(BUCKET_SCRIPT_STATUS).await.map_err(|e| {
        anyhow::anyhow!(
            "KV bucket '{BUCKET_SCRIPT_STATUS}' not found ({e}) — run `kanade jetstream setup` first"
        )
    })?;
    store
        .put(cmd_id, bytes::Bytes::copy_from_slice(value.as_bytes()))
        .await?;
    Ok(())
}
