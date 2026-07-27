//! Support-mode unlock grants — the process-wide half of the
//! `client.unlock` gate.
//!
//! The IT desk types an operator-issued code into the Client App
//! (`support.unlock`); the agent verifies it against the argon2id hashes in
//! `ServerSettings::support_codes` and records a **grant** here. While a
//! grant is live, `jobs.list` reveals that scope's jobs; when it lapses they
//! drop out of the catalog again.
//!
//! The gate is **listing-only** — `jobs.execute` does not consult it, so a
//! visible row is always runnable and there is no race between the two. That
//! makes this a UX affordance rather than a security boundary; see
//! `ClientHint::unlock`. Everything below is still written to fail closed,
//! because a grant that outlives its window or leaks to the wrong OS user
//! would put helpdesk-only buttons in front of an end user, which is a real
//! (if non-catastrophic) failure.
//!
//! Two design choices are load-bearing:
//!
//! - **Grants are keyed by OS user (SID), not by connection.** The Client
//!   App reconnects on its own — a pipe hiccup, a restart from the tray, the
//!   #468 supervisor — and per-connection state would re-lock the machine
//!   mid-support-call with no explanation and no way for the user to tell the
//!   desk what happened. The SID is captured from the OS at connect time
//!   (never from the payload), so keying on it can't be spoofed by a client.
//!   Grants die with the agent process, which is the correct fail-closed
//!   behaviour for a reboot or a service restart.
//!
//! - **Expiry is enforced on a monotonic clock.** The stored deadline is an
//!   [`Instant`]; the wall-clock `expires_at` on the wire exists only so the
//!   Client App can render a countdown. A user who cannot otherwise extend a
//!   grant must not be able to extend it by winding the system clock back,
//!   and on Windows an end user often can (`SeSystemtimePrivilege` is granted
//!   to Users in some default policies).
//!
//! Wrong codes are rate-limited per SID: a short code typed at a keyboard is
//! guessable at machine speed otherwise, and the attacker here is sitting at
//! the machine with an interactive session.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use kanade_shared::ipc::support::UnlockGrant;

/// Failed-attempt budget per OS user before the agent stops answering
/// unlock attempts at all. Sized for a human at a keyboard: mistyping an
/// operator code twice in a row is ordinary, five times is not.
const MAX_FAILURES: u32 = 5;

/// How long the failure count is remembered. Attempts spread thinner than
/// this never accumulate into a lockout, so an honest user who mistypes
/// once a week is never affected.
const FAILURE_WINDOW: Duration = Duration::from_secs(300);

/// How long unlock attempts are refused once the budget is spent. Cheap for
/// the desk (retry after the call resumes) and ruinous for a guesser: five
/// tries per five minutes caps a 6-character alphanumeric code at millennia.
const LOCKOUT: Duration = Duration::from_secs(300);

/// One live grant. `deadline` is what the gates check; `expires_at` is the
/// display copy handed to the client (see the module docs on why the two
/// clocks differ).
#[derive(Debug, Clone)]
struct Grant {
    scope: String,
    label: Option<String>,
    deadline: Instant,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Failed-attempt bookkeeping for one OS user.
#[derive(Debug, Clone, Default)]
struct Failures {
    count: u32,
    /// Start of the window `count` accumulated in; a first failure after
    /// [`FAILURE_WINDOW`] has elapsed restarts the count from 1.
    window_start: Option<Instant>,
    /// Set once the budget is spent; attempts are refused until it passes.
    blocked_until: Option<Instant>,
}

/// Process-wide grant store: OS user SID → that user's live grants.
///
/// A `std::sync::Mutex` (not tokio's): every critical section here is a few
/// map operations with no `.await` inside, so an async mutex would add a
/// scheduler round-trip for nothing. Poisoning is handled by recovering the
/// guard — a panic while holding this lock must not wedge unlocking for the
/// life of the process, and the data behind it has no invariant a panic
/// could have half-broken.
static GRANTS: LazyLock<Mutex<HashMap<String, Vec<Grant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-wide failed-attempt counters: OS user SID → [`Failures`].
static FAILURES: LazyLock<Mutex<HashMap<String, Failures>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock a static, recovering from poisoning. See [`GRANTS`].
fn lock_recover<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record a grant for `sid`, replacing any existing grant for the same
/// scope (re-entering the code refreshes the window rather than stacking
/// duplicate entries). Returns the caller's full grant set afterwards.
pub fn grant(sid: &str, scope: &str, label: Option<String>, ttl_minutes: u32) -> Vec<UnlockGrant> {
    let ttl = Duration::from_secs(u64::from(ttl_minutes) * 60);
    let now = Instant::now();
    let mut map = lock_recover(&GRANTS);
    let entry = map.entry(sid.to_string()).or_default();
    entry.retain(|g| g.deadline > now && g.scope != scope);
    entry.push(Grant {
        scope: scope.to_string(),
        label,
        deadline: now + ttl,
        // Derived from the same TTL, so the countdown the user sees matches
        // the deadline the gates enforce — as long as the wall clock behaves.
        // If it doesn't, the monotonic deadline is the one that decides.
        expires_at: chrono::Utc::now()
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::minutes(15)),
    });
    to_wire(entry, now)
}

/// The caller's live grants, expired ones swept first. Empty ⇒ locked.
pub fn grants(sid: &str) -> Vec<UnlockGrant> {
    let now = Instant::now();
    let mut map = lock_recover(&GRANTS);
    let Some(entry) = map.get_mut(sid) else {
        return Vec::new();
    };
    entry.retain(|g| g.deadline > now);
    if entry.is_empty() {
        map.remove(sid);
        return Vec::new();
    }
    to_wire(entry, now)
}

/// Whether `sid` may currently **see** jobs gated on `scope`. Consulted by
/// the `jobs.list` filter; the run path deliberately does not (see the module
/// docs), so anything this reveals stays runnable even once it lapses.
pub fn holds(sid: &str, scope: &str) -> bool {
    let now = Instant::now();
    let map = lock_recover(&GRANTS);
    map.get(sid)
        .is_some_and(|gs| gs.iter().any(|g| g.scope == scope && g.deadline > now))
}

/// Drop every grant `sid` holds. Returns how many were dropped (0 when the
/// user held none — locking is idempotent).
pub fn lock(sid: &str) -> usize {
    let now = Instant::now();
    let mut map = lock_recover(&GRANTS);
    match map.remove(sid) {
        // Count only the ones that were actually still in force, so the
        // client isn't told it released a grant that had already lapsed.
        Some(gs) => gs.iter().filter(|g| g.deadline > now).count(),
        None => 0,
    }
}

fn to_wire(grants: &[Grant], now: Instant) -> Vec<UnlockGrant> {
    grants
        .iter()
        .filter(|g| g.deadline > now)
        .map(|g| UnlockGrant {
            scope: g.scope.clone(),
            label: g.label.clone(),
            expires_at: g.expires_at,
        })
        .collect()
}

/// How much longer `sid` is locked out of unlock attempts, if it is.
/// `None` ⇒ attempts are allowed right now.
pub fn lockout_remaining(sid: &str) -> Option<Duration> {
    let now = Instant::now();
    let mut map = lock_recover(&FAILURES);
    let f = map.get_mut(sid)?;
    match f.blocked_until {
        Some(until) if until > now => Some(until - now),
        // Lockout served: clear it (and the count that caused it) so the
        // next attempt starts from a clean budget.
        Some(_) => {
            *f = Failures::default();
            None
        }
        None => None,
    }
}

/// Record a failed attempt, escalating to a lockout once the budget within
/// [`FAILURE_WINDOW`] is spent. Returns the lockout it just imposed, if any.
pub fn record_failure(sid: &str) -> Option<Duration> {
    let now = Instant::now();
    let mut map = lock_recover(&FAILURES);
    let f = map.entry(sid.to_string()).or_default();
    // A stale window restarts the count — failures have to be *bunched* to
    // look like guessing.
    let fresh = f
        .window_start
        .is_none_or(|start| now.duration_since(start) > FAILURE_WINDOW);
    if fresh {
        f.count = 0;
        f.window_start = Some(now);
    }
    f.count += 1;
    if f.count >= MAX_FAILURES {
        f.blocked_until = Some(now + LOCKOUT);
        return Some(LOCKOUT);
    }
    None
}

/// Clear the failure budget for `sid` after a successful unlock, so a desk
/// that fumbled the code twice before getting it right doesn't carry those
/// strikes into the next call.
pub fn clear_failures(sid: &str) {
    lock_recover(&FAILURES).remove(sid);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stores are process-wide, so every test uses its own SID rather
    /// than resetting shared state — that keeps them independent under the
    /// default parallel test runner.
    fn sid(tag: &str) -> String {
        format!("S-1-5-21-test-{tag}")
    }

    #[test]
    fn grant_then_holds_then_lock() {
        let s = sid("basic");
        assert!(!holds(&s, "support"), "starts locked");
        let g = grant(&s, "support", Some("ヘルプデスク".into()), 15);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].scope, "support");
        assert_eq!(g[0].label.as_deref(), Some("ヘルプデスク"));
        assert!(holds(&s, "support"));
        assert!(!holds(&s, "admin"), "a grant opens only its own scope");
        assert_eq!(lock(&s), 1);
        assert!(!holds(&s, "support"), "lock closes it");
        assert_eq!(lock(&s), 0, "lock is idempotent");
    }

    #[test]
    fn grants_are_per_user() {
        // Two users on a shared PC: unlocking for one must not unlock for
        // the other, or a helpdesk call would leave the machine open to
        // whoever logs in next.
        let a = sid("user-a");
        let b = sid("user-b");
        grant(&a, "support", None, 15);
        assert!(holds(&a, "support"));
        assert!(!holds(&b, "support"));
    }

    #[test]
    fn re_granting_a_scope_refreshes_instead_of_duplicating() {
        let s = sid("refresh");
        grant(&s, "support", None, 15);
        let g = grant(&s, "support", None, 30);
        assert_eq!(g.len(), 1, "one entry per scope: {g:?}");
    }

    #[test]
    fn multiple_scopes_coexist() {
        let s = sid("multi");
        grant(&s, "support", None, 15);
        let g = grant(&s, "admin", None, 15);
        assert_eq!(g.len(), 2);
        assert!(holds(&s, "support") && holds(&s, "admin"));
    }

    #[test]
    fn expired_grants_stop_holding_and_are_swept() {
        let s = sid("expiry");
        // Reach past the public API to plant an already-lapsed grant: the
        // TTL is in whole minutes, so there's no way to author this through
        // `grant()` without sleeping for a minute in the test.
        let past = Instant::now() - Duration::from_secs(1);
        lock_recover(&GRANTS).insert(
            s.clone(),
            vec![Grant {
                scope: "support".into(),
                label: None,
                deadline: past,
                expires_at: chrono::Utc::now(),
            }],
        );
        assert!(!holds(&s, "support"), "expired grant must not hold");
        assert!(grants(&s).is_empty(), "expired grant must not be listed");
        assert!(
            !lock_recover(&GRANTS).contains_key(&s),
            "the empty entry is swept, not left to accumulate",
        );
    }

    #[test]
    fn expired_grant_does_not_block_a_fresh_one() {
        let s = sid("expiry-regrant");
        lock_recover(&GRANTS).insert(
            s.clone(),
            vec![Grant {
                scope: "support".into(),
                label: None,
                deadline: Instant::now() - Duration::from_secs(1),
                expires_at: chrono::Utc::now(),
            }],
        );
        let g = grant(&s, "support", None, 15);
        assert_eq!(g.len(), 1, "the lapsed entry was replaced, not kept: {g:?}");
        assert!(holds(&s, "support"));
    }

    #[test]
    fn failures_escalate_to_a_lockout() {
        let s = sid("lockout");
        assert!(lockout_remaining(&s).is_none(), "starts unblocked");
        for i in 1..MAX_FAILURES {
            assert!(
                record_failure(&s).is_none(),
                "attempt {i} must not lock out yet"
            );
            assert!(lockout_remaining(&s).is_none());
        }
        assert!(
            record_failure(&s).is_some(),
            "the budgeted attempt imposes the lockout"
        );
        assert!(lockout_remaining(&s).is_some(), "and it is in force");
    }

    #[test]
    fn a_success_clears_the_failure_budget() {
        let s = sid("clear");
        record_failure(&s);
        record_failure(&s);
        clear_failures(&s);
        // Back to a full budget: the next MAX_FAILURES-1 attempts must not
        // trip the lockout.
        for _ in 1..MAX_FAILURES {
            assert!(record_failure(&s).is_none());
        }
    }

    #[test]
    fn a_served_lockout_expires_and_resets_the_count() {
        let s = sid("served");
        {
            let mut map = lock_recover(&FAILURES);
            map.insert(
                s.clone(),
                Failures {
                    count: MAX_FAILURES,
                    window_start: Some(Instant::now() - Duration::from_secs(1)),
                    blocked_until: Some(Instant::now() - Duration::from_secs(1)),
                },
            );
        }
        assert!(
            lockout_remaining(&s).is_none(),
            "a lockout in the past is served"
        );
        assert!(
            record_failure(&s).is_none(),
            "and the count restarted, so one more failure doesn't re-lock",
        );
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let s = sid("window");
        {
            let mut map = lock_recover(&FAILURES);
            map.insert(
                s.clone(),
                Failures {
                    count: MAX_FAILURES - 1,
                    window_start: Some(Instant::now() - FAILURE_WINDOW - Duration::from_secs(1)),
                    blocked_until: None,
                },
            );
        }
        assert!(
            record_failure(&s).is_none(),
            "a failure long after the window restarts the count",
        );
    }
}
