//! Consumer-visible one-shot diagnostic outcomes against real broker handshakes.
//! Unix tests isolate credentials in child environments; Windows deliberately
//! prefers installed HKLM credentials, which these tests must not overwrite.
#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

struct Broker {
    child: Child,
    dir: TempDir,
    url: String,
}

impl Broker {
    async fn start(auth: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let port = portpicker::pick_unused_port().unwrap();
        let config = dir.path().join("nats.conf");
        std::fs::write(&config, format!("host: 127.0.0.1\nport: {port}\n{auth}\n")).unwrap();
        let mut child = Command::new("nats-server")
            .arg("-c")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("nats-server must be installed for this ignored test");
        tokio::time::timeout(Duration::from_secs(10), async {
            while TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                assert!(child.try_wait().unwrap().is_none(), "broker exited");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("broker readiness");
        Self {
            child,
            dir,
            url: format!("nats://127.0.0.1:{port}"),
        }
    }

    async fn stop(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
        drop(self.dir);
    }
}

fn config(url: &str) -> String {
    format!(
        "[agent]\nid = 'connection-check-test'\nnats_url = '{url}'\n\
         [log]\npath = 'logs/agent.log'\nlevel = 'info'\n"
    )
}

async fn check(
    config: &str,
    token: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
    want: i32,
) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(&path, config).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kanade-agent"));
    command
        .args(["--check-connection", "--config"])
        .arg(&path)
        .current_dir(dir.path())
        .env("KANADE_AGENT_DATA_DIR", dir.path().join("data"))
        .env_remove("KANADE_NATS_TOKEN")
        .env_remove("KANADE_NATS_USER")
        .env_remove("KANADE_NATS_PASSWORD")
        .kill_on_drop(true);
    for (name, value) in [
        ("KANADE_NATS_TOKEN", token),
        ("KANADE_NATS_USER", user),
        ("KANADE_NATS_PASSWORD", password),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("diagnostic exceeded its bound")
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(want),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // No boot sentinel, state database, log directory, or other daemon files.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("agent.toml")]);
    for secret in [token, password].into_iter().flatten() {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
}

#[tokio::test]
async fn invalid_configuration_and_half_credentials_exit_three_without_state() {
    check("not valid toml", None, None, None, 3).await;
    check(&config("http://not-a-nats-broker"), None, None, None, 3).await;
    check(
        &config("nats://127.0.0.1:1"),
        None,
        Some("test-user"),
        None,
        3,
    )
    .await;
    check(
        &config("nats://127.0.0.1:1"),
        None,
        None,
        Some("test-password"),
        3,
    )
    .await;
    check(
        &config("nats://127.0.0.1:1").replace("connection-check-test", "too.many.agent.id.labels"),
        None,
        None,
        None,
        3,
    )
    .await;
}

#[tokio::test]
async fn offline_and_silent_brokers_exit_five_within_the_bound_without_state() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!("nats://{}", listener.local_addr().unwrap());
    // The listening socket accepts TCP but never supplies a NATS greeting.
    check(
        &config(&url),
        Some("test-token"),
        Some("test-user"),
        Some("test-password"),
        5,
    )
    .await;
    check(&config(&url), Some("test-token"), None, None, 5).await;
    drop(listener);
    check(&config(&url), None, None, None, 5).await;
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn token_handshake_and_user_fallback_report_real_authentication_results() {
    let broker = Broker::start("authorization { token: \"test-token\" }").await;
    let cfg = config(&broker.url);
    check(&cfg, Some("test-token"), None, None, 0).await;
    check(&cfg, Some("wrong-token"), None, None, 4).await;
    check(&cfg, None, None, None, 4).await;
    check(
        &cfg,
        Some("test-token"),
        Some("test-user"),
        Some("test-password"),
        0,
    )
    .await;
    check(
        &cfg,
        Some("wrong-token"),
        Some("test-user"),
        Some("test-password"),
        4,
    )
    .await;
    broker.stop().await;
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn user_handshake_reports_wrong_user_and_password_without_state() {
    let broker = Broker::start(
        "authorization { users: [{ user: \"test-user\", password: \"test-password\" }] }",
    )
    .await;
    let cfg = config(&broker.url);
    check(&cfg, None, Some("test-user"), Some("test-password"), 0).await;
    check(
        &cfg,
        Some("wrong-token"),
        Some("test-user"),
        Some("test-password"),
        0,
    )
    .await;
    check(&cfg, None, Some("wrong-user"), Some("test-password"), 4).await;
    check(&cfg, None, Some("test-user"), Some("wrong-password"), 4).await;
    check(
        &cfg,
        Some("wrong-token"),
        Some("test-user"),
        Some("wrong-password"),
        4,
    )
    .await;
    broker.stop().await;
}
