//! Real-broker conformance test for the role-level NATS authorization block
//! checked in as `configs/nats-server.users.conf`.
//!
//! The block is meant to replace the single shared token. A permission that is
//! missing does not fail loudly — a JetStream read stalls, a publish is
//! dropped, a request times out — so the only trustworthy evidence that the
//! block is right is to run every role's real flows against a real
//! `nats-server` started with it. This file does that, and is the gate for the
//! later broker switch. It changes no production configuration.
//!
//! Ignored by default (needs `nats-server` and the three binaries):
//!
//! ```text
//! cargo build -p kanade-backend -p kanade-agent -p kanade
//! cargo test -p kanade-agent --test nats_role_conformance -- --ignored
//! ```
//!
//! Locally it skips cleanly when `nats-server` or a binary is absent. Under CI
//! (`CI` set) a missing prerequisite is a failure, not a skip, and the broker
//! version must equal `NATS_SERVER_VERSION` when that is set — a gate that
//! skips is not a gate.
//!
//! What runs, and what each part is evidence of:
//!
//! * `allowed_flows_complete_under_the_users_block` (A) — the real backend,
//!   agent and `kanade run` binaries, plus the real `connect` helper for the
//!   flows that live in no binary of their own. Completion is judged from
//!   business effects (a result lands, an outbox file is acknowledged and
//!   removed, an object holds the bytes, a KV change reaches the agent), never
//!   from "connected". A second oracle scans the broker's own log: not one
//!   permission violation may appear during the allowed flows, and the parser
//!   is proven against a violation raised on purpose first.
//! * `denied_operations_are_denied_per_role` (B) — each denial is asserted
//!   from the server's permission-violation error delivered to the client's
//!   event callback, or from absence of delivery on another connection after
//!   a positive control proved the path. A timeout alone is never accepted.
//! * `token_to_users_and_back_with_processes_running` (C, unix) — the
//!   `connect` helper's credential selection end to end, processes left
//!   running across a reload to `users` and back.
//! * `consumer_delivery_target_residual_is_recorded` (D) — a known broker
//!   behaviour the block cannot close, recorded as it is today.
//!
//! Windows covers A, B and D. C needs a signal-driven reload, which has no
//! equivalent for a console broker, so it is `cfg(unix)`. The Windows-only
//! Client App pipe (KLP) is not driven here: its NATS traffic is the same
//! `notifications_read` write the agent role is probed with directly.
//!
//! Narrowing the command plane later is a one-line edit in the conf (delete
//! the `NARROWING` lines) plus flipping [`COMMAND_BROADCAST_SUBSCRIBE`] below.

#[path = "common/mod.rs"]
mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_nats::jetstream::{self, kv, object_store};
use async_nats::{Client, Event};
use bytes::Bytes;
use futures::StreamExt;
use kanade_shared::ExecResult;
use kanade_shared::nats_client::{
    NatsCredentials, NatsRole, connect_with_credentials_and_event_callback,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

const USERS_CONF: &str = include_str!("../../../configs/nats-server.users.conf");
const TOKEN: &str = "conformance-fleet-token";

/// Whether the agent may still subscribe to the broadcast command subjects.
/// The block keeps them for now (the command plane is not yet narrowed). When
/// the `NARROWING` lines leave the conf, flip this to `Denied` and the
/// expectation table, the agent process's own boot, and the zero-violation
/// oracle all follow from it.
const COMMAND_BROADCAST_SUBSCRIBE: Expect = Expect::Allowed;

/// Buckets the agent must not write. (Its only write is its own notification
/// read-state.)
const AGENT_PROTECTED_BUCKETS: &[&str] = &[
    "jobs",
    "schedules",
    "script_current",
    "script_status",
    "agent_config",
    "agent_groups",
    "agent_groups_derived",
    "agent_meta",
    "fleet_config",
    "server_settings",
];

/// The subset of those no agent flow reads either. `server_settings` holds
/// the installer's secrets and SMTP settings; `agent_meta` is operator-owned
/// inventory metadata nothing on the endpoint consumes.
const AGENT_UNREADABLE_BUCKETS: &[&str] = &["agent_meta", "server_settings"];

// ───────────────────────────── prerequisites ─────────────────────────────

fn in_ci() -> bool {
    std::env::var_os("CI").is_some()
}

fn exe(name: &str) -> PathBuf {
    let mut here = std::env::current_exe().expect("test exe path");
    here.pop(); // deps
    here.pop(); // debug / release
    here.push(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    here
}

/// The backend binary. A debug build of it overflows the 1 MiB main-thread
/// stack on Windows (the production build is a release build), so CI points
/// this at a release binary there.
fn backend_exe() -> PathBuf {
    std::env::var_os("KANADE_CONFORMANCE_BACKEND_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe("kanade-backend"))
}

fn agent_exe() -> PathBuf {
    env!("CARGO_BIN_EXE_kanade-agent").into()
}

/// `false` means "skip this test". Panics under CI.
fn prerequisites() -> bool {
    let mut missing = Vec::new();
    if !common::nats_server_in_path() {
        missing.push("`nats-server` in PATH".to_string());
    } else if let Ok(want) = std::env::var("NATS_SERVER_VERSION") {
        let out = std::process::Command::new("nats-server")
            .arg("--version")
            .output()
            .expect("nats-server --version");
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        if !got.contains(&want) {
            missing.push(format!("nats-server {want} (found: {})", got.trim()));
        }
    }
    for bin in [backend_exe(), exe("kanade")] {
        if !bin.exists() {
            missing.push(format!(
                "{} (run `cargo build -p kanade-backend -p kanade-agent -p kanade`)",
                bin.display()
            ));
        }
    }
    if missing.is_empty() {
        return true;
    }
    let msg = format!("conformance prerequisites missing: {}", missing.join(", "));
    if in_ci() {
        panic!("{msg}");
    }
    eprintln!("skipping: {msg}");
    false
}

macro_rules! prereqs_or_skip {
    () => {
        if !prerequisites() {
            return;
        }
    };
}

// ─────────────────────────────── roles ───────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Role {
    Agent,
    Backend,
    Breakglass,
}

impl Role {
    const ALL: [Role; 3] = [Role::Agent, Role::Backend, Role::Breakglass];

    fn user(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Backend => "backend",
            Role::Breakglass => "breakglass",
        }
    }

    fn password(self) -> String {
        format!("conformance-pw-{}", self.user())
    }

    fn env_name(self) -> String {
        format!("KANADE_CONFORMANCE_{}_PASSWORD", self.user().to_uppercase())
    }

    fn nats_role(self) -> NatsRole {
        match self {
            Role::Agent => NatsRole::Agent,
            Role::Backend => NatsRole::Backend,
            Role::Breakglass => NatsRole::Cli,
        }
    }

    fn credentials(self, with_token: bool) -> NatsCredentials {
        NatsCredentials::new(
            with_token.then(|| TOKEN.to_string()),
            Some((self.user().to_string(), self.password())),
        )
    }
}

// ─────────────────────────────── broker ───────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Auth {
    Users,
    /// Only the switch scenario (unix) runs a token broker.
    #[cfg(unix)]
    Token,
}

struct Broker {
    child: Option<Child>,
    dir: TempDir,
    port: u16,
    http_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    user: String,
    kind: String,
    subject: String,
}

impl Broker {
    async fn start(auth: Auth) -> Self {
        let dir = TempDir::new().expect("broker tempdir");
        let port = portpicker::pick_unused_port().expect("port");
        let http_port = portpicker::pick_unused_port().expect("http port");
        // The checked-in file is copied byte for byte: `include` resolves
        // relative to the main config, so the test exercises exactly the
        // bytes an operator would deploy.
        std::fs::write(dir.path().join("nats-server.users.conf"), USERS_CONF).expect("users conf");
        let mut b = Self {
            child: None,
            dir,
            port,
            http_port,
        };
        b.write_conf(auth);
        b.spawn().await;
        b
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    fn log_path(&self) -> PathBuf {
        self.dir.path().join("nats.log")
    }

    fn write_conf(&self, auth: Auth) {
        let store = self.dir.path().join("js");
        let auth_block = match auth {
            Auth::Users => "include \"nats-server.users.conf\"".to_string(),
            #[cfg(unix)]
            Auth::Token => format!("authorization {{ token: \"{TOKEN}\" }}"),
        };
        let conf = format!(
            "host: 127.0.0.1\nport: {}\nhttp: 127.0.0.1:{}\njetstream {{ store_dir: \"{}\" }}\n{}\n",
            self.port,
            self.http_port,
            store.to_string_lossy().replace('\\', "/"),
            auth_block,
        );
        std::fs::write(self.dir.path().join("nats.conf"), conf).expect("nats.conf");
    }

    async fn spawn(&mut self) {
        // Append, so a restart keeps the earlier violations for the oracle.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .expect("broker log");
        let log2 = log.try_clone().expect("clone log handle");
        let mut cmd = Command::new("nats-server");
        cmd.arg("-c")
            .arg(self.dir.path().join("nats.conf"))
            .arg("-T=false")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .kill_on_drop(true);
        for role in Role::ALL {
            cmd.env(role.env_name(), role.password());
        }
        self.child = Some(cmd.spawn().expect("spawn nats-server"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while tokio::net::TcpStream::connect(("127.0.0.1", self.http_port))
            .await
            .is_err()
        {
            assert!(
                Instant::now() < deadline,
                "nats-server did not come up:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }

    /// Stop and start again on the same ports and store, so JetStream state
    /// and the clients' reconnect targets survive.
    async fn restart(&mut self, auth: Auth) {
        self.stop().await;
        self.write_conf(auth);
        self.spawn().await;
    }

    #[cfg(unix)]
    fn reload(&self, auth: Auth) {
        self.write_conf(auth);
        let pid = self
            .child
            .as_ref()
            .and_then(|c| c.id())
            .expect("broker pid");
        let ok = std::process::Command::new("kill")
            .args(["-HUP", &pid.to_string()])
            .status()
            .expect("kill -HUP")
            .success();
        assert!(ok, "could not signal the broker to reload");
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    /// Every permission violation the broker has logged, in order.
    fn violations(&self) -> Vec<Violation> {
        parse_violations(&self.log())
    }

    async fn monitor(&self, path: &str) -> serde_json::Value {
        let body = http_get(self.http_port, path).await;
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null)
    }

    /// `(connection name, authorized_user)` for every live connection.
    #[cfg(unix)]
    async fn connections(&self) -> Vec<(String, String)> {
        let v = self.monitor("/connz?auth=1").await;
        v["connections"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| {
                        (
                            c["name"].as_str().unwrap_or_default().to_string(),
                            c["authorized_user"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `ack_floor.stream_seq` of a durable consumer, or 0 when absent.
    async fn ack_floor(&self, stream: &str, consumer: &str) -> u64 {
        let v = self.monitor("/jsz?streams=1&consumers=1").await;
        for acc in v["account_details"].as_array().into_iter().flatten() {
            for s in acc["stream_detail"].as_array().into_iter().flatten() {
                if s["name"] != stream {
                    continue;
                }
                for c in s["consumer_detail"].as_array().into_iter().flatten() {
                    if c["name"] == consumer {
                        return c["ack_floor"]["stream_seq"].as_u64().unwrap_or(0);
                    }
                }
            }
        }
        0
    }
}

/// The server logs `... - "$G/user:<name>" - Publish Violation - Subject "<s>"`
/// (and `Subscription Violation`). Parsed by hand: the format is the
/// pinned broker's, and a self-check below fails loudly when it drifts.
fn parse_violations(log: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    for line in log.lines() {
        let Some(idx) = line.find(" Violation - ") else {
            continue;
        };
        let head = &line[..idx];
        let kind = head.rsplit(' ').next().unwrap_or_default().to_string();
        let user = line
            .split("user:")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_default()
            .to_string();
        let subject = line[idx..]
            .split("Subject \"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_default()
            .to_string();
        out.push(Violation {
            user,
            kind,
            subject,
        });
    }
    out
}

async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &str,
) -> (u16, String) {
    // 0 means "nothing answered", so a caller polling for readiness can retry.
    let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return (0, String::new());
    };
    let ct = content_type
        .map(|c| format!("Content-Type: {c}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n{ct}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.expect("http write");
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw).await;
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    (status, body)
}

async fn http_get(port: u16, path: &str) -> String {
    http_request(port, "GET", path, None, "").await.1
}

// ───────────────────────────── role clients ─────────────────────────────

/// A client opened with the real `connect` helper under a role's credentials,
/// plus every server error it has been told about.
struct RoleClient {
    client: Client,
    js: jetstream::Context,
    errors: Arc<Mutex<Vec<String>>>,
}

impl RoleClient {
    async fn open(url: &str, role: Role) -> Self {
        Self::open_with(url, role, None).await
    }

    /// For denial probes: a refused JetStream call is only ever answered by
    /// silence, so a short API timeout keeps the matrix quick. The verdict is
    /// still the violation, never the timeout.
    async fn open_fast(url: &str, role: Role) -> Self {
        Self::open_with(url, role, Some(Duration::from_secs(2))).await
    }

    async fn open_with(url: &str, role: Role, js_timeout: Option<Duration>) -> Self {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let client = connect_with_credentials_and_event_callback(
            role.nats_role(),
            url,
            role.credentials(false),
            move |ev| {
                if let Event::ServerError(e) = ev {
                    sink.lock().unwrap().push(e.to_string());
                }
                std::future::ready(())
            },
        )
        .await
        .unwrap_or_else(|e| panic!("connect as {}: {e:#}", role.user()));
        let js = match js_timeout {
            Some(t) => jetstream::context::ContextBuilder::new()
                .timeout(t)
                .build(client.clone()),
            None => jetstream::new(client.clone()),
        };
        Self { js, client, errors }
    }

    /// A round trip to the broker: every error caused by an earlier operation
    /// has been delivered once this returns (the server answers in order).
    async fn settle(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.client.flush()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fn violated(&self, kind: &str, subject: &str) -> bool {
        let needle = format!("{kind} to \"{subject}\"");
        self.errors
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("Permissions Violation") && e.contains(&needle))
    }

    fn violations_since(&self, mark: usize) -> Vec<String> {
        self.errors
            .lock()
            .unwrap()
            .iter()
            .skip(mark)
            .filter(|e| e.contains("Permissions Violation"))
            .cloned()
            .collect()
    }

    fn violated_since(&self, mark: usize, kind: &str, subject: &str) -> bool {
        let needle = format!("{kind} to \"{subject}\"");
        self.violations_since(mark)
            .iter()
            .any(|e| e.contains(&needle))
    }

    fn any_violation(&self) -> Vec<String> {
        self.errors
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains("Permissions Violation"))
            .cloned()
            .collect()
    }
}

async fn until<F, Fut>(what: &str, within: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    loop {
        if f().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ───────────────────────────── child processes ─────────────────────────────

struct Proc {
    child: Child,
}

impl Proc {
    fn spawn(mut cmd: Command, prefix: &'static str) -> Self {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {prefix}: {e}"));
        if let Some(o) = child.stdout.take() {
            common::forward_lines(o, prefix);
        }
        if let Some(e) = child.stderr.take() {
            common::forward_lines(e, prefix);
        }
        Self { child }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Credentials a spawned binary is given. The helper resolves them from the
/// environment, so the real credential-selection logic is what runs.
fn creds_env(cmd: &mut Command, role: Role, with_token: bool) {
    cmd.env_remove("KANADE_NATS_TOKEN")
        .env_remove("KANADE_NATS_USER")
        .env_remove("KANADE_NATS_PASSWORD")
        .env("KANADE_NATS_USER", role.user())
        .env("KANADE_NATS_PASSWORD", role.password());
    if with_token {
        cmd.env("KANADE_NATS_TOKEN", TOKEN);
    }
}

/// One whole fleet slice: a broker, the real backend, one real agent.
struct Fleet {
    broker: Broker,
    /// URL the processes dial. The broker's, unless a proxy sits in between.
    dial_url: String,
    with_token: bool,
    backend: Option<Proc>,
    agent: Option<Proc>,
    backend_dir: TempDir,
    agent_dir: TempDir,
    http_port: u16,
    pc_id: String,
}

impl Fleet {
    async fn new(auth: Auth, with_token: bool, dial_url: Option<String>) -> Self {
        let broker = Broker::start(auth).await;
        let dial_url = dial_url.unwrap_or_else(|| broker.url());
        Self {
            broker,
            dial_url,
            with_token,
            backend: None,
            agent: None,
            backend_dir: TempDir::new().expect("backend dir"),
            agent_dir: TempDir::new().expect("agent dir"),
            http_port: portpicker::pick_unused_port().expect("http port"),
            pc_id: format!("conf-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.agent_dir.path().join("data")
    }

    async fn start_backend(&mut self) {
        let dir = self.backend_dir.path();
        let toml = format!(
            "[server]\nbind = '127.0.0.1:{}'\n\n[nats]\nurl = '{}'\nmonitor_url = 'http://127.0.0.1:{}'\n\n\
             [db]\nsqlite_path = '{}'\n\n[log]\npath = '{}'\nlevel = 'info'\nkeep_days = 0\n",
            self.http_port,
            self.dial_url,
            self.broker.http_port,
            toml_path(&dir.join("backend.db")),
            toml_path(&dir.join("backend.log")),
        );
        let cfg = dir.join("backend.toml");
        std::fs::write(&cfg, toml).expect("backend.toml");
        let mut cmd = Command::new(backend_exe());
        cmd.arg("--config")
            .arg(&cfg)
            // A throwaway data dir keeps the boot sentinel and key material
            // off the developer's real install.
            .env("KANADE_AGENT_DATA_DIR", dir.join("data"))
            .env("KANADE_AUTH_DISABLE", "1")
            .env("RUST_LOG", "info");
        creds_env(&mut cmd, Role::Backend, self.with_token);
        self.backend = Some(Proc::spawn(cmd, "[backend]"));
        let port = self.http_port;
        until("backend HTTP up", Duration::from_secs(60), || async move {
            http_request(port, "GET", "/api/jobs", None, "").await.0 == 200
        })
        .await;
    }

    async fn start_agent(&mut self) {
        let cfg_dir = self.agent_dir.path();
        let toml = format!(
            "[agent]\nid = \"{}\"\nnats_url = \"{}\"\n\n[log]\npath = {:?}\nlevel = \"info\"\nkeep_days = 0\n",
            self.pc_id,
            self.dial_url,
            cfg_dir.join("agent.log"),
        );
        let cfg = cfg_dir.join("agent.toml");
        std::fs::write(&cfg, toml).expect("agent.toml");
        let mut cmd = Command::new(agent_exe());
        cmd.arg("--config")
            .arg(&cfg)
            .env("KANADE_AGENT_DATA_DIR", self.data_dir())
            .env("RUST_LOG", "info,kanade_agent=debug");
        creds_env(&mut cmd, Role::Agent, self.with_token);
        self.agent = Some(Proc::spawn(cmd, "[agent]"));
    }

    async fn stop_agent(&mut self) {
        if let Some(mut a) = self.agent.take() {
            a.stop().await;
        }
    }

    /// `kanade run`/`kill` as the break-glass operator, against the broker.
    async fn cli(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(exe("kanade"));
        cmd.arg("--server").arg(&self.dial_url).args(args);
        creds_env(&mut cmd, Role::Breakglass, self.with_token);
        cmd.stdin(Stdio::null());
        tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .expect("kanade CLI hung")
            .expect("run kanade CLI")
    }

    /// Start `kanade run` and leave it running: it waits for its result over
    /// a subscription that has to survive whatever the broker does next.
    #[cfg(unix)]
    fn cli_spawn_slow(&self, marker: &str, sleep_secs: u32) -> Child {
        let mut cmd = Command::new(exe("kanade"));
        cmd.arg("--server")
            .arg(&self.dial_url)
            .args([
                "run",
                &self.pc_id,
                "--shell",
                "sh",
                "--timeout",
                "120",
                "--",
            ])
            .arg(format!("sleep {sleep_secs}; echo {marker}"));
        creds_env(&mut cmd, Role::Breakglass, self.with_token);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.spawn().expect("spawn kanade run")
    }

    /// `kanade run <pc> -- echo <marker>`; returns stdout.
    async fn cli_run_echo(&self, marker: &str, timeout_secs: u64) -> std::process::Output {
        let t = timeout_secs.to_string();
        let script = format!("echo {marker}");
        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        self.cli(&[
            "run",
            &self.pc_id,
            "--shell",
            shell,
            "--timeout",
            &t,
            "--",
            &script,
        ])
        .await
    }

    async fn role(&self, role: Role) -> RoleClient {
        RoleClient::open(&self.dial_url, role).await
    }

    async fn role_fast(&self, role: Role) -> RoleClient {
        RoleClient::open_fast(&self.dial_url, role).await
    }

    /// Wait until the agent has published a heartbeat to `sub`.
    async fn heartbeat(&self, sub: &mut async_nats::Subscriber, within: Duration) {
        tokio::time::timeout(within, sub.next())
            .await
            .unwrap_or_else(|_| panic!("no heartbeat from {} within {within:?}", self.pc_id))
            .expect("heartbeat subscription closed");
    }
}

/// A failed run is diagnosed from what the broker refused, so say it.
impl Drop for Fleet {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let seen: BTreeSet<Violation> = self.broker.violations().into_iter().collect();
            eprintln!("broker permission violations at failure: {seen:#?}");
        }
    }
}

fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_cli_ok(out: &std::process::Output, marker: &str) {
    assert!(
        out.status.success() && stdout_of(out).contains(marker),
        "kanade run did not complete: status={:?}\nstdout={}\nstderr={}",
        out.status,
        stdout_of(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fresh(tag: &str) -> String {
    format!("{tag}{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

// ═══════════════════════════ A. allowed flows ═══════════════════════════

/// The violation parser has to see a violation before its silence means
/// anything. Raises one on purpose as the break-glass user.
async fn prove_the_violation_oracle(f: &Fleet) {
    let probe = f.role(Role::Breakglass).await;
    let subject = format!("heartbeat.oracle-{}", fresh(""));
    probe
        .client
        .publish(subject.clone(), Bytes::new())
        .await
        .unwrap();
    let _ = probe.client.subscribe("commands.oracle.>").await.unwrap();
    probe.settle().await;
    let seen = f.broker.violations();
    assert!(
        seen.iter()
            .any(|v| v.user == "breakglass" && v.kind == "Publish" && v.subject == subject),
        "the broker log parser missed a deliberate publish violation — the log format \
         has drifted from what this test expects.\n{}",
        f.broker.log()
    );
    assert!(
        seen.iter().any(|v| v.user == "breakglass"
            && v.kind == "Subscription"
            && v.subject == "commands.oracle.>"),
        "the broker log parser missed a deliberate subscription violation.\n{}",
        f.broker.log()
    );
}

/// Violations that were raised on purpose before the allowed flows began.
fn deliberate(seen: &[Violation]) -> BTreeSet<Violation> {
    seen.iter().cloned().collect()
}

#[tokio::test]
#[ignore = "requires nats-server in PATH and built binaries; cargo test -- --ignored"]
async fn allowed_flows_complete_under_the_users_block() {
    prereqs_or_skip!();
    let mut f = Fleet::new(Auth::Users, false, None).await;

    prove_the_violation_oracle(&f).await;
    let baseline = deliberate(&f.broker.violations());

    // Backend: bootstraps every stream / bucket / store, runs its projectors,
    // creates scheduler buckets lazily, serves HTTP.
    f.start_backend().await;
    let backend = f.role(Role::Backend).await;
    // Every resource the bootstrap promises exists, read back through the
    // backend's own credential.
    for s in kanade_shared::kv::ALL_STREAMS {
        backend
            .js
            .get_stream(*s)
            .await
            .unwrap_or_else(|e| panic!("stream {s}: {e}"));
    }
    for b in kanade_shared::kv::ALL_KV_BUCKETS {
        backend
            .js
            .get_key_value(*b)
            .await
            .unwrap_or_else(|e| panic!("bucket {b}: {e}"));
    }
    for o in kanade_shared::kv::ALL_OBJECT_STORES {
        backend
            .js
            .get_object_store(*o)
            .await
            .unwrap_or_else(|e| panic!("store {o}: {e}"));
    }
    // Buckets bootstrap does not create: first use creates them.
    backend
        .js
        .get_key_value("scheduler_dispatch")
        .await
        .expect("scheduler_dispatch");

    // Lazily created buckets, fired through the real HTTP handlers.
    let yaml = Some("application/yaml");
    let group = include_str!("../../../configs/groups/hostname-prefix.yaml");
    let view = include_str!("../../../configs/views/dashboards-fleet.yaml");
    let (st, body) = http_request(f.http_port, "POST", "/api/group-defs", yaml, group).await;
    assert!(st == 200 || st == 201, "group-defs create: {st} {body}");
    let (st, body) = http_request(f.http_port, "POST", "/api/views", yaml, view).await;
    assert!(st == 200 || st == 201, "views create: {st} {body}");
    for bucket in ["group_defs", "views"] {
        backend
            .js
            .get_key_value(bucket)
            .await
            .unwrap_or_else(|e| panic!("lazily created bucket {bucket}: {e}"));
    }

    // Agent boots and publishes.
    let mut hb = backend
        .client
        .subscribe(format!("heartbeat.{}", f.pc_id))
        .await
        .unwrap();
    f.start_agent().await;
    f.heartbeat(&mut hb, Duration::from_secs(90)).await;

    // KV watch: a change written through the backend's credential reaches the
    // agent's config watcher. 30 s is the default cadence, so 1 s showing up
    // proves the watch delivered (ordered consumer, _INBOX delivery, flow
    // control path).
    let agent_config = backend.js.get_key_value("agent_config").await.unwrap();
    agent_config
        .put(
            "global",
            Bytes::from_static(br#"{"heartbeat_interval":"1s"}"#),
        )
        .await
        .expect("backend writes agent_config");
    // The first heartbeat after the write may still be on the old cadence;
    // after it, three more inside a few seconds are only possible at 1 s.
    f.heartbeat(&mut hb, Duration::from_secs(45)).await;
    let started = Instant::now();
    for _ in 0..3 {
        f.heartbeat(&mut hb, Duration::from_secs(20)).await;
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "heartbeats did not speed up: the agent's KV watch is not delivering"
    );

    // Break-glass `run`: command out, result back, agent executes, agent
    // publishes the result (JetStream publish with ack via the outbox), and
    // the backend's results projector acknowledges it.
    let marker = fresh("mk");
    let out = f.cli_run_echo(&marker, 30).await;
    assert_cli_ok(&out, &marker);
    until(
        "backend results projector ack",
        Duration::from_secs(120),
        || async {
            f.broker
                .ack_floor("RESULTS", "backend_results_projector")
                .await
                >= 1
        },
    )
    .await;
    // …and the audit the CLI published for the run reaches the backend's
    // audit projector.
    until("audit projector ack", Duration::from_secs(60), || async {
        f.broker.ack_floor("AUDIT", "backend_audit_projector").await >= 1
    })
    .await;

    // The same command was also retained by the command stream and replayed
    // through the agent's durable consumer: the consumer exists under the
    // agent's credential and its position advanced.
    let replay = format!("agent_replay_{}", f.pc_id);
    until(
        "agent replay consumer ack",
        Duration::from_secs(30),
        || async { f.broker.ack_floor("EXEC", &replay).await >= 1 },
    )
    .await;

    // Offline agent: a break-glass command sent while the agent is down lands
    // in the command stream (the CLI only times out), and the agent executes
    // it when it comes back.
    let mut results = backend.client.subscribe("results.*").await.unwrap();
    f.stop_agent().await;
    let offline_marker = fresh("off");
    let out = f.cli_run_echo(&offline_marker, 1).await;
    assert!(
        !out.status.success(),
        "run against a stopped agent unexpectedly completed"
    );
    f.start_agent().await;
    wait_result_containing(&mut results, &offline_marker, Duration::from_secs(90)).await;

    // Request/reply: ping and log fetch through the real backend handlers,
    // tail directly (it needs a running job to be interesting; the permission
    // surface is the same).
    let (st, body) = http_request(
        f.http_port,
        "POST",
        &format!("/api/agents/{}/ping", f.pc_id),
        None,
        "",
    )
    .await;
    assert_eq!(st, 200, "ping: {body}");
    let (st, body) = http_request(
        f.http_port,
        "GET",
        &format!("/api/agents/{}/logs?tail=20", f.pc_id),
        None,
        "",
    )
    .await;
    assert_eq!(st, 200, "logs.fetch: {}", &body[..body.len().min(200)]);
    let tail = backend
        .client
        .request(format!("job.tail.{}", f.pc_id), Bytes::from_static(b"{}"))
        .await;
    assert!(tail.is_ok(), "job.tail request got no reply: {tail:?}");

    // Outbox drain: a spilled result (JetStream publish with ack plus an
    // object-store upload), then an overwrite of the same key, which makes
    // the upload purge the previous chunks.
    let outbox = f.data_dir().join("outbox");
    let request_id = fresh("big");
    let first = vec![b'a'; kanade_shared::kv::STDOUT_INLINE_THRESHOLD + 4096];
    let second = vec![b'b'; kanade_shared::kv::STDOUT_INLINE_THRESHOLD + 8192];
    for (label, body) in [("first", &first), ("overwrite", &second)] {
        enqueue_outbox(&outbox, &request_id, &f.pc_id, body);
        let path = outbox.join(format!("{request_id}.json"));
        until(
            &format!("outbox {label} drained"),
            Duration::from_secs(60),
            || {
                let gone = !path.exists();
                async move { gone }
            },
        )
        .await;
        let store = backend.js.get_object_store("result_output").await.unwrap();
        let mut obj = store
            .get(format!("{request_id}/stdout"))
            .await
            .unwrap_or_else(|e| panic!("object after {label}: {e}"));
        let mut got = Vec::new();
        obj.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), body.len(), "object size after {label}");
        assert_eq!(got.first(), body.first(), "object content after {label}");
    }

    // Agent reads and watches, under its own credential and through the real
    // helper: every bucket it reads, a watch on each, the keys() walk, and a
    // large object read, which is what makes the broker ask for flow-control
    // replies (the `$JS.FC` trap).
    let agent = f.role(Role::Agent).await;
    for b in [
        "agent_config",
        "agent_groups",
        "agent_groups_derived",
        "script_current",
        "script_status",
        "jobs",
        "schedules",
        "fleet_config",
        "notifications_read",
    ] {
        let kv = agent
            .js
            .get_key_value(b)
            .await
            .unwrap_or_else(|e| panic!("agent get_key_value({b}): {e}"));
        let _ = kv
            .get("nope")
            .await
            .unwrap_or_else(|e| panic!("agent kv get {b}: {e}"));
        let walk = async {
            let mut keys = kv
                .keys()
                .await
                .unwrap_or_else(|e| panic!("agent keys {b}: {e}"));
            while keys.next().await.is_some() {}
        };
        tokio::time::timeout(Duration::from_secs(15), walk)
            .await
            .unwrap_or_else(|_| panic!("agent keys() on {b} stalled"));
        // A watch must deliver: a distinguishable update written through the
        // backend's credential has to arrive within a deadline.
        let mut w = kv
            .watch_all()
            .await
            .unwrap_or_else(|e| panic!("agent watch {b}: {e}"));
        let probe = fresh("w");
        let writer = backend.js.get_key_value(b).await.unwrap();
        writer
            .put("conformance-watch", Bytes::from(probe.clone()))
            .await
            .unwrap_or_else(|e| panic!("backend writes {b}: {e}"));
        let seen = async {
            while let Some(entry) = w.next().await {
                let entry = entry.unwrap_or_else(|e| panic!("watch {b} delivery error: {e}"));
                if entry.key == "conformance-watch" && entry.value == probe.as_bytes() {
                    return true;
                }
            }
            false
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(15), seen).await == Ok(true),
            "the agent's watch on {b} never delivered the update (stalled or closed)"
        );
        let _ = writer.delete("conformance-watch").await;
    }
    // Notification read-state: the one bucket an agent writes.
    let read_state = agent.js.get_key_value("notifications_read").await.unwrap();
    read_state
        .put(format!("{}.u1", f.pc_id), Bytes::from_static(b"{}"))
        .await
        .expect("agent writes its notification read-state");

    // Notifications: the backend publishes one, the NOTIFICATIONS stream
    // retains it, the agent's role receives it, and the backend's own list
    // handler (an ephemeral consumer on the stream) reads it back.
    let mut note = agent
        .client
        .subscribe(format!("notifications.pc.{}", f.pc_id))
        .await
        .unwrap();
    agent.settle().await;
    let body = serde_json::json!({
        "target": {"pcs": [f.pc_id]},
        "priority": "info",
        "title": "conformance",
        "body": "probe",
    })
    .to_string();
    let (st, resp) = http_request(
        f.http_port,
        "POST",
        "/api/notifications",
        Some("application/json"),
        &body,
    )
    .await;
    assert_eq!(st, 200, "notification publish: {resp}");
    tokio::time::timeout(Duration::from_secs(10), note.next())
        .await
        .expect("notification never reached the agent role")
        .unwrap();
    let (st, list) = http_request(f.http_port, "GET", "/api/notifications", None, "").await;
    assert!(
        st == 200 && list.contains("conformance"),
        "notification list: {st} {list}"
    );

    // A read-state entry that stays, so the replay below has something of
    // the agent's own to find.
    read_state
        .put(format!("{}.u2", f.pc_id), Bytes::from_static(b"{}"))
        .await
        .expect("agent writes a second read-state entry");

    // The Client App pipe (KLP) is Windows-only and is not driven here, but
    // its NATS traffic is plain JetStream: an unack (a KV delete plus an
    // acknowledged event publish), then throwaway pull consumers over the
    // NOTIFICATIONS stream and over the read-state stream, fetched to the
    // end. Done through the agent credential so the permissions it needs are
    // proven on every OS.
    read_state
        .delete(format!("{}.u1", f.pc_id))
        .await
        .expect("agent deletes its notification read-state");
    agent
        .js
        .publish(
            format!("events.notifications.acked.{}.sid.n1", f.pc_id),
            serde_json::json!({
                "notification_id": "n1",
                "pc_id": f.pc_id,
                "user_sid": "sid",
                "acked_at": chrono::Utc::now(),
            })
            .to_string()
            .into(),
        )
        .await
        .expect("agent publishes a notification ack event")
        .await
        .expect("notification ack event is acknowledged");
    for (stream, filter, needle) in [
        (
            "NOTIFICATIONS",
            "notifications.pc.>".to_string(),
            "conformance",
        ),
        (
            "KV_notifications_read",
            format!("$KV.notifications_read.{}.>", f.pc_id),
            ".u2",
        ),
    ] {
        use jetstream::consumer::{AckPolicy, DeliverPolicy, pull::Config as PullConfig};
        let stream = agent
            .js
            .get_stream(stream)
            .await
            .unwrap_or_else(|e| panic!("agent get_stream({stream}): {e}"));
        let name = stream.cached_info().config.name.clone();
        let consumer = stream
            .create_consumer(PullConfig {
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::None,
                filter_subjects: vec![filter],
                inactive_threshold: Duration::from_secs(30),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("agent ephemeral consumer on {name}: {e}"));
        let mut batch = consumer
            .fetch()
            .max_messages(100)
            .expires(Duration::from_secs(5))
            .messages()
            .await
            .expect("agent fetch");
        // The replay must hand back the identifiable entry prepared above,
        // within a deadline; a stall, a delivery error or an empty replay all
        // fail.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found = false;
        while let Ok(next) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            batch.next(),
        )
        .await
        {
            match next {
                Some(Ok(m)) => {
                    if m.subject.as_str().contains(needle)
                        || String::from_utf8_lossy(&m.payload).contains(needle)
                    {
                        found = true;
                        break;
                    }
                }
                Some(Err(e)) => panic!("replay on {name} delivery error: {e}"),
                None => break,
            }
        }
        assert!(found, "replay on {name} never delivered the prepared entry");
    }

    let big = vec![7u8; 6 * 1024 * 1024];
    let scripts = backend.js.get_object_store("scripts").await.unwrap();
    let mut cursor = std::io::Cursor::new(big.clone());
    scripts
        .put("conformance/big", &mut cursor)
        .await
        .expect("backend writes scripts");
    let agent_scripts = agent.js.get_object_store("scripts").await.unwrap();
    let mut obj = tokio::time::timeout(
        Duration::from_secs(60),
        agent_scripts.get("conformance/big"),
    )
    .await
    .expect("agent object get stalled (flow control?)")
    .expect("agent object get");
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), obj.read_to_end(&mut got))
        .await
        .expect("agent object read stalled (flow control?)")
        .unwrap();
    assert_eq!(got.len(), big.len());
    // agent_releases: a real read of an object the backend published.
    let releases = backend.js.get_object_store("agent_releases").await.unwrap();
    let mut cursor = std::io::Cursor::new(b"release-bytes".to_vec());
    releases
        .put("conformance/rel", &mut cursor)
        .await
        .expect("backend writes agent_releases");
    let agent_releases = agent
        .js
        .get_object_store("agent_releases")
        .await
        .expect("agent_releases");
    let mut obj = tokio::time::timeout(
        Duration::from_secs(30),
        agent_releases.get("conformance/rel"),
    )
    .await
    .expect("agent_releases get stalled")
    .expect("agent_releases get");
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), obj.read_to_end(&mut got))
        .await
        .expect("agent_releases read stalled")
        .unwrap();
    assert_eq!(got, b"release-bytes");

    // collections: a real manifest run with a `collect:` hint, through the
    // backend's job and exec handlers, the agent's collect-and-upload path and
    // the outbox. The result must name the stored bundle, which must be there.
    let fixture = f.agent_dir.path().join("collect-fixture.txt");
    std::fs::write(&fixture, b"collected").unwrap();
    let listing = toml_path(&fixture);
    let (shell, script) = if cfg!(windows) {
        (
            "powershell",
            format!("Write-Output '{{\"files\":[\"{listing}\"]}}'"),
        )
    } else {
        ("sh", format!("printf '{{\"files\":[\"%s\"]}}' '{listing}'"))
    };
    let manifest = format!(
        "id: conformance-collect\nversion: 0.1.0\nexecute:\n  shell: {shell}\n  timeout: 30s\n  \
         script: |\n    {script}\ncollect:\n  name: conformance\n"
    );
    let (st, body) = http_request(f.http_port, "POST", "/api/jobs", yaml, &manifest).await;
    assert!(st == 200 || st == 201, "job create: {st} {body}");
    let mut collected = backend.client.subscribe("results.*").await.unwrap();
    backend.settle().await;
    let plan = serde_json::json!({"target": {"pcs": [f.pc_id]}}).to_string();
    let (st, body) = http_request(
        f.http_port,
        "POST",
        "/api/exec/conformance-collect",
        Some("application/json"),
        &plan,
    )
    .await;
    assert!(st == 200 || st == 201, "exec: {st} {body}");
    let key = wait_collect_object(&mut collected, &f.pc_id, Duration::from_secs(60)).await;
    let mut bundle = backend
        .js
        .get_object_store("collections")
        .await
        .unwrap()
        .get(key.clone())
        .await
        .unwrap_or_else(|e| panic!("collected bundle {key}: {e}"));
    let mut zip = Vec::new();
    bundle.read_to_end(&mut zip).await.unwrap();
    assert!(
        zip.starts_with(b"PK"),
        "collected bundle {key} is not a zip"
    );

    // Break-glass `kill`, plus its audit subject.
    let out = f.cli(&["kill", "no-such-exec"]).await;
    assert!(
        out.status.success(),
        "kanade kill: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Reconnect: restart the broker under the running processes. Everything
    // re-establishes (re-auth, resubscribe, ordered consumers, durables) and a
    // fresh break-glass run still completes.
    f.broker.restart(Auth::Users).await;
    f.heartbeat(&mut hb, Duration::from_secs(120)).await;
    let marker = fresh("re");
    // The results subscription the CLI opens is new; the agent has to have
    // re-subscribed `commands.pc.*` for this to complete.
    let out = f.cli_run_echo(&marker, 60).await;
    assert_cli_ok(&out, &marker);

    // Projectors: every durable the backend runs has consumed and
    // acknowledged what the flows above produced.
    for (stream, durable) in [
        ("EVENTS", "backend_events_projector"),
        ("EVENTS", "backend_notification_acks_projector_v2"),
        ("OBS_EVENTS", "backend_obs_events_projector"),
        ("RESULTS", "backend_results_projector"),
        ("AUDIT", "backend_audit_projector"),
    ] {
        until(
            &format!("{durable} acked"),
            Duration::from_secs(60),
            || async { f.broker.ack_floor(stream, durable).await >= 1 },
        )
        .await;
    }

    // The second oracle: nothing above may have tripped a permission.
    let after: BTreeSet<Violation> = f.broker.violations().into_iter().collect();
    let new: Vec<_> = after.difference(&baseline).collect();
    assert!(
        new.is_empty(),
        "allowed flows tripped permission violations: {new:#?}\n\
         Each is a subject a real flow needs but the block lacks (or a flow the \
         inventory in the conf has wrong).",
    );
    let (backend_ok, agent_ok) = (
        f.backend.as_mut().unwrap().alive(),
        f.agent.as_mut().unwrap().alive(),
    );
    assert!(
        backend_ok && agent_ok,
        "a process exited during the allowed flows"
    );
}

/// The `collect_object` key of the first result from `pc_id` that has one.
async fn wait_collect_object(
    sub: &mut async_nats::Subscriber,
    pc_id: &str,
    within: Duration,
) -> String {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, sub.next())
            .await
            .unwrap_or_else(|_| panic!("no collect result within {within:?}"))
            .expect("results subscription closed");
        let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap_or_default();
        if v["pc_id"] == pc_id
            && let Some(k) = v["collect_object"].as_str()
        {
            return k.to_string();
        }
    }
}

async fn wait_result_containing(sub: &mut async_nats::Subscriber, needle: &str, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, sub.next())
            .await
            .unwrap_or_else(|_| panic!("no result containing {needle:?} within {within:?}"))
            .expect("results subscription closed");
        if String::from_utf8_lossy(&msg.payload).contains(needle) {
            return;
        }
    }
}

fn enqueue_outbox(dir: &Path, request_id: &str, pc_id: &str, stdout: &[u8]) {
    std::fs::create_dir_all(dir).expect("outbox dir");
    let now = chrono::Utc::now();
    let r = ExecResult {
        result_id: uuid::Uuid::new_v4().to_string(),
        request_id: request_id.into(),
        exec_id: None,
        parent_result_id: None,
        pc_id: pc_id.into(),
        exit_code: 0,
        skipped: Some(false),
        stdout: String::from_utf8(stdout.to_vec()).unwrap(),
        stderr: String::new(),
        started_at: now,
        finished_at: now,
        stdout_object: None,
        stderr_object: None,
        manifest_id: None,
        collect_object: None,
    };
    let tmp = dir.join(format!("{request_id}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&r).unwrap()).unwrap();
    std::fs::rename(&tmp, dir.join(format!("{request_id}.json"))).unwrap();
}

// ═══════════════════════════ B. denied operations ═══════════════════════════

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Allowed,
    Denied,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Publish(&'static str),
    Subscribe(&'static str),
    /// A request whose reply (if any) is not looked at: a JetStream API call.
    Request(&'static str),
}

impl Op {
    fn subject(self) -> &'static str {
        match self {
            Op::Publish(s) | Op::Subscribe(s) | Op::Request(s) => s,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Op::Subscribe(_) => "Subscription",
            _ => "Publish",
        }
    }
}

/// `(role, op, expectation)` — one row per claim the block makes. The reason
/// for each sits in the conf next to the grant.
fn expectations() -> Vec<(Role, Op, Expect)> {
    use Expect::*;
    use Op::*;
    use Role::*;
    let mut t = vec![
        // ── agent: denied ──
        (Agent, Publish("commands.pc.victim"), Denied),
        (Agent, Publish("commands.all"), Denied),
        (Agent, Publish("commands.group.victim"), Denied),
        (Agent, Publish("kill.victim"), Denied),
        (Agent, Publish("notif-amend"), Denied),
        (Agent, Publish("audit.operator.run.x"), Denied),
        (Agent, Publish("notifications.all"), Denied),
        (Agent, Publish("_INBOX.forged.reply"), Denied),
        (Agent, Subscribe("heartbeat.>"), Denied),
        (Agent, Subscribe("results.*"), Denied),
        (Agent, Subscribe("audit.>"), Denied),
        (Agent, Subscribe("remote.frame.*"), Denied),
        (Agent, Subscribe("$KV.server_settings.>"), Denied),
        (Agent, Request("$JS.API.INFO"), Denied),
        (Agent, Request("$JS.API.STREAM.CREATE.EVIL"), Denied),
        (Agent, Request("$JS.API.STREAM.CREATE.KV_evil"), Denied),
        (Agent, Request("$JS.API.STREAM.UPDATE.KV_jobs"), Denied),
        (Agent, Request("$JS.API.STREAM.UPDATE.EXEC"), Denied),
        (Agent, Request("$JS.API.STREAM.DELETE.KV_jobs"), Denied),
        (
            Agent,
            Request("$JS.API.STREAM.DELETE.OBJ_result_output"),
            Denied,
        ),
        (Agent, Request("$JS.API.STREAM.PURGE.KV_jobs"), Denied),
        (Agent, Request("$JS.API.STREAM.PURGE.OBJ_scripts"), Denied),
        (Agent, Request("$JS.API.STREAM.PURGE.RESULTS"), Denied),
        (
            Agent,
            Request("$JS.API.STREAM.MSG.DELETE.NOTIFICATIONS"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.STREAM.INFO.KV_server_settings"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.CONSUMER.CREATE.KV_server_settings"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.DIRECT.GET.KV_server_settings.$KV.server_settings.current"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.CONSUMER.DELETE.EXEC.someone"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.CONSUMER.CREATE.RESULTS.evil"),
            Denied,
        ),
        // ── agent: allowed ──
        (Agent, Publish("heartbeat.x"), Allowed),
        (Agent, Publish("results.x"), Allowed),
        (Agent, Publish("$KV.notifications_read.x"), Allowed),
        (Agent, Publish("$O.result_output.C.x"), Allowed),
        (Agent, Request("$JS.API.STREAM.INFO.KV_jobs"), Allowed),
        (Agent, Request("$JS.API.STREAM.INFO.EXEC"), Allowed),
        (Agent, Subscribe("kill.*"), Allowed),
        (Agent, Subscribe("agents.x.ping"), Allowed),
        // The one group the narrowing edit changes:
        (
            Agent,
            Subscribe("commands.all"),
            COMMAND_BROADCAST_SUBSCRIBE,
        ),
        (
            Agent,
            Subscribe("commands.group.x"),
            COMMAND_BROADCAST_SUBSCRIBE,
        ),
        (
            Agent,
            Subscribe("commands.pc.x"),
            COMMAND_BROADCAST_SUBSCRIBE,
        ),
        // ── break-glass: nothing but its four flows ──
        (Breakglass, Publish("commands.all"), Denied),
        (Breakglass, Publish("commands.group.x"), Denied),
        (Breakglass, Publish("heartbeat.x"), Denied),
        (Breakglass, Publish("results.x"), Denied),
        (Breakglass, Publish("audit.operator.exec.x"), Denied),
        (Breakglass, Publish("audit.backend.run.x"), Denied),
        (Breakglass, Publish("$KV.jobs.x"), Denied),
        (Breakglass, Publish("$O.scripts.C.x"), Denied),
        (Breakglass, Request("$JS.API.INFO"), Denied),
        (Breakglass, Request("$JS.API.STREAM.INFO.EXEC"), Denied),
        (Breakglass, Request("$JS.API.STREAM.CREATE.EVIL"), Denied),
        (
            Breakglass,
            Request("$JS.API.CONSUMER.CREATE.EXEC.x"),
            Denied,
        ),
        (Breakglass, Subscribe("commands.>"), Denied),
        (Breakglass, Subscribe("heartbeat.>"), Denied),
        (Breakglass, Subscribe("_INBOX.>"), Denied),
        (Breakglass, Publish("_INBOX.forged.reply"), Denied),
        (Breakglass, Subscribe(">"), Denied),
        (Breakglass, Publish("commands.pc.x"), Allowed),
        (Breakglass, Publish("kill.x"), Allowed),
        (Breakglass, Publish("audit.operator.run.x"), Allowed),
        (Breakglass, Publish("audit.operator.kill.x"), Allowed),
        (Breakglass, Subscribe("results.x"), Allowed),
        // ── backend: allowed its administration, still not everything ──
        (Backend, Request("$JS.API.INFO"), Allowed),
        (Backend, Request("$JS.API.STREAM.INFO.EXEC"), Allowed),
        (Backend, Publish("commands.pc.x"), Allowed),
        (Backend, Publish("audit.backend.x"), Allowed),
        (Backend, Subscribe("heartbeat.>"), Allowed),
        (Backend, Subscribe("results.*"), Allowed),
        (Backend, Publish("heartbeat.x"), Denied),
        (Backend, Publish("results.x"), Denied),
        (Backend, Publish("_INBOX.forged.reply"), Denied),
        (Backend, Subscribe("commands.>"), Denied),
        (Backend, Subscribe(">"), Denied),
    ];
    // The ten buckets an agent must not write, each as a JetStream KV write.
    for b in AGENT_PROTECTED_BUCKETS {
        let subject: &'static str = Box::leak(format!("$KV.{b}.evil").into_boxed_str());
        t.push((Agent, Publish(subject), Denied));
    }
    t
}

async fn run_op(c: &RoleClient, op: Op) {
    match op {
        Op::Publish(s) => {
            c.client.publish(s, Bytes::from_static(b"x")).await.unwrap();
        }
        Op::Subscribe(s) => {
            // Kept alive until the client drops, as a real subscription is.
            std::mem::forget(c.client.subscribe(s).await.unwrap());
        }
        Op::Request(s) => {
            // Not awaited: a refused request never gets a reply, and the
            // verdict comes from the violation, not from the wait.
            let client = c.client.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    client.request(s, Bytes::from_static(b"{}")),
                )
                .await;
            });
        }
    }
}

#[tokio::test]
#[ignore = "requires nats-server in PATH and built binaries; cargo test -- --ignored"]
async fn denied_operations_are_denied_per_role() {
    prereqs_or_skip!();
    let mut f = Fleet::new(Auth::Users, false, None).await;
    f.start_backend().await;

    // The table. One connection per role, rows run in order; a row's verdict
    // looks only at what the broker said after that row began, so an earlier
    // violation cannot be mistaken for a later one.
    let mut wrong = Vec::new();
    let mut clients = std::collections::HashMap::new();
    for role in Role::ALL {
        clients.insert(role, f.role_fast(role).await);
    }
    for (role, op, expect) in expectations() {
        let c = &clients[&role];
        let mark = c.errors.lock().unwrap().len();
        run_op(c, op).await;
        c.settle().await;
        if expect == Expect::Denied {
            // The error is on its way; give it room, but stop at the first.
            let deadline = Instant::now() + Duration::from_secs(3);
            while !c.violated_since(mark, op.kind(), op.subject()) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let denied = c.violated_since(mark, op.kind(), op.subject());
        let said = c.violations_since(mark);
        match expect {
            Expect::Denied if !denied => wrong.push(format!(
                "{role:?} {op:?}: expected a permission violation, got {said:?}"
            )),
            Expect::Allowed if !said.is_empty() => wrong.push(format!(
                "{role:?} {op:?}: expected to be allowed but the broker said {said:?}"
            )),
            _ => {}
        }
    }
    assert!(
        wrong.is_empty(),
        "expectation table mismatches:\n{}",
        wrong.join("\n")
    );

    // Delivery denials, from absence of delivery on another connection after
    // a positive control proved the path.
    let backend = f.role_fast(Role::Backend).await;
    let agent = f.role_fast(Role::Agent).await;
    let observer = f.role_fast(Role::Agent).await; // agents may subscribe `commands.pc.*`
    let glass = f.role_fast(Role::Breakglass).await;
    let mut seen = observer
        .client
        .subscribe("commands.pc.victim")
        .await
        .unwrap();
    observer.settle().await;
    glass
        .client
        .publish("commands.pc.victim", Bytes::from_static(b"control"))
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), seen.next())
        .await
        .expect("positive control: break-glass publish never reached the observer")
        .unwrap();
    assert_eq!(&got.payload[..], b"control");
    agent
        .client
        .publish("commands.pc.victim", Bytes::from_static(b"forged"))
        .await
        .unwrap();
    agent.settle().await;
    assert!(
        agent.violated("Publish", "commands.pc.victim"),
        "agent publish to commands.* was not refused"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), seen.next())
            .await
            .is_err(),
        "an agent's forged command reached another subscriber"
    );

    // KV writes the agent must not make, through the real `kv.put`. Control
    // first: the backend writes and reads back, so a missing forged key means
    // the write was refused rather than the bucket being unreadable. The
    // attempts run together so a refusal (answered only by silence) costs one
    // timeout, not ten.
    let mut writable = Vec::new();
    for b in AGENT_PROTECTED_BUCKETS {
        // server_settings is created on first use; absent here is fine — the
        // agent still must not be able to create it.
        let Ok(write) = backend.js.get_key_value(*b).await else {
            continue;
        };
        write
            .put("control", Bytes::from_static(b"ok"))
            .await
            .expect("backend control write");
        assert_eq!(
            write.get("control").await.unwrap().as_deref(),
            Some(&b"ok"[..])
        );
        writable.push((*b, write));
    }
    let attempts = writable.iter().map(|(b, _)| {
        let js = agent.js.clone();
        async move {
            // The agent either cannot see the bucket at all (no flow of its
            // reads it) or can read it but not write it. The raw
            // `$KV.<bucket>.evil` publish for every one is in the table above.
            match js.get_key_value(*b).await {
                Err(_) => (*b, false),
                Ok(kv) => {
                    let r = tokio::time::timeout(
                        Duration::from_secs(3),
                        kv.put("evil", Bytes::from_static(b"forged")),
                    )
                    .await;
                    (*b, matches!(r, Ok(Ok(_))))
                }
            }
        }
    });
    for (b, wrote) in futures::future::join_all(attempts).await {
        assert!(!wrote, "agent wrote {b}");
    }
    agent.settle().await;
    for (b, write) in &writable {
        if AGENT_UNREADABLE_BUCKETS.contains(b) {
            assert!(
                agent.violated("Publish", &format!("$JS.API.STREAM.INFO.KV_{b}")),
                "agent was not refused a read of {b}"
            );
        } else {
            assert!(
                agent.violated("Publish", &format!("$KV.{b}.evil")),
                "no permission violation for an agent write to {b}"
            );
        }
        assert!(
            write.get("evil").await.unwrap().is_none(),
            "forged key landed in {b}"
        );
    }

    // Real-API denials: create / update / delete / purge streams, buckets and
    // stores the agent does not own, and read server_settings. Fired together;
    // each is judged by the violation the broker sent and by the resources
    // still being exactly as the backend left them.
    let jobs_history = backend
        .js
        .get_stream("KV_jobs")
        .await
        .unwrap()
        .cached_info()
        .config
        .max_messages_per_subject;
    let a = agent.js.clone();
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            a.get_key_value("server_settings"),
            a.create_key_value(kv::Config {
                bucket: "evil".into(),
                ..Default::default()
            }),
            a.create_object_store(object_store::Config {
                bucket: "evil".into(),
                ..Default::default()
            }),
            a.update_key_value(kv::Config {
                bucket: "jobs".into(),
                history: 50,
                ..Default::default()
            }),
            a.update_stream(jetstream::stream::Config {
                name: "KV_schedules".into(),
                max_messages: 1,
                ..Default::default()
            }),
            a.delete_key_value("jobs"),
            a.delete_object_store("scripts"),
            a.delete_stream("EXEC"),
            async {
                if let Ok(s) = a.get_stream("KV_jobs").await {
                    let _ = s.purge().await;
                }
            },
            async {
                // The stream the agent must not even see.
                let _ = a.get_stream("KV_server_settings").await;
            },
        )
    })
    .await;
    agent.settle().await;
    for (kind, subject) in [
        ("Publish", "$JS.API.STREAM.INFO.KV_server_settings"),
        // create_key_value asks for account info before it creates anything.
        ("Publish", "$JS.API.INFO"),
        ("Publish", "$JS.API.STREAM.CREATE.OBJ_evil"),
        ("Publish", "$JS.API.STREAM.UPDATE.KV_schedules"),
        ("Publish", "$JS.API.STREAM.DELETE.KV_jobs"),
        ("Publish", "$JS.API.STREAM.DELETE.OBJ_scripts"),
        ("Publish", "$JS.API.STREAM.DELETE.EXEC"),
        ("Publish", "$JS.API.STREAM.PURGE.KV_jobs"),
    ] {
        assert!(
            agent.violated(kind, subject),
            "no violation recorded for {kind} {subject}; the agent was told {:#?}",
            agent.any_violation()
        );
    }
    for s in ["KV_jobs", "KV_schedules", "OBJ_scripts", "EXEC"] {
        let info = backend
            .js
            .get_stream(s)
            .await
            .unwrap_or_else(|e| panic!("{s} vanished: {e}"));
        assert_ne!(
            info.cached_info().config.max_messages,
            1,
            "{s} was reconfigured"
        );
    }
    assert_eq!(
        backend
            .js
            .get_stream("KV_jobs")
            .await
            .unwrap()
            .cached_info()
            .config
            .max_messages_per_subject,
        jobs_history,
        "KV_jobs history was changed by the agent"
    );
    for s in ["KV_evil", "OBJ_evil"] {
        assert!(backend.js.get_stream(s).await.is_err(), "agent created {s}");
    }

    // Break-glass through the real CLI: its four flows work (A covers the
    // happy path); a CLI flow that is not one of them is refused.
    let g = f.role_fast(Role::Breakglass).await;
    assert!(
        g.js.get_stream("EXEC").await.is_err(),
        "break-glass reached JetStream"
    );
    g.settle().await;
    assert!(g.violated("Publish", "$JS.API.STREAM.INFO.EXEC"));

    // Documented residual, asserted so a change in either direction is seen:
    // the broker cannot tell one agent's `notifications_read` key from
    // another's, because the three users are shared by every host of a role.
    let rs = agent.js.get_key_value("notifications_read").await.unwrap();
    assert!(
        rs.put("someone-elses-host.u1", Bytes::from_static(b"{}"))
            .await
            .is_ok(),
        "residual changed: a per-host notifications_read write is now refused"
    );
}

// ═══════════════════════ C. token → users → token ═══════════════════════

/// A TCP pass-through whose live connections can all be cut at once, so a
/// client is forced to authenticate again — which is the moment its
/// credential selection runs.
#[cfg(unix)]
struct Proxy {
    port: u16,
    conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    accept: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl Proxy {
    async fn start(target: u16) -> Self {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("proxy bind");
        let port = l.local_addr().unwrap().port();
        let conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let reg = conns.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = l.accept().await else {
                    return;
                };
                let h = tokio::spawn(async move {
                    if let Ok(mut out) = tokio::net::TcpStream::connect(("127.0.0.1", target)).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                    }
                });
                reg.lock().unwrap().push(h.abort_handle());
            }
        });
        Self {
            port,
            conns,
            accept,
        }
    }

    fn cut(&self) {
        for h in self.conns.lock().unwrap().drain(..) {
            h.abort();
        }
    }
}

#[cfg(unix)]
impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.cut();
    }
}

/// What the broker says each kanade connection authenticated as.
#[cfg(unix)]
async fn principals(b: &Broker) -> Vec<(String, String)> {
    b.connections()
        .await
        .into_iter()
        .filter(|(n, _)| n.starts_with("kanade-"))
        .collect()
}

#[cfg(unix)]
fn on_users(p: &[(String, String)], pc: &str) -> bool {
    let agent = p.iter().any(|(n, u)| n.contains(pc) && u == "agent");
    let backend = p
        .iter()
        .any(|(n, u)| n == "kanade-backend" && u == "backend");
    agent && backend
}

#[cfg(unix)]
fn on_token(p: &[(String, String)], pc: &str) -> bool {
    let agent = p.iter().any(|(n, u)| n.contains(pc) && u != "agent");
    let backend = p
        .iter()
        .any(|(n, u)| n == "kanade-backend" && u != "backend");
    agent && backend
}

/// Everything the three roles do, once, with fresh identifiers: the agent
/// heartbeats, the backend pings it and fetches its log through the real
/// handlers, and the break-glass CLI runs a command and gets the result.
#[cfg(unix)]
async fn every_role_works(f: &mut Fleet, stage: &str, observe_as: Auth) {
    let mut hb = observe_heartbeats(f, observe_as).await;
    f.heartbeat(&mut hb, Duration::from_secs(90)).await;
    let (st, body) = http_request(
        f.http_port,
        "POST",
        &format!("/api/agents/{}/ping", f.pc_id),
        None,
        "",
    )
    .await;
    assert_eq!(st, 200, "[{stage}] backend → agent ping: {body}");
    let (st, _) = http_request(
        f.http_port,
        "GET",
        &format!("/api/agents/{}/logs?tail=5", f.pc_id),
        None,
        "",
    )
    .await;
    assert_eq!(st, 200, "[{stage}] backend → agent log fetch");
    let marker = fresh("sw");
    let out = f.cli_run_echo(&marker, 60).await;
    assert!(
        out.status.success() && stdout_of(&out).contains(&marker),
        "[{stage}] break-glass run did not complete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        f.agent.as_mut().unwrap().alive(),
        "[{stage}] the agent exited"
    );
    assert!(
        f.backend.as_mut().unwrap().alive(),
        "[{stage}] the backend exited"
    );
}

/// The CLI that was started before a switch must still receive its result.
#[cfg(unix)]
async fn assert_slow_cli_completes(child: Child, marker: &str, stage: &str) {
    let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("[{stage}] the waiting kanade run hung"))
        .expect("kanade run output");
    assert!(
        out.status.success() && stdout_of(&out).contains(marker),
        "[{stage}] a kanade run kept across the switch lost its result: {}\n{}",
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A heartbeat subscription on whichever credential the broker accepts in
/// that stage. Kept alive for the rest of the test.
#[cfg(unix)]
async fn observe_heartbeats(f: &Fleet, auth: Auth) -> async_nats::Subscriber {
    let opts = match auth {
        Auth::Token => async_nats::ConnectOptions::with_token(TOKEN.into()),
        Auth::Users => async_nats::ConnectOptions::with_user_and_password(
            Role::Backend.user().into(),
            Role::Backend.password(),
        ),
    };
    let c = opts
        .connect(&f.broker.url())
        .await
        .expect("observer connect");
    let s = c.subscribe(format!("heartbeat.{}", f.pc_id)).await.unwrap();
    std::mem::forget(c);
    s
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires nats-server in PATH and built binaries; cargo test -- --ignored"]
async fn token_to_users_and_back_with_processes_running() {
    prereqs_or_skip!();
    // Every process is given the token AND its role's user, as a host in the
    // middle of the migration would be, and dials the broker through a proxy
    // that can cut every live connection so each stage forces a fresh
    // authentication.
    let mut f = Fleet::new(Auth::Token, true, None).await;
    let proxy = Proxy::start(f.broker.port).await;
    f.dial_url = format!("nats://127.0.0.1:{}", proxy.port);
    f.start_backend().await;
    f.start_agent().await;
    // 30 s is the default heartbeat cadence; every stage below waits for one.
    {
        let c = async_nats::ConnectOptions::with_token(TOKEN.into())
            .connect(&f.broker.url())
            .await
            .expect("config writer connect");
        let kv = jetstream::new(c)
            .get_key_value("agent_config")
            .await
            .expect("agent_config");
        kv.put(
            "global",
            Bytes::from_static(br#"{"heartbeat_interval":"1s"}"#),
        )
        .await
        .expect("write agent_config");
    }

    // Stage 1 — token broker. The user the helper offers is refused by the
    // probe, so the token is what goes on the wire.
    every_role_works(&mut f, "token", Auth::Token).await;
    until(
        "both processes on the token",
        Duration::from_secs(30),
        || async { on_token(&principals(&f.broker).await, &f.pc_id) },
    )
    .await;

    // Stage 2 — reload as `users`, processes untouched. The broker stops
    // admitting the token; the clients pick the user at their next attempt.
    f.broker.reload(Auth::Users);
    proxy.cut();
    until(
        "both processes re-authenticated as their users",
        Duration::from_secs(90),
        || async { on_users(&principals(&f.broker).await, &f.pc_id) },
    )
    .await;
    every_role_works(&mut f, "users", Auth::Users).await;

    // Stage 3 — reload back to the token, processes still untouched. A reload
    // back leaves the earlier users accepted on this broker, so the clients
    // (whose probe still succeeds) stay on their users. That is recorded, not
    // assumed away: the evidence is the principal the broker reports after a
    // forced reconnect, and the processes keep working either way.
    let slow_marker = fresh("slow");
    let slow = f.cli_spawn_slow(&slow_marker, 20);
    tokio::time::sleep(Duration::from_secs(3)).await;
    f.broker.reload(Auth::Token);
    proxy.cut();
    let reverted_by_reload = {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if on_token(&principals(&f.broker).await, &f.pc_id) {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    eprintln!("revert by reload alone returned the clients to the token: {reverted_by_reload}");
    assert_slow_cli_completes(slow, &slow_marker, "users → token (reload)").await;
    if !reverted_by_reload {
        every_role_works(&mut f, "reload-back (users still accepted)", Auth::Users).await;
        // Rollback therefore needs a broker restart; it has to work under the
        // running processes and bring every role back onto the token.
        f.broker.restart(Auth::Token).await;
        proxy.cut();
        until(
            "both processes back on the token after a restart",
            Duration::from_secs(90),
            || async { on_token(&principals(&f.broker).await, &f.pc_id) },
        )
        .await;
    }
    every_role_works(&mut f, "token again", Auth::Token).await;
}

// ═══════════════════════════ D. known residuals ═══════════════════════════

/// An agent can create consumers on streams it can write (its own
/// `notifications_read` bucket, which the KV watch needs). A consumer's
/// delivery target is the subject the broker pushes its messages to. This
/// probes whether the broker delivers an agent-chosen payload to a subject the
/// agent is not allowed to publish (`commands.pc.<victim>`), the delivery
/// target bypass seen on earlier broker versions.
///
/// The test records what the pinned broker does today. It is documentation of
/// current behaviour, not a statement that the outcome is acceptable or
/// permanent. What makes injected bytes inert is command verification on the
/// receiving agent, not this block; but code that still accepts unsigned
/// commands (signing enforcement is optional today) means "inert" must be
/// confirmed against the deployment's enforcement setting before it is
/// claimed.
#[tokio::test]
#[ignore = "requires nats-server in PATH and built binaries; cargo test -- --ignored"]
async fn consumer_delivery_target_residual_is_recorded() {
    prereqs_or_skip!();
    let mut f = Fleet::new(Auth::Users, false, None).await;
    f.start_backend().await;

    let attacker = f.role_fast(Role::Agent).await;
    let victim = f.role_fast(Role::Agent).await;

    // The attacker writes bytes of its choosing into a stream it may write.
    let kvb = attacker
        .js
        .get_key_value("notifications_read")
        .await
        .unwrap();
    kvb.put("residual.probe", Bytes::from_static(b"ATTACKER-BYTES"))
        .await
        .unwrap();

    // Positive control: the victim's subject is observable, and a legitimate
    // publish reaches it.
    let target = "commands.pc.residual-victim";
    let mut seen = victim.client.subscribe(target).await.unwrap();
    victim.settle().await;
    let glass = f.role_fast(Role::Breakglass).await;
    glass
        .client
        .publish(target, Bytes::from_static(b"control"))
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), seen.next())
        .await
        .expect("positive control did not arrive")
        .unwrap();
    assert_eq!(&got.payload[..], b"control");

    // The probe: a push consumer on the attacker's stream, delivering to the
    // victim's subject.
    let cfg = serde_json::json!({
        "stream_name": "KV_notifications_read",
        "config": {
            "name": "residual_probe",
            "deliver_subject": target,
            "deliver_policy": "all",
            "ack_policy": "none",
            "filter_subject": "$KV.notifications_read.residual.probe",
        }
    });
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        attacker.client.request(
            "$JS.API.CONSUMER.CREATE.KV_notifications_read",
            serde_json::to_vec(&cfg).unwrap().into(),
        ),
    )
    .await;
    let created = match reply {
        Ok(Ok(m)) => {
            let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap_or_default();
            v.get("error").is_none()
        }
        _ => false,
    };
    let delivered = tokio::time::timeout(Duration::from_secs(3), seen.next())
        .await
        .ok()
        .flatten()
        .map(|m| m.payload.to_vec());
    eprintln!(
        "residual probe: consumer created={created}, attacker bytes delivered to {target}={}",
        delivered.is_some()
    );

    // Recorded outcome on the pinned broker. Update deliberately if a broker
    // bump changes it, and record why.
    assert_eq!(
        (created, delivered.as_deref()),
        RECORDED_DELIVERY_BYPASS,
        "the consumer delivery-target behaviour of this broker changed; \
         re-assess the residual and update the record"
    );
}

/// `(consumer created, bytes delivered)` observed on the pinned broker.
const RECORDED_DELIVERY_BYPASS: (bool, Option<&[u8]>) = (true, Some(b"ATTACKER-BYTES"));
