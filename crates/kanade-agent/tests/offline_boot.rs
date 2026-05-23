//! Offline-boot integration tests (Issue #141, closes the
//! regression-coverage gap from #137).
//!
//! Each test spawns a fresh `nats-server` + a fresh `kanade-agent`
//! binary so the assertions exercise the real wire protocol and the
//! real startup sequence — including `tracing-subscriber` init,
//! config loading, JetStream resource bootstrap, and the
//! `nats_retry` exponential backoff path.
//!
//! ## Why `#[ignore]`?
//!
//! The dedicated `Integration` workflow (`.github/workflows/integration.yml`)
//! installs `nats-server` per runner and opts in via `-- --ignored`.
//! The local `cargo test` / `cargo make check` paths skip these
//! tests so they stay fast on machines without `nats-server`. The
//! file-level `skip_if_no_nats_server!` macro turns a missing
//! binary into a clean "skipping" log so the manual `--ignored`
//! invocation on a fresh checkout doesn't panic with a confusing
//! process-not-found error.
//!
//! Run with:
//!
//! ```text
//! cargo test -p kanade-agent --test offline_boot -- --ignored --nocapture
//! ```

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use crate::common::{AgentHandle, NatsServer, await_heartbeat};

/// Heartbeat cadence override. The default is 30 s (see
/// `EffectiveConfig::builtin_defaults`). Cutting to 1 s keeps the
/// per-test wait below 10 s; otherwise case 3 alone would take
/// nearly two minutes.
const HEARTBEAT_INTERVAL_SECS: u64 = 1;

/// Max time we wait for a heartbeat in the success branches.
/// Generous enough to absorb the agent's own `wait_for_kv` backoff
/// plus the broker's bootstrap RTT plus runner jitter.
///
/// The `wait_for_kv` schedule is 1 + 2 + 4 + 8 = 15 s nominal but
/// jitters up to ±25%, so a burst that misses one cycle and rolls
/// into the next 16 s slot already approaches 30 s. 60 s keeps
/// the test resilient against Windows-runner slowness;
/// `tokio::time::timeout` returns the instant the heartbeat
/// arrives, so the green-run wall time is unchanged.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// Wait for an obviously-bogus port to disappoint the agent. Pick
/// something the kernel will reject quickly — 1 (reserved) works on
/// all three platforms.
const BOGUS_NATS_URL: &str = "nats://127.0.0.1:1";

/// ## Case 1 (issue #141 AC) — broker not running, agent boots.
///
/// Assert the agent process stays alive even with no broker
/// reachable. Before #137 the agent would exit on
/// `connect_with_event_callback().await?`; with
/// `retry_on_initial_connect` the process must remain running while
/// the client retries in the background.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn boots_with_no_broker_stays_alive() {
    skip_if_no_nats_server!();

    let mut agent = AgentHandle::spawn(BOGUS_NATS_URL)
        .await
        .expect("spawn agent");
    // Give the agent enough time to crash if it's going to.
    // 3 s comfortably exceeds the wait_for_kv first-backoff (1 s)
    // plus the 30 s gate timeout was tightened to 5 s in
    // nats_retry.rs after #147 review — but we never have a broker
    // here, so the agent should just sit in its retry loop.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        agent.is_alive(),
        "agent exited with no broker reachable; #137 regression?"
    );
}

/// ## Case 2 (issue #141 AC) — broker comes up after agent boot.
///
/// The agent is spawned against a port nothing is listening on.
/// After a short grace, we start a real broker on that same port,
/// bootstrap JetStream, override the heartbeat cadence, and assert
/// that a heartbeat eventually arrives.
///
/// Heartbeats publish on **core NATS, not JetStream** (see
/// `crates/kanade-agent/src/heartbeat.rs`), so technically this
/// would pass without `bootstrap_jetstream` — but we still call it
/// so the test mirrors production and exercises the `wait_for_kv`
/// catch-up path inside `config_supervisor`.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn catches_up_when_broker_starts_later() {
    skip_if_no_nats_server!();

    // Pick the port we'll later bind nats-server to so the agent
    // can be told the URL up front.
    let port = portpicker::pick_unused_port().expect("pick port");
    let url = format!("nats://127.0.0.1:{port}");

    let agent = AgentHandle::spawn(&url).await.expect("spawn agent");
    // Grace so the agent has gone through at least one wait_for_kv
    // failure cycle. This proves the path "broker absent → broker
    // appears → catch-up" works end-to-end, not just "broker present
    // at boot."
    tokio::time::sleep(Duration::from_secs(2)).await;

    let nats = NatsServer::start_on_port(port).await.expect("start nats");
    nats.bootstrap_jetstream()
        .await
        .expect("ensure_jetstream_resources");
    nats.set_heartbeat_interval(HEARTBEAT_INTERVAL_SECS)
        .await
        .expect("set heartbeat cadence");

    let hb = await_heartbeat(&nats.url, &agent.pc_id, HEARTBEAT_TIMEOUT)
        .await
        .expect("agent should publish a heartbeat after broker comes up");
    assert_eq!(hb.pc_id, agent.pc_id);
}

/// ## Case 3 (issue #141 AC) — broker killed and restarted mid-run.
///
/// Full happy path first (agent + broker + one heartbeat received),
/// then the broker is killed, the test sleeps long enough that any
/// in-flight publish would have drained, the broker is restarted on
/// the same port, JetStream + cadence are re-applied, and we assert
/// a fresh heartbeat arrives.
///
/// The interesting part is the *resume* — `config_supervisor` /
/// `command_replay` / `local_scheduler` each have to detect the
/// dropped watch / consumer and re-establish state without an agent
/// restart.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn recovers_after_broker_restart() {
    skip_if_no_nats_server!();

    let mut nats = NatsServer::start().await.expect("start nats");
    nats.bootstrap_jetstream()
        .await
        .expect("ensure_jetstream_resources");
    nats.set_heartbeat_interval(HEARTBEAT_INTERVAL_SECS)
        .await
        .expect("set heartbeat cadence");
    let port = nats.port;

    let agent = AgentHandle::spawn(&nats.url).await.expect("spawn agent");

    // Pre-restart sanity heartbeat.
    let hb_before = await_heartbeat(&nats.url, &agent.pc_id, HEARTBEAT_TIMEOUT)
        .await
        .expect("pre-restart heartbeat");
    assert_eq!(hb_before.pc_id, agent.pc_id);

    nats.kill().await.expect("kill nats");
    // Brief gap so async-nats' internal reconnect sees the dropped
    // connection at least once.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let nats = NatsServer::start_on_port(port)
        .await
        .expect("restart nats on same port");
    nats.bootstrap_jetstream()
        .await
        .expect("re-bootstrap after restart");
    nats.set_heartbeat_interval(HEARTBEAT_INTERVAL_SECS)
        .await
        .expect("re-apply cadence");

    let hb_after = await_heartbeat(&nats.url, &agent.pc_id, HEARTBEAT_TIMEOUT)
        .await
        .expect("post-restart heartbeat");
    assert_eq!(hb_after.pc_id, agent.pc_id);
    // Same agent → same pc_id but a later timestamp.
    assert!(
        hb_after.at >= hb_before.at,
        "post-restart heartbeat timestamp regressed",
    );
}
