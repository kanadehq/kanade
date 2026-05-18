//! Pure dedup policy for v0.19 schedule modes. Kept side-effect-
//! free so the boundary semantics are pinned by unit tests — the
//! `scheduler::run` async loop just orchestrates "resolve target →
//! fetch completions → call [`decide_fire`] → publish / persist".
//!
//! Semantics (covered by tests below):
//!
//! * `EveryTick` — always [`FireAction::FireWholeTarget`]. No dedup.
//! * `OncePerPc` — for each `expected_pcs` entry, fire unless that
//!   pc has a completion within the cooldown window. `cooldown =
//!   None` ⇒ "succeed once ⇒ permanent skip".
//! * `OncePerTarget` — the whole target is gated by the most
//!   recent completion *among current target members*. Once it
//!   passes (or cooldown expires), fire the whole target.
//! * `auto_disable_when_done` triggers only when the schedule's
//!   lifecycle is permanently terminated (`cooldown == None` AND
//!   actual completions exist that satisfy the mode). Cooldown'd
//!   schedules never auto-disable because they always re-arm.

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

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub action: FireAction,
    /// When true, the caller should persist the schedule with
    /// `enabled = false` and emit `schedule_completed` audit.
    /// Only set when the lifecycle is permanently terminated
    /// (`cooldown == None` AND mode dedup says everything's
    /// done AND we've actually seen the completion that
    /// terminated it).
    pub auto_disable: bool,
}

/// Pure policy decision. All inputs are caller-resolved snapshots;
/// the function is free of IO so its behavior is locked down by the
/// unit tests below.
pub fn decide_fire(
    mode: ExecMode,
    cooldown: Option<ChronoDuration>,
    auto_disable_when_done: bool,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> Decision {
    match mode {
        ExecMode::EveryTick => Decision {
            action: FireAction::FireWholeTarget,
            auto_disable: false,
        },
        ExecMode::OncePerPc => decide_once_per_pc(
            cooldown,
            auto_disable_when_done,
            expected_pcs,
            completions,
            now,
        ),
        ExecMode::OncePerTarget => decide_once_per_target(
            cooldown,
            auto_disable_when_done,
            expected_pcs,
            completions,
            now,
        ),
    }
}

fn decide_once_per_pc(
    cooldown: Option<ChronoDuration>,
    auto_disable_when_done: bool,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> Decision {
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
    let mut all_done = !expected_pcs.is_empty();
    for pc in expected_pcs {
        let armed = match last_success.get(pc.as_str()) {
            None => {
                all_done = false;
                true
            }
            Some(t) => match cooldown {
                None => false, // succeeded ever ⇒ permanently skip
                Some(cd) => {
                    // Boundary: re-arm when elapsed ≥ cooldown.
                    let elapsed = now.signed_duration_since(*t);
                    if elapsed >= cd {
                        all_done = false;
                        true
                    } else {
                        false
                    }
                }
            },
        };
        if armed {
            eligible.push(pc.clone());
        }
    }

    if eligible.is_empty() {
        let auto = auto_disable_when_done && cooldown.is_none() && all_done;
        return Decision {
            action: FireAction::Skip,
            auto_disable: auto,
        };
    }
    Decision {
        action: FireAction::FirePcs(eligible),
        auto_disable: false,
    }
}

fn decide_once_per_target(
    cooldown: Option<ChronoDuration>,
    auto_disable_when_done: bool,
    expected_pcs: &[String],
    completions: &[Completion],
    now: DateTime<Utc>,
) -> Decision {
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

    if armed {
        if expected_pcs.is_empty() {
            return Decision {
                action: FireAction::Skip,
                auto_disable: false,
            };
        }
        return Decision {
            action: FireAction::FireWholeTarget,
            auto_disable: false,
        };
    }
    // Disarmed: auto_disable only when there's an in-scope
    // completion that's keeping us muted AND cooldown is None.
    let auto = auto_disable_when_done && cooldown.is_none() && latest.is_some();
    Decision {
        action: FireAction::Skip,
        auto_disable: auto,
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
        let d = decide_fire(
            ExecMode::EveryTick,
            None,
            true,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1)],
            t(100),
        );
        assert_eq!(d.action, FireAction::FireWholeTarget);
        assert!(!d.auto_disable);
    }

    #[test]
    fn every_tick_ignores_empty_expected() {
        // EveryTick fires at original target subjects regardless of
        // expected_pcs enumeration — the broker handles fan-out.
        let d = decide_fire(ExecMode::EveryTick, None, false, &[], &[], t(0));
        assert_eq!(d.action, FireAction::FireWholeTarget);
    }

    // ── OncePerPc: no-cooldown lifecycle ─────────────────────

    #[test]
    fn once_per_pc_no_completions_fires_every_pc() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            None,
            false,
            &pcs(&["a", "b", "c"]),
            &[],
            t(0),
        );
        assert_eq!(d.action, FireAction::FirePcs(pcs(&["a", "b", "c"])));
        assert!(!d.auto_disable);
    }

    #[test]
    fn once_per_pc_partial_completion_fires_remainder() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            None,
            false,
            &pcs(&["a", "b", "c"]),
            &[done("a", 0)],
            t(100),
        );
        assert_eq!(d.action, FireAction::FirePcs(pcs(&["b", "c"])));
        assert!(!d.auto_disable);
    }

    #[test]
    fn once_per_pc_all_completed_skips() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            None,
            false,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1)],
            t(100),
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(!d.auto_disable);
    }

    #[test]
    fn once_per_pc_all_completed_with_auto_disable() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            None,
            true,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1)],
            t(100),
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(d.auto_disable);
    }

    #[test]
    fn once_per_pc_empty_expected_does_not_auto_disable() {
        // Empty target ≠ "all done" — operator might just need to
        // wait for pcs to heartbeat. Don't auto-disable on the
        // first tick before any pc has shown up.
        let d = decide_fire(ExecMode::OncePerPc, None, true, &[], &[], t(0));
        assert_eq!(d.action, FireAction::Skip);
        assert!(!d.auto_disable);
    }

    // ── OncePerPc: cooldown boundaries ───────────────────────

    #[test]
    fn once_per_pc_cooldown_recent_excluded() {
        // Finished at t=100, now=110, cooldown=60s → 10s elapsed →
        // not yet re-armed.
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a"]),
            &[done("a", 100)],
            t(110),
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(!d.auto_disable, "cooldown'd schedules never auto-disable");
    }

    #[test]
    fn once_per_pc_cooldown_exactly_at_boundary_rearmed() {
        // Boundary policy: elapsed >= cooldown re-arms.
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a"]),
            &[done("a", 100)],
            t(160), // exactly 60s
        );
        assert_eq!(d.action, FireAction::FirePcs(pcs(&["a"])));
    }

    #[test]
    fn once_per_pc_cooldown_one_second_before_boundary_excluded() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a"]),
            &[done("a", 100)],
            t(159),
        );
        assert_eq!(d.action, FireAction::Skip);
    }

    #[test]
    fn once_per_pc_cooldown_past_boundary_rearmed() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 100), done("b", 200)],
            t(500), // a:400s, b:300s past → both >= 60s
        );
        assert_eq!(d.action, FireAction::FirePcs(pcs(&["a", "b"])));
    }

    #[test]
    fn once_per_pc_cooldown_mix_one_armed_one_not() {
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 100), done("b", 150)],
            t(170), // a:70s past, b:20s past
        );
        assert_eq!(d.action, FireAction::FirePcs(pcs(&["a"])));
    }

    #[test]
    fn once_per_pc_uses_latest_success_per_pc() {
        // Multiple completion records for the same pc — only the
        // newest counts (so an old success doesn't outlast its
        // cooldown when a fresh one has reset it).
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a"]),
            &[done("a", 0), done("a", 150), done("a", 50)],
            t(170), // newest = 150 → elapsed 20s → still in cooldown
        );
        assert_eq!(d.action, FireAction::Skip);
    }

    #[test]
    fn once_per_pc_cooldown_no_auto_disable_when_in_cooldown() {
        // Permanently terminated only when cooldown is None.
        let d = decide_fire(
            ExecMode::OncePerPc,
            Some(ChronoDuration::hours(1)),
            true,
            &pcs(&["a"]),
            &[done("a", 0)],
            t(100), // still in cooldown
        );
        assert!(!d.auto_disable);
    }

    // ── OncePerPc: target shape edge cases ───────────────────

    #[test]
    fn once_per_pc_decommissioned_pc_completion_ignored() {
        // Old success from a pc that's no longer in the target —
        // doesn't shrink the eligible set, doesn't promote
        // auto_disable.
        let d = decide_fire(
            ExecMode::OncePerPc,
            None,
            true,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 1), done("ghost", 2)],
            t(100),
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(d.auto_disable);
    }

    // ── OncePerTarget: no-cooldown lifecycle ─────────────────

    #[test]
    fn once_per_target_no_success_fires_whole_target() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            None,
            false,
            &pcs(&["a", "b"]),
            &[],
            t(0),
        );
        assert_eq!(d.action, FireAction::FireWholeTarget);
        assert!(!d.auto_disable);
    }

    #[test]
    fn once_per_target_any_in_scope_success_skips() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            None,
            false,
            &pcs(&["a", "b"]),
            &[done("a", 0)],
            t(100),
        );
        assert_eq!(d.action, FireAction::Skip);
    }

    #[test]
    fn once_per_target_any_success_with_auto_disable() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            None,
            true,
            &pcs(&["a", "b"]),
            &[done("a", 0)],
            t(100),
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(d.auto_disable);
    }

    #[test]
    fn once_per_target_out_of_scope_success_does_not_count() {
        // Success exists, but the only pc that did it was
        // decommissioned (no longer in expected). Still armed.
        let d = decide_fire(
            ExecMode::OncePerTarget,
            None,
            true,
            &pcs(&["a", "b"]),
            &[done("ghost", 0)],
            t(100),
        );
        assert_eq!(d.action, FireAction::FireWholeTarget);
        assert!(
            !d.auto_disable,
            "no in-scope completion ⇒ schedule still hunting; never auto-disable on first tick"
        );
    }

    #[test]
    fn once_per_target_empty_expected_no_completion_skips() {
        let d = decide_fire(ExecMode::OncePerTarget, None, true, &[], &[], t(0));
        assert_eq!(d.action, FireAction::Skip);
        assert!(!d.auto_disable);
    }

    // ── OncePerTarget: cooldown boundaries ───────────────────

    #[test]
    fn once_per_target_cooldown_recent_skipped() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(110), // 10s elapsed < 60s cooldown
        );
        assert_eq!(d.action, FireAction::Skip);
        assert!(!d.auto_disable);
    }

    #[test]
    fn once_per_target_cooldown_exactly_at_boundary_rearmed() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(160), // exactly 60s elapsed
        );
        assert_eq!(d.action, FireAction::FireWholeTarget);
    }

    #[test]
    fn once_per_target_cooldown_one_second_before_boundary_skipped() {
        let d = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 100)],
            t(159),
        );
        assert_eq!(d.action, FireAction::Skip);
    }

    #[test]
    fn once_per_target_uses_most_recent_completion() {
        // Two completions across the target; the most recent gates
        // the cooldown calc.
        let d = decide_fire(
            ExecMode::OncePerTarget,
            Some(ChronoDuration::seconds(60)),
            false,
            &pcs(&["a", "b"]),
            &[done("a", 0), done("b", 200)], // newest = b@200
            t(259),                          // 59s elapsed since b — still locked
        );
        assert_eq!(d.action, FireAction::Skip);
    }
}
