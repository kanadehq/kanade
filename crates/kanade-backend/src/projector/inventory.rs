use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull::Config as PullConfig};
use futures::StreamExt;
use kanade_shared::kv::STREAM_INVENTORY;
use kanade_shared::wire::HwInventory;
use sqlx::SqlitePool;
use tracing::{info, warn};

const CONSUMER_NAME: &str = "backend_inventory_projector";

/// Consume the INVENTORY stream and upsert each `HwInventory` snapshot
/// into the `agents` table. Durable consumer so the projector picks up
/// where it left off after a restart.
pub async fn run(js: jetstream::Context, pool: SqlitePool) -> Result<()> {
    let stream = js
        .get_stream(STREAM_INVENTORY)
        .await
        .with_context(|| format!("get stream {STREAM_INVENTORY}"))?;
    let consumer = stream
        .get_or_create_consumer(
            CONSUMER_NAME,
            PullConfig {
                durable_name: Some(CONSUMER_NAME.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .context("create inventory consumer")?;
    info!(
        stream = STREAM_INVENTORY,
        consumer = CONSUMER_NAME,
        "inventory projector started"
    );

    let mut messages = consumer
        .messages()
        .await
        .context("subscribe inventory messages")?;
    while let Some(msg) = messages.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "inventory consumer error");
                continue;
            }
        };
        match serde_json::from_slice::<HwInventory>(&msg.payload) {
            Ok(hw) => {
                if let Err(e) = upsert_agent(&pool, &hw).await {
                    warn!(error = %e, pc_id = %hw.pc_id, "upsert agent failed");
                } else {
                    info!(pc_id = %hw.pc_id, "projected inventory");
                }
            }
            Err(e) => warn!(error = %e, subject = %msg.subject, "deserialize HwInventory"),
        }
        if let Err(e) = msg.ack().await {
            warn!(error = ?e, "ack inventory message");
        }
    }
    Ok(())
}

async fn upsert_agent(pool: &SqlitePool, hw: &HwInventory) -> Result<()> {
    let disks_json = serde_json::to_string(&hw.disks)?;
    sqlx::query(
        "INSERT INTO agents (
             pc_id, hostname, os_name, os_version, os_build,
             cpu_model, cpu_cores, ram_bytes, disks_json,
             last_inventory, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(pc_id) DO UPDATE SET
             hostname       = excluded.hostname,
             os_name        = excluded.os_name,
             os_version     = excluded.os_version,
             os_build       = excluded.os_build,
             cpu_model      = excluded.cpu_model,
             cpu_cores      = excluded.cpu_cores,
             ram_bytes      = excluded.ram_bytes,
             disks_json     = excluded.disks_json,
             last_inventory = excluded.last_inventory,
             updated_at     = CURRENT_TIMESTAMP",
    )
    .bind(&hw.pc_id)
    .bind(&hw.hostname)
    .bind(&hw.os_name)
    .bind(&hw.os_version)
    .bind(&hw.os_build)
    .bind(&hw.cpu_model)
    .bind(hw.cpu_cores as i64)
    .bind(hw.ram_bytes as i64)
    .bind(disks_json)
    .bind(hw.collected_at)
    .execute(pool)
    .await?;
    Ok(())
}
