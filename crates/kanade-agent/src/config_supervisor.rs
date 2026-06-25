//! Layered agent_config watcher (Sprint 6 phase 2).
//!
//! Owns the in-memory mirror of the agent_config + agent_groups KV
//! buckets, recomputes the [`EffectiveConfig`] on every change, and
//! republishes it on a [`tokio::sync::watch`] channel. Heartbeat /
//! inventory / self_update subscribe to that channel and react to
//! cadence-or-target shifts without restarting the agent.
//!
//! Pure helpers ([`classify_cfg_key`], [`State::apply_cfg_change`])
//! are kept testable without a live NATS connection; the async
//! [`run`] glue around them does the bucket I/O.

use std::collections::BTreeMap;

use async_nats::jetstream;
use futures::StreamExt;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, KEY_AGENT_CONFIG_GLOBAL,
    parse_agent_config_group_key, parse_agent_config_pc_key,
};
use kanade_shared::wire::{AgentGroups, ConfigScope, EffectiveConfig, ResolutionWarning, resolve};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::nats_retry;
use crate::staleness::Tracker;

#[derive(Debug, PartialEq, Eq)]
enum CfgKeyKind<'a> {
    Global,
    Group(&'a str),
    /// `pcs.<my pc_id>` — applies to this agent.
    PcSelf,
    /// `pcs.<some other pc_id>` — applies to a different agent;
    /// safe to ignore on this side.
    PcOther,
    Unknown,
}

fn classify_cfg_key<'a>(key: &'a str, my_pc_id: &str) -> CfgKeyKind<'a> {
    if key == KEY_AGENT_CONFIG_GLOBAL {
        CfgKeyKind::Global
    } else if let Some(group) = parse_agent_config_group_key(key) {
        CfgKeyKind::Group(group)
    } else if let Some(pc) = parse_agent_config_pc_key(key) {
        if pc == my_pc_id {
            CfgKeyKind::PcSelf
        } else {
            CfgKeyKind::PcOther
        }
    } else {
        CfgKeyKind::Unknown
    }
}

#[derive(Default, Debug, Clone)]
struct State {
    global: Option<ConfigScope>,
    groups: BTreeMap<String, ConfigScope>,
    pc: Option<ConfigScope>,
    my_groups: Vec<String>,
}

/// What a single bucket entry causes the supervisor to do.
#[derive(Debug, PartialEq, Eq)]
enum ChangeOutcome {
    /// State was updated; the caller should recompute + republish.
    Touched,
    /// State was unchanged (e.g. PcOther key, or unknown key).
    Ignored,
}

impl State {
    fn resolved(&self) -> (EffectiveConfig, Vec<ResolutionWarning>) {
        resolve(
            self.global.as_ref(),
            &self.groups,
            self.pc.as_ref(),
            &self.my_groups,
        )
    }

    /// Apply a put / delete from the `agent_config` bucket.
    ///
    /// `is_delete` covers KV tombstones — the entry arrived as a
    /// `Delete` or `Purge` operation, meaning the row is gone and
    /// the in-memory copy should follow.
    fn apply_cfg_change(
        &mut self,
        key: &str,
        value: &[u8],
        is_delete: bool,
        my_pc_id: &str,
    ) -> ChangeOutcome {
        match classify_cfg_key(key, my_pc_id) {
            CfgKeyKind::Global => {
                if is_delete {
                    if self.global.is_some() {
                        self.global = None;
                        ChangeOutcome::Touched
                    } else {
                        ChangeOutcome::Ignored
                    }
                } else {
                    match serde_json::from_slice::<ConfigScope>(value) {
                        Ok(s) => {
                            self.global = Some(s);
                            ChangeOutcome::Touched
                        }
                        Err(e) => {
                            warn!(error = %e, key, "decode global ConfigScope");
                            ChangeOutcome::Ignored
                        }
                    }
                }
            }
            CfgKeyKind::Group(name) => {
                let name = name.to_string();
                if is_delete {
                    if self.groups.remove(&name).is_some() {
                        ChangeOutcome::Touched
                    } else {
                        ChangeOutcome::Ignored
                    }
                } else {
                    match serde_json::from_slice::<ConfigScope>(value) {
                        Ok(s) => {
                            self.groups.insert(name, s);
                            ChangeOutcome::Touched
                        }
                        Err(e) => {
                            warn!(error = %e, key, "decode group ConfigScope");
                            ChangeOutcome::Ignored
                        }
                    }
                }
            }
            CfgKeyKind::PcSelf => {
                if is_delete {
                    if self.pc.is_some() {
                        self.pc = None;
                        ChangeOutcome::Touched
                    } else {
                        ChangeOutcome::Ignored
                    }
                } else {
                    match serde_json::from_slice::<ConfigScope>(value) {
                        Ok(s) => {
                            self.pc = Some(s);
                            ChangeOutcome::Touched
                        }
                        Err(e) => {
                            warn!(error = %e, key, "decode pc ConfigScope");
                            ChangeOutcome::Ignored
                        }
                    }
                }
            }
            CfgKeyKind::PcOther | CfgKeyKind::Unknown => ChangeOutcome::Ignored,
        }
    }

    /// Apply a put / delete from the `agent_groups` bucket. Only
    /// the row for this agent's pc_id matters; the caller is expected
    /// to have already filtered to that key.
    fn apply_groups_change(&mut self, value: &[u8], is_delete: bool) -> ChangeOutcome {
        let new_groups = if is_delete {
            Vec::new()
        } else {
            match serde_json::from_slice::<AgentGroups>(value) {
                Ok(g) => g.groups,
                Err(e) => {
                    warn!(error = %e, "decode AgentGroups");
                    return ChangeOutcome::Ignored;
                }
            }
        };
        if new_groups == self.my_groups {
            ChangeOutcome::Ignored
        } else {
            self.my_groups = new_groups;
            ChangeOutcome::Touched
        }
    }
}

/// Spawn the supervisor and hand back the watch receiver subscribers
/// will use.
///
/// v0.38 / #137: takes a [`Tracker`] so the inner reconnect loop can
/// short-circuit its backoff sleep on a Connected event. The
/// supervisor outlives broker outages — it republishes the current
/// `EffectiveConfig` on every reconnect, picking up edits made while
/// disconnected — so subscribers (heartbeat / inventory /
/// self_update) keep getting fresh settings without an agent
/// restart.
pub fn spawn(
    client: async_nats::Client,
    pc_id: String,
    tracker: Tracker,
) -> watch::Receiver<EffectiveConfig> {
    let (tx, rx) = watch::channel(EffectiveConfig::builtin_defaults());
    tokio::spawn(run(client, pc_id, tracker, tx));
    rx
}

async fn run(
    client: async_nats::Client,
    pc_id: String,
    tracker: Tracker,
    tx: watch::Sender<EffectiveConfig>,
) {
    let js = jetstream::new(client.clone());

    // Long-lived state: persists across reconnects. We swap into it
    // *only* when `initial_sync` succeeds end-to-end, so a transient
    // walk failure can't briefly publish `builtin_defaults` to
    // subscribers (which would revert heartbeat / inventory cadences
    // to defaults until the next watch event).
    //
    // The initial `State::default()` value is never read (every
    // execution path that reaches `publish` first does
    // `state = new_state`), but the binding must exist for the swap
    // and the inner watch loop's `state.apply_*_change` calls.
    #[allow(unused_assignments)]
    let mut state = State::default();

    loop {
        let cfg_kv =
            nats_retry::wait_for_kv(&js, &client, &tracker, BUCKET_AGENT_CONFIG, "agent_config")
                .await;
        let groups_kv =
            nats_retry::wait_for_kv(&js, &client, &tracker, BUCKET_AGENT_GROUPS, "agent_groups")
                .await;

        // Build the new state into a *fresh* `State::default()` so
        // we can detect partial-walk failure and skip the swap. The
        // existing `state` keeps running until both walks succeed,
        // which preserves heartbeat / inventory cadences during a
        // transient KV-walk failure (Gemini #147 review).
        let mut new_state = State::default();
        let (mut cfg_hwms, mut groups_hwm) = match initial_sync(
            &cfg_kv,
            &groups_kv,
            &pc_id,
            &mut new_state,
        )
        .await
        {
            Ok(hwm) => hwm,
            Err(()) => {
                warn!(
                    "config_supervisor: initial_sync incomplete; keeping previous EffectiveConfig and reopening"
                );
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        state = new_state;
        publish(&tx, &state);

        // Watch both buckets concurrently. watch_all on agent_config
        // surfaces every key change; watch(pc_id) on agent_groups
        // surfaces our membership flips.
        let mut cfg_watch = match cfg_kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "watch_all agent_config failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };
        let mut groups_watch = match groups_kv.watch(&pc_id).await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "watch agent_groups for pc failed; reopening");
                nats_retry::reopen_pause().await;
                continue;
            }
        };

        // Inner watch loop. `break` (instead of `return`) on either
        // watch dropping so the outer reconnect loop reopens both.
        let dropped = 'inner: loop {
            tokio::select! {
                entry = cfg_watch.next() => {
                    let Some(entry) = entry else { break 'inner "agent_config" };
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => { warn!(error = %e, "agent_config watch entry"); continue; }
                    };
                    // Drop history the broker replayed on reconnect: a
                    // revision at or below what we've already applied FOR
                    // THIS KEY is a duplicate, not a new operator change.
                    // Republishing it would feed self-update a stale
                    // target_version and flap the binary backward
                    // (#828). The mark is per-key, not bucket-wide:
                    // NATS revisions are globally ordered, so a single mark
                    // would let one key's high revision permanently mask a
                    // lower-revision key that a transient error skipped during
                    // initial_sync — the replay would never re-apply it. A
                    // per-key mark defaults to 0 for an un-synced key, so the
                    // replay self-heals it (and any stale intermediate it
                    // replays on the way is caught by self-update's downgrade
                    // settle guard).
                    if entry.revision <= cfg_hwms.get(&entry.key).copied().unwrap_or(0) {
                        continue;
                    }
                    cfg_hwms.insert(entry.key.clone(), entry.revision);
                    let is_delete = matches!(
                        entry.operation,
                        async_nats::jetstream::kv::Operation::Delete
                            | async_nats::jetstream::kv::Operation::Purge
                    );
                    if state.apply_cfg_change(&entry.key, &entry.value, is_delete, &pc_id)
                        == ChangeOutcome::Touched
                    {
                        publish(&tx, &state);
                    }
                }
                entry = groups_watch.next() => {
                    let Some(entry) = entry else { break 'inner "agent_groups" };
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => { warn!(error = %e, "agent_groups watch entry"); continue; }
                    };
                    // Same replay gate as agent_config above — a reconnect
                    // also replays this PC's group-membership history, which
                    // would otherwise churn every subscription on each blip.
                    if entry.revision <= groups_hwm {
                        continue;
                    }
                    groups_hwm = entry.revision;
                    let is_delete = matches!(
                        entry.operation,
                        async_nats::jetstream::kv::Operation::Delete
                            | async_nats::jetstream::kv::Operation::Purge
                    );
                    if state.apply_groups_change(&entry.value, is_delete) == ChangeOutcome::Touched {
                        publish(&tx, &state);
                    }
                }
            }
        };
        warn!(dropped, "config_supervisor watch ended; reopening");
        nats_retry::reopen_pause().await;
    }
}

/// Walk both KV buckets into `state`. Returns `Err(())` if either
/// `kv.keys()` or `groups_kv.get(pc_id)` fails — caller must NOT swap
/// the result into the live state, because a partial walk would
/// silently drop config rows. Per-row decode / get failures inside
/// the keys() walk are logged but tolerated (they're row-level
/// problems, not connectivity-level).
/// On success returns the KV revision high-water marks the caller hands
/// to the watch loop, which drops any entry at or below them: a broker
/// reconnect replays the bucket history (old `target_version`s, old group
/// membership) through the watch, and those replayed entries carry
/// revisions we have already applied. Without the gate the supervisor
/// re-publishes stale config and self-update flaps the binary backward
/// (#828).
///
/// agent_config's mark is **per key** (`BTreeMap<key, revision>`), not a
/// single bucket-wide number. NATS revisions are globally ordered, so one
/// shared mark would let a high-revision key (say `pcs.<id>` at rev 20)
/// permanently mask a lower-revision key (`global` at rev 10) that a
/// transient per-row error skipped here — the watch replay would see
/// `10 <= 20` and never re-apply `global`. A per-key map leaves an
/// un-synced key at the default 0, so the replay self-heals it; any stale
/// intermediate the replay walks through on the way is caught by
/// self-update's downgrade settle guard. agent_groups is a single key, so
/// its mark stays a plain `u64`.
async fn initial_sync(
    cfg_kv: &jetstream::kv::Store,
    groups_kv: &jetstream::kv::Store,
    pc_id: &str,
    state: &mut State,
) -> Result<(BTreeMap<String, u64>, u64), ()> {
    use async_nats::jetstream::kv::Operation;

    // Walk every current row in agent_config and apply it, recording each
    // key's revision (`entry()` carries the revision; `get()` does not,
    // which is why we switched off it here). A key whose `entry()` errors
    // is left out of the map entirely (mark 0) so the watch replay can
    // self-heal it.
    let mut cfg_hwms: BTreeMap<String, u64> = BTreeMap::new();
    let mut keys = match cfg_kv.keys().await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "agent_config keys() initial sync failed");
            return Err(());
        }
    };
    while let Some(k) = keys.next().await {
        let key = match k {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %e, "agent_config keys() entry");
                continue;
            }
        };
        match cfg_kv.entry(&key).await {
            Ok(Some(entry)) => {
                cfg_hwms.insert(entry.key.clone(), entry.revision);
                let is_delete = matches!(entry.operation, Operation::Delete | Operation::Purge);
                state.apply_cfg_change(&entry.key, &entry.value, is_delete, pc_id);
            }
            // Key vanished between keys() and entry() — nothing to apply.
            Ok(None) => {}
            // Transient per-row error: log it and, crucially, do NOT record a
            // revision for this key. Left at the default 0, the watch replay
            // re-applies it (self-heals) instead of the gap silently
            // persisting until the next operator write to that key.
            Err(e) => {
                warn!(error = %e, key, "agent_config entry() during initial sync; key left to self-heal from watch replay");
            }
        }
    }

    // Read our own groups row.
    let mut groups_hwm = 0u64;
    match groups_kv.entry(pc_id).await {
        Ok(Some(entry)) => {
            groups_hwm = entry.revision;
            let is_delete = matches!(entry.operation, Operation::Delete | Operation::Purge);
            state.apply_groups_change(&entry.value, is_delete);
        }
        Ok(None) => {
            info!(
                pc_id,
                "no agent_groups row yet — starting with empty membership"
            );
        }
        Err(e) => {
            warn!(error = %e, "agent_groups get initial sync failed");
            return Err(());
        }
    }
    Ok((cfg_hwms, groups_hwm))
}

fn publish(tx: &watch::Sender<EffectiveConfig>, state: &State) {
    let (eff, warns) = state.resolved();
    for w in &warns {
        warn!(?w, "agent_config resolution warning");
    }
    // send_if_modified returns false if the new value equals the
    // current one — saves a wakeup on the subscriber side.
    tx.send_if_modified(|current| {
        if *current == eff {
            false
        } else {
            info!(?eff, "effective config updated");
            *current = eff.clone();
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_global() {
        assert_eq!(classify_cfg_key("global", "PC-01"), CfgKeyKind::Global);
    }

    #[test]
    fn classify_group() {
        assert_eq!(
            classify_cfg_key("groups.canary", "PC-01"),
            CfgKeyKind::Group("canary"),
        );
    }

    #[test]
    fn classify_pc_self_vs_other() {
        assert_eq!(classify_cfg_key("pcs.PC-01", "PC-01"), CfgKeyKind::PcSelf,);
        assert_eq!(
            classify_cfg_key("pcs.OTHERPC", "PC-01"),
            CfgKeyKind::PcOther,
        );
    }

    #[test]
    fn classify_unknown_key() {
        assert_eq!(classify_cfg_key("random-key", "PC-01"), CfgKeyKind::Unknown);
    }

    #[test]
    fn apply_global_put_updates_state() {
        let mut s = State::default();
        let scope = ConfigScope {
            heartbeat_interval: Some("60s".into()),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&scope).unwrap();
        assert_eq!(
            s.apply_cfg_change("global", &bytes, false, "PC-01"),
            ChangeOutcome::Touched,
        );
        assert_eq!(
            s.global.as_ref().unwrap().heartbeat_interval.as_deref(),
            Some("60s")
        );
    }

    #[test]
    fn apply_global_delete_clears_state() {
        let mut s = State {
            global: Some(ConfigScope {
                heartbeat_interval: Some("60s".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            s.apply_cfg_change("global", b"", true, "PC-01"),
            ChangeOutcome::Touched,
        );
        assert!(s.global.is_none());
    }

    #[test]
    fn apply_global_delete_on_absent_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("global", b"", true, "PC-01"),
            ChangeOutcome::Ignored,
        );
    }

    #[test]
    fn apply_group_put_then_delete() {
        let mut s = State::default();
        let scope = ConfigScope {
            target_version: Some("0.3.0".into()),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&scope).unwrap();
        assert_eq!(
            s.apply_cfg_change("groups.canary", &bytes, false, "PC-01"),
            ChangeOutcome::Touched,
        );
        assert!(s.groups.contains_key("canary"));
        assert_eq!(
            s.apply_cfg_change("groups.canary", b"", true, "PC-01"),
            ChangeOutcome::Touched,
        );
        assert!(!s.groups.contains_key("canary"));
    }

    #[test]
    fn apply_pc_self_routes_to_pc_scope() {
        let mut s = State::default();
        let scope = ConfigScope {
            heartbeat_interval: Some("15s".into()),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&scope).unwrap();
        assert_eq!(
            s.apply_cfg_change("pcs.PC-01", &bytes, false, "PC-01"),
            ChangeOutcome::Touched,
        );
        assert!(s.pc.is_some());
    }

    #[test]
    fn apply_pc_other_is_ignored() {
        let mut s = State::default();
        let scope = ConfigScope {
            heartbeat_interval: Some("15s".into()),
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&scope).unwrap();
        assert_eq!(
            s.apply_cfg_change("pcs.OTHERPC", &bytes, false, "PC-01"),
            ChangeOutcome::Ignored,
        );
        assert!(s.pc.is_none());
    }

    #[test]
    fn apply_unknown_key_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("garbage", b"{}", false, "PC-01"),
            ChangeOutcome::Ignored,
        );
    }

    #[test]
    fn apply_malformed_json_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("global", b"not-json", false, "PC-01"),
            ChangeOutcome::Ignored,
        );
        assert!(s.global.is_none());
    }

    #[test]
    fn apply_groups_change_updates_my_groups() {
        let mut s = State::default();
        let g = AgentGroups::new(["wave1", "canary"]);
        let bytes = serde_json::to_vec(&g).unwrap();
        assert_eq!(s.apply_groups_change(&bytes, false), ChangeOutcome::Touched);
        assert_eq!(s.my_groups, vec!["canary".to_string(), "wave1".to_string()]);
        // Same value again -> no change.
        assert_eq!(s.apply_groups_change(&bytes, false), ChangeOutcome::Ignored);
    }

    #[test]
    fn apply_groups_delete_clears_my_groups() {
        let mut s = State {
            my_groups: vec!["wave1".into()],
            ..Default::default()
        };
        assert_eq!(s.apply_groups_change(b"", true), ChangeOutcome::Touched);
        assert!(s.my_groups.is_empty());
    }

    #[test]
    fn resolved_reflects_layered_state() {
        let mut s = State {
            global: Some(ConfigScope {
                heartbeat_interval: Some("60s".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        s.groups.insert(
            "canary".into(),
            ConfigScope {
                heartbeat_interval: Some("5s".into()),
                ..Default::default()
            },
        );
        s.my_groups = vec!["canary".into()];
        let (eff, warns) = s.resolved();
        assert_eq!(eff.heartbeat_interval, "5s");
        assert!(warns.is_empty());
    }
}
