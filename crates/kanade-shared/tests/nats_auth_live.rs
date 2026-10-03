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
    NatsCredentials, NatsRole, connect_with_credentials,
    connect_with_credentials_and_event_callback, is_dead, wait_until_dead_every,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    dir: tempfile::TempDir,
    port: u16,
    http_port: u16,
}

impl Broker {
    async fn start(auth: &str) -> Option<Self> {
        if !nats_server_available() {
            return None;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let port = portpicker::pick_unused_port().expect("pick port");
        let http_port = portpicker::pick_unused_port().expect("pick http port");
        let child = spawn_server(dir.path(), port, http_port, auth).await;
        Some(Self {
            child,
            dir,
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
        self.child = spawn_server(self.dir.path(), self.port, self.http_port, auth).await;
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    /// Rewrite the authorization block and signal a config reload.
    #[cfg(unix)]
    fn reload(&self, auth: &str) {
        write_config(self.dir.path(), self.port, self.http_port, auth);
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

async fn spawn_server(
    dir: &std::path::Path,
    port: u16,
    http_port: u16,
    auth: &str,
) -> tokio::process::Child {
    write_config(dir, port, http_port, auth);
    let child = tokio::process::Command::new("nats-server")
        .arg("-c")
        .arg(dir.join("nats.conf"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nats-server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while tokio::net::TcpStream::connect(("127.0.0.1", http_port))
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "nats-server did not come up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child
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

/// Connect with `creds` and an event sink, and report whether the client
/// surfaced an authentication failure while never becoming usable.
async fn fails_visibly(broker: &Broker, creds: NatsCredentials) -> bool {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let client = connect_with_credentials_and_event_callback(
        NatsRole::Cli,
        &broker.url(),
        creds,
        move |ev| {
            let _ = tx.send(ev.to_string());
            std::future::ready(())
        },
    )
    .await
    .unwrap();
    // Never usable: the flush is bounded, so a client that cannot
    // authenticate shows up as `false` here instead of hanging the caller.
    let usable = round_trip(&client, Duration::from_secs(6)).await;
    let mut reported = false;
    while let Ok(ev) = rx.try_recv() {
        reported |= ev.contains("authorization violation") || ev.contains("signing nonce");
    }
    !usable && reported && broker.authorized_user().await.is_none()
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_token_only_client_against_a_users_broker_fails_visibly() {
    let broker = broker_or_skip!(USERS_AUTH);
    assert!(
        fails_visibly(&broker, NatsCredentials::new(Some(TOKEN.into()), None)).await,
        "a token the broker rejects must surface as an error event and never connect"
    );
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_user_only_client_against_a_token_broker_fails_visibly() {
    // The user is rejected and there is no token to fall back to: the
    // callback fails naming the missing credential on every attempt.
    let broker = broker_or_skip!(TOKEN_AUTH);
    assert!(
        fails_visibly(
            &broker,
            NatsCredentials::new(None, Some((USER.into(), PASSWORD.into())))
        )
        .await
    );
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
        wait_until_dead_every(&client, Duration::from_millis(250)),
    )
    .await
    .expect("a client whose connection task ended must be reported dead");
    assert!(is_dead(&client).await);
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_live_client_is_not_reported_dead_while_the_broker_is_down() {
    // An ordinary disconnect buffers publishes; it must not be mistaken for
    // a terminated connection task.
    let mut broker = broker_or_skip!(TOKEN_AUTH);
    let client = connect_with_credentials(NatsRole::Cli, &broker.url(), both())
        .await
        .unwrap();
    assert!(round_trip(&client, Duration::from_secs(15)).await);
    broker.child.kill().await.expect("stop broker");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!is_dead(&client).await);
}
