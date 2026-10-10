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
//! that count. Errors are counted from the client's event callback, and from
//! the broker's own log as a cross-check.
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
        let dir = tempfile::TempDir::new().expect("tempdir");
        let port = portpicker::pick_unused_port().expect("broker port");
        let conf = format!(
            "host: 127.0.0.1\nport: {port}\n{}\n",
            users_conf(auth_timeout)
        );
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

    /// Lines of the current spawn's log that mention each rejection.
    fn log_counts(&self) -> (usize, usize) {
        let text = std::fs::read_to_string(self.log_path()).unwrap_or_default();
        let count = |needle: &str| {
            text.lines()
                .filter(|l| l.to_lowercase().contains(needle))
                .count()
        };
        (count("authentication timeout"), count("authorization violation"))
    }
}

/// What one client saw, written from its event callback.
struct Stat {
    base: Instant,
    connected: AtomicU64,
    last_connected_us: AtomicU64,
    errors: AtomicU64,
    timeouts: AtomicU64,
    violations: AtomicU64,
    other: AtomicU64,
    retries_before_first: AtomicU64,
    notify: Notify,
}

impl Stat {
    fn new(base: Instant) -> Self {
        Self {
            base,
            connected: AtomicU64::new(0),
            last_connected_us: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            violations: AtomicU64::new(0),
            other: AtomicU64::new(0),
            retries_before_first: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    fn on_event(&self, ev: async_nats::Event) {
        match ev {
            async_nats::Event::Connected => {
                if self.connected.load(Ordering::SeqCst) == 0 {
                    self.retries_before_first
                        .store(self.errors.load(Ordering::SeqCst), Ordering::SeqCst);
                }
                self.last_connected_us
                    .store(self.base.elapsed().as_micros() as u64, Ordering::SeqCst);
                self.connected.fetch_add(1, Ordering::SeqCst);
                self.notify.notify_one();
            }
            async_nats::Event::Disconnected => {}
            other => {
                let text = format!("{other:?}").to_lowercase();
                self.errors.fetch_add(1, Ordering::SeqCst);
                if text.contains("authentication timeout") {
                    self.timeouts.fetch_add(1, Ordering::SeqCst);
                } else if text.contains("authorization") {
                    self.violations.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.other.fetch_add(1, Ordering::SeqCst);
                }
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
            errors: self.errors.load(Ordering::SeqCst),
            timeouts: self.timeouts.load(Ordering::SeqCst),
            violations: self.violations.load(Ordering::SeqCst),
            other: self.other.load(Ordering::SeqCst),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    errors: u64,
    timeouts: u64,
    violations: u64,
    other: u64,
}

struct Report {
    scenario: &'static str,
    cost: u32,
    timeout: String,
    n: usize,
    times_ms: Vec<f64>,
    clients_with_errors: usize,
    timeouts: u64,
    violations: u64,
    other: u64,
    max_retries: u64,
    broker_timeouts: usize,
    broker_violations: usize,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

impl Report {
    fn header() -> &'static str {
        "| scenario | cost | auth timeout | N | not converged | min ms | median ms | p95 ms | max ms | clients with auth errors | client Authentication Timeout | client Authorization Violation | client other errors | max retries | broker Authentication Timeout | broker Authorization Violation |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    }

    fn row(&self) -> String {
        let mut t = self.times_ms.clone();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ok = t.len();
        format!(
            "| {} | {} | {} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {} | {} | {} | {} | {} | {} | {} |",
            self.scenario,
            self.cost,
            self.timeout,
            self.n,
            self.n - ok,
            t.first().copied().unwrap_or(f64::NAN),
            percentile(&t, 0.5),
            percentile(&t, 0.95),
            t.last().copied().unwrap_or(f64::NAN),
            self.clients_with_errors,
            self.timeouts,
            self.violations,
            self.other,
            self.max_retries,
            self.broker_timeouts,
            self.broker_violations,
        )
    }
}

/// Wait for every client to reach `targets[i]` connections and fold the result
/// into a report. `since` is the scenario start on the clients' clock.
async fn collect(
    scenario: &'static str,
    cost: u32,
    timeout: &str,
    stats: &[Arc<Stat>],
    targets: &[u64],
    before: &[Snapshot],
    since_us: u64,
    deadline: Instant,
    broker: &Broker,
) -> Report {
    let mut times_ms = Vec::new();
    let (mut with_err, mut to, mut av, mut ot, mut max_retries) = (0, 0, 0, 0, 0);
    for (i, s) in stats.iter().enumerate() {
        let ok = s.wait_connected(targets[i], deadline).await;
        if ok {
            let us = s.last_connected_us.load(Ordering::SeqCst);
            times_ms.push(us.saturating_sub(since_us) as f64 / 1000.0);
        }
        let now = s.snapshot();
        let d = (
            now.errors - before[i].errors,
            now.timeouts - before[i].timeouts,
            now.violations - before[i].violations,
            now.other - before[i].other,
        );
        if d.1 + d.2 > 0 {
            with_err += 1;
        }
        to += d.1;
        av += d.2;
        ot += d.3;
        max_retries = max_retries.max(d.0);
    }
    let (bt, bv) = broker.log_counts();
    Report {
        scenario,
        cost,
        timeout: timeout.to_string(),
        n: stats.len(),
        times_ms,
        clients_with_errors: with_err,
        timeouts: to,
        violations: av,
        other: ot,
        max_retries,
        broker_timeouts: bt,
        broker_violations: bv,
    }
}

async fn run_config(n: usize, cost: u32, timeout_label: &str, deadline_secs: u64) -> Vec<Report> {
    let auth_timeout = timeout_label.parse::<u64>().ok();
    let mut broker = Broker::new(cost, auth_timeout);
    broker.up().await;
    let url = broker.url();
    let base = Instant::now();
    let stats: Vec<Arc<Stat>> = (0..n).map(|_| Arc::new(Stat::new(base))).collect();
    let barrier = Arc::new(Barrier::new(n + 1));
    let started_us = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for s in &stats {
        let (s, url, barrier) = (s.clone(), url.clone(), barrier.clone());
        handles.push(tokio::spawn(async move {
            let creds = NatsCredentials::new(None, Some(("agent".into(), password("agent"))));
            barrier.wait().await;
            let cb = {
                let s = s.clone();
                move |ev| {
                    let s = s.clone();
                    async move { s.on_event(ev) }
                }
            };
            connect_with_credentials_and_event_callback(NatsRole::Agent, &url, creds, cb)
                .await
                .expect("connect returns before authentication")
        }));
    }
    started_us.store(base.elapsed().as_micros() as u64, Ordering::SeqCst);
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
    let start_report = collect(
        "start", cost, timeout_label, &stats, &ones, &zero, since_us, deadline, &broker,
    )
    .await;

    // Restart under N connected clients.
    let targets: Vec<u64> = stats
        .iter()
        .map(|s| s.connected.load(Ordering::SeqCst) + 1)
        .collect();
    broker.down().await;
    let restarted = broker.up().await;
    // Taken once the port answers: refusals while the broker was down are
    // connection errors, not authentication ones.
    let before: Vec<Snapshot> = stats.iter().map(|s| s.snapshot()).collect();
    let since_us = (restarted - base).as_micros() as u64;
    let deadline = restarted + Duration::from_secs(deadline_secs);
    let restart_report = collect(
        "restart", cost, timeout_label, &stats, &targets, &before, since_us, deadline, &broker,
    )
    .await;

    drop(clients);
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
        "nats-server: {}; N={n}; GOMAXPROCS={}; deadline={deadline_secs}s",
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
/// the median of serial user connects to a real broker (which includes the
/// connection itself; the cost-4 row is the floor to subtract).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs nats-server in PATH; measurement, run with --nocapture"]
async fn cost_of_one_verification() {
    if !nats_server_available() {
        eprintln!("nats-server not in PATH; skipping");
        return;
    }
    let mut costs = vec!["4".to_string()];
    costs.extend(env_list("KANADE_AUTH_BURST_COSTS", "8,9,10,11,12"));
    println!("nats-server: {}", nats_server_version());
    println!("| cost | bcrypt::verify ms (1 thread) | serial connect median ms (broker) |\n|---|---|---|");
    for cost in costs {
        let cost: u32 = cost.parse().expect("cost is a number");
        let hash = bcrypt::hash_with_result("probe-pw", cost)
            .unwrap()
            .format_for_version(bcrypt::Version::TwoA);
        let iters = 8;
        let t = tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            for _ in 0..iters {
                assert!(bcrypt::verify("probe-pw", &hash).unwrap());
            }
            start.elapsed().as_secs_f64() * 1000.0 / iters as f64
        })
        .await
        .unwrap();

        let mut broker = Broker::new(cost, None);
        broker.up().await;
        let url = broker.url();
        let mut samples = Vec::new();
        for _ in 0..12 {
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
        println!("| {cost} | {t:.1} | {:.1} |", percentile(&samples, 0.5));
        broker.down().await;
    }
}
