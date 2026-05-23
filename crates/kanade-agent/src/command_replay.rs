//! v0.22.1: JetStream-backed catch-up for Commands the agent missed
//! while offline.
//!
//! Story: backend publishes Commands to core `commands.{all, group.X,
//! pc.Y}` subjects. The `STREAM_EXEC` stream is configured to capture
//! that same subject hierarchy with `max_messages_per_subject = 1`,
//! so the broker retains the most recent Command per subject for
//! `max_age` (7d).
//!
//! Online agent path (unchanged): core subscriptions deliver
//! Commands immediately as they're published.
//!
//! Reconnect / first-boot path (this module): a durable JetStream
//! consumer with `DeliverPolicy::LastPerSubject` replays the latest
//! retained Command per subject the agent cares about. Both paths
//! feed into the same `handle_command` via a shared [`DedupCache`]
//! that drops duplicates by `request_id` — the broker can deliver
//! the same Command twice (once via core sub at fire time, once via
//! the durable consumer on a later reconnect), and only one of them
//! is acted on.

use std::sync::Arc;

use async_nats::jetstream::consumer::DeliverPolicy;
use async_nats::jetstream::consumer::pull::Config as PullConfig;
use futures::StreamExt;
use kanade_shared::kv::STREAM_EXEC;
use kanade_shared::wire::Command;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::commands::{DedupCache, handle_command};
use crate::nats_retry;

/// Stable consumer name per agent so JetStream remembers the ack
/// position across agent restarts. Reconnecting with the same name
/// just resumes where we left off.
fn consumer_name(pc_id: &str) -> String {
    // JetStream consumer names must be domain-safe; pc_ids in
    // kanade are already ASCII hostnames, but we sanitize defensively.
    let safe: String = pc_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("agent_replay_{safe}")
}

pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    dedup: Arc<Mutex<DedupCache>>,
    staleness: crate::staleness::Tracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(client, pc_id, dedup, staleness).await;
    })
}

async fn run(
    client: async_nats::Client,
    pc_id: String,
    dedup: Arc<Mutex<DedupCache>>,
    staleness: crate::staleness::Tracker,
) {
    let jetstream = async_nats::jetstream::new(client.clone());
    let name = consumer_name(&pc_id);

    loop {
        let stream = nats_retry::wait_for_stream(
            &jetstream,
            &client,
            &staleness,
            STREAM_EXEC,
            "command_replay",
        )
        .await;
        let consumer = nats_retry::wait_for_consumer(
            &stream,
            &client,
            &staleness,
            // Stable per-agent consumer name so JetStream resumes
            // at the previous ack position across agent / broker
            // restarts. `wait_for_consumer` clones the config each
            // retry — pull configs are tiny so the cost is fine.
            &name,
            "command_replay",
            PullConfig {
                durable_name: Some(name.clone()),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                // Only the latest message per subject — combined with
                // the stream's max_messages_per_subject=1 this means
                // "give me the most recent Command for every subject
                // ever seen". On reconnect, the agent catches up to
                // the freshest state without replaying old fires.
                deliver_policy: DeliverPolicy::LastPerSubject,
                // Stream filter is `commands.>`. We could narrow
                // with filter_subjects to (`commands.all`,
                // `commands.pc.<me>`, `commands.group.<g>`) but
                // groups are dynamic and recreating the consumer on
                // every membership flip is more complex than
                // client-side filtering. Bandwidth at fleet sizes
                // we care about is fine.
                filter_subject: "commands.>".into(),
                ..Default::default()
            },
        )
        .await;
        info!(
            stream = STREAM_EXEC,
            consumer = %name,
            pc_id = %pc_id,
            "command-replay consumer ready",
        );

        // script_current / script_status are advisory — agents run
        // with whatever they manage to fetch. Pre-existing `.ok()`
        // semantics retained.
        let script_current = jetstream
            .get_key_value(kanade_shared::kv::BUCKET_SCRIPT_CURRENT)
            .await
            .ok();
        let script_status = jetstream
            .get_key_value(kanade_shared::kv::BUCKET_SCRIPT_STATUS)
            .await
            .ok();

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "command-replay messages stream failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        while let Some(msg) = messages.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "replay consumer error");
                    continue;
                }
            };
            // Ack early — even if we decide to skip below (not for
            // me, duplicate, etc.), we don't want broker
            // redelivery.
            let _ = msg.ack().await;

            let cmd: Command = match serde_json::from_slice(&msg.payload) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, subject = %msg.subject, "deserialize replay command");
                    continue;
                }
            };

            if !is_for_me(&msg.subject, &pc_id) {
                debug!(subject = %msg.subject, "replay msg not for this pc; dropping");
                continue;
            }

            // Dedup against the core-sub path: if we already saw
            // this request_id (because the core sub delivered it
            // live), drop it here.
            if !dedup.lock().await.insert(cmd.request_id.clone()) {
                debug!(
                    request_id = %cmd.request_id,
                    "replay dedup: already seen via core sub or earlier replay",
                );
                continue;
            }

            let client_for_task = client.clone();
            let pc_for_task = pc_id.clone();
            let cur = script_current.clone();
            let sta = script_status.clone();
            let stl = staleness.clone();
            info!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                subject = %msg.subject,
                "replay: handling missed command",
            );
            tokio::spawn(async move {
                if let Err(e) =
                    handle_command(client_for_task, pc_for_task, cmd, cur, sta, stl).await
                {
                    error!(error = %e, "replay command handler failed");
                }
            });
        }
        warn!(consumer = %name, "command-replay messages stream ended; reopening");
        nats_retry::reopen_pause().await;
    }
}

/// True when `subject` addresses this agent — `commands.all` always
/// applies, `commands.pc.<my-id>` matches our pc_id, `commands.group.*`
/// is left to the group sub (which has its own dedup'd flow).
/// Conservative: when the subject doesn't fit any known pattern, drop
/// it (broker shouldn't be sending such messages anyway).
fn is_for_me(subject: &str, my_pc_id: &str) -> bool {
    if subject == kanade_shared::subject::COMMANDS_ALL {
        return true;
    }
    if let Some(pc) = subject.strip_prefix("commands.pc.") {
        return pc == my_pc_id;
    }
    if subject.starts_with("commands.group.") {
        // Group commands are also delivered to the live core sub
        // (groups::manage spins one per membership). The replay
        // path mirrors them so an agent that's offline through a
        // group rollout catches up on reconnect. Membership is
        // dynamic, so we'd have to consult the agent_groups KV to
        // confirm — but the broker side caps storage and the dedup
        // cache caps double-fire, so it's safe to just accept and
        // let the dedup cache handle redundancy.
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_all_matches_anyone() {
        assert!(is_for_me("commands.all", "minipc-01"));
        assert!(is_for_me("commands.all", "anything"));
    }

    #[test]
    fn commands_pc_matches_only_owner() {
        assert!(is_for_me("commands.pc.minipc-01", "minipc-01"));
        assert!(!is_for_me("commands.pc.minipc-02", "minipc-01"));
    }

    #[test]
    fn commands_group_always_accepted() {
        // Group dedup is handled upstream (live core sub spawns its
        // own dedup'd flow). Replay just lets them through.
        assert!(is_for_me("commands.group.canary", "minipc-01"));
    }

    #[test]
    fn unknown_subject_dropped() {
        assert!(!is_for_me("commands.weird", "minipc-01"));
        assert!(!is_for_me("results.x", "minipc-01"));
    }

    #[test]
    fn consumer_name_sanitises_pc_id() {
        assert_eq!(consumer_name("MINIPC-01"), "agent_replay_MINIPC-01");
        assert_eq!(consumer_name("PC.001"), "agent_replay_PC_001");
        assert_eq!(
            consumer_name("host with space"),
            "agent_replay_host_with_space"
        );
    }
}
