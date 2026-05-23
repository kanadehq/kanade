//! Layer 2 staleness tracker (v0.26, spec §2.6.2).
//!
//! Tracks "when did we last definitely talk to the broker" so
//! [`commands::handle_command`](crate::commands) can decide whether a
//! `Staleness::Strict { max_cache_age }` Manifest should run or skip.
//!
//! Push-based via async-nats' `event_callback`: the agent's `main`
//! creates a [`Tracker`], hands its callback to
//! [`kanade_shared::nats_client::connect_with_event_callback`], and
//! every `Event::Connected` fires through the closure to stamp a
//! shared `Mutex<Option<Instant>>`. No polling — idle agents have
//! zero overhead from this tracker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::connection::State;
use tokio::sync::Notify;
use tokio::time::Instant;

/// Shared, cheap-to-clone handle to the agent's "last known
/// connected at" timestamp. Constructed up front so the
/// `event_callback` closure can capture its inner Arc before the
/// client even exists.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    inner: Arc<TrackerInner>,
}

#[derive(Debug, Default)]
struct TrackerInner {
    /// The most recent monotonic instant at which we received an
    /// `Event::Connected`. `None` ⇒ never connected since agent
    /// start.
    last_connected_at: Mutex<Option<Instant>>,
    /// v0.38 / #137: wake-on-Connected for offline-tolerant
    /// subsystems. `wait_connected()` futures attached before the
    /// next handshake observe `notify_waiters()` and proceed without
    /// burning through their exponential-backoff sleep.
    connected: Notify,
}

impl Tracker {
    /// Build an empty tracker. Pair with [`Self::on_event`] to get
    /// the callback to hand to `ConnectOptions::event_callback`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a closure suitable for
    /// `nats_client::connect_with_event_callback`. The closure
    /// captures only an `Arc<TrackerInner>` so it's cheap to box,
    /// `Send + Sync + 'static`, and survives the connect path's
    /// reconnect loop without rebuilding.
    pub fn on_event(
        &self,
    ) -> impl Fn(async_nats::Event) -> std::future::Ready<()> + Send + Sync + 'static + use<> {
        let inner = self.inner.clone();
        move |event| {
            if matches!(event, async_nats::Event::Connected) {
                if let Ok(mut g) = inner.last_connected_at.lock() {
                    *g = Some(Instant::now());
                }
                // v0.38 / #137: kick every wait_connected() future.
                // Subsystem retry loops park on this between backoff
                // sleeps, so this is what makes broker-recovery
                // catch-up feel instant instead of "happens whenever
                // the next 5-minute tick lands."
                inner.connected.notify_waiters();
            }
            std::future::ready(())
        }
    }

    /// Park until the next `Event::Connected` lands.
    ///
    /// **Not edge-aware**: a Notify only delivers to waiters present
    /// at the moment `notify_waiters()` is called. Callers must
    /// always pair this with a fallback timer (see `nats_retry`'s
    /// backoff `select!`) so a missed event — async-nats dispatches
    /// `Event::Connected` via `try_send` and silently drops if its
    /// channel is full — doesn't park the caller forever.
    pub async fn wait_connected(&self) {
        self.inner.connected.notified().await;
    }

    /// "How stale" is the agent's view of the broker right now?
    ///
    /// - `Duration::ZERO` when the client is currently `Connected`
    ///   AND the callback has stamped at least one Connected event
    ///   (cache provably up to date — NATS KV-watch push contract).
    /// - `now - last_connected_at` when we were once connected but
    ///   have since dropped.
    /// - `Duration::MAX` when the agent has never received a
    ///   `Connected` event (cold-start fire before the broker
    ///   handshake completed).
    pub fn staleness(&self, client: &async_nats::Client) -> Duration {
        // Fast path — currently Connected ⇒ cache is fresh.
        if client.connection_state() == State::Connected {
            return Duration::ZERO;
        }
        match self.inner.last_connected_at.lock().ok().and_then(|g| *g) {
            Some(t) => Instant::now().saturating_duration_since(t),
            None => Duration::MAX,
        }
    }
}

/// Pure decision: given a Manifest's [`Staleness`](kanade_shared::wire::Staleness)
/// policy and the current agent-side staleness reading, should the
/// agent fire the script or publish a synthetic skipped result?
///
/// Kept as a free function (no NATS / no async) so the tests can pin
/// every boundary case without spinning up a runtime.
pub fn decide(policy: &kanade_shared::wire::Staleness, staleness: Duration) -> StalenessDecision {
    use kanade_shared::wire::Staleness;
    match policy {
        Staleness::Unchecked | Staleness::Cached => StalenessDecision::Proceed,
        Staleness::Strict { max_cache_age } => {
            // Parse humantime up front. A bogus value (operator typo
            // in the Manifest) is treated as "0s" — the strictest
            // interpretation — so a malformed config fails closed
            // rather than open.
            let max = humantime::parse_duration(max_cache_age).unwrap_or(Duration::ZERO);
            if staleness <= max {
                StalenessDecision::Proceed
            } else {
                StalenessDecision::Skip {
                    observed: staleness,
                    allowed: max,
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StalenessDecision {
    Proceed,
    Skip {
        observed: Duration,
        allowed: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::wire::Staleness;

    #[test]
    fn cached_always_proceeds() {
        assert_eq!(
            decide(&Staleness::Cached, Duration::MAX),
            StalenessDecision::Proceed,
        );
        assert_eq!(
            decide(&Staleness::Cached, Duration::ZERO),
            StalenessDecision::Proceed,
        );
    }

    #[test]
    fn unchecked_always_proceeds() {
        assert_eq!(
            decide(&Staleness::Unchecked, Duration::MAX),
            StalenessDecision::Proceed,
        );
    }

    #[test]
    fn strict_zero_max_proceeds_when_currently_connected() {
        // `staleness = ZERO` ⇒ caller observed `connection_state() ==
        // Connected` right now. That satisfies `max_cache_age: 0s`.
        let policy = Staleness::Strict {
            max_cache_age: "0s".into(),
        };
        assert_eq!(decide(&policy, Duration::ZERO), StalenessDecision::Proceed);
    }

    #[test]
    fn strict_zero_max_skips_when_any_disconnect() {
        let policy = Staleness::Strict {
            max_cache_age: "0s".into(),
        };
        let result = decide(&policy, Duration::from_secs(1));
        match result {
            StalenessDecision::Skip { observed, allowed } => {
                assert_eq!(observed, Duration::from_secs(1));
                assert_eq!(allowed, Duration::ZERO);
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn strict_window_inclusive_boundary() {
        // Boundary: staleness == max_cache_age still proceeds (the
        // window is "within"). One micro past skips.
        let policy = Staleness::Strict {
            max_cache_age: "5m".into(),
        };
        assert_eq!(
            decide(&policy, Duration::from_secs(300)),
            StalenessDecision::Proceed
        );
        assert!(matches!(
            decide(&policy, Duration::from_secs(301)),
            StalenessDecision::Skip { .. }
        ));
    }

    #[test]
    fn strict_never_connected_skips() {
        let policy = Staleness::Strict {
            max_cache_age: "1h".into(),
        };
        assert!(matches!(
            decide(&policy, Duration::MAX),
            StalenessDecision::Skip { .. }
        ));
    }

    #[test]
    fn strict_bogus_max_cache_age_fails_closed() {
        // Operator typo in yaml ⇒ fall back to "0s" so we don't
        // accidentally let a malformed manifest bypass the gate.
        let policy = Staleness::Strict {
            max_cache_age: "notaduration".into(),
        };
        assert!(matches!(
            decide(&policy, Duration::from_secs(1)),
            StalenessDecision::Skip { .. }
        ));
    }
}
