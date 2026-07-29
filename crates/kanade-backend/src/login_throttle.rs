//! In-memory login throttling for the public `POST /api/auth/login` route
//! (#1191). The login is single-factor and internet-reachable behind Caddy,
//! so an unthrottled password guess is the softest surface on a public
//! deployment.
//!
//! Two independent limits, both counting only *failed* attempts:
//!
//!   * **per-account lockout** — after [`MAX_USER_FAILURES`] consecutive
//!     failures for a username, reject for [`USER_LOCKOUT`], regardless of
//!     source IP. This is the primary defence: a targeted brute force against
//!     a known operator stalls even from a botnet of addresses.
//!   * **per-IP window** — at most [`IP_MAX_ATTEMPTS`] failures per
//!     [`IP_WINDOW`] from one address, to blunt username spraying.
//!
//! A successful login clears the username's failure state. State is
//! in-memory (the backend is single-node) and pruned lazily so a spray of
//! random source IPs can't grow the maps without bound.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive failures for one username before it locks.
const MAX_USER_FAILURES: u32 = 5;
/// How long a username stays locked after tripping the threshold.
const USER_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failed attempts allowed from one IP within [`IP_WINDOW`].
const IP_MAX_ATTEMPTS: u32 = 30;
const IP_WINDOW: Duration = Duration::from_secs(60);
/// Prune expired entries once a map grows past this many keys (bounds memory
/// against random-IP spraying).
const PRUNE_AT: usize = 4096;

#[derive(Default)]
struct UserState {
    failures: u32,
    locked_until: Option<Instant>,
}

struct IpState {
    count: u32,
    window_start: Instant,
}

/// Shared login-attempt tracker. Cheap to clone-wrap in an `Arc` on
/// [`AppState`](crate::api::AppState).
#[derive(Default)]
pub struct LoginThrottle {
    users: Mutex<HashMap<String, UserState>>,
    ips: Mutex<HashMap<String, IpState>>,
}

impl LoginThrottle {
    /// May this `(username, ip)` attempt a login now? `Err(secs)` = throttled,
    /// wait `secs` seconds (surface it as `429` + `Retry-After`).
    pub fn check(&self, username: &str, ip: &str) -> Result<(), u64> {
        self.check_at(username, ip, Instant::now())
    }

    /// Record a failed attempt. Trips the per-account lock at the threshold
    /// and advances the per-IP window.
    pub fn record_failure(&self, username: &str, ip: &str) {
        self.record_failure_at(username, ip, Instant::now());
    }

    /// Clear a username's failure state after a successful login.
    pub fn record_success(&self, username: &str) {
        self.users.lock().unwrap().remove(username);
    }

    // ---- time-injected cores (testable without sleeping) ----------------

    fn check_at(&self, username: &str, ip: &str, now: Instant) -> Result<(), u64> {
        if let Some(u) = self.users.lock().unwrap().get(username) {
            if let Some(until) = u.locked_until {
                if until > now {
                    return Err((until - now).as_secs() + 1);
                }
            }
        }
        if let Some(s) = self.ips.lock().unwrap().get(ip) {
            let elapsed = now.saturating_duration_since(s.window_start);
            if elapsed < IP_WINDOW && s.count >= IP_MAX_ATTEMPTS {
                return Err((IP_WINDOW - elapsed).as_secs() + 1);
            }
        }
        Ok(())
    }

    fn record_failure_at(&self, username: &str, ip: &str, now: Instant) {
        {
            let mut users = self.users.lock().unwrap();
            if users.len() >= PRUNE_AT {
                users.retain(|_, u| u.locked_until.is_some_and(|t| t > now));
            }
            let u = users.entry(username.to_string()).or_default();
            // A lock that has already expired resets the streak so the account
            // gets a fresh set of attempts rather than re-locking on failure 1.
            if u.locked_until.is_some_and(|t| t <= now) {
                u.failures = 0;
                u.locked_until = None;
            }
            u.failures += 1;
            if u.failures >= MAX_USER_FAILURES {
                u.locked_until = Some(now + USER_LOCKOUT);
            }
        }
        {
            let mut ips = self.ips.lock().unwrap();
            if ips.len() >= PRUNE_AT {
                ips.retain(|_, s| now.saturating_duration_since(s.window_start) < IP_WINDOW);
            }
            let s = ips.entry(ip.to_string()).or_insert(IpState {
                count: 0,
                window_start: now,
            });
            if now.saturating_duration_since(s.window_start) >= IP_WINDOW {
                s.count = 0;
                s.window_start = now;
            }
            s.count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_locks_after_threshold_and_clears_on_success() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..MAX_USER_FAILURES {
            assert!(t.check_at("admin", "1.1.1.1", now).is_ok());
            t.record_failure_at("admin", "1.1.1.1", now);
        }
        // Locked now, from any IP.
        assert!(t.check_at("admin", "9.9.9.9", now).is_err());
        // A different account is unaffected.
        assert!(t.check_at("other", "9.9.9.9", now).is_ok());
        // Lock expires.
        let later = now + USER_LOCKOUT + Duration::from_secs(1);
        assert!(t.check_at("admin", "1.1.1.1", later).is_ok());
        // A success wipes the streak so the account is clean immediately.
        t.record_success("admin");
        assert!(t.check_at("admin", "1.1.1.1", now).is_ok());
    }

    #[test]
    fn expired_lock_gives_a_fresh_attempt_budget() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        for _ in 0..MAX_USER_FAILURES {
            t.record_failure_at("u", "ip", now);
        }
        let later = now + USER_LOCKOUT + Duration::from_secs(1);
        // One failure after expiry must NOT immediately re-lock.
        t.record_failure_at("u", "ip", later);
        assert!(t.check_at("u", "otherip", later).is_ok());
    }

    #[test]
    fn ip_window_caps_spray_then_resets() {
        let t = LoginThrottle::default();
        let now = Instant::now();
        // Spray distinct usernames from one IP so no per-account lock fires
        // first; the per-IP window is what should stop it.
        for i in 0..IP_MAX_ATTEMPTS {
            let user = format!("u{i}");
            assert!(t.check_at(&user, "5.5.5.5", now).is_ok());
            t.record_failure_at(&user, "5.5.5.5", now);
        }
        assert!(t.check_at("unext", "5.5.5.5", now).is_err());
        // A different IP is unaffected.
        assert!(t.check_at("unext", "6.6.6.6", now).is_ok());
        // The window rolls over.
        let later = now + IP_WINDOW + Duration::from_secs(1);
        assert!(t.check_at("unext", "5.5.5.5", later).is_ok());
    }
}
