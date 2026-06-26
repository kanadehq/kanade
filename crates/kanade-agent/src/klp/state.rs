//! Endpoint state evaluator (SPEC §2.1 / §2.12.5 state.snapshot).
//!
//! Runs on a 30 s cadence in a background task spawned from
//! `main.rs`. Each tick:
//!
//! 1. Snapshots the current `EffectiveConfig` to read the rollout
//!    target version (for the `agent_self_update` check).
//! 2. Evaluates the agent's intrinsic, non-scriptable checks
//!    (currently just `agent_self_update`) and merges in any
//!    operator-defined `check:` results cached from the command
//!    path (see [`crate::check_cache`]).
//! 3. Builds a fresh [`StateSnapshot`] and publishes via the
//!    `watch::Sender`. The [`klp::handlers::state`] forwarder
//!    tasks pick up the change and push it down each subscribed
//!    KLP connection.
//!
//! The first snapshot is evaluated synchronously at startup
//! (`eval_once`) so the watch channel has a meaningful initial
//! value before any client connects — `state.snapshot` returns
//! the cached value without waiting for an evaluator tick.

use std::time::Duration;

use async_nats::connection::State;
use kanade_shared::ipc::state::{Check, CheckStatus, StateSnapshot};
use kanade_shared::wire::EffectiveConfig;
use tokio::sync::watch;
use tracing::debug;

/// How often the evaluator re-checks the endpoint. Picked so the
/// SPA's Health tab feels live without burning CPU — the intrinsic
/// `agent_self_update` check is a sub-millisecond version compare,
/// and operator-defined `check:` results are read from cache (their
/// PowerShell runs on the command path, not here).
const EVAL_INTERVAL: Duration = Duration::from_secs(30);

/// Whether the agent currently holds a live broker connection —
/// the value behind `StateSnapshot.online` (#288). Mirrors the
/// `connection_state() == Connected` check already used by
/// [`crate::nats_retry`] / [`crate::staleness`]; reading it is a
/// cheap atomic load, so the evaluator re-samples it every tick and
/// a dropped broker flips the Health tab to offline within one
/// [`EVAL_INTERVAL`].
pub fn client_online(client: &async_nats::Client) -> bool {
    client.connection_state() == State::Connected
}

/// Build a fresh snapshot synchronously. Called by `main.rs` once
/// at startup to seed the watch channel, then by `eval_loop`
/// every tick.
///
/// `online` is the agent's live broker-connection status, sampled
/// by the caller via [`client_online`] — kept as a parameter (not
/// read in here) so this stays a pure, runtime-free function the
/// unit tests can pin both ways.
///
/// `extra_checks` are operator-defined health checks (#290) the
/// command path has run and cached (see [`crate::check_cache`]). They
/// are merged onto the agent's intrinsic check(s); an operator
/// check overrides a built-in of the same name.
pub fn eval_once(
    pc_id: &str,
    agent_version: &str,
    cfg: &EffectiveConfig,
    online: bool,
    extra_checks: &[Check],
) -> StateSnapshot {
    // The only intrinsic, non-scriptable check is the version-target
    // compare — it reads the agent's own running version against the
    // rollout target, state no operator PowerShell can observe.
    // Everything probe-able from the box (disk_free / bitlocker / av /
    // cert / …) is an operator-defined `check:` job merged in from the
    // cache, shipped as example YAMLs under `configs/jobs/` (disk_free
    // → `check-disk-space.yaml`).
    let intrinsic = vec![agent_self_update_check(
        agent_version,
        cfg.target_version.as_deref(),
    )];
    let checks = crate::check_cache::merge_checks(intrinsic, extra_checks);
    StateSnapshot {
        pc_id: pc_id.to_string(),
        // Real broker-connection state, sampled by the caller (see
        // `client_online`) so this stays unit-testable (#288).
        online,
        // VPN posture is intentionally NOT a snapshot field — it is
        // site-specific and probe-able from the box, so a site that
        // wants it ships an operator-defined `check:` (e.g.
        // `check-vpn.yaml`) and it flows in via `extra_checks`, same
        // as disk_free / bitlocker. See `StateSnapshot` docs.
        checks,
        agent_version: agent_version.to_string(),
        target_version: cfg
            .target_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| agent_version.to_string()),
    }
}

/// Run forever (background task). Every [`EVAL_INTERVAL`], build a
/// new [`StateSnapshot`] and send via `state_tx`. The
/// [`watch::Sender`] collapses identical successive values, so
/// idle endpoints don't wake the forwarders unnecessarily —
/// `state.changed` push fires only on a real diff.
pub async fn eval_loop(
    state_tx: watch::Sender<StateSnapshot>,
    cfg_rx: watch::Receiver<EffectiveConfig>,
    pc_id: String,
    agent_version: String,
    client: async_nats::Client,
    check_sink: crate::check_cache::CheckSink,
) {
    let mut tick = tokio::time::interval(EVAL_INTERVAL);
    // Skip the immediate first fire; main.rs already seeded the
    // channel with eval_once.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    loop {
        // Re-publish on the regular cadence OR as soon as a `check:`
        // job records a fresh result (#290), so the Health tab updates
        // promptly instead of waiting up to EVAL_INTERVAL.
        tokio::select! {
            _ = tick.tick() => {}
            _ = check_sink.wait() => {}
        }
        let snapshot = eval_once(
            &pc_id,
            &agent_version,
            &cfg_rx.borrow(),
            client_online(&client),
            &check_sink.checks(),
        );
        // TODO(perf): use `send_if_modified` once StateSnapshot
        // derives PartialEq in `kanade-shared`. For now we send
        // unconditionally and every forwarder wakes every 30 s
        // even if nothing changed — measurable but tiny (a 0–2
        // connection agent emits ~60 B/s of spurious pushes).
        if state_tx.send(snapshot).is_err() {
            // All receivers dropped — the listener is shutting
            // down. Exit cleanly so the task doesn't spin.
            debug!(pc_id = %pc_id, "state.eval_loop: no receivers, exiting");
            return;
        }
    }
}

// ============================================================
// Check implementations
// ============================================================

/// `agent_self_update` — compares the running version against the
/// rollout target published by Sprint 6's config supervisor.
///
/// - Equal (or target unset) → Ok.
/// - Differ → Warn with detail so the SPA's Health tab shows
///   "restart pending" without yet being a hard failure.
fn agent_self_update_check(running: &str, target: Option<&str>) -> Check {
    // Human-facing title for the one intrinsic check — operator-defined
    // checks carry their own `label`; this is the built-in equivalent so
    // the Health tab never shows the bare `agent_self_update` slug.
    let label = Some("エージェントの自動更新".to_string());
    let target = target.filter(|s| !s.is_empty()).unwrap_or(running);
    if running == target {
        Check {
            name: "agent_self_update".into(),
            label,
            status: CheckStatus::Ok,
            detail: Some(format!("running {running} (target matches)")),
            troubleshoot: None,
        }
    } else {
        Check {
            name: "agent_self_update".into(),
            label,
            status: CheckStatus::Warn,
            detail: Some(format!(
                "running {running}, target {target} — restart pending"
            )),
            // No user-invokable manifest yet for self-update; the
            // self_update background task handles this without
            // user action.
            troubleshoot: None,
        }
    }
}

// disk_free / bitlocker / av_signature / cert_expiry are no longer
// hardcoded here (#290 / #674-follow-up) — they are operator-defined
// `check:` jobs merged in from `crate::check_cache`, shipped as
// example YAMLs under `configs/jobs/` (disk_free → `check-disk-space`,
// status-light name `disk_space`). The agent keeps only the intrinsic,
// non-scriptable `agent_self_update` check built in — anything probe-
// able from the endpoint belongs in a config file, not in the binary.

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(target: Option<&str>) -> EffectiveConfig {
        let mut cfg = EffectiveConfig::builtin_defaults();
        cfg.target_version = target.map(str::to_owned);
        cfg
    }

    #[test]
    fn eval_once_produces_well_formed_snapshot() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(None), true, &[]);
        assert_eq!(snap.pc_id, "PC1234");
        assert!(snap.online);
        assert_eq!(snap.agent_version, "0.41.0");
        assert_eq!(snap.target_version, "0.41.0"); // target unset → falls back
        // With no operator checks, only the single intrinsic one (#290).
        let names: Vec<&str> = snap.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["agent_self_update"]);
    }

    #[test]
    fn eval_once_merges_operator_checks() {
        // #290: cached operator-defined checks appear alongside the
        // intrinsic one, and one named like a built-in overrides it.
        let extra = vec![
            Check {
                name: "disk_space".into(),
                label: None,
                status: CheckStatus::Warn,
                detail: Some("C: 8% free".into()),
                troubleshoot: None,
            },
            // Same name as the intrinsic check → must override, not
            // duplicate.
            Check {
                name: "agent_self_update".into(),
                label: None,
                status: CheckStatus::Fail,
                detail: Some("operator override".into()),
                troubleshoot: None,
            },
        ];
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(None), true, &extra);
        let names: Vec<&str> = snap.checks.iter().map(|c| c.name.as_str()).collect();
        // disk_space appended; agent_self_update overridden (not
        // duplicated).
        assert!(names.contains(&"disk_space"));
        assert_eq!(
            names.iter().filter(|n| **n == "agent_self_update").count(),
            1
        );
        let asu = snap
            .checks
            .iter()
            .find(|c| c.name == "agent_self_update")
            .unwrap();
        assert_eq!(asu.status, CheckStatus::Fail);
    }

    #[test]
    fn eval_once_online_reflects_the_passed_flag() {
        // #288: `online` must mirror the broker-connection bool the
        // caller samples, not a hardcoded `true` — a disconnected
        // agent has to surface as offline on the Health tab.
        let offline = eval_once("PC1234", "0.41.0", &cfg_with(None), false, &[]);
        assert!(!offline.online, "online must follow the passed flag");
        let online = eval_once("PC1234", "0.41.0", &cfg_with(None), true, &[]);
        assert!(online.online);
    }

    #[test]
    fn agent_self_update_ok_when_running_matches_target() {
        let c = agent_self_update_check("0.41.0", Some("0.41.0"));
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_ok_when_target_unset() {
        let c = agent_self_update_check("0.41.0", None);
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_ok_when_target_empty_string() {
        // Same defensive read as handle_version — a backend that
        // sets Some("") instead of clearing the field shouldn't
        // trip a phantom warning.
        let c = agent_self_update_check("0.41.0", Some(""));
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn agent_self_update_warn_when_target_differs() {
        let c = agent_self_update_check("0.41.0", Some("0.42.0"));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.unwrap().contains("restart pending"));
    }

    #[test]
    fn snapshot_target_version_falls_back_when_cfg_target_empty() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("")), true, &[]);
        assert_eq!(snap.target_version, "0.41.0");
    }

    #[test]
    fn snapshot_target_version_surfaces_when_cfg_target_set() {
        let snap = eval_once("PC1234", "0.41.0", &cfg_with(Some("0.42.0")), true, &[]);
        assert_eq!(snap.target_version, "0.42.0");
    }
}
