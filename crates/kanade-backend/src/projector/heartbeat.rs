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

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use kanade_shared::wire::Heartbeat;
use sqlx::SqlitePool;
use tracing::{info, warn};

/// #488: micro-batch window. At 3,000 agents × 30 s cadence the old
/// one-transaction-per-heartbeat shape was ~100 write transactions/s
/// perpetually competing for SQLite's single writer lock with the
/// results/events projectors and cleanup DELETEs. Buffering for 1 s
/// and flushing every buffered PC's NEWEST heartbeat in ONE
/// transaction cuts that to ≤1 tx/s with no semantic change — a
/// heartbeat is a liveness sample, and within a 1 s window only the
/// latest per PC matters.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// Backstop so a reconnect-storm burst (whole fleet re-heartbeating
/// at once) can't grow the buffer unboundedly within one window.
const FLUSH_MAX_BUFFERED: usize = 1000;

pub async fn run(client: async_nats::Client, pool: SqlitePool) -> Result<()> {
    let mut sub = client
        .subscribe("heartbeat.>")
        .await
        .context("subscribe heartbeat.>")?;
    info!("heartbeat projector started (subject: heartbeat.>)");

    // Newest heartbeat per pc_id within the current window.
    let mut buf: HashMap<String, Heartbeat> = HashMap::new();
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe = sub.next() => match maybe {
                Some(msg) => {
                    match serde_json::from_slice::<Heartbeat>(&msg.payload) {
                        Ok(hb) => {
                            buf.insert(hb.pc_id.clone(), hb);
                            if buf.len() >= FLUSH_MAX_BUFFERED {
                                flush(&pool, &mut buf).await;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, subject = %msg.subject, "decode Heartbeat");
                        }
                    }
                }
                None => {
                    flush(&pool, &mut buf).await;
                    return Ok(());
                }
            },
            // No `if !buf.is_empty()` guard on the branch: disabling
            // it stops polling the interval, so the first message
            // after an idle period would re-enable a long-overdue
            // tick and flush solo — defeating the batching exactly
            // when a burst starts (review PR #553, gemini). Poll the
            // tick unconditionally; skip the no-op flush inside.
            _ = tick.tick() => {
                if !buf.is_empty() {
                    flush(&pool, &mut buf).await;
                }
            }
        }
    }
}

/// Write every buffered heartbeat in one transaction. Heartbeats are
/// lossy by design (core NATS, no JetStream durability), so a failed
/// flush warns and drops the window — the next 30 s cadence delivers
/// fresher data anyway.
async fn flush(pool: &SqlitePool, buf: &mut HashMap<String, Heartbeat>) {
    if buf.is_empty() {
        return;
    }
    let n = buf.len();
    let res = async {
        let mut tx = pool.begin().await.context("begin heartbeat flush tx")?;
        for hb in buf.values() {
            upsert_baseline(&mut *tx, hb).await?;
        }
        tx.commit().await.context("commit heartbeat flush tx")
    }
    .await;
    if let Err(e) = res {
        warn!(error = %e, buffered = n, "heartbeat flush failed; dropping window");
    }
    buf.clear();
}

async fn upsert_baseline<'e, E>(executor: E, hb: &Heartbeat) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
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
    // #582 Phase 2: store the agent's boot-sentinel quarantine list as
    // a JSON array (NULL when empty). Replaced (not COALESCE'd) like
    // the perf fields — it's a live signal: a version leaving quarantine
    // must clear from the row.
    let quarantined_json = if hb.quarantined_versions.is_empty() {
        None
    } else {
        // Serialising a `Vec<String>` is infallible; a previous `"[]"`
        // fallback would have silently CLEARED the agent's quarantine
        // from the DB (REPLACE semantics) on an impossible error.
        Some(
            serde_json::to_string(&hb.quarantined_versions)
                .expect("serialising Vec<String> is infallible"),
        )
    };
    sqlx::query(
        "INSERT INTO agents (
             pc_id, hostname, os_family, agent_version,
             last_heartbeat,
             agent_cpu_pct, agent_rss_bytes,
             agent_disk_read_bytes, agent_disk_written_bytes,
             quarantined_versions,
             updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(pc_id) DO UPDATE SET
             hostname                  = COALESCE(agents.hostname, excluded.hostname),
             os_family                 = COALESCE(agents.os_family, excluded.os_family),
             agent_version             = excluded.agent_version,
             last_heartbeat            = excluded.last_heartbeat,
             agent_cpu_pct             = excluded.agent_cpu_pct,
             agent_rss_bytes           = excluded.agent_rss_bytes,
             agent_disk_read_bytes     = excluded.agent_disk_read_bytes,
             agent_disk_written_bytes  = excluded.agent_disk_written_bytes,
             quarantined_versions      = excluded.quarantined_versions,
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
    .bind(quarantined_json)
    .execute(executor)
    .await?;
    Ok(())
}
