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

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::HostPerf;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// #488: micro-batch window — same rationale as the heartbeat
/// projector (one transaction per buffered window instead of one per
/// sample; ~50 tx/s → ≤1 tx/s at 3,000 agents). Samples are
/// append-only, so the buffer is a plain Vec — INSERT OR IGNORE
/// still dedupes restart re-publishes inside the transaction.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const FLUSH_MAX_BUFFERED: usize = 1000;

pub async fn run(client: async_nats::Client, pool: SqlitePool) -> Result<()> {
    let mut sub = client
        .subscribe("host_perf.>")
        .await
        .context("subscribe host_perf.>")?;
    info!("host_perf projector started (subject: host_perf.>)");

    let mut buf: Vec<HostPerf> = Vec::new();
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe = sub.next() => match maybe {
                Some(msg) => {
                    match serde_json::from_slice::<HostPerf>(&msg.payload) {
                        Ok(s) => {
                            buf.push(s);
                            if buf.len() >= FLUSH_MAX_BUFFERED {
                                flush(&pool, &mut buf).await;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, subject = %msg.subject, "decode HostPerf");
                        }
                    }
                }
                None => {
                    flush(&pool, &mut buf).await;
                    return Ok(());
                }
            },
            // Unconditional tick poll — same rationale as the
            // heartbeat projector (a guarded branch stops polling
            // the interval and the first message after idle flushes
            // solo on the overdue tick; review PR #553, gemini).
            _ = tick.tick() => {
                if !buf.is_empty() {
                    flush(&pool, &mut buf).await;
                }
            }
        }
    }
}

/// Write every buffered sample in one transaction. Perf samples are
/// lossy by design (a dropped window = one gap in the chart), so a
/// failed flush warns and drops the window.
async fn flush(pool: &SqlitePool, buf: &mut Vec<HostPerf>) {
    if buf.is_empty() {
        return;
    }
    let n = buf.len();
    let res = async {
        let mut tx = pool.begin().await.context("begin host_perf flush tx")?;
        for s in buf.iter() {
            insert_sample(&mut *tx, s).await?;
        }
        tx.commit().await.context("commit host_perf flush tx")
    }
    .await;
    if let Err(e) = res {
        warn!(error = %e, buffered = n, "host_perf flush failed; dropping window");
    }
    buf.clear();
}

async fn insert_sample<'e, E>(executor: E, s: &HostPerf) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
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
    .execute(executor)
    .await?;
    Ok(())
}
