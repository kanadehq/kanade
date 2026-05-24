//! `ProcessPerf` → `process_perf_samples` projector (v0.41 / Phase 2).
//!
//! Mirrors the host_perf projector's shape: core NATS subscribe, no
//! JetStream durability, append-only `INSERT OR IGNORE`. Process-perf
//! is even more "gappy data is fine" than host-perf — it only flows
//! during an active investigation window, and a missed tick just
//! means one fewer sample on a chart the operator is staring at live.

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::ProcessPerf;
use sqlx::SqlitePool;
use tracing::{info, warn};

pub async fn run(client: async_nats::Client, pool: SqlitePool) -> Result<()> {
    let mut sub = client
        .subscribe("process_perf.>")
        .await
        .context("subscribe process_perf.>")?;
    info!("process_perf projector started (subject: process_perf.>)");

    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<ProcessPerf>(&msg.payload) {
            Ok(s) => {
                if let Err(e) = insert_snapshot(&pool, &s).await {
                    warn!(error = %e, pc_id = %s.pc_id, "process_perf insert failed");
                }
            }
            Err(e) => {
                warn!(error = %e, subject = %msg.subject, "decode ProcessPerf");
            }
        }
    }
    Ok(())
}

async fn insert_snapshot(pool: &SqlitePool, s: &ProcessPerf) -> Result<()> {
    if s.processes.is_empty() {
        return Ok(());
    }
    // Single multi-VALUES insert so a top-20 snapshot is one
    // round-trip instead of 20 — same pattern history.rs uses for
    // batched event writes. `OR IGNORE` so a re-published in-flight
    // sample from agent restart doesn't error.
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT OR IGNORE INTO process_perf_samples (
             pc_id, at, pid, name,
             cpu_pct, rss_bytes,
             disk_read_bytes_per_sec, disk_written_bytes_per_sec
         ) ",
    );
    qb.push_values(&s.processes, |mut b, p| {
        b.push_bind(&s.pc_id)
            .push_bind(s.at)
            .push_bind(p.pid)
            .push_bind(&p.name)
            .push_bind(p.cpu_pct)
            .push_bind(p.rss_bytes)
            .push_bind(p.disk_read_bytes_per_sec)
            .push_bind(p.disk_written_bytes_per_sec);
    });
    qb.build().execute(pool).await?;
    Ok(())
}
