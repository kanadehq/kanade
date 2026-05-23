//! Offline-tolerant boot / reconnect helpers (v0.38, Issue #137).
//!
//! Every subsystem that needs JetStream KV / stream / consumer access
//! at boot used to call `get_key_value().await?` or
//! `get_stream().await?` directly. When the broker was unreachable —
//! either because operations hadn't run `kanade jetstream setup` yet or
//! because the agent booted before the broker process did — those
//! calls failed and the subsystem either crashed the agent process
//! (Category A, see #137) or killed its own task forever (Category B)
//! or silently returned with builtin defaults (Category C).
//!
//! This module centralises the retry behaviour:
//!
//! - [`wait_for_kv`] / [`wait_for_stream`] / [`wait_for_consumer`] —
//!   try, fail, back off, retry. Backoff is exponential with jitter
//!   and caps at 5 minutes; that's gentle enough not to hammer a
//!   slow-starting broker yet fast enough that recovery feels
//!   instant once the link is up (the `Tracker`'s
//!   [`wait_connected`](crate::staleness::Tracker::wait_connected)
//!   interrupts the sleep on `Event::Connected`).
//! - [`reopen_pause`] — short fixed sleep between reconnect attempts
//!   in a subsystem's outer `loop { ... }`. Keeps a flapping broker
//!   from spinning the loop.
//!
//! Each subsystem owns its own `loop { ... }` rather than calling
//! into a generic `with_reconnect_loop(F)` helper, because the
//! "FnMut closure that returns an async block borrowing from its
//! captures" pattern that the obvious helper signature requires
//! doesn't compose with subsystems that persist state across
//! reconnects (e.g. groups.rs holds `subs: HashMap<...>` to avoid
//! double-subscribing on every reopen). Inline reconnect loops are
//! a handful of lines per call site and read cleanly.
//!
//! All helpers are `Send` and uncancellable except by task abort.
//! They never propagate the underlying error — the contract is
//! "this returns when the resource is available," not "this may
//! fail." Subsystems that need to surface a "bucket truly absent
//! forever" signal would have to layer their own timeout, but the
//! existing design (silent retry forever) matches the agent's
//! offline-first posture and that's deliberate.
//!
//! Failure-log severity follows a streak-aware pattern (#140): the
//! first failure in a streak emits `warn!`, subsequent failures emit
//! `debug!`, and the first successful call after a streak emits an
//! `info!` summary with the failure count. See [`log_failure`].
//!
//! ## Why not use async-nats's own reconnect?
//!
//! `async-nats` already reconnects the *transport* automatically and
//! `retry_on_initial_connect` keeps the `Client` alive through a
//! broker-absent boot. What it doesn't do is re-establish *application-
//! level* state: a `KeyValue::Store` handle, a JetStream `Consumer`,
//! or the iterator returned by `kv.watch_all()`. When the broker
//! disconnects mid-watch the iterator yields `None` and never
//! reopens. Those re-establishments are what this module owns.
//!
//! ## What we deliberately don't do
//!
//! - **Error-class discrimination.** async-nats's KV/stream error
//!   taxonomy is private-ish and varies between minor versions; we
//!   treat every error the same (back off, retry). A genuinely
//!   missing bucket and a transient timeout look identical to the
//!   helper, which is fine — the *operator-visible* surface is a
//!   warn log either way, and the cure is the same (run
//!   `kanade jetstream setup` or fix the broker).
//! - **Bounded retry.** No "give up after N attempts" knob. The
//!   subsystem is designed to outlive transient outages of arbitrary
//!   length; bounding retries would just turn long outages into
//!   silent failures, which is what #137 is fixing in the first
//!   place.

use std::time::Duration;

use async_nats::connection::State;
use async_nats::jetstream;
use rand::Rng;
use tracing::{debug, info, warn};

use crate::staleness::Tracker;

/// Initial backoff between retries. Doubles each attempt up to
/// [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Cap on the exponential backoff. 5 min keeps recovery latency
/// reasonable while not pummelling a slow-starting broker.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// ±25% jitter on each sleep. Fleets of agents that booted at the
/// same moment after a broker outage would otherwise hit the broker
/// in lockstep; jitter spreads them.
const JITTER_FRACTION: f64 = 0.25;

/// Wait until [`jetstream::Context::get_key_value`] succeeds for
/// `bucket`. Retries with exponential backoff + jitter, interrupted
/// by the tracker's [`Tracker::wait_connected`] signal.
///
/// `label` is woven into the warn log so an operator scanning a
/// noisy log can tell which subsystem is currently waiting on what.
pub async fn wait_for_kv(
    js: &jetstream::Context,
    client: &async_nats::Client,
    tracker: &Tracker,
    bucket: &str,
    label: &'static str,
) -> jetstream::kv::Store {
    let mut backoff = INITIAL_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    loop {
        gate_on_connection(client, tracker, label, "kv").await;
        match js.get_key_value(bucket).await {
            Ok(store) => {
                if consecutive_failures > 0 {
                    info!(
                        label,
                        kind = "kv",
                        resource = bucket,
                        consecutive_failures,
                        "nats_retry: kv recovered",
                    );
                }
                debug!(label, bucket, "nats_retry: kv ready");
                return store;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                log_failure(consecutive_failures, backoff, "kv", label, bucket, &e);
                sleep_or_wake(backoff, tracker).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Wait until [`jetstream::Context::get_stream`] succeeds for
/// `name`. Same retry contract as [`wait_for_kv`].
pub async fn wait_for_stream(
    js: &jetstream::Context,
    client: &async_nats::Client,
    tracker: &Tracker,
    name: &str,
    label: &'static str,
) -> jetstream::stream::Stream {
    let mut backoff = INITIAL_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    loop {
        gate_on_connection(client, tracker, label, "stream").await;
        match js.get_stream(name).await {
            Ok(s) => {
                if consecutive_failures > 0 {
                    info!(
                        label,
                        kind = "stream",
                        resource = name,
                        consecutive_failures,
                        "nats_retry: stream recovered",
                    );
                }
                debug!(label, stream = name, "nats_retry: stream ready");
                return s;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                log_failure(consecutive_failures, backoff, "stream", label, name, &e);
                sleep_or_wake(backoff, tracker).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Wait until
/// [`jetstream::stream::Stream::get_or_create_consumer`] succeeds for
/// `name` with `config`. Same retry contract as [`wait_for_kv`].
///
/// The consumer config is `Clone`d on every retry because
/// `get_or_create_consumer` takes it by value. That's fine — pull
/// consumer configs are tiny.
pub async fn wait_for_consumer<C>(
    stream: &jetstream::stream::Stream,
    client: &async_nats::Client,
    tracker: &Tracker,
    name: &str,
    label: &'static str,
    config: C,
) -> jetstream::consumer::Consumer<C>
where
    C: jetstream::consumer::IntoConsumerConfig + jetstream::consumer::FromConsumer + Clone,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    loop {
        gate_on_connection(client, tracker, label, "consumer").await;
        match stream.get_or_create_consumer(name, config.clone()).await {
            Ok(c) => {
                if consecutive_failures > 0 {
                    info!(
                        label,
                        kind = "consumer",
                        resource = name,
                        consecutive_failures,
                        "nats_retry: consumer recovered",
                    );
                }
                debug!(label, consumer = name, "nats_retry: consumer ready");
                return c;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                log_failure(consecutive_failures, backoff, "consumer", label, name, &e);
                sleep_or_wake(backoff, tracker).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Short fixed gap a subsystem should sleep before reopening a watch
/// that returned None. Keeps a flapping broker from spinning the
/// outer `loop { ... }` at 100% CPU.
///
/// Subsystems use it like:
///
/// ```ignore
/// loop {
///     let kv = nats_retry::wait_for_kv(...).await;
///     /* re-prime cache from KV */
///     let mut watch = match kv.watch(...).await { Ok(w) => w, Err(_) => { nats_retry::reopen_pause().await; continue; } };
///     while let Some(entry) = watch.next().await { /* handle */ }
///     warn!("watch ended; reopening");
///     nats_retry::reopen_pause().await;
/// }
/// ```
pub async fn reopen_pause() {
    tokio::time::sleep(Duration::from_secs(1)).await;
}

// ----------------------------------------------------------------
// Internals
// ----------------------------------------------------------------

/// Log one retry-attempt failure with severity that depends on
/// whether this is the first failure in the current streak (#140).
///
/// - First failure (`consecutive == 1`): `warn!` so operators see
///   the initial "broker / bucket unavailable" signal once per
///   streak.
/// - Subsequent failures (`consecutive >= 2`): `debug!`. Without
///   this, a multi-hour outage spams `warn!` once per backoff
///   tick (every 5 min after the cap) for the duration of the
///   outage — operationally useless and crowds out actual signal.
///
/// On the next successful `wait_for_*` call the counter starts
/// fresh, so the next streak after a recovery will warn again on
/// its first failure. That's the "recovery → first failure after
/// success → back to warn" behavior #140 asked for; recovery
/// itself is logged at `info!` by the success path.
fn log_failure(
    consecutive: u32,
    backoff: Duration,
    kind: &'static str,
    label: &'static str,
    resource: &str,
    error: &dyn std::fmt::Display,
) {
    if consecutive == 1 {
        warn!(
            label,
            kind,
            resource,
            error = %error,
            backoff_secs = backoff.as_secs(),
            "nats_retry: unavailable, retrying after backoff",
        );
    } else {
        debug!(
            label,
            kind,
            resource,
            error = %error,
            backoff_secs = backoff.as_secs(),
            consecutive_failures = consecutive,
            "nats_retry: still unavailable, retrying after backoff",
        );
    }
}

/// If the client isn't currently `State::Connected`, park on the
/// tracker's wake until either it fires or a short timeout elapses.
/// Saves the per-attempt round-trip request that's certain to fail
/// when the link is down — and the log noise that comes with it.
///
/// The 5s timeout is a safety net for the rare case where async-nats
/// dispatches `Event::Connected` via `try_send` and drops it because
/// the events channel is full. Subsequent iterations of `wait_for_*`
/// will re-enter and re-check — keep the gate cheap so the
/// outer-loop's documented exponential backoff dominates the
/// observed retry cadence (sub-agent #147 review F3: an earlier
/// 30s gate dominated the 1-32s low-backoff iterations).
async fn gate_on_connection(
    client: &async_nats::Client,
    tracker: &Tracker,
    label: &'static str,
    kind: &'static str,
) {
    if client.connection_state() == State::Connected {
        return;
    }
    debug!(
        label,
        kind, "nats_retry: client not connected, waiting on Notify"
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), tracker.wait_connected()).await;
}

/// Race `sleep(d)` against the tracker's wake. Returns as soon as
/// either fires.
async fn sleep_or_wake(d: Duration, tracker: &Tracker) {
    tokio::select! {
        biased;
        _ = tracker.wait_connected() => {}
        _ = tokio::time::sleep(jitter(d)) => {}
    }
}

/// Doubling backoff with a hard cap.
fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > MAX_BACKOFF {
        MAX_BACKOFF
    } else {
        doubled
    }
}

/// Apply ±[`JITTER_FRACTION`] uniform jitter to `d`.
fn jitter(d: Duration) -> Duration {
    let factor = 1.0 + rand::rng().random_range(-JITTER_FRACTION..=JITTER_FRACTION);
    d.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_doubles_then_caps() {
        let mut d = INITIAL_BACKOFF;
        // 1, 2, 4, 8, 16, 32, 64, 128, 256, 300 (cap), 300, …
        let expected_secs = [1u64, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300];
        let mut observed = Vec::new();
        for _ in 0..expected_secs.len() {
            observed.push(d.as_secs());
            d = next_backoff(d);
        }
        assert_eq!(observed, expected_secs);
    }

    #[test]
    fn jitter_stays_within_band() {
        // Pure sanity — the band is ±25%. Empirically sweep many
        // samples and assert each lands in the band.
        let base = Duration::from_secs(10);
        for _ in 0..1000 {
            let j = jitter(base);
            let secs = j.as_secs_f64();
            assert!(
                (7.499..=12.501).contains(&secs),
                "jitter sample {secs:.3}s outside ±25% band of 10s",
            );
        }
    }

    /// `sleep_or_wake` must return early when the tracker fires
    /// `notify_waiters()` — this is the "broker just came back"
    /// signal that makes subsystem retry loops feel instant
    /// instead of "waits out the next 5-minute backoff."
    ///
    /// Run under paused-time tokio so the test doesn't burn 30s
    /// real-world on the sleep arm; the Notify arm wins so time
    /// advance is never needed.
    #[tokio::test(start_paused = true)]
    async fn sleep_or_wake_returns_early_on_connected_event() {
        let tracker = Tracker::new();
        let cb = tracker.on_event();
        let tracker_for_waiter = tracker.clone();

        let waiter = tokio::spawn(async move {
            sleep_or_wake(Duration::from_secs(60), &tracker_for_waiter).await;
        });

        // Yield twice to give the spawned task a chance to enter
        // its `select!` and park on both arms.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Fire Connected. notify_waiters() wakes every parked
        // waiter; the select!'s Notify arm should resolve and the
        // spawned task should complete without us advancing time.
        let _ = cb(async_nats::Event::Connected).await;

        waiter.await.expect("waiter task panicked");
    }

    /// Sanity: a non-Connected event must NOT wake the waiter.
    /// (Only the Connected variant fires the Notify in
    /// `Tracker::on_event`.)
    ///
    /// We can't easily prove a negative under paused-time without
    /// `advance`, so the assertion is "the waiter is still pending
    /// after we yield several times" — `try_join!` with a no-op
    /// future wins.
    #[tokio::test(start_paused = true)]
    async fn sleep_or_wake_ignores_non_connected_events() {
        let tracker = Tracker::new();
        let cb = tracker.on_event();
        let tracker_for_waiter = tracker.clone();

        let mut waiter = tokio::spawn(async move {
            sleep_or_wake(Duration::from_secs(60), &tracker_for_waiter).await;
        });

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Fire a Disconnected event — must NOT trip the Notify.
        let _ = cb(async_nats::Event::Disconnected).await;

        // Yield a few more times to give the waiter a chance to
        // wake if (incorrectly) it would.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        // The waiter must still be pending — `try_join!` proves it
        // by trying once and failing.
        match futures::poll!(&mut waiter) {
            std::task::Poll::Pending => { /* expected */ }
            std::task::Poll::Ready(r) => {
                panic!("waiter woke on Disconnected event (incorrectly): {r:?}");
            }
        }
        waiter.abort();
    }
}
