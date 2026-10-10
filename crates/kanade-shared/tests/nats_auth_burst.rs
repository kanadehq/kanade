//! How the role-level users configuration behaves when many clients
//! authenticate at once.
//!
//! nats-server verifies a bcrypt hash on every connect, which is CPU work, and
//! enforces an authorization timeout from the moment the connection is
//! accepted. A fleet reconnecting after a broker restart is exactly that
//! burst. This file measures it against a real broker started from the
//! checked-in `configs/nats-server.users.conf`, with bcrypt hashes of a chosen
//! cost, through the real `connect` machinery of `kanade_shared::nats_client`.
//!
//! It is a measurement, not a gate: it asserts nothing about the numbers and
//! prints a markdown table. Absolute values belong to the machine they were
//! taken on and must be taken again on production hardware.
//!
//! Ignored by default (needs `nats-server` in PATH; release mode recommended):
//!
//! ```text
//! KANADE_AUTH_BURST_N=200 KANADE_AUTH_BURST_COSTS=10,11,12 \
//!   cargo test -p kanade-shared --release --test nats_auth_burst -- --ignored --nocapture
//! ```
//!
//! Environment:
//!
//! * `KANADE_AUTH_BURST_N`              clients per scenario (default 200)
//! * `KANADE_AUTH_BURST_COSTS`          comma list of bcrypt costs (default 11)
//! * `KANADE_AUTH_BURST_AUTH_TIMEOUTS`  comma list of broker authorization
//!   timeouts in seconds; `shipped` keeps whatever the checked-in file says
//!   (default `shipped`)
//! * `KANADE_AUTH_BURST_GOMAXPROCS`     CPU limit handed to the broker, to
//!   stand in for a smaller host
//! * `KANADE_AUTH_BURST_DEADLINE_SECS`  how long a scenario waits for every
//!   client before counting the rest as not converged (default 120)
//!
//! Two scenarios per configuration. `start`: N clients begin together against
//! a running broker. `restart`: N connected clients, the broker is killed and
//! started again on the same port.
//!
//! "Time to authenticated" is measured from the scenario start to the
//! `Connected` event of that client, not to the return of `connect`: with
//! `retry_on_initial_connect` the helper returns before any authentication.
//! A client is `not converged` when it has not connected by the deadline;
//! the distribution covers converged clients only, so read it together with
//! that count. Each client reaches the broker through a listener of its own
//! that counts the broker's `Authentication Timeout` and `Authorization
//! Violation` answers (async-nats hands the first to the event callback only as
//! a generic I/O error, and the credential probe has no callback), so errors
//! and retries are per client; the broker's own log is the cross-check.
//! `KANADE_AUTH_BURST_JSONL=<file>` appends one JSON line per client.
//!
//! Each user client costs the broker two bcrypt verifications per attempt: the
//! credential probe and the real connection.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kanade_shared::nats_client::{
    NatsCredentials, NatsRole, connect_with_credentials_and_event_callback,
};
use tokio::net::TcpStream;
use tokio::sync::{Barrier, Notify};

const USERS_CONF: &str = include_str!("../../../configs/nats-server.users.conf");
const ROLES: [&str; 3] = ["agent", "backend", "breakglass"];

fn password(user: &str) -> String {
    format!("burst-pw-{user}")
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

fn nats_server_version() -> String {
    std::process::Command::new("nats-server")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A burst of N clients needs roughly N sockets in this process and in the
/// broker, plus the transient probe connections. Running out is not an
/// authentication failure and must not be counted as one.
fn require_fd_limit(n: usize) {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("sh")
            .args(["-c", "ulimit -n"])
            .output()
            .expect("sh");
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Ok(limit) = text.parse::<usize>() {
            assert!(
                limit >= 3 * n + 256,
                "open-file limit {limit} is too low for N={n}; raise it with `ulimit -n`"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = n;
}

fn hashes(cost: u32) -> Vec<(&'static str, String)> {
    ROLES
        .iter()
        .map(|u| {
            let h = bcrypt::hash_with_result(password(u), cost)
                .expect("bcrypt hash")
                .format_for_version(bcrypt::Version::TwoA);
            (*u, h)
        })
        .collect()
}

/// The shipped users block, with the authorization timeout replaced when a
/// number is asked for. `None` keeps the file as it is checked in.
fn users_conf(auth_timeout: Option<u64>) -> String {
    match auth_timeout {
        None => USERS_CONF.to_string(),
        Some(secs) => {
            let mut out = String::new();
            for line in USERS_CONF.lines() {
                let t = line.trim_start();
                if t.starts_with("timeout:") {
                    continue;
                }
                out.push_str(line);
                out.push('\n');
                if t.starts_with("authorization {") {
                    out.push_str(&format!("  timeout: {secs}\n"));
                }
            }
            out
        }
    }
}

struct Broker {
    child: Option<tokio::process::Child>,
    dir: tempfile::TempDir,
    port: u16,
    hashes: Vec<(&'static str, String)>,
    gomaxprocs: Option<String>,
    spawns: usize,
}

impl Broker {
    fn new(cost: u32, auth_timeout: Option<u64>) -> Self {
        Self::with_conf(&users_conf(auth_timeout), cost)
    }

    /// A broker on a free port whose authorization is `auth_conf`.
    fn with_conf(auth_conf: &str, cost: u32) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let port = portpicker::pick_unused_port().expect("broker port");
        let conf = format!("host: 127.0.0.1\nport: {port}\n{auth_conf}\n");
        std::fs::write(dir.path().join("nats.conf"), conf).expect("write nats.conf");
        Self {
            child: None,
            dir,
            port,
            hashes: hashes(cost),
            gomaxprocs: std::env::var("KANADE_AUTH_BURST_GOMAXPROCS").ok(),
            spawns: 0,
        }
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    fn log_path(&self) -> std::path::PathBuf {
        self.dir.path().join(format!("broker-{}.log", self.spawns))
    }

    /// Start (or restart) the broker; returns once it accepts connections.
    async fn up(&mut self) -> Instant {
        self.spawns += 1;
        let log = std::fs::File::create(self.log_path()).expect("broker log");
        let log2 = log.try_clone().expect("clone log");
        let mut cmd = tokio::process::Command::new("nats-server");
        cmd.arg("-c")
            .arg(self.dir.path().join("nats.conf"))
            .arg("-T=false")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .kill_on_drop(true);
        for (user, hash) in &self.hashes {
            // The hash is quoted: bcrypt contains `$`, which the
            // configuration language otherwise reads as a variable.
            cmd.env(
                format!("KANADE_NATS_{}_PASSWORD_HASH", user.to_uppercase()),
                format!("\"{hash}\""),
            );
        }
        if let Some(g) = &self.gomaxprocs {
            cmd.env("GOMAXPROCS", g);
        }
        let started = Instant::now();
        self.child = Some(cmd.spawn().expect("spawn nats-server"));
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", self.port)).await.is_err() {
            assert!(Instant::now() < deadline, "nats-server did not come up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        started
    }

    async fn down(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }

    /// Lines of the current spawn's log: connections closed before they were
    /// registered (what an authentication timeout leaves; the broker does not
    /// log the timeout itself at the default level), and explicit auth errors.
    fn log_counts(&self) -> (usize, usize) {
        let text = std::fs::read_to_string(self.log_path()).unwrap_or_default();
        let count = |needle: &str| {
            text.lines()
                .filter(|l| l.to_lowercase().contains(needle))
                .count()
        };
        (
            count("problem registering with account"),
            count("authorization violation")
                + count("authentication error")
                + count("authentication timeout"),
        )
    }

    /// The most frequent log messages with addresses and ids stripped, so a
    /// failure the counters do not name is still visible.
    fn log_digest(&self) -> Vec<(String, usize)> {
        let text = std::fs::read_to_string(self.log_path()).unwrap_or_default();
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        for line in text.lines() {
            let msg = line.split(" - ").last().unwrap_or(line);
            let norm: String = msg.chars().filter(|c| !c.is_ascii_digit()).collect();
            *seen.entry(norm).or_default() += 1;
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        v.truncate(3);
        v
    }
}

/// What one client saw: its own event callback, and the counting proxy that
/// sits between it and the broker. The proxy is what makes the broker's
/// `Authentication Timeout` visible per client: async-nats reports it to the
/// event callback only as a generic I/O error, and the credential probe has
/// no callback at all.
struct Stat {
    base: Instant,
    connected: AtomicU64,
    last_connected_us: AtomicU64,
    event_errors: AtomicU64,
    /// TCP connections that reached the broker, probes included.
    conns: AtomicU64,
    /// Of those, the credential probes (recognised by name in CONNECT).
    probe_conns: AtomicU64,
    timeouts: AtomicU64,
    violations: AtomicU64,
    notify: Notify,
}

impl Stat {
    fn new(base: Instant) -> Self {
        Self {
            base,
            connected: AtomicU64::new(0),
            last_connected_us: AtomicU64::new(0),
            event_errors: AtomicU64::new(0),
            conns: AtomicU64::new(0),
            probe_conns: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            violations: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    fn on_event(&self, ev: async_nats::Event) {
        match ev {
            async_nats::Event::Connected => {
                self.last_connected_us
                    .store(self.base.elapsed().as_micros() as u64, Ordering::SeqCst);
                self.connected.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_one();
            }
            async_nats::Event::Disconnected => {}
            _ => {
                self.event_errors.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    async fn wait_connected(&self, target: u64, deadline: Instant) -> bool {
        loop {
            if self.connected.load(Ordering::SeqCst) >= target {
                return true;
            }
            let wait = tokio::time::timeout_at(deadline.into(), self.notify.notified());
            if wait.await.is_err() {
                return self.connected.load(Ordering::SeqCst) >= target;
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            event_errors: self.event_errors.load(Ordering::SeqCst),
            conns: self.conns.load(Ordering::SeqCst),
            probe_conns: self.probe_conns.load(Ordering::SeqCst),
            timeouts: self.timeouts.load(Ordering::SeqCst),
            violations: self.violations.load(Ordering::SeqCst),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    event_errors: u64,
    conns: u64,
    probe_conns: u64,
    timeouts: u64,
    violations: u64,
}

/// One client's own listener, forwarding to the broker. Port 0, so the OS
/// picks it. Counts, per accepted-and-forwarded connection, what the broker
/// answered; it adds no delay of its own.
async fn client_proxy(upstream: u16, stat: Arc<Stat>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("proxy listener");
    let port = listener.local_addr().expect("proxy addr").port();
    let accept = tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(proxy_conn(client, upstream, stat.clone()));
        }
    });
    (port, accept)
}

async fn proxy_conn(mut client: TcpStream, upstream: u16, stat: Arc<Stat>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
        return;
    };
    stat.conns.fetch_add(1, Ordering::SeqCst);
    let (mut cr, mut cw) = client.split();
    let (mut sr, mut sw) = server.split();
    let to_client = async {
        let mut buf = [0u8; 4096];
        loop {
            match sr.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    if text.contains("authentication timeout") {
                        stat.timeouts.fetch_add(1, Ordering::SeqCst);
                    }
                    if text.contains("authorization violation") {
                        stat.violations.fetch_add(1, Ordering::SeqCst);
                    }
                    if cw.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    let to_server = async {
        let mut buf = [0u8; 4096];
        let mut probe = false;
        loop {
            match cr.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if !probe
                        && String::from_utf8_lossy(&buf[..n]).contains("\"name\":\"auth-probe\"")
                    {
                        probe = true;
                        stat.probe_conns.fetch_add(1, Ordering::SeqCst);
                    }
                    if sw.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    tokio::select! {
        _ = to_client => {}
        _ = to_server => {}
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// One client's outcome in one scenario.
struct ClientResult {
    time_ms: Option<f64>,
    timeouts: u64,
    violations: u64,
    /// Real (non-probe) connection attempts beyond the first.
    retries: u64,
    event_errors: u64,
}

struct Report {
    scenario: &'static str,
    cost: u32,
    timeout: String,
    clients: Vec<ClientResult>,
    attempts: u64,
    broker_timeouts: usize,
    broker_violations: usize,
}

impl Report {
    fn header() -> &'static str {
        "| scenario | cost | auth timeout | N | not converged | min ms | median ms | p95 ms | max ms | clients with auth errors | Authentication Timeout (proxy) | Authorization Violation (proxy) | max retries | connections to broker | broker log 'Problem registering' | broker log auth errors |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    }

    fn row(&self) -> String {
        let mut t: Vec<f64> = self.clients.iter().filter_map(|c| c.time_ms).collect();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.clients.len();
        let sum = |f: fn(&ClientResult) -> u64| self.clients.iter().map(f).sum::<u64>();
        format!(
            "| {} | {} | {} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {} | {} | {} | {} | {} | {} | {} |",
            self.scenario,
            self.cost,
            self.timeout,
            n,
            n - t.len(),
            t.first().copied().unwrap_or(f64::NAN),
            percentile(&t, 0.5),
            percentile(&t, 0.95),
            t.last().copied().unwrap_or(f64::NAN),
            self.clients
                .iter()
                .filter(|c| c.timeouts + c.violations > 0)
                .count(),
            sum(|c| c.timeouts),
            sum(|c| c.violations),
            self.clients.iter().map(|c| c.retries).max().unwrap_or(0),
            self.attempts,
            self.broker_timeouts,
            self.broker_violations,
        )
    }

    /// One JSON line per client, when `KANADE_AUTH_BURST_JSONL` names a file.
    fn append_jsonl(&self) {
        use std::io::Write as _;
        let Ok(path) = std::env::var("KANADE_AUTH_BURST_JSONL") else {
            return;
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("jsonl file");
        for (i, c) in self.clients.iter().enumerate() {
            let time = c.time_ms.map_or("null".to_string(), |t| format!("{t:.1}"));
            writeln!(
                f,
                "{{\"scenario\":\"{}\",\"cost\":{},\"auth_timeout\":\"{}\",\"client\":{i},\"time_ms\":{time},\"authentication_timeouts\":{},\"authorization_violations\":{},\"retries\":{},\"event_errors\":{}}}",
                self.scenario, self.cost, self.timeout, c.timeouts, c.violations, c.retries, c.event_errors
            )
            .expect("jsonl write");
        }
    }
}

/// What a scenario is measured against.
struct Scenario<'a> {
    name: &'static str,
    cost: u32,
    timeout: &'a str,
    broker: &'a Broker,
    stats: &'a [Arc<Stat>],
}

/// Wait for every client to reach `targets[i]` connections and fold the result
/// into a report. `since_us` is the scenario start on the clients' clock;
/// counters are reported relative to `before`.
async fn collect(
    sc: &Scenario<'_>,
    targets: &[u64],
    before: &[Snapshot],
    since_us: u64,
    deadline: Instant,
) -> Report {
    let mut clients = Vec::new();
    let mut attempts = 0;
    for (i, s) in sc.stats.iter().enumerate() {
        let ok = s.wait_connected(targets[i], deadline).await;
        let now = s.snapshot();
        let conns = now.conns - before[i].conns;
        let real = conns - (now.probe_conns - before[i].probe_conns);
        attempts += conns;
        clients.push(ClientResult {
            time_ms: ok.then(|| {
                s.last_connected_us
                    .load(Ordering::SeqCst)
                    .saturating_sub(since_us) as f64
                    / 1000.0
            }),
            timeouts: now.timeouts - before[i].timeouts,
            violations: now.violations - before[i].violations,
            retries: real.saturating_sub(1),
            event_errors: now.event_errors - before[i].event_errors,
        });
    }
    let (broker_timeouts, broker_violations) = sc.broker.log_counts();
    let report = Report {
        scenario: sc.name,
        cost: sc.cost,
        timeout: sc.timeout.to_string(),
        clients,
        attempts,
        broker_timeouts,
        broker_violations,
    };
    report.append_jsonl();
    for (line, n) in sc.broker.log_digest() {
        eprintln!(
            "  [{} cost {} timeout {}] broker log x{n}: {line}",
            sc.name, sc.cost, sc.timeout
        );
    }
    report
}

async fn run_config(n: usize, cost: u32, timeout_label: &str, deadline_secs: u64) -> Vec<Report> {
    let auth_timeout = timeout_label.parse::<u64>().ok();
    let mut broker = Broker::new(cost, auth_timeout);
    broker.up().await;
    let base = Instant::now();
    let stats: Vec<Arc<Stat>> = (0..n).map(|_| Arc::new(Stat::new(base))).collect();
    let mut proxies = Vec::new();
    for s in &stats {
        proxies.push(client_proxy(broker.port, s.clone()).await);
    }
    let barrier = Arc::new(Barrier::new(n + 1));

    let mut handles = Vec::new();
    for (s, (port, _)) in stats.iter().zip(&proxies) {
        let (s, url, barrier) = (
            s.clone(),
            format!("nats://127.0.0.1:{port}"),
            barrier.clone(),
        );
        handles.push(tokio::spawn(async move {
            let creds = NatsCredentials::new(None, Some(("agent".into(), password("agent"))));
            let cb = {
                let s = s.clone();
                move |ev| {
                    let s = s.clone();
                    async move { s.on_event(ev) }
                }
            };
            barrier.wait().await;
            connect_with_credentials_and_event_callback(NatsRole::Agent, &url, creds, cb)
                .await
                .expect("connect returns before authentication")
        }));
    }
    let start = Instant::now();
    barrier.wait().await;
    let since_us = (start - base).as_micros() as u64;
    let mut clients = Vec::new();
    for h in handles {
        clients.push(h.await.expect("client task"));
    }
    let deadline = start + Duration::from_secs(deadline_secs);
    let zero = vec![Snapshot::default(); n];
    let ones = vec![1u64; n];
    let sc = Scenario {
        name: "start",
        cost,
        timeout: timeout_label,
        broker: &broker,
        stats: &stats,
    };
    let start_report = collect(&sc, &ones, &zero, since_us, deadline).await;

    // Restart under N connected clients.
    let targets: Vec<u64> = stats
        .iter()
        .map(|s| s.connected.load(Ordering::SeqCst) + 1)
        .collect();
    broker.down().await;
    // Taken while the broker is down: refusals never reach it (they do not
    // increment `conns`), whereas reconnects can land as soon as the port
    // opens, before `up()` returns, and must be counted.
    let before: Vec<Snapshot> = stats.iter().map(|s| s.snapshot()).collect();
    let restarted = broker.up().await;
    let since_us = (restarted - base).as_micros() as u64;
    let deadline = restarted + Duration::from_secs(deadline_secs);
    let sc = Scenario {
        name: "restart",
        cost,
        timeout: timeout_label,
        broker: &broker,
        stats: &stats,
    };
    let restart_report = collect(&sc, &targets, &before, since_us, deadline).await;

    drop(clients);
    for (_, accept) in &proxies {
        accept.abort();
    }
    broker.down().await;
    vec![start_report, restart_report]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs nats-server in PATH; measurement, run with --nocapture"]
async fn burst_of_user_connections() {
    if !nats_server_available() {
        eprintln!("nats-server not in PATH; skipping");
        return;
    }
    let n: usize = env_or("KANADE_AUTH_BURST_N", 200);
    let deadline_secs: u64 = env_or("KANADE_AUTH_BURST_DEADLINE_SECS", 120);
    let costs = env_list("KANADE_AUTH_BURST_COSTS", "11");
    let timeouts = env_list("KANADE_AUTH_BURST_AUTH_TIMEOUTS", "shipped");
    require_fd_limit(n);
    println!(
        "{}; N={n}; GOMAXPROCS={}; deadline={deadline_secs}s",
        nats_server_version(),
        std::env::var("KANADE_AUTH_BURST_GOMAXPROCS").unwrap_or_else(|_| "(all cores)".into())
    );
    println!("{}", Report::header());
    for cost in &costs {
        let cost: u32 = cost.parse().expect("cost is a number");
        for timeout in &timeouts {
            for r in run_config(n, cost, timeout, deadline_secs).await {
                println!("{}", r.row());
            }
        }
    }
}

/// Cost of one verification: a loop of `bcrypt::verify` on this thread, and
/// serial user connects to a real broker (which include the connection
/// itself; the cost-4 row is the floor to subtract).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs nats-server in PATH; measurement, run with --nocapture"]
async fn cost_of_one_verification() {
    if !nats_server_available() {
        eprintln!("nats-server not in PATH; skipping");
        return;
    }
    let mut costs = vec!["4".to_string()];
    costs.extend(env_list("KANADE_AUTH_BURST_COSTS", "8,9,10,11,12"));
    println!("{}", nats_server_version());
    println!(
        "| cost | bcrypt::verify median ms (1 thread) | serial connect median ms (broker) | serial connect min ms |\n|---|---|---|---|"
    );
    for cost in costs {
        let cost: u32 = cost.parse().expect("cost is a number");
        let hash = bcrypt::hash_with_result("probe-pw", cost)
            .unwrap()
            .format_for_version(bcrypt::Version::TwoA);
        let mut local = tokio::task::spawn_blocking(move || {
            (0..9)
                .map(|_| {
                    let start = Instant::now();
                    assert!(bcrypt::verify("probe-pw", &hash).unwrap());
                    start.elapsed().as_secs_f64() * 1000.0
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        local.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let mut broker = Broker::new(cost, None);
        broker.up().await;
        let url = broker.url();
        let mut samples = Vec::new();
        for _ in 0..15 {
            let start = Instant::now();
            let c = async_nats::ConnectOptions::new()
                .user_and_password("agent".into(), password("agent"))
                .connect(&url)
                .await
                .expect("serial connect");
            c.flush().await.expect("flush");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "| {cost} | {:.1} | {:.1} | {:.1} |",
            percentile(&local, 0.5),
            percentile(&samples, 0.5),
            samples[0]
        );
        broker.down().await;
    }
}

/// Whether `authorization { timeout }` is accepted where the repository puts
/// its authorization, and whether it is enforced: a connection that never
/// sends CONNECT is timed from the broker's greeting until the broker answers
/// `Authentication Timeout` and closes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs nats-server in PATH; measurement, run with --nocapture"]
async fn authorization_timeout_is_enforced() {
    use tokio::io::AsyncReadExt;
    if !nats_server_available() {
        eprintln!("nats-server not in PATH; skipping");
        return;
    }
    println!("{}", nats_server_version());
    println!(
        "| configuration | seconds until the silent connection is cut | broker said |\n|---|---|---|"
    );
    let token = |t: Option<u64>| {
        let timeout = t.map_or(String::new(), |t| format!(", timeout: {t}"));
        format!("authorization {{ token: \"burst-token\"{timeout} }}\n")
    };
    let variants: Vec<(String, Option<String>, Option<u64>)> = vec![
        ("users, shipped file".into(), None, None),
        ("users, timeout 5".into(), None, Some(5)),
        ("users, timeout 10".into(), None, Some(10)),
        ("token, no timeout".into(), Some(token(None)), None),
        ("token, timeout 5".into(), Some(token(Some(5))), None),
    ];
    for (label, conf, timeout) in variants {
        let mut broker = match conf {
            Some(text) => Broker::with_conf(&text, 4),
            None => Broker::new(4, timeout),
        };
        broker.up().await;
        let mut sock = TcpStream::connect(("127.0.0.1", broker.port))
            .await
            .expect("connect");
        let start = Instant::now();
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        let cut = loop {
            match tokio::time::timeout(Duration::from_secs(30), sock.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break start.elapsed(),
                Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
                Err(_) => break Duration::from_secs(30),
            }
        };
        let said = String::from_utf8_lossy(&seen)
            .lines()
            .find(|l| l.starts_with("-ERR"))
            .unwrap_or("(nothing)")
            .to_string();
        println!("| {label} | {:.2} | {said} |", cut.as_secs_f64());
        broker.down().await;
    }
}
