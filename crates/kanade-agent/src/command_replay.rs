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

use crate::commands::{CommandSource, DedupCache, handle_command};
use crate::nats_retry;
use crate::script_cache::ScriptCache;

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

/// The server-side subject filters this agent's replay consumer
/// should carry: `commands.all`, its own `commands.pc.<id>`, and one
/// `commands.group.<g>` per current group membership (#483).
///
/// Narrow filters are load-bearing at fleet scale: a `commands.>`
/// filter would deliver every other PC's Commands to every agent
/// (O(N²) broker fan-out — 3,000 per-PC Commands × 3,000 consumers),
/// and — worse — would deliver *other groups'* Commands, which the
/// old client-side `is_for_me` then accepted and executed because a
/// non-member has no live group sub priming the dedup cache.
fn filter_subjects(pc_id: &str, groups: &[String]) -> Vec<String> {
    // Group subjects are sorted + deduped so the list is a
    // deterministic function of the membership *set* — equal sets
    // compare equal regardless of KV ordering, which both keeps the
    // unchanged-membership comparison below honest and avoids
    // pointless create-or-update churn on the durable.
    let mut group_subjects: Vec<String> = groups
        .iter()
        .map(|g| kanade_shared::subject::commands_group(g))
        .collect();
    group_subjects.sort();
    group_subjects.dedup();

    let mut subjects = Vec::with_capacity(2 + group_subjects.len());
    subjects.push(kanade_shared::subject::COMMANDS_ALL.to_string());
    subjects.push(kanade_shared::subject::commands_pc(pc_id));
    subjects.extend(group_subjects);
    subjects
}

// One argument over the limit since #1165 added the verifier. Same
// handling as the other long plumbing signatures in this crate (10+
// existing allows): these thread the agent's shared state to a task, and
// bundling them is a refactor of unrelated code, not part of a security
// change.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    dedup: Arc<Mutex<DedupCache>>,
    staleness: crate::staleness::Tracker,
    script_cache: ScriptCache,
    check_sink: crate::check_cache::CheckSink,
    groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    verifier: std::sync::Arc<crate::command_verify::Verifier>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(
            client,
            pc_id,
            dedup,
            staleness,
            script_cache,
            check_sink,
            groups_rx,
            verifier,
        )
        .await;
    })
}

// One argument over the limit since #1165 added the verifier. Same
// handling as the other long plumbing signatures in this crate (10+
// existing allows): these thread the agent's shared state to a task, and
// bundling them is a refactor of unrelated code, not part of a security
// change.
#[allow(clippy::too_many_arguments)]
async fn run(
    client: async_nats::Client,
    pc_id: String,
    dedup: Arc<Mutex<DedupCache>>,
    staleness: crate::staleness::Tracker,
    script_cache: ScriptCache,
    check_sink: crate::check_cache::CheckSink,
    mut groups_rx: tokio::sync::watch::Receiver<Vec<String>>,
    verifier: std::sync::Arc<crate::command_verify::Verifier>,
) {
    let jetstream = async_nats::jetstream::new(client.clone());
    let name = consumer_name(&pc_id);
    // Set false once the groups task drops its sender so the select
    // arm stops polling a closed channel (a closed `changed()`
    // resolves immediately and would busy-loop the select).
    // Deliberately declared OUTSIDE the outer loop: a dropped sender
    // is permanent (the groups task never restarts within a process),
    // so the flag must survive consumer-reopen iterations.
    let mut groups_watch_alive = true;

    loop {
        // Snapshot membership BEFORE creating the consumer and mark
        // it seen — `changed()` then wakes only on a *later* flip,
        // so a flip racing the consumer setup still triggers a
        // recreate rather than being lost.
        let groups: Vec<String> = groups_rx.borrow_and_update().clone();
        let current_filters = filter_subjects(&pc_id, &groups);

        let stream = nats_retry::wait_for_stream(
            &jetstream,
            &client,
            &staleness,
            STREAM_EXEC,
            "command_replay",
        )
        .await;
        let consumer = nats_retry::wait_for_consumer_updating(
            &stream,
            &client,
            &staleness,
            // Stable per-agent consumer name so JetStream resumes
            // at the previous ack position across agent / broker
            // restarts. `wait_for_consumer_updating` (create-or-
            // update, not get-or-create) applies the current
            // filter_subjects to an existing durable — required
            // both for membership flips and for the one-time
            // upgrade away from the old `commands.>` filter.
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
                // #483: server-side narrowing to exactly the
                // subjects this agent is addressed by. Membership
                // flips break the message loop below and re-enter
                // this setup with the fresh list.
                filter_subjects: current_filters.clone(),
                ..Default::default()
            },
        )
        .await;
        info!(
            stream = STREAM_EXEC,
            consumer = %name,
            pc_id = %pc_id,
            groups = ?groups,
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
        // Drain messages until either the stream ends (reopen with a
        // pause) or group membership changes (re-enter setup
        // immediately so the durable's filter_subjects follow the
        // membership).
        let mut membership_changed = false;
        loop {
            let msg = tokio::select! {
                changed = groups_rx.changed(), if groups_watch_alive => {
                    match changed {
                        Ok(()) => {
                            // Only recreate when the *resolved*
                            // filters actually differ — a watch tick
                            // that doesn't change the set (same
                            // membership re-published, ordering
                            // shuffle) shouldn't tear down a healthy
                            // consumer.
                            let next =
                                filter_subjects(&pc_id, &groups_rx.borrow_and_update());
                            if next == current_filters {
                                debug!(
                                    consumer = %name,
                                    "group watch tick without effective membership change; keeping consumer",
                                );
                                continue;
                            }
                            info!(
                                consumer = %name,
                                "group membership changed; recreating replay consumer with updated filters",
                            );
                            membership_changed = true;
                            break;
                        }
                        Err(_) => {
                            // groups task is gone (agent shutdown in
                            // practice); stop polling the closed
                            // channel and keep draining messages.
                            groups_watch_alive = false;
                            continue;
                        }
                    }
                }
                maybe_msg = messages.next() => match maybe_msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        warn!(error = %e, "replay consumer error");
                        continue;
                    }
                    None => break,
                },
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
            // Addressed-to-me runs BEFORE provenance, and the order became
            // load-bearing when refusals started producing an `ExecResult`
            // (#1165 stage 3). The race this check exists for — a group
            // command in flight while a membership removal propagates — would
            // otherwise have this host publish a REFUSED result under its own
            // pc_id for a command it was never responsible for, and report a
            // signing-state transition it has no business reporting.
            //
            // Verifying first bought nothing anyway: a command not addressed
            // here is dropped whatever its signature says.
            if !is_for_me(&msg.subject, &pc_id, &groups) {
                // warn, not debug: with server-side filter_subjects
                // narrowing delivery, anything landing here is the
                // rare race window (group command in flight while a
                // membership removal propagates) — and the early ack
                // above has already consumed it permanently, so the
                // drop should be operator-visible (PR #540 review).
                warn!(
                    subject = %msg.subject,
                    "replay msg not addressed to this agent; dropping (already acked)",
                );
                continue;
            }

            // #1165. This path matters more than the live one: the bypass
            // #1155 measured *is* a JetStream consumer, so a verifier covering
            // only `command_loop` would leave the attack it exists to stop
            // running through the other door. That applies to the refusal
            // below exactly as it applied to the observation.
            let outcome = verifier.observe(
                &msg.payload,
                &crate::command_verify::headers_of(&msg),
                &cmd.request_id,
            );
            if let Some(reason) = verifier.refusal(outcome) {
                warn!(
                    request_id = %cmd.request_id,
                    subject = %msg.subject,
                    reason,
                    "REFUSED: replayed command did not verify",
                );
                crate::commands::publish_signature_refused(&pc_id, &cmd, reason);
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
            let sc = script_cache.clone();
            let cs = check_sink.clone();
            info!(
                cmd_id = %cmd.id,
                request_id = %cmd.request_id,
                subject = %msg.subject,
                "replay: handling missed command",
            );
            tokio::spawn(async move {
                if let Err(e) = handle_command(
                    client_for_task,
                    pc_for_task,
                    cmd,
                    cur,
                    sta,
                    stl,
                    sc,
                    cs,
                    CommandSource::Nats,
                )
                .await
                {
                    error!(error = %e, "replay command handler failed");
                }
            });
        }
        if membership_changed {
            // Re-enter setup immediately — the broker is healthy;
            // we only need new filter_subjects on the durable.
            continue;
        }
        warn!(consumer = %name, "command-replay messages stream ended; reopening");
        nats_retry::reopen_pause().await;
    }
}

/// True when `subject` addresses this agent — `commands.all` always
/// applies, `commands.pc.<my-id>` matches our pc_id, and
/// `commands.group.<g>` matches only when this agent currently
/// belongs to `g`. Conservative: when the subject doesn't fit any
/// known pattern, drop it.
///
/// The server-side `filter_subjects` already narrows delivery to
/// exactly these subjects, so this is a belt-and-braces guard for
/// the windows where the consumer's filters lag reality: a
/// membership *removal* that races messages already in flight, or a
/// pre-#483 durable that briefly still carries the old `commands.>`
/// filter until the first create-or-update applies. The old
/// accept-all-groups behaviour executed other groups' Commands on
/// every non-member (#483) — the dedup cache never saw them because
/// non-members have no live group sub.
fn is_for_me(subject: &str, my_pc_id: &str, my_groups: &[String]) -> bool {
    if subject == kanade_shared::subject::COMMANDS_ALL {
        return true;
    }
    if let Some(pc) = subject.strip_prefix("commands.pc.") {
        return pc == my_pc_id;
    }
    if let Some(group) = subject.strip_prefix("commands.group.") {
        return my_groups.iter().any(|g| g == group);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands_all_matches_anyone() {
        assert!(is_for_me("commands.all", "pc-01", &[]));
        assert!(is_for_me("commands.all", "anything", &groups(&["g1"])));
    }

    #[test]
    fn commands_pc_matches_only_owner() {
        assert!(is_for_me("commands.pc.pc-01", "pc-01", &[]));
        assert!(!is_for_me("commands.pc.pc-02", "pc-01", &[]));
    }

    #[test]
    fn commands_group_matches_only_members() {
        // #483: a non-member must NOT execute another group's
        // replayed Command — the old accept-all behaviour ran canary
        // scripts fleet-wide after a reboot wave.
        let mine = groups(&["canary", "wave1"]);
        assert!(is_for_me("commands.group.canary", "pc-01", &mine));
        assert!(is_for_me("commands.group.wave1", "pc-01", &mine));
        assert!(!is_for_me("commands.group.prod", "pc-01", &mine));
        assert!(!is_for_me("commands.group.canary", "pc-01", &[]));
        // Prefix of a real membership is still a different group.
        assert!(!is_for_me("commands.group.can", "pc-01", &mine));
    }

    #[test]
    fn unknown_subject_dropped() {
        assert!(!is_for_me("commands.weird", "pc-01", &[]));
        assert!(!is_for_me("results.x", "pc-01", &[]));
    }

    #[test]
    fn filter_subjects_cover_all_pc_and_groups() {
        assert_eq!(
            filter_subjects("pc-01", &groups(&["canary", "wave1"])),
            vec![
                "commands.all".to_string(),
                "commands.pc.pc-01".to_string(),
                "commands.group.canary".to_string(),
                "commands.group.wave1".to_string(),
            ],
        );
        assert_eq!(
            filter_subjects("pc-01", &[]),
            vec!["commands.all".to_string(), "commands.pc.pc-01".to_string()],
        );
    }

    #[test]
    fn consumer_name_sanitises_pc_id() {
        assert_eq!(consumer_name("PC-01"), "agent_replay_PC-01");
        assert_eq!(consumer_name("PC.001"), "agent_replay_PC_001");
        assert_eq!(
            consumer_name("host with space"),
            "agent_replay_host_with_space"
        );
    }
}
