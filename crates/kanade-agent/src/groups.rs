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
use kanade_shared::kv::{BUCKET_AGENT_GROUPS, BUCKET_AGENT_GROUPS_DERIVED};
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
    verifier: std::sync::Arc<crate::command_verify::Verifier>,
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
            verifier,
        )
        .await;
    });
    (rx, handle)
}

// One argument over the limit since #1165 added the verifier. Same
// handling as the other long plumbing signatures in this crate (10+
// existing allows): these thread the agent's shared state to a task, and
// bundling them is a refactor of unrelated code, not part of a security
// change.
#[allow(clippy::too_many_arguments)]
async fn manage(
    client: async_nats::Client,
    pc_id: String,
    dedup: std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: crate::staleness::Tracker,
    script_cache: crate::script_cache::ScriptCache,
    check_sink: crate::check_cache::CheckSink,
    groups_tx: tokio::sync::watch::Sender<Vec<String>>,
    verifier: std::sync::Arc<crate::command_verify::Verifier>,
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
        // #1032①: the agent's effective membership is the UNION of two
        // per-PC buckets — the operator-owned `agent_groups` (manual
        // `kanade group add`) and the materializer-owned
        // `agent_groups_derived` (declared/dynamic GroupDefs). A group
        // reaches this agent via either. wait_for_kv retries until each
        // bucket is available.
        let manual_kv = nats_retry::wait_for_kv(
            &js,
            &client,
            &staleness,
            BUCKET_AGENT_GROUPS,
            "agent_groups",
        )
        .await;
        let derived_kv = nats_retry::wait_for_kv(
            &js,
            &client,
            &staleness,
            BUCKET_AGENT_GROUPS_DERIVED,
            "agent_groups_derived",
        )
        .await;

        // Re-prime both on every (re)connect: pick up edits that landed
        // while we were disconnected.
        //
        // Gemini #147 fix: a transient `kv.get` error must NOT fall
        // through with an empty set, which would diff against
        // `current=subs.keys()` and abort every per-group SUB the
        // agent is running. Pause + reopen instead so the membership
        // stays intact until the next read succeeds.
        let Some(mut manual) = prime(&manual_kv, &pc_id, "agent_groups").await else {
            nats_retry::reopen_pause().await;
            continue;
        };
        let Some(mut derived) = prime(&derived_kv, &pc_id, "agent_groups_derived").await else {
            nats_retry::reopen_pause().await;
            continue;
        };

        reconcile_and_publish(
            &mut subs,
            &manual,
            &derived,
            &client,
            &pc_id,
            &dedup,
            &staleness,
            &script_cache,
            &check_sink,
            &groups_tx,
            &verifier,
            "prime",
        )
        .await;

        let mut manual_watch = match manual_kv.watch(&pc_id).await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "watch agent_groups KV key failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        let mut derived_watch = match derived_kv.watch(&pc_id).await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "watch agent_groups_derived KV key failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };

        // Fold both watch streams. Either stream ending ⇒ break to reopen
        // BOTH (the reconnect path above re-primes, so no update is lost).
        // A per-entry error is logged and skipped without reconciling.
        loop {
            let changed = tokio::select! {
                entry = manual_watch.next() => match entry {
                    Some(Ok(e)) => { manual = decode_entry(&e); true }
                    Some(Err(e)) => { warn!(error = %e, "agent_groups watch entry"); false }
                    None => { warn!("agent_groups watch ended; reopening"); break; }
                },
                entry = derived_watch.next() => match entry {
                    Some(Ok(e)) => { derived = decode_entry(&e); true }
                    Some(Err(e)) => { warn!(error = %e, "agent_groups_derived watch entry"); false }
                    None => { warn!("agent_groups_derived watch ended; reopening"); break; }
                },
            };
            if changed {
                reconcile_and_publish(
                    &mut subs,
                    &manual,
                    &derived,
                    &client,
                    &pc_id,
                    &dedup,
                    &staleness,
                    &script_cache,
                    &check_sink,
                    &groups_tx,
                    &verifier,
                    "update",
                )
                .await;
            }
        }
        nats_retry::reopen_pause().await;
    }
}

/// Read + decode one PC's membership from a bucket. `None` signals a transient
/// error (caller should reopen — NOT treat as empty, which would abort live
/// SUBs, Gemini #147); an absent key resolves to an empty set.
async fn prime(kv: &jetstream::kv::Store, pc_id: &str, bucket: &str) -> Option<Vec<String>> {
    match kv.get(pc_id).await {
        Ok(Some(bytes)) => Some(parse_groups(&bytes)),
        Ok(None) => Some(Vec::new()),
        Err(e) => {
            warn!(error = %e, bucket, "initial membership KV read failed; pausing and reopening");
            None
        }
    }
}

/// Decode a KV watch entry into a membership list. A `Delete`/`Purge` (the
/// materializer clearing a PC that left every declared group) reads as an
/// empty set without a spurious "did not parse" warning.
fn decode_entry(entry: &jetstream::kv::Entry) -> Vec<String> {
    use async_nats::jetstream::kv::Operation;
    match entry.operation {
        Operation::Put => parse_groups(&entry.value),
        Operation::Delete | Operation::Purge => Vec::new(),
    }
}

/// The agent's effective group set: `manual ∪ derived`, sorted + deduped.
/// `diff_groups` also dedups, but publishing a clean set on `groups_tx` keeps
/// every downstream consumer (local_scheduler / notify_bus / command_replay)
/// on a canonical list.
pub fn union_groups(manual: &[String], derived: &[String]) -> Vec<String> {
    let mut v: Vec<String> = manual.iter().chain(derived).cloned().collect();
    v.sort();
    v.dedup();
    v
}

/// Compute the union, reconcile `commands.group.<name>` subscriptions to it,
/// and publish the effective set on `groups_tx` (which every other agent
/// consumer reads). Always publishes — consumers care about the value itself,
/// not whether the SUB set changed.
#[allow(clippy::too_many_arguments)]
async fn reconcile_and_publish(
    subs: &mut HashMap<String, JoinHandle<()>>,
    manual: &[String],
    derived: &[String],
    client: &async_nats::Client,
    pc_id: &str,
    dedup: &std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: &crate::staleness::Tracker,
    script_cache: &crate::script_cache::ScriptCache,
    check_sink: &crate::check_cache::CheckSink,
    groups_tx: &tokio::sync::watch::Sender<Vec<String>>,
    verifier: &std::sync::Arc<crate::command_verify::Verifier>,
    reason: &str,
) {
    let desired = union_groups(manual, derived);
    let current: Vec<String> = subs.keys().cloned().collect();
    let delta = diff_groups(&current, &desired);
    if !delta.is_empty() {
        info!(
            add = ?delta.to_subscribe,
            drop = ?delta.to_unsubscribe,
            reason,
            "reconciling group subscriptions (manual ∪ derived)",
        );
        apply_delta(
            &delta,
            subs,
            client,
            pc_id,
            dedup,
            staleness,
            script_cache,
            check_sink,
            verifier,
        )
        .await;
    }
    let _ = groups_tx.send(desired);
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
    verifier: &std::sync::Arc<crate::command_verify::Verifier>,
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
                    verifier.clone(),
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
    fn union_merges_dedups_and_sorts() {
        // manual ∪ derived, with an overlap — a group present in both appears
        // once, output is sorted.
        let manual = vec!["pilot-ring".to_string(), "clients".to_string()];
        let derived = vec!["clients".to_string(), "win-24h2".to_string()];
        assert_eq!(
            union_groups(&manual, &derived),
            vec!["clients", "pilot-ring", "win-24h2"]
        );
    }

    #[test]
    fn union_handles_empty_sides() {
        let manual = vec!["a".to_string()];
        assert_eq!(union_groups(&manual, &[]), vec!["a"]);
        assert_eq!(union_groups(&[], &manual), vec!["a"]);
        assert!(union_groups(&[], &[]).is_empty());
    }

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
