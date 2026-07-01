//! Execution-tier guard (#vuln-roadmap). A job with `tier: controller`
//! may run ONLY on the trusted `controller_group` runners — never on
//! employee endpoints. Both dispatch paths (operator `kanade exec` and the
//! scheduler) intersect the job's target with this group's members before
//! publishing; an **unset group, or any read failure, dispatches nowhere**
//! (fail closed), so an external `feed:` fetch can never land on an
//! endpoint by accident.

use std::collections::HashSet;

use kanade_shared::manifest::{Manifest, Target, Tier};

use crate::api::AppState;

/// True if `m` must be confined to the controller tier. This is the **sole
/// confinement chokepoint** (`exec_manifest` calls it before publishing on
/// both the operator and scheduler paths), so any condition that should
/// force controller-tier dispatch MUST be added here.
///
/// Two conditions force confinement:
///   1. an explicit `tier: controller`, and
///   2. a non-empty `feed:` block — feeds fetch external URLs and must never
///      run on an employee endpoint, so the hint *implies* the controller
///      tier (`Manifest::validate()` already rejects the contradictory
///      `feed:` + `tier: endpoint`).
///
/// An unrecognised tier (`Tier::Unknown`) never reaches here because
/// `Manifest::validate()` already rejects it (fail closed). Any future
/// privileged hint that must be confined adds its condition HERE — this is
/// the sole gate, not a scattered audit.
pub fn requires_controller(m: &Manifest) -> bool {
    matches!(m.tier, Some(Tier::Controller)) || !m.feed.is_empty()
}

/// The set of **alive** controller-tier runner pc_ids, or `None` when
/// `controller_group` is unset.
///
/// `Err` on a settings/roster read failure so callers fail **closed** (a
/// controller job must never widen to endpoints because a KV read blipped).
/// `Some(set)` may be empty — the group is configured but no member is
/// currently online; the caller decides whether that's an error (exec) or a
/// skipped fire (scheduler).
pub async fn member_set(s: &AppState) -> anyhow::Result<Option<HashSet<String>>> {
    let settings = crate::api::server_settings::load(s).await?;
    let Some(group) = settings.effective_controller_group() else {
        return Ok(None);
    };
    // alive_only = true: a runner must be online to take the job, matching
    // the dispatch semantics of `resolve_expected_pcs`.
    let members = crate::scheduler::resolve_roster(
        s,
        &Target {
            all: false,
            groups: vec![group.to_string()],
            pcs: Vec::new(),
        },
        true,
    )
    .await?;
    Ok(Some(members.into_iter().collect()))
}

/// Result of confining a requested pc set to the controller tier.
#[derive(Debug, PartialEq, Eq)]
pub enum Constrained {
    /// Dispatch to exactly these pcs (guaranteed non-empty).
    Pcs(Vec<String>),
    /// `controller_group` is unset — fail closed, run nowhere.
    GroupUnset,
    /// The group is set but no requested pc is an (online) member.
    NoMember,
}

/// Intersect the operator-`requested` pcs with the controller `members`
/// (`None` ⇒ group unset). Pure + infrastructure-free so the dispatch
/// decision is unit-testable; the async `member_set` / target resolution
/// feed it. An `all` target resolves to the whole roster upstream, so the
/// intersection is "all controller runners".
pub fn constrain(requested: Vec<String>, members: &Option<HashSet<String>>) -> Constrained {
    match members {
        None => Constrained::GroupUnset,
        Some(m) => {
            let pcs: Vec<String> = requested.into_iter().filter(|p| m.contains(p)).collect();
            if pcs.is_empty() {
                Constrained::NoMember
            } else {
                Constrained::Pcs(pcs)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn manifest(extra: &str) -> Manifest {
        serde_yaml::from_str(&format!(
            "id: j\nversion: 0.0.1\nexecute:\n  shell: powershell\n  script: x\n  timeout: 30s\n{extra}"
        ))
        .expect("manifest parse")
    }

    #[test]
    fn requires_controller_for_tier_and_feed() {
        // A plain endpoint job is unconfined.
        assert!(!requires_controller(&manifest("")));
        // An explicit controller tier confines it.
        assert!(requires_controller(&manifest("tier: controller\n")));
        // A `feed:` block confines it even with no explicit tier (the
        // hint implies controller — the single gate enforces it).
        assert!(requires_controller(&manifest(
            "feed:\n  - id: cisa-kev\n    field: vulnerabilities\n    primary_key: [cveID]\n"
        )));
    }

    #[test]
    fn unset_group_fails_closed() {
        assert_eq!(
            constrain(vec!["minipc".into()], &None),
            Constrained::GroupUnset
        );
    }

    #[test]
    fn intersects_requested_with_members() {
        let members = Some(set(&["minipc", "dmz1"]));
        // `all` upstream → requested = whole roster; only controller members survive.
        let out = constrain(
            vec!["minipc".into(), "laptop7".into(), "dmz1".into()],
            &members,
        );
        match out {
            Constrained::Pcs(mut pcs) => {
                pcs.sort();
                assert_eq!(pcs, vec!["dmz1".to_string(), "minipc".to_string()]);
            }
            other => panic!("expected Pcs, got {other:?}"),
        }
    }

    #[test]
    fn no_overlap_is_no_member() {
        let members = Some(set(&["minipc"]));
        assert_eq!(
            constrain(vec!["laptop7".into()], &members),
            Constrained::NoMember
        );
    }

    #[test]
    fn empty_member_set_is_no_member() {
        // Group configured but no member online.
        assert_eq!(
            constrain(vec!["minipc".into()], &Some(HashSet::new())),
            Constrained::NoMember
        );
    }
}
