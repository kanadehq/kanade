//! Automatic toast/launch eligibility only. Never use this to authorize KLP,
//! filter session fan-out, mark a notification read, or block manual history.
//! A resolver must recheck these facts immediately before automatic delivery.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verification {
    Verified,
    Rejected,
    #[default]
    Unknown,
}

/// Adapters verify OS/version/architecture/desktop, not just environment strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Environment {
    MacOs14OrLaterAppleSilicon,
    Ubuntu2404X86_64GnomeX11,
    Ubuntu2404X86_64GnomeWayland,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Freshness {
    Current,
    Stale,
    #[default]
    Unknown,
}

/// Verified session identity for display routing; not a KLP session identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiTarget {
    pub uid: u32,
    pub session_id: String,
}

/// Unknown defaults ensure partial discovery cannot authorize automatic UI.
/// `local_active_gui` includes console/seat0 selection, user class and GUI
/// membership, excluding SSH/headless/remote and background sessions.
/// `unique` rejects multiple GUI sessions/seats even when the UID is the same.
/// `unlocked` requires concordant OS/helper evidence, never a missing lock hint.
#[derive(Debug, Clone, Default)]
pub struct GuiSessionFacts {
    pub target: Option<GuiTarget>,
    pub environment: Environment,
    pub local_active_gui: Verification,
    pub unlocked: Verification,
    pub unique: Verification,
    pub freshness: Freshness,
}

/// Discovery is OS-specific; errors and incomplete evidence become history-only.
/// No launch, notification API, credentials or process ownership live here.
pub trait GuiSessionResolver {
    type Error;
    fn resolve(&self, target: &GuiTarget) -> Result<GuiSessionFacts, Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum HistoryReason {
    IdentityUnverified,
    UnsupportedEnvironment,
    SessionNotVerified,
    LockNotVerified,
    AmbiguousSession,
    StaleOrUnknown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AutomaticDelivery {
    Eligible,
    HistoryOnly(HistoryReason),
}

pub fn automatic_delivery(target: &GuiTarget, facts: &GuiSessionFacts) -> AutomaticDelivery {
    use HistoryReason::*;
    let reason = if target.session_id.is_empty() || facts.target.as_ref() != Some(target) {
        Some(IdentityUnverified)
    } else if matches!(
        facts.environment,
        Environment::Unsupported | Environment::Unknown
    ) {
        Some(UnsupportedEnvironment)
    } else if facts.freshness != Freshness::Current {
        Some(StaleOrUnknown)
    } else if facts.unique != Verification::Verified {
        Some(AmbiguousSession)
    } else if facts.local_active_gui != Verification::Verified {
        Some(SessionNotVerified)
    } else if facts.unlocked != Verification::Verified {
        Some(LockNotVerified)
    } else {
        None
    };
    reason.map_or(AutomaticDelivery::Eligible, AutomaticDelivery::HistoryOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> GuiTarget {
        GuiTarget {
            uid: 1001,
            session_id: "gui-1".into(),
        }
    }
    fn verified() -> GuiSessionFacts {
        GuiSessionFacts {
            target: Some(target()),
            environment: Environment::Ubuntu2404X86_64GnomeWayland,
            local_active_gui: Verification::Verified,
            unlocked: Verification::Verified,
            unique: Verification::Verified,
            freshness: Freshness::Current,
        }
    }

    #[test]
    fn only_complete_current_local_unlocked_gui_is_eligible() {
        for environment in [
            Environment::MacOs14OrLaterAppleSilicon,
            Environment::Ubuntu2404X86_64GnomeX11,
            Environment::Ubuntu2404X86_64GnomeWayland,
        ] {
            let mut facts = verified();
            facts.environment = environment;
            assert_eq!(
                automatic_delivery(&target(), &facts),
                AutomaticDelivery::Eligible
            );
        }
        assert!(matches!(
            automatic_delivery(&target(), &Default::default()),
            AutomaticDelivery::HistoryOnly(_)
        ));
    }

    #[test]
    fn locked_unknown_stale_ambiguous_ssh_headless_and_unsupported_use_history() {
        let mut cases = Vec::new();
        for evidence in [Verification::Rejected, Verification::Unknown] {
            let mut f = verified();
            f.unlocked = evidence;
            cases.push(f);
            let mut f = verified();
            f.unique = evidence;
            cases.push(f);
            let mut f = verified();
            f.local_active_gui = evidence;
            cases.push(f);
        }
        for freshness in [Freshness::Stale, Freshness::Unknown] {
            let mut f = verified();
            f.freshness = freshness;
            cases.push(f);
        }
        for environment in [Environment::Unsupported, Environment::Unknown] {
            let mut f = verified();
            f.environment = environment;
            cases.push(f);
        }
        let mut f = verified();
        f.target.as_mut().unwrap().uid = 1002;
        cases.push(f);
        let mut f = verified();
        f.target.as_mut().unwrap().session_id = "gui-2".into();
        cases.push(f);
        for facts in cases {
            assert!(
                matches!(
                    automatic_delivery(&target(), &facts),
                    AutomaticDelivery::HistoryOnly(_)
                ),
                "{facts:?}"
            );
            // Manual history is deliberately not an input/output of this policy.
            assert!(crate::startup::StartupIntent::default().initially_visible());
        }
    }
}
