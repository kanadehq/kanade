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

use anyhow::{Context, Result};
use async_nats::jetstream;
use futures::StreamExt;
use kanade_shared::kv::BUCKET_AGENT_GROUPS;
use kanade_shared::subject;
use kanade_shared::wire::AgentGroups;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::commands;

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

/// Top-level group-membership manager. Run as a spawned task; exits
/// if either the KV bucket is missing (broker hasn't been
/// `kanade jetstream bootstrap`-ed yet) or the watch stream errors
/// terminally. Per-group subscribe tasks are aborted on removal so
/// agents that lose membership stop processing further commands
/// for that group within a NATS heartbeat.
/// Spawn the group-membership manager + return a watch channel
/// carrying the current membership list. Local consumers
/// (`local_scheduler`) can subscribe to it to re-reconcile their
/// own state when the agent's groups change.
pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    dedup: std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: crate::staleness::Tracker,
) -> (
    tokio::sync::watch::Receiver<Vec<String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::watch::channel(Vec::<String>::new());
    let handle = tokio::spawn(async move {
        if let Err(e) = manage(client, pc_id, dedup, staleness, tx).await {
            tracing::error!(error = ?e, "group-membership manager exited with error");
        }
    });
    (rx, handle)
}

async fn manage(
    client: async_nats::Client,
    pc_id: String,
    dedup: std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: crate::staleness::Tracker,
    groups_tx: tokio::sync::watch::Sender<Vec<String>>,
) -> Result<()> {
    let js = jetstream::new(client.clone());
    let kv = match js.get_key_value(BUCKET_AGENT_GROUPS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(
                error = %e,
                bucket = BUCKET_AGENT_GROUPS,
                "agent_groups KV bucket missing — group subscriptions idle until bootstrap"
            );
            return Ok(());
        }
    };

    let mut subs: HashMap<String, JoinHandle<()>> = HashMap::new();

    let initial_desired = match kv.get(&pc_id).await {
        Ok(Some(bytes)) => parse_groups(&bytes),
        Ok(None) => {
            info!(pc_id = %pc_id, "no agent_groups entry — starting with empty membership");
            Vec::new()
        }
        Err(e) => {
            warn!(error = %e, "initial agent_groups KV read failed");
            Vec::new()
        }
    };
    apply_delta(
        &diff_groups::<String, String>(&[], &initial_desired),
        &mut subs,
        &client,
        &pc_id,
        &dedup,
        &staleness,
    )
    .await;
    // Publish the initial membership snapshot so `local_scheduler`
    // boots with the right set instead of [] until the first watch
    // event fires.
    let _ = groups_tx.send(initial_desired);

    let mut watch = kv
        .watch(&pc_id)
        .await
        .context("watch agent_groups KV key")?;
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
            apply_delta(&delta, &mut subs, &client, &pc_id, &dedup, &staleness).await;
        }
        // Always publish — `local_scheduler` cares about the
        // membership value, not whether *core sub subscriptions*
        // changed (they can be a no-op when membership stayed the
        // same modulo ordering).
        let _ = groups_tx.send(desired);
    }
    Ok(())
}

async fn apply_delta(
    delta: &SubscriptionDelta,
    subs: &mut HashMap<String, JoinHandle<()>>,
    client: &async_nats::Client,
    pc_id: &str,
    dedup: &std::sync::Arc<tokio::sync::Mutex<crate::commands::DedupCache>>,
    staleness: &crate::staleness::Tracker,
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
                ));
                subs.insert(g.clone(), handle);
                info!(group = %g, "subscribed to group");
            }
            Err(e) => warn!(error = %e, group = %g, "subscribe to group failed"),
        }
    }
}

fn parse_groups(bytes: &[u8]) -> Vec<String> {
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
