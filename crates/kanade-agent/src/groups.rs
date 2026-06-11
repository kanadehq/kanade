//! Dynamic group-subscription manager (Sprint 5).
//!
//! Replaces the static `agent.toml::[agent] groups` loop with a KV
//! watcher: this agent reads `agent_groups.{pc_id}` on startup, then
//! watches the same key. Every time the value flips we diff the
//! desired set against the currently-spawned subscriptions and
//! issue exactly the right add / drop operations.
//!
//! [`diff_groups`] is the pure functional core, kept separate so it's
//! unit-testable without a live NATS connection. [`manage`] is the
//! integration layer that owns the per-group subscribe tasks and
//! reacts to KV updates.

use std::collections::HashMap;

use async_nats::jetstream;
use futures::StreamExt;
use kanade_shared::kv::BUCKET_AGENT_GROUPS;
use kanade_shared::subject;
use kanade_shared::wire::AgentGroups;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::commands;
use crate::nats_retry;

/// Outcome of comparing the currently-subscribed groups against the
/// fleet manager's desired set: what to spawn and what to abort.
///
/// Both vectors are sorted ascending for deterministic logging.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubscriptionDelta {
    pub to_subscribe: Vec<String>,
    pub to_unsubscribe: Vec<String>,
}

impl SubscriptionDelta {
    pub fn is_empty(&self) -> bool {
        self.to_subscribe.is_empty() && self.to_unsubscribe.is_empty()
    }
}

/// Spawn the group-membership manager and hand back a watch channel
/// carrying the current membership list. Local consumers
/// (`local_scheduler`) can subscribe to it to re-reconcile their own
/// state when the agent's groups change.
///
/// The spawned task is offline-tolerant (v0.38 / #137): if the broker
/// is unreachable at boot it backs off and retries via
/// [`nats_retry::wait_for_kv`]; if the watch ends because of a
/// disconnect, the wrapper restarts it. Per-group SUB subscriptions
/// outlive the reconnect cycle — async-nats re-issues them
/// internally — so the `subs` map is held across iterations to avoid
/// double-subscribe on every reopen.
pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    dedup: std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: crate::staleness::Tracker,
    script_cache: crate::script_cache::ScriptCache,
    check_sink: crate::check_cache::CheckSink,
) -> (
    tokio::sync::watch::Receiver<Vec<String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::watch::channel(Vec::<String>::new());
    let handle = tokio::spawn(async move {
        manage(
            client,
            pc_id,
            dedup,
            staleness,
            script_cache,
            check_sink,
            tx,
        )
        .await;
    });
    (rx, handle)
}

async fn manage(
    client: async_nats::Client,
    pc_id: String,
    dedup: std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: crate::staleness::Tracker,
    script_cache: crate::script_cache::ScriptCache,
    check_sink: crate::check_cache::CheckSink,
    groups_tx: tokio::sync::watch::Sender<Vec<String>>,
) {
    let js = jetstream::new(client.clone());

    // Persist across reconnects. Each per-group `command_loop` task
    // backs onto a NATS Subscriber that async-nats reconnects
    // automatically; aborting + respawning them on every broker
    // drop would defeat that. Stale handles (e.g. a command_loop
    // that exited because its Subscriber closed) just sit in the
    // map until the agent restarts — acceptable tradeoff since
    // diff_groups will simply skip re-subscribing for any group
    // still in `current ∩ desired`.
    let mut subs: HashMap<String, JoinHandle<()>> = HashMap::new();

    loop {
        let kv = nats_retry::wait_for_kv(
            &js,
            &client,
            &staleness,
            BUCKET_AGENT_GROUPS,
            "agent_groups",
        )
        .await;

        // Re-prime on every (re)connect: pick up edits that landed
        // while we were disconnected.
        //
        // Gemini #147 fix: a transient `kv.get` error must NOT fall
        // through with `desired = []`, which would diff against
        // `current=subs.keys()` and abort every per-group SUB the
        // agent is running. Pause + reopen instead so the membership
        // stays intact until the next read succeeds.
        let desired = match kv.get(&pc_id).await {
            Ok(Some(bytes)) => parse_groups(&bytes),
            Ok(None) => {
                info!(pc_id = %pc_id, "no agent_groups entry — starting with empty membership");
                Vec::new()
            }
            Err(e) => {
                warn!(error = %e, "initial agent_groups KV read failed; pausing and reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        let current: Vec<String> = subs.keys().cloned().collect();
        let delta = diff_groups(&current, &desired);
        if !delta.is_empty() {
            info!(
                add = ?delta.to_subscribe,
                drop = ?delta.to_unsubscribe,
                "agent_groups (re-)prime — reconciling subscriptions",
            );
            apply_delta(
                &delta,
                &mut subs,
                &client,
                &pc_id,
                &dedup,
                &staleness,
                &script_cache,
                &check_sink,
            )
            .await;
        }
        let _ = groups_tx.send(desired);

        let mut watch = match kv.watch(&pc_id).await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "watch agent_groups KV key failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        while let Some(entry) = watch.next().await {
            let bytes = match entry {
                Ok(e) => e.value,
                Err(e) => {
                    warn!(error = %e, "agent_groups watch entry");
                    continue;
                }
            };
            let desired = parse_groups(&bytes);
            let current: Vec<String> = subs.keys().cloned().collect();
            let delta = diff_groups(&current, &desired);
            if !delta.is_empty() {
                info!(
                    add = ?delta.to_subscribe,
                    drop = ?delta.to_unsubscribe,
                    "agent_groups update — reconciling subscriptions",
                );
                apply_delta(
                    &delta,
                    &mut subs,
                    &client,
                    &pc_id,
                    &dedup,
                    &staleness,
                    &script_cache,
                    &check_sink,
                )
                .await;
            }
            // Always publish — local_scheduler cares about the
            // membership value itself, not whether *core sub*
            // subscriptions changed (they can be a no-op when
            // membership stayed the same modulo ordering).
            let _ = groups_tx.send(desired);
        }
        warn!("agent_groups watch ended; reopening");
        nats_retry::reopen_pause().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_delta(
    delta: &SubscriptionDelta,
    subs: &mut HashMap<String, JoinHandle<()>>,
    client: &async_nats::Client,
    pc_id: &str,
    dedup: &std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: &crate::staleness::Tracker,
    script_cache: &crate::script_cache::ScriptCache,
    check_sink: &crate::check_cache::CheckSink,
) {
    for g in &delta.to_unsubscribe {
        if let Some(handle) = subs.remove(g) {
            handle.abort();
            info!(group = %g, "unsubscribed from group");
        }
    }
    for g in &delta.to_subscribe {
        match client.subscribe(subject::commands_group(g)).await {
            Ok(sub) => {
                // Match the existing pattern in main.rs: flush right
                // after subscribe so the server-side SUB is registered
                // before any publisher sees us as a member, closing the
                // race window documented in
                // reference_async_nats_subscribe_race.
                let _ = client.flush().await;
                let handle = tokio::spawn(commands::command_loop(
                    client.clone(),
                    pc_id.to_string(),
                    dedup.clone(),
                    staleness.clone(),
                    sub,
                    script_cache.clone(),
                    check_sink.clone(),
                ));
                subs.insert(g.clone(), handle);
                info!(group = %g, "subscribed to group");
            }
            Err(e) => warn!(error = %e, group = %g, "subscribe to group failed"),
        }
    }
}

/// Deserialize an `agent_groups.{pc_id}` KV value into the membership
/// list, treating a malformed blob as empty (logged). Shared with the
/// KLP `maintenance.list` handler so the preview resolves group
/// targeting exactly the way the live subscriber does.
pub(crate) fn parse_groups(bytes: &[u8]) -> Vec<String> {
    match serde_json::from_slice::<AgentGroups>(bytes) {
        Ok(g) => g.groups,
        Err(e) => {
            warn!(
                error = %e,
                bytes = bytes.len(),
                "agent_groups value did not parse as AgentGroups JSON; treating as empty"
            );
            Vec::new()
        }
    }
}

/// Pure diff: which groups must this agent newly subscribe to
/// (`to_subscribe`) and which existing subscriptions must it drop
/// (`to_unsubscribe`) so the live set matches `desired`?
///
/// Inputs need not be sorted or deduped; outputs always are.
pub fn diff_groups<S: AsRef<str>, T: AsRef<str>>(
    current: &[S],
    desired: &[T],
) -> SubscriptionDelta {
    use std::collections::BTreeSet;
    let current_set: BTreeSet<&str> = current.iter().map(AsRef::as_ref).collect();
    let desired_set: BTreeSet<&str> = desired.iter().map(AsRef::as_ref).collect();

    SubscriptionDelta {
        to_subscribe: desired_set
            .difference(&current_set)
            .map(|s| (*s).to_string())
            .collect(),
        to_unsubscribe: current_set
            .difference(&desired_set)
            .map(|s| (*s).to_string())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_change_when_sets_match() {
        let d = diff_groups::<&str, &str>(&["wave1", "canary"], &["canary", "wave1"]);
        assert_eq!(d, SubscriptionDelta::default());
        assert!(d.is_empty());
    }

    #[test]
    fn no_change_on_both_empty() {
        let d: SubscriptionDelta = diff_groups::<&str, &str>(&[], &[]);
        assert!(d.is_empty());
    }

    #[test]
    fn initial_subscribe_when_current_empty() {
        let d = diff_groups::<&str, &str>(&[], &["wave1", "canary"]);
        // Sorted ascending.
        assert_eq!(d.to_subscribe, vec!["canary", "wave1"]);
        assert!(d.to_unsubscribe.is_empty());
    }

    #[test]
    fn drop_all_when_desired_empty() {
        let d = diff_groups::<&str, &str>(&["wave1", "canary"], &[]);
        assert!(d.to_subscribe.is_empty());
        assert_eq!(d.to_unsubscribe, vec!["canary", "wave1"]);
    }

    #[test]
    fn add_one_keep_rest() {
        let d = diff_groups::<&str, &str>(&["canary"], &["canary", "wave1"]);
        assert_eq!(d.to_subscribe, vec!["wave1"]);
        assert!(d.to_unsubscribe.is_empty());
    }

    #[test]
    fn drop_one_keep_rest() {
        let d = diff_groups::<&str, &str>(&["canary", "wave1"], &["canary"]);
        assert!(d.to_subscribe.is_empty());
        assert_eq!(d.to_unsubscribe, vec!["wave1"]);
    }

    #[test]
    fn full_swap() {
        let d = diff_groups::<&str, &str>(&["wave1", "wave2"], &["dept-eng", "canary"]);
        assert_eq!(d.to_subscribe, vec!["canary", "dept-eng"]);
        assert_eq!(d.to_unsubscribe, vec!["wave1", "wave2"]);
    }

    #[test]
    fn dedups_inputs() {
        // Caller-side bugs (e.g. an AgentGroups snapshot that briefly
        // contains a duplicate before normalisation) should not cause
        // double-subscribe attempts.
        let d = diff_groups::<&str, &str>(&["canary", "canary"], &["canary", "canary", "wave1"]);
        assert_eq!(d.to_subscribe, vec!["wave1"]);
        assert!(d.to_unsubscribe.is_empty());
    }

    #[test]
    fn output_is_sorted_regardless_of_input_order() {
        let d = diff_groups::<&str, &str>(&[], &["zeta", "alpha", "mu"]);
        assert_eq!(d.to_subscribe, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn accepts_string_and_str_inputs() {
        // Generic over AsRef<str>, so the caller can pass either &[String]
        // (read from KV) or &[&str] (literal in tests) without copying.
        let current: Vec<String> = vec!["wave1".into()];
        let desired: Vec<&str> = vec!["wave1", "canary"];
        let d = diff_groups(&current, &desired);
        assert_eq!(d.to_subscribe, vec!["canary"]);
    }
}
