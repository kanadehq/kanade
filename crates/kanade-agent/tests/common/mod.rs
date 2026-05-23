//! Integration test harness — Issue #141.
//!
//! Spawns a real `nats-server` and the agent binary (via
//! `env!("CARGO_BIN_EXE_kanade-agent")`) so the offline-tolerant
//! boot path from #137 can be exercised end-to-end. The harness is
//! deliberately minimal:
//!
//! - [`NatsServer`] is a `tokio::process::Child` wrapper that picks
//!   a free port via `portpicker`, spawns `nats-server -js -p <port>
//!   -sd <tempdir>`, and probes a TCP connect until the broker is
//!   ready. `Drop` kills the child — `std::process::Child::drop`
//!   does NOT kill on either platform, so the explicit kill is what
//!   keeps the test from leaving zombies.
//! - [`AgentHandle`] is the same shape, spawned against a minimal
//!   `agent.toml` in a temp dir. `KANADE_NATS_TOKEN` is stripped
//!   from the child env so a developer's HKLM-registry-resident
//!   prod token doesn't accidentally cross over into a test broker
//!   that accepts unauthenticated connects.
//!
//! Both wrappers expose tiny lifecycle APIs (`start`, `kill`,
//! `is_alive`) plus a couple of `NatsServer` conveniences
//! (`bootstrap_jetstream`, `set_heartbeat_interval`) that the
//! offline_boot tests use to set up the broker side.
//!
//! Why not a shared workspace crate (`crates/kanade-test-harness`):
//! YAGNI. Promote when `kanade-backend` writes its first
//! integration test that needs `NatsServer` too. Until then the
//! `tests/common/mod.rs` form keeps the test build fast and the
//! API surface internal.

#![allow(dead_code)] // Different tests use different helpers.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_nats::jetstream;
use kanade_shared::bootstrap::ensure_jetstream_resources;
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, KEY_AGENT_CONFIG_GLOBAL};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

/// Sentinel printed on every line forwarded from the agent
/// subprocess so a failed test's captured stderr is easy to scan.
const AGENT_LOG_PREFIX: &str = "[agent]";

/// Same for `nats-server`. Less interesting most of the time but
/// hugely useful when the broker itself misbehaves.
const NATS_LOG_PREFIX: &str = "[nats]";

/// Bind probe timeout — if the broker hasn't started accepting
/// connections inside this window we assume the launch is wedged.
const NATS_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-test broker storage lives in a tempdir; this is the maximum
/// number of port allocation attempts before the harness gives up.
/// portpicker hands out a port without binding it, so a TOCTOU
/// race is theoretically possible — three attempts is plenty in
/// practice.
const PORT_ALLOC_ATTEMPTS: u32 = 3;

/// Returns `true` if `nats-server` is on PATH. Tests in this crate
/// use this for a clean "skip with explanation" rather than
/// panicking with a confusing `program not found` error on
/// developer machines that haven't installed the broker yet.
pub fn nats_server_in_path() -> bool {
    which::which("nats-server").is_ok()
}

/// Reusable skip macro for the offline_boot tests. Logs the reason
/// to stderr (captured per-test by cargo) and returns early.
#[macro_export]
macro_rules! skip_if_no_nats_server {
    () => {
        if !$crate::common::nats_server_in_path() {
            eprintln!(
                "skipping: `nats-server` not in PATH. Install via \
                 scoop / brew / GitHub release and re-run."
            );
            return;
        }
    };
}

// ────────────────────────────────────────────────────────────────
// NatsServer
// ────────────────────────────────────────────────────────────────

pub struct NatsServer {
    child: Child,
    pub port: u16,
    pub url: String,
    // Owned for the lifetime of the broker so the JetStream storage
    // directory is removed on Drop. Must outlive `child` — declared
    // last so Rust drops in field-order (top → bottom), killing the
    // child before the dir vanishes.
    storage_dir: TempDir,
}

impl NatsServer {
    /// Pick a free port, spawn `nats-server`, wait until it answers
    /// TCP. Retries with a fresh port on bind-fail up to
    /// [`PORT_ALLOC_ATTEMPTS`] times.
    pub async fn start() -> Result<Self> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=PORT_ALLOC_ATTEMPTS {
            let port = portpicker::pick_unused_port()
                .ok_or_else(|| anyhow!("no free TCP port available"))?;
            match Self::start_on_port(port).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    eprintln!(
                        "{NATS_LOG_PREFIX} attempt {attempt}/{PORT_ALLOC_ATTEMPTS} \
                         on port {port} failed: {e:#}"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("nats-server failed to start (no attempts ran)")))
    }

    /// Lower-level entry: spawn `nats-server` on a specific port.
    /// Used by tests that need to restart on the same port (case 2 /
    /// case 3 in offline_boot).
    pub async fn start_on_port(port: u16) -> Result<Self> {
        let storage_dir = TempDir::new().context("create nats-server storage tempdir")?;
        let mut cmd = Command::new("nats-server");
        cmd.arg("-js")
            .arg("-p")
            .arg(port.to_string())
            .arg("-sd")
            .arg(storage_dir.path())
            // Suppress nats-server's own log timestamps so the
            // [nats] prefix and cargo's test-capture timestamps are
            // the only sources of timing info.
            .arg("-T=false")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawn nats-server (is it in PATH?)")?;

        // Pipe broker output to test stderr with a prefix.
        if let Some(stdout) = child.stdout.take() {
            forward_lines(stdout, NATS_LOG_PREFIX);
        }
        if let Some(stderr) = child.stderr.take() {
            forward_lines(stderr, NATS_LOG_PREFIX);
        }

        let url = format!("nats://127.0.0.1:{port}");
        wait_for_tcp(port, NATS_READY_TIMEOUT)
            .await
            .with_context(|| format!("nats-server did not become ready on {url}"))?;
        Ok(Self {
            child,
            port,
            url,
            storage_dir,
        })
    }

    /// Force-kill the broker. Synchronous-feeling but uses
    /// `Child::start_kill` so we don't block; `wait` is awaited so
    /// the OS releases the PID before the next test allocates one.
    pub async fn kill(&mut self) -> Result<()> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }

    /// Open an `async_nats::Client` against this broker.
    pub async fn client(&self) -> Result<async_nats::Client> {
        async_nats::connect(&self.url)
            .await
            .with_context(|| format!("connect to {}", self.url))
    }

    /// Provision every JetStream resource the agent / backend
    /// expect (mirrors backend startup at
    /// `crates/kanade-backend/src/main.rs:127-134`). Idempotent —
    /// safe to call multiple times across a broker-restart in the
    /// same test.
    pub async fn bootstrap_jetstream(&self) -> Result<()> {
        let client = self.client().await?;
        let js = jetstream::new(client);
        ensure_jetstream_resources(&js)
            .await
            .context("ensure_jetstream_resources")
    }

    /// Write `{"heartbeat_interval": "<secs>s"}` to
    /// `BUCKET_AGENT_CONFIG/global` so `config_supervisor` picks it
    /// up and reschedules `heartbeat_loop` to fire that fast. The
    /// default cadence is 30 s — test-suite-runtime is dominated by
    /// waiting for the next heartbeat, so this lever cuts test
    /// runtime from minutes to seconds. **Call this AFTER
    /// `bootstrap_jetstream` since it requires the KV bucket to
    /// exist.**
    pub async fn set_heartbeat_interval(&self, secs: u64) -> Result<()> {
        let client = self.client().await?;
        let js = jetstream::new(client);
        let kv = js
            .get_key_value(BUCKET_AGENT_CONFIG)
            .await
            .context("get_key_value(agent_config) — did you forget bootstrap_jetstream()?")?;
        let payload = json!({ "heartbeat_interval": format!("{secs}s") });
        kv.put(
            KEY_AGENT_CONFIG_GLOBAL,
            serde_json::to_vec(&payload)?.into(),
        )
        .await
        .context("kv.put(agent_config/global)")?;
        Ok(())
    }
}

impl Drop for NatsServer {
    fn drop(&mut self) {
        // Synchronous kill — `Drop` can't await. `kill_on_drop(true)`
        // also helps, but doing it explicitly is belt-and-braces and
        // makes the intent obvious to a future reader.
        let _ = self.child.start_kill();
    }
}

// ────────────────────────────────────────────────────────────────
// AgentHandle
// ────────────────────────────────────────────────────────────────

pub struct AgentHandle {
    child: Child,
    pub pc_id: String,
    // Owned for log/config dir lifetime. Field order: child first
    // so the kill on Drop happens before the dir vanishes.
    _config_dir: TempDir,
}

impl AgentHandle {
    /// Spawn the kanade-agent binary against the given NATS URL.
    /// The pc_id is a fresh UUID per spawn so two parallel tests
    /// don't cross-match each other's heartbeats.
    pub async fn spawn(nats_url: &str) -> Result<Self> {
        let config_dir = TempDir::new().context("create agent config tempdir")?;
        let pc_id = format!("itest-{}", uuid::Uuid::new_v4().simple());
        let log_path = config_dir.path().join("agent.log");
        let toml = format!(
            r#"[agent]
id = "{pc_id}"
nats_url = "{nats_url}"

[log]
path = {log_path:?}
level = "info"
# 0 disables the rolling file appender so the test doesn't race
# with the tracing-appender background flusher when the tempdir
# gets cleaned up.
keep_days = 0
"#
        );
        let config_path = config_dir.path().join("agent.toml");
        std::fs::write(&config_path, toml).context("write temp agent.toml")?;

        let bin: PathBuf = env!("CARGO_BIN_EXE_kanade-agent").into();
        let mut cmd = Command::new(bin);
        cmd.arg("--config")
            .arg(&config_path)
            // Strip any host-side token (HKLM still applies on
            // Windows but the test broker accepts unauthenticated
            // connects, so the token is harmlessly discarded).
            .env_remove("KANADE_NATS_TOKEN")
            .env("RUST_LOG", "info,kanade_agent=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawn kanade-agent binary")?;

        if let Some(stdout) = child.stdout.take() {
            forward_lines(stdout, AGENT_LOG_PREFIX);
        }
        if let Some(stderr) = child.stderr.take() {
            forward_lines(stderr, AGENT_LOG_PREFIX);
        }
        Ok(Self {
            child,
            pc_id,
            _config_dir: config_dir,
        })
    }

    /// True if the agent process is still running. Used by case 1
    /// to assert the broker-down boot didn't kill the agent.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

// ────────────────────────────────────────────────────────────────
// Internals
// ────────────────────────────────────────────────────────────────

/// TCP-probe loop until the broker accepts a connection or the
/// timeout elapses. nats-server doesn't have a clean "ready" probe
/// other than answering its client port, so this is the standard
/// approach.
async fn wait_for_tcp(port: u16, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "nats-server did not accept TCP on 127.0.0.1:{port} within {timeout:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spawn a task that reads `stream` line-by-line and forwards each
/// line to stderr with a prefix. cargo test captures stderr per-test
/// (no `--nocapture` needed for it), so green runs stay quiet but
/// failed tests dump the subprocess context inline.
fn forward_lines<R>(stream: R, prefix: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{prefix} {line}");
        }
    });
}

// ────────────────────────────────────────────────────────────────
// Cross-test helpers (used by offline_boot.rs)
// ────────────────────────────────────────────────────────────────

use futures::StreamExt;
use kanade_shared::subject;
use kanade_shared::wire::Heartbeat;

/// Subscribe to `heartbeat.<pc_id>` on `url` and wait up to
/// `timeout` for one. Returns the deserialised payload; panics on
/// timeout or subscription closure.
pub async fn await_heartbeat(url: &str, pc_id: &str, timeout: Duration) -> Result<Heartbeat> {
    let client = async_nats::connect(url)
        .await
        .context("connect for subscribe")?;
    let mut sub = client
        .subscribe(subject::heartbeat(pc_id))
        .await
        .with_context(|| format!("subscribe heartbeat.{pc_id}"))?;
    let msg = tokio::time::timeout(timeout, sub.next())
        .await
        .map_err(|_| anyhow!("no heartbeat from {pc_id} within {timeout:?}"))?
        .ok_or_else(|| anyhow!("heartbeat subscription closed"))?;
    let hb: Heartbeat = serde_json::from_slice(&msg.payload).context("decode Heartbeat payload")?;
    Ok(hb)
}
