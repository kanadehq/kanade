//! #1032 follow-up ①: the group **materializer** — the backend background task
//! that reflects declared-group membership onto the **agent side**.
//!
//! PR #1050 made dynamic groups resolvable backend-side (a schedule's
//! `target.groups` expands them at dispatch). But the other membership
//! consumers — `kanade exec --groups`, `notifications.group.<name>`,
//! `client.visible_to`, `runs_on: agent` schedules — run agent-side, where an
//! agent decides its groups from the `agent_groups.<pc_id>` KV it watches. A
//! dynamic group is never written there, so those paths never see it.
//!
//! This materializer closes that gap. On a cadence (and immediately whenever a
//! [`GroupDef`] changes) it resolves every declared group — static `members:`
//! or dynamic `query:`, via the shared cache-backed
//! [`resolve_group_members`] — inverts the result to a per-PC map, and writes
//! each PC's derived group set into `BUCKET_AGENT_GROUPS_DERIVED`. Agents watch
//! that key alongside their manual `agent_groups` and union the two.
//!
//! **Sole writer.** This task is the only writer of `agent_groups_derived`, and
//! the operator is the only writer of `agent_groups` — the two never touch each
//! other's bucket, so operator-set membership is structurally safe from a
//! materializer bug (see the bucket docs in `kanade_shared::kv`).
//!
//! **Reconcile, not append.** Each pass writes the *desired* per-PC set and
//! **deletes** derived entries for PCs no longer in any group — otherwise a PC
//! that leaves a dynamic group (reimaged, query changed) would keep its stale
//! membership and stay subscribed to that group's `commands.group.<name>`.
//!
//! **Cheap.** Resolution is cache-backed (SQL runs only per a group's `refresh`
//! TTL) and writes are changed-gated (an unchanged PC is skipped), so a routine
//! tick over an unchanged fleet does no SQL and no KV writes — and therefore
//! wakes no agent's derived-watch.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::kv::{Config as KvConfig, Store};
use futures::{StreamExt, TryStreamExt};
use kanade_shared::kv::{BUCKET_AGENT_GROUPS_DERIVED, BUCKET_GROUP_DEFS};
use kanade_shared::manifest::GroupDef;
use kanade_shared::wire::AgentGroups;
use tracing::{debug, info, warn};

use crate::api::AppState;
use crate::api::group_sql::resolve_group_members;

/// Backbone reconcile cadence. Short-ish is fine: resolution is cache-backed
/// and writes are changed-gated, so an unchanged tick is nearly free. Bounds
/// how long a data-driven membership change (e.g. inventory reclassifies a PC)
/// takes to reach agents — at most this plus the group's own `refresh` TTL.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Run the materializer forever. Reconciles once at boot, then on every timer
/// tick or `GroupDef` change. Never returns; a reconcile error is logged and
/// retried on the next trigger (a transient KV blip must not kill the task).
pub async fn run(state: AppState) {
    info!("group materializer started");
    loop {
        if let Err(e) = reconcile_once(&state).await {
            warn!(error = %e, "group materializer reconcile failed; will retry");
        }
        wait_for_trigger(&state).await;
    }
}

/// Block until the next reconcile is due: the periodic timer fires, OR a
/// `GroupDef` create/update/delete lands (so a new dynamic group goes live
/// agent-side within seconds rather than up to a full tick). If the
/// `group_defs` bucket doesn't exist yet (no group ever created), there is
/// nothing to watch — fall back to the timer alone.
async fn wait_for_trigger(state: &AppState) {
    let tick = tokio::time::sleep(RECONCILE_INTERVAL);
    let Ok(kv) = state.jetstream.get_key_value(BUCKET_GROUP_DEFS).await else {
        tick.await;
        return;
    };
    let Ok(mut watch) = kv.watch_all().await else {
        tick.await;
        return;
    };
    tokio::select! {
        _ = tick => {}
        ev = watch.next() => {
            if ev.is_some() {
                debug!("group_defs changed — reconciling derived membership");
            }
        }
    }
}

/// One full reconcile: resolve every declared group, invert to a per-PC desired
/// map, and drive `agent_groups_derived` to it.
async fn reconcile_once(state: &AppState) -> Result<()> {
    let defs = load_group_defs(state).await?;
    let mut resolved: Vec<(String, Vec<String>)> = Vec::with_capacity(defs.len());
    let mut failed: BTreeSet<String> = BTreeSet::new();
    for g in &defs {
        match resolve_group_members(state, g).await {
            Ok(pcs) => resolved.push((g.id.clone(), pcs)),
            // A group whose resolve failed this pass (a transient SQLite lock,
            // or a persistently broken query) is recorded as FAILED rather than
            // silently dropped. `write_derived` then PRESERVES its existing
            // members instead of clearing them — otherwise a transient error
            // would delete every member's derived entry and churn a whole
            // group's `commands.group.<name>` subscriptions until the next good
            // tick (Gemini: fail-deadly). Membership only changes on a
            // successful resolve.
            Err(e) => {
                warn!(group = %g.id, error = %e, "materializer: group resolve failed; preserving existing members");
                failed.insert(g.id.clone());
            }
        }
    }
    let desired = build_desired(&resolved);
    write_derived(state, &desired, &failed).await
}

/// Pure inverse: a list of `(group_id, [pc_id])` → per-PC set of the group
/// names that PC belongs to. A PC in several declared groups gets all their
/// names in **one** entry, so the write below is per-PC (never group-scoped,
/// which would clobber a PC's other memberships).
fn build_desired(resolved: &[(String, Vec<String>)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut desired: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (group_id, pcs) in resolved {
        for pc in pcs {
            desired
                .entry(pc.clone())
                .or_default()
                .insert(group_id.clone());
        }
    }
    desired
}

/// Drive `agent_groups_derived` to `desired`, reconciling every candidate PC
/// (desired now ∪ stored today) concurrently — a per-PC read + changed-gated
/// write. `failed` is the set of groups whose resolve failed this pass; a PC's
/// membership in such a group is preserved from its current value rather than
/// cleared (see [`target_for_pc`]).
async fn write_derived(
    state: &AppState,
    desired: &BTreeMap<String, BTreeSet<String>>,
    failed: &BTreeSet<String>,
) -> Result<()> {
    let kv = state
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_AGENT_GROUPS_DERIVED.into(),
            history: 1,
            ..Default::default()
        })
        .await
        .context("ensure agent_groups_derived KV")?;

    // Candidate PCs: everyone desired now, plus everyone with a stored entry
    // today (so departures and failed-group preservation are both handled).
    // `keys()` can error on an empty bucket on some async-nats versions —
    // degrade to none rather than crashing the loop (Gemini).
    let existing: Vec<String> = match kv.keys().await {
        Ok(k) => k.try_collect().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut candidates: BTreeSet<String> = desired.keys().cloned().collect();
    candidates.extend(existing);

    // Reconcile each PC concurrently (bounded) rather than one round-trip at a
    // time — the dominant cost on a large fleet, and it keeps the tick well
    // under the reconcile interval (Gemini).
    const WRITE_CONCURRENCY: usize = 16;
    let _: Vec<()> = futures::stream::iter(candidates)
        .map(|pc| {
            let kv = kv.clone();
            async move { reconcile_pc(&kv, &pc, desired.get(&pc), failed).await }
        })
        .buffer_unordered(WRITE_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(())
}

/// Reconcile one PC's derived entry to its target set (changed-gated). A target
/// of ∅ deletes the key (guarded on a live value so a tombstone isn't
/// re-deleted every pass).
async fn reconcile_pc(
    kv: &Store,
    pc: &str,
    desired: Option<&BTreeSet<String>>,
    failed: &BTreeSet<String>,
) -> Result<()> {
    let current = read_derived(kv, pc).await;
    let current_groups = current.as_ref().map(|g| g.groups.as_slice()).unwrap_or(&[]);
    let target = target_for_pc(desired, current_groups, failed);

    if target.is_empty() {
        if current.is_some() {
            kv.delete(pc)
                .await
                .with_context(|| format!("delete stale derived membership for {pc}"))?;
            debug!(pc = %pc, "cleared derived membership (left all declared groups)");
        }
        return Ok(());
    }

    let want = AgentGroups::new(target);
    if current.as_ref() == Some(&want) {
        return Ok(()); // changed-gated: unchanged ⇒ no write, no watch wake.
    }
    let bytes = serde_json::to_vec(&want).context("encode derived AgentGroups")?;
    kv.put(pc, bytes.into())
        .await
        .with_context(|| format!("put derived membership for {pc}"))?;
    debug!(pc = %pc, groups = ?want.groups, "materialized derived membership");
    Ok(())
}

/// A PC's target derived set: the groups it belongs to per this pass's
/// **successful** resolves (`desired`), UNION any group it **currently** has
/// whose resolve **failed** this pass (`current ∩ failed`) — preserved so a
/// transient resolve error doesn't churn that group's membership.
fn target_for_pc(
    desired: Option<&BTreeSet<String>>,
    current: &[String],
    failed: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut target: BTreeSet<String> = desired.cloned().unwrap_or_default();
    for g in current {
        if failed.contains(g) {
            target.insert(g.clone());
        }
    }
    target
}

/// Read+decode one PC's derived membership, treating any error / absence as
/// `None` (the reconcile then upserts or ignores accordingly).
async fn read_derived(kv: &Store, pc: &str) -> Option<AgentGroups> {
    match kv.get(pc).await {
        Ok(Some(bytes)) => serde_json::from_slice::<AgentGroups>(&bytes).ok(),
        _ => None,
    }
}

/// Load every registered `GroupDef`. A missing bucket (no group ever created)
/// ⇒ empty, which reconciles the derived bucket to empty.
async fn load_group_defs(state: &AppState) -> Result<Vec<GroupDef>> {
    let Ok(kv) = state.jetstream.get_key_value(BUCKET_GROUP_DEFS).await else {
        return Ok(Vec::new());
    };
    let keys: Vec<String> = match kv.keys().await {
        Ok(k) => k.try_collect().await.unwrap_or_default(),
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        if let Ok(Some(bytes)) = kv.get(&k).await
            && let Ok(g) = serde_json::from_slice::<GroupDef>(&bytes)
        {
            out.push(g);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_desired_inverts_group_to_pc() {
        let resolved = vec![
            ("clients".to_string(), vec!["PC-1".into(), "PC-2".into()]),
            ("servers".to_string(), vec!["SRV-1".into()]),
        ];
        let desired = build_desired(&resolved);
        assert_eq!(desired.get("PC-1"), Some(&set(&["clients"])));
        assert_eq!(desired.get("PC-2"), Some(&set(&["clients"])));
        assert_eq!(desired.get("SRV-1"), Some(&set(&["servers"])));
        assert_eq!(desired.len(), 3);
    }

    #[test]
    fn build_desired_unions_multiple_groups_per_pc() {
        // A PC in two dynamic groups gets BOTH names in one entry — so the
        // per-PC write can't clobber one membership with the other.
        let resolved = vec![
            ("clients".to_string(), vec!["PC-1".into()]),
            ("win-24h2".to_string(), vec!["PC-1".into()]),
        ];
        let desired = build_desired(&resolved);
        assert_eq!(desired.get("PC-1"), Some(&set(&["clients", "win-24h2"])));
        assert_eq!(desired.len(), 1);
    }

    fn slice(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target_uses_desired_when_nothing_failed() {
        // Normal case: target is exactly the freshly-resolved set.
        let desired = set(&["clients"]);
        let t = target_for_pc(
            Some(&desired),
            &slice(&["clients", "old"]),
            &BTreeSet::new(),
        );
        assert_eq!(t, set(&["clients"]));
    }

    #[test]
    fn target_preserves_a_failed_group_the_pc_currently_has() {
        // `servers` failed to resolve this pass; the PC currently has it, so it
        // is preserved (no churn) even though it's absent from `desired`.
        let desired = set(&["clients"]);
        let failed = set(&["servers"]);
        let t = target_for_pc(Some(&desired), &slice(&["clients", "servers"]), &failed);
        assert_eq!(t, set(&["clients", "servers"]));
    }

    #[test]
    fn target_does_not_add_a_failed_group_the_pc_lacks() {
        // A failed group the PC never had is NOT invented — only preserved.
        let failed = set(&["servers"]);
        let t = target_for_pc(None, &slice(&["clients"]), &failed);
        assert!(t.is_empty());
    }

    #[test]
    fn target_empty_means_delete() {
        // PC left every (successful) group and holds no failed-group membership.
        let t = target_for_pc(None, &slice(&["gone"]), &BTreeSet::new());
        assert!(t.is_empty());
    }

    #[test]
    fn build_desired_empty_when_no_groups() {
        assert!(build_desired(&[]).is_empty());
        // A group that resolved to zero members contributes no PC entries.
        let resolved = vec![("empty".to_string(), vec![])];
        assert!(build_desired(&resolved).is_empty());
    }
}
