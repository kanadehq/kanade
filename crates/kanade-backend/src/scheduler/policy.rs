//! Pure dedup policy for schedule modes. Kept side-effect-free so
//! the boundary semantics are pinned by unit tests — the
//! `scheduler::run` async loop just orchestrates "resolve target →
//! fetch completions → call [`decide_fire`] → publish / persist".
//!
//! The mode/cooldown inputs come from `Schedule::lowered()`
//! (kanade-shared) since #418 Phase 1 — operators write `when:`,
//! the engine still thinks in `(ExecMode, cooldown)`.
//!
//! Semantics (covered by tests below):
//!
//! * `EveryTick` — always [`FireAction::FireWholeTarget`]. No dedup.
//!   (Only reachable through the `when: { cron: ... }` escape hatch.)
//! * `OncePerPc` — for each `expected_pcs` entry, fire unless that
//!   pc has a completion within the cooldown window. `cooldown =
//!   None` ⇒ "succeed once ⇒ permanent skip".
//! * `OncePerTarget` — the whole target is gated by the most
//!   recent completion *among current target members*. Once it
//!   passes (or cooldown expires), fire the whole target.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use kanade_shared::manifest::ExecMode;

/// One "this pc successfully ran this job at this time" record.
/// Multiple records per pc are fine — [`decide_fire`] keeps the
/// most recent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub pc_id: String,
    pub finished_at: DateTime<Utc>,
}

/// What the policy decided this tick should do.
#[derive(Debug, PartialEq, Eq)]
pub enum FireAction {
    /// Nothing to fire — dedup says we're caught up (or the
    /// schedule is mis-configured with empty target).
    Skip,
    /// Fire at the schedule's original target via the usual
    /// `commands.all` / `commands.group.<g>` / `commands.pc.<id>`
    /// subjects. Used by `EveryTick` and by `OncePerTarget` when
    /// armed.
    FireWholeTarget,
    /// Fire at exactly these pcs via `commands.pc.<id>`. Used by
    /// `OncePerPc` after filtering out already-completed entries.
    FirePcs(Vec<String>),
}

/// Pure policy decision. All inputs are caller-resolved snapshots;
/// the function is free of IO so its behavior is locked down by the
/// unit tests below.
pub fn decide_fire(
    mode: ExecMode,
    cooldown: Option<ChronoDuration>,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> FireAction {
    match mode {
        ExecMode::EveryTick => FireAction::FireWholeTarget,
        ExecMode::OncePerPc => decide_once_per_pc(cooldown, expected_pcs, completions, now),
        ExecMode::OncePerTarget => decide_once_per_target(cooldown, expected_pcs, completions, now),
        // Event triggers (`when: { on }`) are agent-only — fired by the
        // agent's OS event source, never by the backend scheduler
        // (`Schedule::validate` rejects them on `runs_on: backend`).
        // Defensive for a hand-edited KV blob: the backend never fires.
        ExecMode::Event => FireAction::Skip,
    }
}

fn decide_once_per_pc(
    cooldown: Option<ChronoDuration>,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> FireAction {
    // Build pc → most-recent success time. Multiple records per pc
    // are fine (replays / re-runs) — keep the latest.
    let mut last_success: HashMap<&str, DateTime<Utc>> = HashMap::new();
    for c in completions {
        last_success
            .entry(c.pc_id.as_str())
            .and_modify(|t| {
                if c.finished_at > *t {
                    *t = c.finished_at;
                }
            })
            .or_insert(c.finished_at);
    }

    let mut eligible: Vec<String> = Vec::new();
    for pc in expected_pcs {
        let armed = match last_success.get(pc.as_str()) {
            None => true,
            Some(t) => match cooldown {
                None => false, // succeeded ever ⇒ permanently skip
                Some(cd) => {
                    // Boundary: re-arm when elapsed ≥ cooldown.
                    now.signed_duration_since(*t) >= cd
                }
            },
        };
        if armed {
            eligible.push(pc.clone());
        }
    }

    if eligible.is_empty() {
        return FireAction::Skip;
    }
    FireAction::FirePcs(eligible)
}

fn decide_once_per_target(
    cooldown: Option<ChronoDuration>,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> FireAction {
    // Only count completions from pcs that are currently in the
    // expected set — a decommissioned PC's old success shouldn't
    // permanently mute a `target.all` once_per_target schedule.
    let expected: HashSet<&str> = expected_pcs.iter().map(String::as_str).collect();
    let latest = completions
        .iter()
        .filter(|c| expected.contains(c.pc_id.as_str()))
        .map(|c| c.finished_at)
        .max();

    let armed = match (latest, cooldown) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(t), Some(cd)) => now.signed_duration_since(t) >= cd,
    };

    if armed && !expected_pcs.is_empty() {
        return FireAction::FireWholeTarget;
    }
    FireAction::Skip
}

/// Bounded in-flight suppression layered on top of [`decide_fire`].
///
/// `decide_fire` arms purely on *completed* runs, so between dispatch
/// and the completion landing in `execution_results` (agent-side
/// `jitter` + run + outbox drain) the every-minute poll keeps seeing
/// the PC/target as due and re-fires it each tick. This filter drops
/// any PC (or the whole target) whose last *dispatch* is younger than
/// `window` — long enough to cover that round-trip, short enough that a
/// dispatch that never produced a completion (agent offline, command
/// lost, `starting_deadline` skip) re-arms on its own once the window
/// lapses. It deliberately suppresses *less* than the completion-based
/// cooldown: the real cooldown takes back over the moment a completion
/// is recorded.
///
/// Inputs are caller-resolved KV snapshots so this stays pure (and unit
/// tested) like [`decide_fire`]:
/// * `dispatched_pcs` — `pc_id → last dispatch instant` (OncePerPc).
/// * `target_dispatched` — last whole-target dispatch (OncePerTarget).
///
/// Boundary mirrors the cooldown rule: elapsed `>= window` re-arms.
pub fn suppress_dispatched(
    action: FireAction,
    dispatched_pcs: &HashMap<String, DateTime<Utc>>,
    target_dispatched: Option<DateTime<Utc>>,
    window: ChronoDuration,
    now: DateTime<Utc>,
) -> FireAction {
    match action {
        FireAction::Skip => FireAction::Skip,
        FireAction::FireWholeTarget => match target_dispatched {
            // Dispatched recently and the completion hasn't had time to
            // land — hold off so the whole target isn't re-fired.
            Some(t) if now.signed_duration_since(t) < window => FireAction::Skip,
            _ => FireAction::FireWholeTarget,
        },
        FireAction::FirePcs(pcs) => {
            let kept: Vec<String> = pcs
                .into_iter()
                .filter(|pc| match dispatched_pcs.get(pc) {
                    // No recent dispatch (or it's older than the window)
                    // ⇒ keep firing this pc.
                    None => true,
                    Some(t) => now.signed_duration_since(*t) >= window,
                })
                .collect();
            if kept.is_empty() {
                FireAction::Skip
            } else {
                FireAction::FirePcs(kept)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).single().unwrap()
    }

    fn pcs(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).into()).collect()
    }

    fn done(pc: &str, secs: i64) -> Completion {
        Completion {
            pc_id: pc.into(),
            finished_at: t(secs),
        }
    }

    // ── EveryTick ─────────────────────────────────────────────

    #[test]
    fn every_tick_always_fires_whole_target() {
        let action = decide_fire(
            ExecMode::EveryTick,
            None,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1)],
            t(100),
        );
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    #[test]
    fn every_tick_ignores_empty_expected() {
        // EveryTick fires at original target subjects regardless of
        // expected_pcs enumeration — the broker handles fan-out.
        let action = decide_fire(ExecMode::EveryTick, None, &[], &[], t(0));
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    // ── OncePerPc: no-cooldown lifecycle ─────────────────────

    #[test]
    fn once_per_pc_no_completions_fires_every_pc() {
        let action = decide_fire(ExecMode::OncePerPc, None, &pcs(&["a", "b", "c"]), &[], t(0));
        assert_eq!(action, FireAction::FirePcs(pcs(&["a", "b", "c"])));
    }

    #[test]
    fn once_per_pc_partial_completion_fires_remainder() {
        let action = decide_fire(
            ExecMode::OncePerPc,
            None,
            &pcs(&["a", "b", "c"]),
            &[done("a", 0)],
            t(100),
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["b", "c"])));
    }

    #[test]
    fn once_per_pc_all_completed_skips() {
        let action = decide_fire(
            ExecMode::OncePerPc,
            None,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1)],
            t(100),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_pc_empty_expected_skips() {
        // Empty target (nobody alive / heartbeating yet) — nothing
        // to fire; the next tick re-evaluates a fresh roster.
        let action = decide_fire(ExecMode::OncePerPc, None, &[], &[], t(0));
        assert_eq!(action, FireAction::Skip);
    }

    // ── OncePerPc: cooldown boundaries ───────────────────────

    #[test]
    fn once_per_pc_cooldown_recent_excluded() {
        // Finished at t=100, now=110, cooldown=60s → 10s elapsed →
        // not yet re-armed.
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a"]),
            &[done("a", 100)],
            t(110),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_pc_cooldown_exactly_at_boundary_rearmed() {
        // Boundary policy: elapsed >= cooldown re-arms.
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a"]),
            &[done("a", 100)],
            t(160), // exactly 60s
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["a"])));
    }

    #[test]
    fn once_per_pc_cooldown_one_second_before_boundary_excluded() {
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a"]),
            &[done("a", 100)],
            t(159),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_pc_cooldown_past_boundary_rearmed() {
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 100), done("b", 200)],
            t(500), // a:400s, b:300s past → both >= 60s
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["a", "b"])));
    }

    #[test]
    fn once_per_pc_cooldown_mix_one_armed_one_not() {
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 100), done("b", 150)],
            t(170), // a:70s past, b:20s past
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["a"])));
    }

    #[test]
    fn once_per_pc_uses_latest_success_per_pc() {
        // Multiple completion records for the same pc — only the
        // newest counts (so an old success doesn't outlast its
        // cooldown when a fresh one has reset it).
        let action = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a"]),
            &[done("a", 0), done("a", 150), done("a", 50)],
            t(170), // newest = 150 → elapsed 20s → still in cooldown
        );
        assert_eq!(action, FireAction::Skip);
    }

    // ── OncePerPc: target shape edge cases ───────────────────

    #[test]
    fn once_per_pc_decommissioned_pc_completion_ignored() {
        // Old success from a pc that's no longer in the target —
        // doesn't shrink the eligible set.
        let action = decide_fire(
            ExecMode::OncePerPc,
            None,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1), done("ghost", 2)],
            t(100),
        );
        assert_eq!(action, FireAction::Skip);
    }

    // ── OncePerTarget: no-cooldown lifecycle ─────────────────

    #[test]
    fn once_per_target_no_success_fires_whole_target() {
        let action = decide_fire(ExecMode::OncePerTarget, None, &pcs(&["a", "b"]), &[], t(0));
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    #[test]
    fn once_per_target_any_in_scope_success_skips() {
        let action = decide_fire(
            ExecMode::OncePerTarget,
            None,
            &pcs(&["a", "b"]),
            &[done("a", 0)],
            t(100),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_target_out_of_scope_success_does_not_count() {
        // Success exists, but the only pc that did it was
        // decommissioned (no longer in expected). Still armed.
        let action = decide_fire(
            ExecMode::OncePerTarget,
            None,
            &pcs(&["a", "b"]),
            &[done("ghost", 0)],
            t(100),
        );
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    #[test]
    fn once_per_target_empty_expected_no_completion_skips() {
        let action = decide_fire(ExecMode::OncePerTarget, None, &[], &[], t(0));
        assert_eq!(action, FireAction::Skip);
    }

    // ── OncePerTarget: cooldown boundaries ───────────────────

    #[test]
    fn once_per_target_cooldown_recent_skipped() {
        let action = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(110), // 10s elapsed < 60s cooldown
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_target_cooldown_exactly_at_boundary_rearmed() {
        let action = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(160), // exactly 60s elapsed
        );
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    #[test]
    fn once_per_target_cooldown_one_second_before_boundary_skipped() {
        let action = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(159),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn once_per_target_uses_most_recent_completion() {
        // Two completions across the target; the most recent gates
        // the cooldown calc.
        let action = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 200)], // newest = b@200
            t(259),                          // 59s elapsed since b — still locked
        );
        assert_eq!(action, FireAction::Skip);
    }

    // ── suppress_dispatched: in-flight gate ──────────────────

    fn marks(entries: &[(&str, i64)]) -> HashMap<String, DateTime<Utc>> {
        entries
            .iter()
            .map(|(pc, secs)| ((*pc).into(), t(*secs)))
            .collect()
    }

    #[test]
    fn suppress_passes_skip_through() {
        let action = suppress_dispatched(
            FireAction::Skip,
            &marks(&[("a", 0)]),
            Some(t(0)),
            ChronoDuration::seconds(60),
            t(10),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn suppress_drops_recently_dispatched_pcs() {
        // a dispatched 10s ago (< 60s window) → suppressed; b never
        // dispatched → kept.
        let action = suppress_dispatched(
            FireAction::FirePcs(pcs(&["a", "b"])),
            &marks(&[("a", 100)]),
            None,
            ChronoDuration::seconds(60),
            t(110),
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["b"])));
    }

    #[test]
    fn suppress_all_pcs_in_flight_becomes_skip() {
        let action = suppress_dispatched(
            FireAction::FirePcs(pcs(&["a", "b"])),
            &marks(&[("a", 100), ("b", 105)]),
            None,
            ChronoDuration::seconds(60),
            t(110),
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn suppress_pc_window_boundary_rearms() {
        // Dispatched at 100, now 160 → exactly 60s elapsed → re-armed.
        let action = suppress_dispatched(
            FireAction::FirePcs(pcs(&["a"])),
            &marks(&[("a", 100)]),
            None,
            ChronoDuration::seconds(60),
            t(160),
        );
        assert_eq!(action, FireAction::FirePcs(pcs(&["a"])));
    }

    #[test]
    fn suppress_whole_target_recent_dispatch_skips() {
        let action = suppress_dispatched(
            FireAction::FireWholeTarget,
            &HashMap::new(),
            Some(t(100)),
            ChronoDuration::seconds(60),
            t(110), // 10s elapsed < 60s
        );
        assert_eq!(action, FireAction::Skip);
    }

    #[test]
    fn suppress_whole_target_window_boundary_rearms() {
        let action = suppress_dispatched(
            FireAction::FireWholeTarget,
            &HashMap::new(),
            Some(t(100)),
            ChronoDuration::seconds(60),
            t(160), // exactly 60s
        );
        assert_eq!(action, FireAction::FireWholeTarget);
    }

    #[test]
    fn suppress_whole_target_no_prior_dispatch_fires() {
        let action = suppress_dispatched(
            FireAction::FireWholeTarget,
            &HashMap::new(),
            None,
            ChronoDuration::seconds(60),
            t(0),
        );
        assert_eq!(action, FireAction::FireWholeTarget);
    }
}
