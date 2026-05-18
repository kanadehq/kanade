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

/// Spawn the supervisor and hand back the watch receiver
/// subscribers will use.
pub fn spawn(client: async_nats::Client, pc_id: String) -> watch::Receiver<EffectiveConfig> {
    let (tx, rx) = watch::channel(EffectiveConfig::builtin_defaults());
    tokio::spawn(run(client, pc_id, tx));
    rx
}

async fn run(client: async_nats::Client, pc_id: String, tx: watch::Sender<EffectiveConfig>) {
    let js = jetstream::new(client.clone());

    let cfg_kv = match js.get_key_value(BUCKET_AGENT_CONFIG).await {
        Ok(k) => k,
        Err(e) => {
            warn!(
                error = %e,
                bucket = BUCKET_AGENT_CONFIG,
                "agent_config KV bucket missing — supervisor idle (built-in defaults stand)"
            );
            return;
        }
    };
    let groups_kv = match js.get_key_value(BUCKET_AGENT_GROUPS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(
                error = %e,
                bucket = BUCKET_AGENT_GROUPS,
                "agent_groups KV bucket missing — supervisor idle"
            );
            return;
        }
    };

    let mut state = State::default();
    initial_sync(&cfg_kv, &groups_kv, &pc_id, &mut state).await;
    publish(&tx, &state);

    // Watch both buckets concurrently. watch_all on agent_config
    // surfaces every key change; watch(pc_id) on agent_groups
    // surfaces our membership flips.
    let mut cfg_watch = match cfg_kv.watch_all().await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "watch_all agent_config");
            return;
        }
    };
    let mut groups_watch = match groups_kv.watch(&pc_id).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "watch agent_groups for pc");
            return;
        }
    };

    loop {
        tokio::select! {
            entry = cfg_watch.next() => {
                let Some(entry) = entry else { return };
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => { warn!(error = %e, "agent_config watch entry"); continue; }
                };
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
                let Some(entry) = entry else { return };
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => { warn!(error = %e, "agent_groups watch entry"); continue; }
                };
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
    }
}

async fn initial_sync(
    cfg_kv: &jetstream::kv::Store,
    groups_kv: &jetstream::kv::Store,
    pc_id: &str,
    state: &mut State,
) {
    // Walk every current row in agent_config and apply it.
    match cfg_kv.keys().await {
        Ok(mut keys) => {
            while let Some(k) = keys.next().await {
                let key = match k {
                    Ok(k) => k,
                    Err(e) => {
                        warn!(error = %e, "agent_config keys()");
                        continue;
                    }
                };
                if let Ok(Some(bytes)) = cfg_kv.get(&key).await {
                    state.apply_cfg_change(&key, &bytes, false, pc_id);
                }
            }
        }
        Err(e) => warn!(error = %e, "agent_config keys() initial sync"),
    }

    // Read our own groups row.
    match groups_kv.get(pc_id).await {
        Ok(Some(bytes)) => {
            state.apply_groups_change(&bytes, false);
        }
        Ok(None) => {
            info!(
                pc_id,
                "no agent_groups row yet — starting with empty membership"
            );
        }
        Err(e) => warn!(error = %e, "agent_groups get initial sync"),
    }
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
        assert_eq!(classify_cfg_key("global", "MINIPC"), CfgKeyKind::Global);
    }

    #[test]
    fn classify_group() {
        assert_eq!(
            classify_cfg_key("groups.canary", "MINIPC"),
            CfgKeyKind::Group("canary"),
        );
    }

    #[test]
    fn classify_pc_self_vs_other() {
        assert_eq!(classify_cfg_key("pcs.MINIPC", "MINIPC"), CfgKeyKind::PcSelf,);
        assert_eq!(
            classify_cfg_key("pcs.OTHERPC", "MINIPC"),
            CfgKeyKind::PcOther,
        );
    }

    #[test]
    fn classify_unknown_key() {
        assert_eq!(
            classify_cfg_key("random-key", "MINIPC"),
            CfgKeyKind::Unknown
        );
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
            s.apply_cfg_change("global", &bytes, false, "MINIPC"),
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
            s.apply_cfg_change("global", b"", true, "MINIPC"),
            ChangeOutcome::Touched,
        );
        assert!(s.global.is_none());
    }

    #[test]
    fn apply_global_delete_on_absent_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("global", b"", true, "MINIPC"),
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
            s.apply_cfg_change("groups.canary", &bytes, false, "MINIPC"),
            ChangeOutcome::Touched,
        );
        assert!(s.groups.contains_key("canary"));
        assert_eq!(
            s.apply_cfg_change("groups.canary", b"", true, "MINIPC"),
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
            s.apply_cfg_change("pcs.MINIPC", &bytes, false, "MINIPC"),
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
            s.apply_cfg_change("pcs.OTHERPC", &bytes, false, "MINIPC"),
            ChangeOutcome::Ignored,
        );
        assert!(s.pc.is_none());
    }

    #[test]
    fn apply_unknown_key_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("garbage", b"{}", false, "MINIPC"),
            ChangeOutcome::Ignored,
        );
    }

    #[test]
    fn apply_malformed_json_is_ignored() {
        let mut s = State::default();
        assert_eq!(
            s.apply_cfg_change("global", b"not-json", false, "MINIPC"),
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
