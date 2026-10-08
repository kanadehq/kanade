//! Live-broker tests for the per-role NATS user and its token fallback in
//! [`kanade_shared::nats_client`].
//!
//! Ignored by default — each test spawns a throwaway `nats-server` (must be
//! in PATH) on random ports, and skips cleanly when the binary is absent:
//!
//! ```text
//! cargo test -p kanade-shared --test nats_auth_live -- --ignored
//! ```
//!
//! The broker is switched between `token` and `users` authorization by
//! rewriting its config and signalling a reload, which is what an operator
//! does — nats-server refuses a config carrying both, so the flip is atomic
//! and a connected client has to follow it.

use std::process::Stdio;
use std::time::{Duration, Instant};

use kanade_shared::nats_client::{
    NatsCredentials, NatsRole, connect_with_credentials, is_dead, wait_until_dead_every,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const STARTUP_ATTEMPTS: usize = 3;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

const TOKEN: &str = "fleet-token";
const USER: &str = "cli-user";
const PASSWORD: &str = "cli-password";

const TOKEN_AUTH: &str = "authorization { token: \"fleet-token\" }";
const USERS_AUTH: &str =
    "authorization { users: [ { user: \"cli-user\", password: \"cli-password\" } ] }";

fn both() -> NatsCredentials {
    NatsCredentials::new(Some(TOKEN.into()), Some((USER.into(), PASSWORD.into())))
}

fn nats_server_available() -> bool {
    std::process::Command::new("nats-server")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

macro_rules! broker_or_skip {
    ($auth:expr) => {
        match Broker::start($auth).await {
            Some(b) => b,
            None => {
                eprintln!("skipping: nats-server not found in PATH");
                return;
            }
        }
    };
}

/// A throwaway broker whose authorization block can be swapped at runtime.
struct Broker {
    child: tokio::process::Child,
    _dir: tempfile::TempDir,
    port: u16,
    http_port: u16,
}

impl Broker {
    async fn start(auth: &str) -> Option<Self> {
        if !nats_server_available() {
            return None;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (child, port, http_port) = spawn_server(dir.path(), auth, None).await;
        Some(Self {
            child,
            _dir: dir,
            port,
            http_port,
        })
    }

    /// Stop the broker and start it again on the same ports with a new
    /// authorization block. A reload from `users` back to `token` leaves the
    /// old users accepted, so a rollback is exercised the way an operator
    /// would do it for real: by restarting the broker.
    #[cfg(unix)]
    async fn restart(&mut self, auth: &str) {
        self.child.kill().await.expect("stop broker");
        (self.child, _, _) =
            spawn_server(self._dir.path(), auth, Some((self.port, self.http_port))).await;
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    /// Rewrite the authorization block and signal a config reload.
    #[cfg(unix)]
    fn reload(&self, auth: &str) {
        write_config(self._dir.path(), self.port, self.http_port, auth);
        let pid = self.child.id().expect("nats-server pid").to_string();
        let status = std::process::Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .expect("kill -HUP");
        assert!(status.success(), "could not signal nats-server to reload");
    }

    /// What the broker reports as `authorized_user` for our connection
    /// (named `kanade-cli`), or `None` when it has no such connection.
    async fn authorized_user(&self) -> Option<String> {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", self.http_port))
            .await
            .ok()?;
        s.write_all(b"GET /connz?auth=1 HTTP/1.0\r\n\r\n")
            .await
            .ok()?;
        let mut raw = String::new();
        s.read_to_string(&mut raw).await.ok()?;
        let body: serde_json::Value = serde_json::from_str(raw.split_once("\r\n\r\n")?.1).ok()?;
        body["connections"]
            .as_array()?
            .iter()
            .find(|c| c["name"] == "kanade-cli")
            .map(|c| {
                c["authorized_user"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
    }
}

/// Port probes do not reserve their sockets. Retry initial starts on fresh,
/// distinct ports; restarts must preserve the live client's reconnect address.
async fn spawn_server(
    dir: &std::path::Path,
    auth: &str,
    fixed_ports: Option<(u16, u16)>,
) -> (tokio::process::Child, u16, u16) {
    let mut failures = Vec::new();
    for attempt in 1..=STARTUP_ATTEMPTS {
        let (port, http_port) = fixed_ports.unwrap_or_else(|| {
            let port = portpicker::pick_unused_port().expect("pick port");
            let http_port = loop {
                let candidate = portpicker::pick_unused_port().expect("pick http port");
                if candidate != port {
                    break candidate;
                }
            };
            (port, http_port)
        });
        write_config(dir, port, http_port, auth);
        // Unique files also retain logs from before a restart. A shared file
        // handle captures both streams without a pipe that could fill up.
        let log = tempfile::Builder::new()
            .prefix(&format!("nats-start-{attempt}-"))
            .suffix(".log")
            .tempfile_in(dir)
            .expect("create nats-server log");
        let (output, log_path) = log.keep().expect("retain nats-server log");
        let server_name = log_path
            .file_name()
            .expect("log filename")
            .to_string_lossy();
        let started = Instant::now();
        let spawned = tokio::process::Command::new("nats-server")
            .arg("-c")
            .arg(dir.join("nats.conf"))
            .arg("--name")
            .arg(server_name.as_ref())
            .stdout(output.try_clone().expect("clone nats-server log"))
            .stderr(output)
            .kill_on_drop(true)
            .spawn();
        let reason = match spawned {
            Ok(mut child) => {
                match wait_for_start(&mut child, port, http_port, &server_name).await {
                    Ok(()) => return (child, port, http_port),
                    Err(reason) => {
                        // kill() waits for exit too. Reap before releasing ports or
                        // reading the log, including when startup already exited.
                        let stopped = child.kill().await;
                        let status = child.wait().await;
                        format!("{reason}; stop={stopped:?}; exit={status:?}")
                    }
                }
            }
            Err(error) => format!("spawn failed: {error}"),
        };
        let failure = format!(
            "attempt {attempt}: client port={port}, monitor port={http_port}, elapsed={:?}: {reason}\nlog tail:\n{}",
            started.elapsed(),
            log_tail(&log_path),
        );
        eprintln!("{failure}");
        failures.push(failure);
        if attempt < STARTUP_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    panic!(
        "nats-server did not come up after {STARTUP_ATTEMPTS} attempts:\n{}",
        failures.join("\n\n")
    );
}

async fn wait_for_start(
    child: &mut tokio::process::Child,
    port: u16,
    http_port: u16,
    server_name: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("nats-server exited before readiness: {status}"));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("could not check nats-server status: {error}")),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("startup timed out after {STARTUP_TIMEOUT:?}"));
        }
        // A TCP accept alone could belong to another test's broker. /varz
        // identifies this attempt's unique server name (it has no PID field),
        // and the whole request (including reads) is bounded so a foreign or
        // stalled listener cannot hang startup.
        if let Ok(Some(())) = tokio::time::timeout(
            remaining.min(Duration::from_millis(250)),
            server_ready(port, http_port, server_name),
        )
        .await
        {
            return Ok(());
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        )
        .await;
    }
}

async fn server_ready(port: u16, http_port: u16, server_name: &str) -> Option<()> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
        .await
        .ok()?;
    stream.write_all(b"GET /varz HTTP/1.0\r\n\r\n").await.ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.ok()?;
    let body: serde_json::Value = serde_json::from_str(raw.split_once("\r\n\r\n")?.1).ok()?;
    if body["server_name"].as_str()? != server_name {
        return None;
    }
    // Monitoring can start before the client listener. Require its INFO too,
    // so a client-port collision is retried instead of escaping the harness.
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let mut reader = tokio::io::BufReader::new(stream.take(8192));
    let mut info = String::new();
    reader.read_line(&mut info).await.ok()?;
    let info: serde_json::Value = serde_json::from_str(info.strip_prefix("INFO ")?).ok()?;
    (info["server_name"].as_str()? == server_name).then_some(())
}

fn log_tail(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let mut lines: Vec<_> = text.lines().rev().take(40).collect();
            lines.reverse();
            lines.join("\n")
        }
        Err(error) => format!("could not read nats-server log: {error}"),
    }
}

fn write_config(dir: &std::path::Path, port: u16, http_port: u16, auth: &str) {
    std::fs::write(
        dir.join("nats.conf"),
        format!("host: 127.0.0.1\nport: {port}\nhttp: 127.0.0.1:{http_port}\n{auth}\n"),
    )
    .expect("write nats.conf");
}

/// Prove the client can talk: a flush completes only once the broker has
/// answered a PING on an authenticated connection. Retried until `within`
/// because a client mid-reconnect buffers rather than fails.
async fn round_trip(client: &async_nats::Client, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(2), client.flush()).await {
            return true;
        }
    }
    false
}

/// Wait until the broker reports our connection as `want`.
async fn wait_for_user(broker: &Broker, want: impl Fn(&str) -> bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(u) = broker.authorized_user().await
            && want(&u)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_client_with_both_credentials_connects_to_a_token_broker() {
    let broker = broker_or_skip!(TOKEN_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    assert!(!is_dead(&client).await);
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_client_with_both_credentials_connects_to_a_users_broker() {
    let broker = broker_or_skip!(USERS_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    assert!(wait_for_user(&broker, |u| u == USER, Duration::from_secs(5)).await);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_connected_client_follows_the_broker_to_users_and_back() {
    let mut broker = broker_or_skip!(TOKEN_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    // Under `token` the broker never names a user.
    assert!(wait_for_user(&broker, |u| u != USER, Duration::from_secs(5)).await);

    // Flip to `users` under the live client: same process, same Client.
    broker.reload(USERS_AUTH);
    assert!(
        wait_for_user(&broker, |u| u == USER, Duration::from_secs(30)).await,
        "client did not re-authenticate as the user after the broker flipped"
    );
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    assert!(!is_dead(&client).await);

    // And back: the broker is rolled back to the token under the same live
    // client, which must present the token on its next attempt.
    broker.restart(TOKEN_AUTH).await;
    assert!(
        wait_for_user(&broker, |u| u != USER, Duration::from_secs(30)).await,
        "client did not fall back to the token after the broker reverted"
    );
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    assert!(!is_dead(&client).await);
}

/// A client the broker keeps refusing must be declared failed — which is what
/// makes the agent and backend exit and the CLI abort — rather than retrying
/// quietly forever. The role is per test because the refusal state is kept
/// per role and the tests run concurrently.
async fn assert_reported_failed(role: NatsRole, broker: &Broker, creds: NatsCredentials) {
    let client = connect_with_credentials(role, &broker.url(), creds)
        .await
        .unwrap();
    tokio::select! {
        () = wait_until_dead_every(role, &client, Duration::from_millis(250)) => {}
        () = tokio::time::sleep(Duration::from_secs(45)) => {
            panic!("a client the broker keeps refusing was never reported failed");
        }
    }
    assert!(
        broker.authorized_user().await.is_none(),
        "the refused client must not hold a broker connection"
    );
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_token_only_client_against_a_users_broker_fails_visibly() {
    let broker = broker_or_skip!(USERS_AUTH);
    assert_reported_failed(
        NatsRole::Backend,
        &broker,
        NatsCredentials::new(Some(TOKEN.into()), None),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_user_only_client_against_a_token_broker_fails_visibly() {
    // The user is rejected and there is no token to fall back to: the
    // callback fails naming the missing credential on every attempt.
    let broker = broker_or_skip!(TOKEN_AUTH);
    assert_reported_failed(
        NatsRole::Agent,
        &broker,
        NatsCredentials::new(None, Some((USER.into(), PASSWORD.into()))),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn dead_client_detection_fires_once_the_connection_task_has_terminated() {
    let broker = broker_or_skip!(TOKEN_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    assert!(!is_dead(&client).await);

    // Draining ends the connection task while the `Client` lives on — the
    // same observable state as the task dying any other way.
    client.drain().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_until_dead_every(NatsRole::Cli, &client, Duration::from_millis(250)),
    )
    .await
    .expect("a client whose connection task ended must be reported dead");
    assert!(is_dead(&client).await);
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_live_client_is_not_reported_dead_while_the_broker_is_down() {
    // An ordinary disconnect leaves the connection task alive but busy
    // reconnecting, so a flush waits; that must not be mistaken for a
    // terminated connection task, however many checks pile up.
    let mut broker = broker_or_skip!(TOKEN_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    broker.child.kill().await.expect("stop broker");
    tokio::time::sleep(Duration::from_secs(1)).await;
    for _ in 0..3 {
        assert!(!is_dead(&client).await);
    }
}
