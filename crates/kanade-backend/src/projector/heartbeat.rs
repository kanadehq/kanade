//! Heartbeat → agents-row baseline projector.
//!
//! Heartbeats are core NATS publishes (no JetStream stream — they're
//! cheap liveness signals on a 30 s cadence, not part of the durable
//! event log). The projector subscribes to `heartbeat.>` directly and
//! upserts a minimal row into `agents` so:
//!
//!   * a freshly-deployed agent shows up in the SPA within one
//!     heartbeat interval (~30 s), without needing inventory to
//!     succeed first;
//!   * hosts where WMI is broken still surface as "alive" agents in
//!     the UI, with the user able to see `last_heartbeat` ticking
//!     even when `last_inventory` stays NULL.
//!
//! The upsert uses COALESCE so a later inventory snapshot doesn't get
//! overwritten by the next heartbeat (heartbeat only fills fields
//! that are still NULL).

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::Heartbeat;
use sqlx::SqlitePool;
use tracing::{info, warn};

pub async fn run(client: async_nats::Client, pool: SqlitePool) -> Result<()> {
    let mut sub = client
        .subscribe("heartbeat.>")
        .await
        .context("subscribe heartbeat.>")?;
    info!("heartbeat projector started (subject: heartbeat.>)");

    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<Heartbeat>(&msg.payload) {
            Ok(hb) => {
                if let Err(e) = upsert_baseline(&pool, &hb).await {
                    warn!(error = %e, pc_id = %hb.pc_id, "heartbeat upsert failed");
                }
            }
            Err(e) => {
                warn!(error = %e, subject = %msg.subject, "decode Heartbeat");
            }
        }
    }
    Ok(())
}

async fn upsert_baseline(pool: &SqlitePool, hb: &Heartbeat) -> Result<()> {
    // COALESCE on existing values so the inventory projector's
    // richer fill (full os_name / os_version / cpu / ram / disks)
    // isn't clobbered when a heartbeat arrives between inventory
    // cycles.
    // v0.37 Part 2: persist self-perf metrics into the agents row.
    // Heartbeat fields are Option, so a heartbeat that didn't carry
    // them (older agent, sysinfo error path) overwrites with NULL —
    // SPA renders that as blank, matching the "metric isn't being
    // reported" state. We DO replace rather than COALESCE here:
    // perf is intentionally a live signal, not a sticky one.
    sqlx::query(
        "INSERT INTO agents (
             pc_id, hostname, os_family, agent_version,
             last_heartbeat,
             agent_cpu_pct, agent_rss_bytes,
             agent_disk_read_bytes, agent_disk_written_bytes,
             updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(pc_id) DO UPDATE SET
             hostname                  = COALESCE(agents.hostname, excluded.hostname),
             os_family                 = COALESCE(agents.os_family, excluded.os_family),
             agent_version             = excluded.agent_version,
             last_heartbeat            = excluded.last_heartbeat,
             agent_cpu_pct             = excluded.agent_cpu_pct,
             agent_rss_bytes           = excluded.agent_rss_bytes,
             agent_disk_read_bytes     = excluded.agent_disk_read_bytes,
             agent_disk_written_bytes  = excluded.agent_disk_written_bytes,
             updated_at                = CURRENT_TIMESTAMP",
    )
    .bind(&hb.pc_id)
    .bind(&hb.hostname)
    .bind(&hb.os_family)
    .bind(&hb.agent_version)
    .bind(hb.at)
    .bind(hb.agent_cpu_pct)
    .bind(hb.agent_rss_bytes)
    .bind(hb.agent_disk_read_bytes)
    .bind(hb.agent_disk_written_bytes)
    .execute(pool)
    .await?;
    Ok(())
}
