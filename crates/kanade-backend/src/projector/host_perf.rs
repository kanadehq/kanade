//! HostPerf → `host_perf_samples` time-series projector.
//!
//! Phase 1 of the perf telemetry pipeline (v0.40). Mirrors the
//! heartbeat projector's shape — core NATS subscribe, no JetStream
//! durable consumer, append-only INSERT instead of UPSERT.
//!
//! Choosing core NATS over JetStream is deliberate: a missed sample
//! shows up as a single gap in the chart, which is acceptable for the
//! "what was this host doing?" use case. JetStream durability would
//! cost an ack RTT per message × every PC in the fleet, with no real
//! benefit when graphs tolerate gaps. Heartbeat made the same call
//! for the same reason.
//!
//! Duplicate samples (same `pc_id` + `at` UTC second, e.g. when an
//! agent restart re-publishes one of its in-flight samples) are
//! dropped via `INSERT OR IGNORE` — the first writer wins and the
//! duplicate quietly disappears.

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::HostPerf;
use sqlx::SqlitePool;
use tracing::{info, warn};

pub async fn run(client: async_nats::Client, pool: SqlitePool) -> Result<()> {
    let mut sub = client
        .subscribe("host_perf.>")
        .await
        .context("subscribe host_perf.>")?;
    info!("host_perf projector started (subject: host_perf.>)");

    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<HostPerf>(&msg.payload) {
            Ok(s) => {
                if let Err(e) = insert_sample(&pool, &s).await {
                    warn!(error = %e, pc_id = %s.pc_id, "host_perf insert failed");
                }
            }
            Err(e) => {
                warn!(error = %e, subject = %msg.subject, "decode HostPerf");
            }
        }
    }
    Ok(())
}

async fn insert_sample(pool: &SqlitePool, s: &HostPerf) -> Result<()> {
    // `OR IGNORE` so a re-published in-flight sample from agent
    // restart doesn't error — the (pc_id, at) PK uniquely identifies
    // a tick, and the first writer wins (re-publishes are byte-
    // identical with the same wall-clock `at`, so dropping the
    // duplicate is harmless).
    sqlx::query(
        "INSERT OR IGNORE INTO host_perf_samples (
             pc_id, at,
             cpu_pct, cpu_count,
             mem_used_bytes, mem_total_bytes,
             swap_used_bytes, swap_total_bytes,
             disk_read_bytes_per_sec, disk_written_bytes_per_sec,
             net_rx_bytes_per_sec, net_tx_bytes_per_sec
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&s.pc_id)
    .bind(s.at)
    .bind(s.cpu_pct)
    .bind(s.cpu_count)
    .bind(s.mem_used_bytes)
    .bind(s.mem_total_bytes)
    .bind(s.swap_used_bytes)
    .bind(s.swap_total_bytes)
    .bind(s.disk_read_bytes_per_sec)
    .bind(s.disk_written_bytes_per_sec)
    .bind(s.net_rx_bytes_per_sec)
    .bind(s.net_tx_bytes_per_sec)
    .execute(pool)
    .await?;
    Ok(())
}
