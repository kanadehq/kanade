//! `agents.<pc_id>.ping` request/reply handler.
//!
//! Operator clicks the SPA Agents page "ping" button → backend issues
//! a NATS request on this subject → the agent answers immediately
//! with a fresh [`Heartbeat`] payload. Round-trip latency is a few
//! milliseconds on a healthy agent vs the previous passive-wait
//! design which slept up to ~30 s for the next periodic heartbeat to
//! land.
//!
//! Lives in its own task next to (not inside) `heartbeat_loop` so
//! the operator's prod is independent of the scheduled tick. Old
//! agents (pre-#133) don't subscribe, so requests time out — the
//! backend surfaces that as the same "offline" 408 it already
//! returned when a passive heartbeat-wait expired. No coordinated
//! upgrade needed.
//!
//! Self-perf fields (`agent_cpu_pct` / `agent_rss_bytes` / …) are
//! left `None` in the ping reply. They're a periodic-cadence signal
//! and require the long-lived `sysinfo::System` owned by the
//! heartbeat loop to be meaningful; an on-demand sample taken from
//! a cold `System` would always be misleading-but-non-null. The
//! periodic heartbeat keeps populating those fields at the regular
//! 30 s cadence.

use chrono::Utc;
use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::Heartbeat;
use tracing::{info, warn};

/// Subscribe to `agents.<pc_id>.ping` and reply with a fresh
/// [`Heartbeat`] for every incoming request.
pub async fn serve(
    client: async_nats::Client,
    pc_id: String,
    agent_version: String,
    hostname: Option<String>,
    os_family: Option<String>,
    tracker: crate::staleness::Tracker,
) {
    let subj = subject::ping(&pc_id);

    // Outer reconnect loop: pre-fix `match client.subscribe ... Err
    // => return;` killed the ping responder permanently when the
    // first subscribe failed (e.g. broker still booting). Now we
    // back off + retry, and if the subscription ever closes (broker
    // restart, server-side cleanup) we reopen.
    loop {
        let mut sub =
            crate::nats_retry::wait_for_subscribe(&client, &tracker, subj.clone(), "ping").await;
        info!(subject = %subj, "ping responder ready");

        while let Some(msg) = sub.next().await {
            let Some(reply) = msg.reply.clone() else {
                // Pure publishes hit this subject only if the operator
                // typoed an `nats pub` — log + ignore.
                warn!(subject = %subj, "ping without reply subject — skipping");
                continue;
            };
            let hb = Heartbeat {
                pc_id: pc_id.clone(),
                at: Utc::now(),
                agent_version: agent_version.clone(),
                hostname: hostname.clone(),
                os_family: os_family.clone(),
                // See module docs: perf fields stay None on ping replies.
                agent_cpu_pct: None,
                agent_rss_bytes: None,
                agent_disk_read_bytes: None,
                agent_disk_written_bytes: None,
            };
            let payload = match serde_json::to_vec(&hb) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "ping: serialize Heartbeat");
                    continue;
                }
            };
            if let Err(e) = client.publish(reply, payload.into()).await {
                warn!(error = %e, "publish ping reply");
            }
        }
        warn!(subject = %subj, "ping subscription closed; reopening");
        crate::nats_retry::reopen_pause().await;
    }
}
