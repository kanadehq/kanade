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
//! Waiting and timing. No step sleeps for a fixed time or gives one call a
//! one-shot deadline. Every wait polls the condition that matters (the broker's
//! monitoring port, a heartbeat, a successful request) under one bound per kind
//! of wait ([`WaitKind`]), and watches the broker, backend and agent processes
//! while it waits, so a dead process fails the test at once with its own
//! output. A bound that runs out says which condition did not occur and what
//! was last seen. Every wait also reports how long it took (stdout, and one
//! JSON line per wait in the file named by `KANADE_CONFORMANCE_TIMINGS`); the
//! integration workflow turns those into the per-OS distribution the bounds
//! and the operations book are based on.
//!
//! Narrowing the command plane later is a one-line edit in the conf (delete
//! the `NARROWING` lines) plus flipping [`COMMAND_BROADCAST_SUBSCRIBE`] below.

#[path = "common/mod.rs"]
mod common;

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
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

// ───────────────────────── waiting, watching, timing ─────────────────────────

/// One kind of wait: what it is called in the timing output, and the bound it
/// runs under. The bound can be stretched without a rebuild:
/// `KANADE_CONFORMANCE_WAIT_<NAME>_SECS` sets one kind, and
/// `KANADE_CONFORMANCE_WAIT_SCALE` multiplies every default.
struct WaitKind {
    name: &'static str,
    default_secs: u64,
}

// The defaults below are derived from a measured distribution, not guessed.
// Source: the integration workflow dispatched with `conformance_repeat=20` on
// ubuntu, windows and macos (run 37533007122, commit fa790fd, 20 suite runs
// per OS; the code under test is unchanged since). Each bound is the worst
// successful wait seen for that kind on any OS, times five, rounded up to
// 5 s, and never below 30 s. Five times leaves room for a runner several
// times slower than the ones measured, so a slow machine is not mistaken for
// a defect, while a hang still fails in well under the old 120 s deadlines.
//
//   kind                  worst ok (OS)      bound
//   broker_ready          0.57 s (windows)    30 s (floor)
//   backend_ready         6.37 s (windows)    35 s
//   agent_first_heartbeat 0.28 s (macos)      30 s (floor)
//   reconnect             7.01 s (windows)    40 s
//   heartbeat_steady      1.01 s (macos)      30 s (floor)
//   heartbeat_cadence     3.51 s (windows)    30 s (floor)
//   catch_up             30.11 s (macos)     155 s
//   operation            11.33 s (macos)      60 s
//   violation_arrival     0.16 s (windows)    30 s (floor)
//
// Whole suite, seconds (p50 / max): linux 50 / 120, windows 51 / 123, macos
// 51 / 81; the two long ones each include a heartbeat_cadence stall.
//
// Excluded: `heartbeat_cadence` hit its old 120 s bound without ever seeing
// the faster heartbeats in 3 of 60 runs (1 linux, 2 windows). A stalled wait
// is not a completion time, so these are not in the table above, and nothing
// here explains them: the bounds are derived from successful waits only and
// the cause of the stall is still open. With the 30 s bound a stall now fails
// in 30 s instead of 120 s.
//
// To re-measure, dispatch the integration workflow with `conformance_repeat`
// and read the per-OS table in the step summary; to run slower for a while,
// set `KANADE_CONFORMANCE_WAIT_SCALE` or `KANADE_CONFORMANCE_WAIT_<NAME>_SECS`.
// A bound is never what makes a healthy run pass: it only has to be far
// enough away that a slow runner is not mistaken for a defect, and finite so
// that a hang still fails.

/// Spawning `nats-server` until its monitoring port answers.
const BROKER_READY: WaitKind = WaitKind {
    name: "broker_ready",
    default_secs: 30,
};
/// Spawning the backend until its HTTP API answers.
const BACKEND_READY: WaitKind = WaitKind {
    name: "backend_ready",
    default_secs: 35,
};
/// Spawning the agent until its first heartbeat (the helper's credential probe,
/// the bootstrap and the first publish all sit in between).
const AGENT_FIRST_HEARTBEAT: WaitKind = WaitKind {
    name: "agent_first_heartbeat",
    default_secs: 30,
};
/// A broker restart or a token-to-users reload until each process and role
/// works again. This is the reconnect time the operations book quotes.
const RECONNECT: WaitKind = WaitKind {
    name: "reconnect",
    default_secs: 40,
};
/// The next heartbeat from an agent that has not lost its connection (only
/// the unix-only switch scenario waits for one).
#[cfg_attr(not(unix), allow(dead_code))]
const HEARTBEAT_STEADY: WaitKind = WaitKind {
    name: "heartbeat_steady",
    default_secs: 30,
};
/// Heartbeats speeding up after a KV change reached the agent.
const HEARTBEAT_CADENCE: WaitKind = WaitKind {
    name: "heartbeat_cadence",
    default_secs: 30,
};
/// A projector, a consumer or the outbox catching up with work already done.
const CATCH_UP: WaitKind = WaitKind {
    name: "catch_up",
    default_secs: 155,
};
/// One JetStream, HTTP or CLI operation that has to complete (a watch
/// delivering, a replay, an object read, a CLI run).
const OPERATION: WaitKind = WaitKind {
    name: "operation",
    default_secs: 60,
};
/// A broker log line, or a server error on a client, that has to arrive.
const VIOLATION_ARRIVAL: WaitKind = WaitKind {
    name: "violation_arrival",
    default_secs: 30,
};

/// How often a wait looks again, and so also how soon a dead child is noticed.
const POLL: Duration = Duration::from_millis(150);

/// A single attempt inside a polling loop. An attempt that hangs is abandoned
/// and retried, still under the kind's bound.
const ATTEMPT_CAP: Duration = Duration::from_secs(20);

/// An absence cannot be proven by waiting for a message, so checks that
/// something was *not* said keep a short quiet period after a round trip.
const QUIET_AFTER_ROUND_TRIP: Duration = Duration::from_millis(100);

/// Lines of a child's output kept for a failure message.
const OUTPUT_TAIL_LINES: usize = 60;

/// Tests running at once in this process, so a sample says how contended the
/// machine was.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct ActiveGuard;

impl ActiveGuard {
    fn enter() -> Self {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn env_seconds(var: &str) -> Option<f64> {
    match std::env::var(var) {
        Ok(v) => match v.trim().parse::<f64>() {
            Ok(x) if x.is_finite() && x > 0.0 => Some(x),
            _ => panic!("{var}={v:?} is not a positive number of seconds"),
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => panic!("{var}: {e}"),
    }
}

impl WaitKind {
    fn env_name(&self) -> String {
        format!(
            "KANADE_CONFORMANCE_WAIT_{}_SECS",
            self.name.to_ascii_uppercase()
        )
    }

    fn bound(&self) -> Duration {
        let secs = env_seconds(&self.env_name()).unwrap_or_else(|| {
            self.default_secs as f64 * env_seconds("KANADE_CONFORMANCE_WAIT_SCALE").unwrap_or(1.0)
        });
        Duration::from_secs_f64(secs)
    }
}

/// Print one measurement and, when asked, append it as a JSON line.
fn record_timing(kind: &str, switch: &str, step: &str, elapsed: Duration, outcome: &str) {
    let thread = std::thread::current();
    let test = thread
        .name()
        .unwrap_or("?")
        .rsplit("::")
        .next()
        .unwrap_or("?");
    let concurrent = ACTIVE.load(Ordering::SeqCst);
    println!(
        "[timing] {kind} | {switch} | {step} | {:.3}s | {outcome} | test={test} concurrent={concurrent} os={}",
        elapsed.as_secs_f64(),
        std::env::consts::OS
    );
    if let Some(path) = std::env::var_os("KANADE_CONFORMANCE_TIMINGS") {
        let line = serde_json::json!({
            "os": std::env::consts::OS,
            "test": test,
            "kind": kind,
            "switch": switch,
            "step": step,
            "secs": elapsed.as_secs_f64(),
            "outcome": outcome,
            "concurrent": concurrent,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            // One write per line, so concurrent tests do not interleave.
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }
}

/// Where a child's output is kept for a failure message.
#[derive(Clone)]
enum Output {
    File(PathBuf),
    Ring(Arc<Mutex<VecDeque<String>>>),
}

/// A child process that is expected to stay up. Waits consult it on every
/// poll, so an exit fails the wait at once. A deliberate stop takes the child
/// out, after which the sentinel reports nothing.
#[derive(Clone)]
struct Sentinel {
    name: &'static str,
    child: Arc<Mutex<Option<Child>>>,
    output: Output,
}

impl Sentinel {
    fn new(name: &'static str, output: Output) -> Self {
        Self {
            name,
            child: Arc::default(),
            output,
        }
    }

    fn set(&self, child: Child) {
        *self.child.lock().unwrap() = Some(child);
    }

    fn take(&self) -> Option<Child> {
        self.child.lock().unwrap().take()
    }

    #[cfg(unix)]
    fn pid(&self) -> Option<u32> {
        self.child.lock().unwrap().as_ref().and_then(|c| c.id())
    }

    fn tail(&self) -> String {
        let lines: Vec<String> = match &self.output {
            Output::File(p) => std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
            Output::Ring(r) => r.lock().unwrap().iter().cloned().collect(),
        };
        let from = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
        lines[from..].join("\n")
    }

    /// `Some(report)` when the process has exited without being stopped.
    fn exited(&self) -> Option<String> {
        let status = self
            .child
            .lock()
            .unwrap()
            .as_mut()?
            .try_wait()
            .ok()
            .flatten()?;
        Some(format!(
            "{} exited on its own ({status}); its last output:\n{}",
            self.name,
            self.tail()
        ))
    }

    fn alive(&self) -> bool {
        self.exited().is_none()
    }
}

/// Forward a child's stream to stderr and keep its tail.
fn capture<R>(stream: R, prefix: &'static str, ring: Arc<Mutex<VecDeque<String>>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{prefix} {line}");
            let mut r = ring.lock().unwrap();
            if r.len() >= OUTPUT_TAIL_LINES {
                r.pop_front();
            }
            r.push_back(line);
        }
    });
}

/// One wait in progress: polls under its kind's bound, watches the children,
/// remembers what it last saw, and reports its duration when it ends.
struct Waiter {
    kind: &'static WaitKind,
    step: String,
    switch: String,
    started: Instant,
    bound: Duration,
    watch: Vec<Sentinel>,
    last: String,
}

impl Waiter {
    fn new(kind: &'static WaitKind, step: &str, watch: Vec<Sentinel>) -> Self {
        Self {
            kind,
            step: step.to_string(),
            switch: String::new(),
            started: Instant::now(),
            bound: kind.bound(),
            watch,
            last: "nothing observed yet".into(),
        }
    }

    /// Name the switch (restart, reload, …) this wait follows.
    fn switch(mut self, switch: &str) -> Self {
        self.switch = switch.to_string();
        self
    }

    /// Count from `t0` rather than from now, so the sample is the time since
    /// the event itself and the bound is measured the same way.
    fn since(mut self, t0: Instant) -> Self {
        self.started = t0;
        self
    }

    fn observed(&mut self, what: impl Into<String>) {
        self.last = what.into();
    }

    fn left(&self) -> Duration {
        self.bound.saturating_sub(self.started.elapsed())
    }

    fn fail(&self, why: &str) -> ! {
        record_timing(
            self.kind.name,
            &self.switch,
            &self.step,
            self.started.elapsed(),
            "failed",
        );
        panic!(
            "{why}\n  waiting for: {} {}\n  kind: {} (bound {:?}, elapsed {:?}; stretch with {} or KANADE_CONFORMANCE_WAIT_SCALE)\n  last observed: {}",
            self.step,
            self.switch,
            self.kind.name,
            self.bound,
            self.started.elapsed(),
            self.kind.env_name(),
            self.last
        );
    }

    /// Fail now if a watched process has died or the bound has run out.
    fn check(&self) {
        for s in &self.watch {
            if let Some(report) = s.exited() {
                self.fail(&format!("a process died while waiting: {report}"));
            }
        }
        if self.started.elapsed() >= self.bound {
            self.fail("the condition did not occur within its bound");
        }
    }

    async fn tick(&self) {
        self.check();
        tokio::time::sleep(POLL.min(self.left())).await;
        self.check();
    }

    /// Run one attempt, abandoning it after [`ATTEMPT_CAP`] or what is left of
    /// the bound. `None` means the attempt did not finish.
    async fn call<T>(&mut self, fut: impl Future<Output = T>) -> Option<T> {
        let cap = ATTEMPT_CAP.min(self.left());
        let attempt_started = Instant::now();
        tokio::pin!(fut);
        // Polled in slices so a child that dies while the attempt is pending
        // is reported at once, not when the attempt returns or is abandoned.
        loop {
            let slice = POLL.min(cap.saturating_sub(attempt_started.elapsed()));
            if let Ok(v) = tokio::time::timeout(slice, &mut fut).await {
                return Some(v);
            }
            self.check();
            if attempt_started.elapsed() >= cap {
                self.observed(format!("an attempt was still pending after {cap:?}"));
                return None;
            }
        }
    }

    /// The condition held. Records and returns the elapsed time.
    fn done(self) -> Duration {
        self.done_as("ok")
    }

    fn done_as(self, outcome: &str) -> Duration {
        let d = self.started.elapsed();
        record_timing(self.kind.name, &self.switch, &self.step, d, outcome);
        d
    }
}

/// Poll `probe` until it is `Ok`. Its `Err` text is what a failure reports as
/// the last observation.
async fn wait_until<F, Fut>(mut w: Waiter, mut probe: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    loop {
        match w.call(probe()).await {
            Some(Ok(())) => return w.done(),
            Some(Err(seen)) => w.observed(seen),
            None => {}
        }
        w.tick().await;
    }
}

/// Run `fut` to completion under `kind`'s bound while watching the children.
/// When the bound runs out the waiter comes back as the error, so the caller
/// can fail with it or record the outcome.
async fn guarded_until_bound<T>(
    kind: &'static WaitKind,
    what: &str,
    watch: Vec<Sentinel>,
    fut: impl Future<Output = T>,
) -> Result<T, Box<Waiter>> {
    let mut w = Waiter::new(kind, what, watch);
    w.observed("the operation is still pending");
    tokio::pin!(fut);
    loop {
        if let Ok(v) = tokio::time::timeout(POLL.min(w.left()), &mut fut).await {
            w.done();
            return Ok(v);
        }
        if w.left().is_zero() {
            return Err(Box::new(w));
        }
        w.check();
    }
}

/// [`guarded_until_bound`], failing when the bound runs out.
async fn guarded<T>(
    kind: &'static WaitKind,
    what: &str,
    watch: Vec<Sentinel>,
    fut: impl Future<Output = T>,
) -> T {
    match guarded_until_bound(kind, what, watch, fut).await {
        Ok(v) => v,
        Err(w) => w.fail("the operation did not finish within its bound"),
    }
}

/// The next message on `sub`, polled so a dead child is noticed.
async fn next_message(w: &mut Waiter, sub: &mut async_nats::Subscriber) -> async_nats::Message {
    loop {
        match tokio::time::timeout(POLL, sub.next()).await {
            Ok(Some(m)) => return m,
            Ok(None) => w.fail("the subscription closed"),
            Err(_) => {
                w.observed("subscribed, nothing received yet");
                w.check();
            }
        }
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
    sentinel: Sentinel,
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
        let sentinel = Sentinel::new("[nats-server]", Output::File(dir.path().join("nats.log")));
        let mut b = Self {
            sentinel,
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
        let spawned = Instant::now();
        self.sentinel.set(cmd.spawn().expect("spawn nats-server"));
        // The monitoring port has to answer a request, not just accept a
        // connection: that is what the later oracles read.
        let port = self.http_port;
        wait_until(
            Waiter::new(
                &BROKER_READY,
                "monitoring port answers",
                vec![self.sentinel.clone()],
            )
            .since(spawned),
            || async move {
                match http_request(port, "GET", "/varz", None, "").await.0 {
                    200 => Ok(()),
                    0 => Err("nothing answered on the monitoring port".to_string()),
                    st => Err(format!("monitoring port answered HTTP {st}")),
                }
            },
        )
        .await;
    }

    async fn stop(&mut self) {
        if let Some(mut c) = self.sentinel.take() {
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
        let pid = self.sentinel.pid().expect("broker pid");
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

    /// `(connection name, authorized_user, connection id)` for every live
    /// connection. The id is never reused, so it tells a connection that
    /// survived from one opened since.
    #[cfg(unix)]
    async fn connections(&self) -> Vec<(String, String, u64)> {
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
                            c["cid"].as_u64().unwrap_or(0),
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

/// Status 0 also stands for a request that got no answer within the
/// operation bound: a reader that stalls must not outlive the caller's bound.
async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &str,
) -> (u16, String) {
    tokio::time::timeout(
        OPERATION.bound(),
        http_request_inner(port, method, path, content_type, body),
    )
    .await
    .unwrap_or((0, "no answer within the operation bound".to_string()))
}

async fn http_request_inner(
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
    /// For denial probes: a refused JetStream call is only ever answered by
    /// silence, so a short API timeout keeps the matrix quick. The verdict is
    /// still the violation, never the timeout.
    async fn open_fast(url: &str, role: Role) -> Self {
        Self::open_with(url, role, Some(Duration::from_secs(2)), false).await
    }

    async fn open_with(
        url: &str,
        role: Role,
        js_timeout: Option<Duration>,
        with_token: bool,
    ) -> Self {
        Self::try_open(url, role, js_timeout, with_token)
            .await
            .unwrap_or_else(|e| panic!("connect as {}: {e}", role.user()))
    }

    /// The real `connect` helper, so its credential selection (and the probe
    /// it makes on every connect) is part of what a caller measures.
    async fn try_open(
        url: &str,
        role: Role,
        js_timeout: Option<Duration>,
        with_token: bool,
    ) -> Result<Self, String> {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let client = connect_with_credentials_and_event_callback(
            role.nats_role(),
            url,
            role.credentials(with_token),
            move |ev| {
                if let Event::ServerError(e) = ev {
                    sink.lock().unwrap().push(e.to_string());
                }
                std::future::ready(())
            },
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        let js = match js_timeout {
            Some(t) => jetstream::context::ContextBuilder::new()
                .timeout(t)
                .build(client.clone()),
            None => jetstream::new(client.clone()),
        };
        Ok(Self { js, client, errors })
    }

    /// A round trip to the broker: every error caused by an earlier operation
    /// has been delivered once this returns (the server answers in order).
    async fn settle(&self) {
        let _ = tokio::time::timeout(OPERATION.bound(), self.client.flush()).await;
        tokio::time::sleep(QUIET_AFTER_ROUND_TRIP).await;
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

// ───────────────────────────── child processes ─────────────────────────────

struct Proc {
    sentinel: Sentinel,
}

impl Proc {
    fn spawn(mut cmd: Command, prefix: &'static str) -> Self {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {prefix}: {e}"));
        let ring = Arc::default();
        if let Some(o) = child.stdout.take() {
            capture(o, prefix, Arc::clone(&ring));
        }
        if let Some(e) = child.stderr.take() {
            capture(e, prefix, Arc::clone(&ring));
        }
        let sentinel = Sentinel::new(prefix, Output::Ring(ring));
        sentinel.set(child);
        Self { sentinel }
    }

    fn alive(&self) -> bool {
        self.sentinel.alive()
    }

    async fn stop(&mut self) {
        if let Some(mut c) = self.sentinel.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
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
    /// When the agent was last spawned, for the first-heartbeat measurement.
    agent_spawned: Option<Instant>,
    _active: ActiveGuard,
}

impl Fleet {
    async fn new(auth: Auth, with_token: bool, dial_url: Option<String>) -> Self {
        let active = ActiveGuard::enter();
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
            agent_spawned: None,
            _active: active,
        }
    }

    /// Every process that is meant to be running right now. A wait fails at
    /// once if one of them has exited.
    fn watch(&self) -> Vec<Sentinel> {
        let mut v = vec![self.broker.sentinel.clone()];
        v.extend(self.backend.iter().map(|p| p.sentinel.clone()));
        v.extend(self.agent.iter().map(|p| p.sentinel.clone()));
        v
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
        let spawned = Instant::now();
        self.backend = Some(Proc::spawn(cmd, "[backend]"));
        let port = self.http_port;
        wait_until(
            Waiter::new(&BACKEND_READY, "backend HTTP answers", self.watch()).since(spawned),
            || async move {
                match http_request(port, "GET", "/api/jobs", None, "").await.0 {
                    200 => Ok(()),
                    st => Err(format!("GET /api/jobs gave status {st}")),
                }
            },
        )
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
        self.agent_spawned = Some(Instant::now());
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
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        guarded(
            &OPERATION,
            "kanade CLI to finish",
            self.watch(),
            cmd.output(),
        )
        .await
        .expect("run kanade CLI")
    }

    /// Start `kanade run` and leave it running: it waits for its result over
    /// a subscription that has to survive whatever the broker does next. The
    /// command writes `started` when the agent begins it and finishes once
    /// `release` exists, so how long it runs is up to the test, not a guess.
    #[cfg(unix)]
    fn cli_spawn_slow(&self, marker: &str, started: &Path, release: &Path) -> Child {
        let mut cmd = Command::new(exe("kanade"));
        cmd.arg("--server")
            .arg(&self.dial_url)
            .args([
                "run",
                &self.pc_id,
                "--shell",
                "sh",
                "--timeout",
                "600",
                "--",
            ])
            .arg(format!(
                "touch '{}'; while [ ! -e '{}' ]; do sleep 0.2; done; echo {marker}",
                started.display(),
                release.display()
            ));
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

    /// One call to the backend's HTTP API, under the operation bound, with the
    /// children watched while it is pending and its duration recorded.
    async fn api(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let step = format!("{method} {}", path.replace(&self.pc_id, "<agent>"));
        guarded(
            &OPERATION,
            &step,
            self.watch(),
            http_request(self.http_port, method, path, content_type, body),
        )
        .await
    }

    async fn role(&self, role: Role) -> RoleClient {
        RoleClient::open_with(&self.dial_url, role, None, self.with_token).await
    }

    async fn role_fast(&self, role: Role) -> RoleClient {
        RoleClient::open_fast(&self.dial_url, role).await
    }

    /// Wait until the agent has published a heartbeat to `sub`, counting from
    /// `since`. Returns how long that took.
    async fn heartbeat(
        &self,
        sub: &mut async_nats::Subscriber,
        kind: &'static WaitKind,
        step: &str,
        since: Instant,
    ) -> Duration {
        let mut w = Waiter::new(kind, step, self.watch()).since(since);
        next_message(&mut w, sub).await;
        w.done()
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

// ═════════════════════ recovery after a restart or a switch ═════════════════════

/// A heartbeat subscription on whichever credential the broker accepts in
/// that stage, opened directly on the broker (not through a proxy).
async fn observe(
    url: &str,
    pc_id: &str,
    auth: Auth,
) -> Result<(Client, async_nats::Subscriber), String> {
    let opts = match auth {
        #[cfg(unix)]
        Auth::Token => async_nats::ConnectOptions::with_token(TOKEN.into()),
        Auth::Users => async_nats::ConnectOptions::with_user_and_password(
            Role::Backend.user().into(),
            Role::Backend.password(),
        ),
    };
    let c = opts.connect(url).await.map_err(|e| e.to_string())?;
    let s = c
        .subscribe(format!("heartbeat.{pc_id}"))
        .await
        .map_err(|e| e.to_string())?;
    Ok((c, s))
}

/// The next heartbeat of the agent, seen through a fresh observer so nothing
/// that was buffered before `since` can be mistaken for it. The observer
/// keeps retrying to connect, because right after a broker restart or reload
/// the broker may still refuse it.
async fn heartbeat_via_fresh_observer(
    f: &Fleet,
    auth: Auth,
    kind: &'static WaitKind,
    step: &str,
    switch: &str,
    since: Instant,
) -> Duration {
    let mut w = Waiter::new(kind, step, f.watch())
        .switch(switch)
        .since(since);
    let url = f.broker.url();
    let (_client, mut sub) = loop {
        match w.call(observe(&url, &f.pc_id, auth)).await {
            Some(Ok(pair)) => break pair,
            Some(Err(e)) => w.observed(format!("observer could not connect: {e}")),
            None => {}
        }
        w.tick().await;
    };
    next_message(&mut w, &mut sub).await;
    w.done()
}

fn short(s: &str) -> String {
    s.chars().take(200).collect()
}

/// A permission violation is never a reason to retry: it means the block
/// lacks something a flow needs, and a retry loop must not hide that.
fn refuse_permission_violation(w: &Waiter, said: &str) {
    if said.contains("Permissions Violation") {
        w.fail(&format!(
            "an allowed call was refused by the broker: {}",
            short(said)
        ));
    }
}

/// After a broker restart or a token-to-users reload, how long each part
/// takes to work again, all counted from `t0` and measured side by side (so
/// one slow part does not inflate the others): the agent's next heartbeat,
/// the backend's ping of the agent, a break-glass `run`, and a fresh call made
/// with each of the agent and backend roles through the real `connect` helper.
/// `extra` runs alongside and its result is returned.
async fn recover<T>(
    f: &Fleet,
    switch: &str,
    observer: Auth,
    t0: Instant,
    extra: impl Future<Output = T>,
) -> T {
    let heartbeat =
        heartbeat_via_fresh_observer(f, observer, &RECONNECT, "agent heartbeat", switch, t0);
    let ping = async {
        let mut w = Waiter::new(&RECONNECT, "backend pings the agent", f.watch())
            .switch(switch)
            .since(t0);
        let path = format!("/api/agents/{}/ping", f.pc_id);
        loop {
            if let Some((st, body)) = w
                .call(http_request(f.http_port, "POST", &path, None, ""))
                .await
            {
                if st == 200 {
                    return w.done();
                }
                refuse_permission_violation(&w, &body);
                w.observed(format!("HTTP {st}: {}", short(&body)));
            }
            w.tick().await;
        }
    };
    let run = async {
        let mut w = Waiter::new(&RECONNECT, "break-glass run completes", f.watch())
            .switch(switch)
            .since(t0);
        loop {
            // A fresh marker per attempt: an answer to an abandoned attempt
            // must not be taken for this one.
            let marker = fresh("rc");
            if let Some(out) = w.call(f.cli_run_echo(&marker, 10)).await {
                if out.status.success() && stdout_of(&out).contains(&marker) {
                    return w.done();
                }
                let err = String::from_utf8_lossy(&out.stderr).to_string();
                refuse_permission_violation(&w, &err);
                w.observed(format!("status {:?}: {}", out.status, short(&err)));
            }
            w.tick().await;
        }
    };
    let role_call = |role: Role| async move {
        let step = format!("{} role call", role.user());
        let mut w = Waiter::new(&RECONNECT, &step, f.watch())
            .switch(switch)
            .since(t0);
        loop {
            let attempt = async {
                let c = RoleClient::try_open(&f.dial_url, role, None, f.with_token).await?;
                match role {
                    Role::Agent => {
                        c.js.get_key_value("agent_config")
                            .await
                            .map_err(|e| e.to_string())?
                            .get("global")
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Role::Backend => {
                        c.js.get_stream("EXEC").await.map_err(|e| e.to_string())?;
                    }
                    Role::Breakglass => unreachable!("covered by the break-glass run"),
                }
                Ok::<_, String>(c.any_violation())
            };
            match w.call(attempt).await {
                Some(Ok(v)) if v.is_empty() => return w.done(),
                Some(Ok(v)) => refuse_permission_violation(&w, &v.join("; ")),
                Some(Err(e)) => w.observed(e),
                None => {}
            }
            w.tick().await;
        }
    };
    let (_, _, _, _, _, t) = tokio::join!(
        heartbeat,
        ping,
        run,
        role_call(Role::Agent),
        role_call(Role::Backend),
        extra
    );
    t
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
    // The broker writes its log on its own schedule, so the parser is given
    // the time the violations take to reach the file; what it never sees is
    // reported with the log, as a drifted format.
    let _ = tokio::time::timeout(OPERATION.bound(), probe.client.flush()).await;
    wait_until(
        Waiter::new(&VIOLATION_ARRIVAL, "deliberate violations in the broker log", f.watch()),
        || async {
            let seen = f.broker.violations();
            let publish = seen.iter().any(|v| {
                v.user == "breakglass" && v.kind == "Publish" && v.subject == subject
            });
            let subscription = seen.iter().any(|v| {
                v.user == "breakglass"
                    && v.kind == "Subscription"
                    && v.subject == "commands.oracle.>"
            });
            if publish && subscription {
                Ok(())
            } else {
                Err(format!(
                    "publish violation parsed={publish}, subscription violation parsed={subscription}; \
                     if the log has them, its format has drifted from what this test expects.\n{}",
                    f.broker.log()
                ))
            }
        },
    )
    .await;
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
    let (st, body) = f.api("POST", "/api/group-defs", yaml, group).await;
    assert!(st == 200 || st == 201, "group-defs create: {st} {body}");
    let (st, body) = f.api("POST", "/api/views", yaml, view).await;
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
    f.heartbeat(
        &mut hb,
        &AGENT_FIRST_HEARTBEAT,
        "agent first heartbeat",
        f.agent_spawned.unwrap(),
    )
    .await;

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
    // The heartbeat in flight when the write landed may still be on the old
    // cadence. At the default cadence two heartbeats are 30 s apart, so three
    // gaps in a row under three seconds are only possible at 1 s: the watch
    // delivered. A slow moment resets the run instead of failing it.
    {
        let mut w = Waiter::new(
            &HEARTBEAT_CADENCE,
            "heartbeats at the 1 s cadence",
            f.watch(),
        );
        let (mut last, mut streak) = (None::<Instant>, 0usize);
        while streak < 3 {
            next_message(&mut w, &mut hb).await;
            let now = Instant::now();
            if let Some(l) = last {
                let gap = now - l;
                streak = if gap < Duration::from_secs(3) {
                    streak + 1
                } else {
                    0
                };
                w.observed(format!(
                    "last gap {gap:?}, {streak} fast gaps in a row of 3 needed"
                ));
            }
            last = Some(now);
            w.check();
        }
        w.done();
    }

    // Break-glass `run`: command out, result back, agent executes, agent
    // publishes the result (JetStream publish with ack via the outbox), and
    // the backend's results projector acknowledges it.
    let marker = fresh("mk");
    let out = f.cli_run_echo(&marker, 30).await;
    assert_cli_ok(&out, &marker);
    acked(&f, "RESULTS", "backend_results_projector").await;
    // …and the audit the CLI published for the run reaches the backend's
    // audit projector.
    acked(&f, "AUDIT", "backend_audit_projector").await;

    // The same command was also retained by the command stream and replayed
    // through the agent's durable consumer: the consumer exists under the
    // agent's credential and its position advanced.
    let replay = format!("agent_replay_{}", f.pc_id);
    acked(&f, "EXEC", &replay).await;

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
    wait_result_containing(&f, &mut results, &offline_marker).await;

    // Request/reply: ping and log fetch through the real backend handlers,
    // tail directly (it needs a running job to be interesting; the permission
    // surface is the same).
    let (st, body) = f
        .api("POST", &format!("/api/agents/{}/ping", f.pc_id), None, "")
        .await;
    assert_eq!(st, 200, "ping: {body}");
    let (st, body) = f
        .api(
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
    // object-store upload). The agent holds no purge right, so it must never
    // put over an existing object: (a) first upload, (b) the same result
    // again — the retry after a lost acknowledgement — reuses the stored
    // object, (c) the same request id with a different body goes to a
    // distinct key and leaves the original intact.
    let outbox = f.data_dir().join("outbox");
    let request_id = fresh("big");
    let first = vec![b'a'; kanade_shared::kv::STDOUT_INLINE_THRESHOLD + 4096];
    let second = vec![b'b'; kanade_shared::kv::STDOUT_INLINE_THRESHOLD + 8192];
    let store = backend.js.get_object_store("result_output").await.unwrap();
    let base_key = format!("{request_id}/{}/stdout", f.pc_id);
    let alt_key = format!("{request_id}/{}/stdout.r1", f.pc_id);
    let mut nuid_before = String::new();
    for (label, body, key) in [
        ("first", &first, &base_key),
        ("identical retry", &first, &base_key),
        ("different body", &second, &alt_key),
    ] {
        enqueue_outbox(&outbox, &request_id, &f.pc_id, body);
        let path = outbox.join(format!("{request_id}.json"));
        wait_until(
            Waiter::new(&CATCH_UP, &format!("outbox {label} drained"), f.watch()),
            || {
                let present = path.exists();
                async move {
                    if present {
                        Err("the outbox file is still there".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;
        let mut obj = store
            .get(key.as_str())
            .await
            .unwrap_or_else(|e| panic!("object after {label}: {e}"));
        let mut got = Vec::new();
        obj.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), body.len(), "object size after {label}");
        assert_eq!(got.first(), body.first(), "object content after {label}");
        let info = store.info(&base_key).await.unwrap();
        match label {
            "first" => nuid_before = info.nuid,
            _ => assert_eq!(info.nuid, nuid_before, "{label} replaced the original"),
        }
        let alt = store.info(&alt_key).await;
        assert_eq!(
            alt.is_ok(),
            label == "different body",
            "alternative key presence after {label}"
        );
    }
    let purges: Vec<_> = f
        .broker
        .violations()
        .into_iter()
        .filter(|v| v.subject.contains("STREAM.PURGE"))
        .collect();
    assert!(
        purges.is_empty(),
        "the outbox flows needed a purge: {purges:?}"
    );

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
        guarded(&OPERATION, &format!("agent keys() on {b}"), f.watch(), walk).await;
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
            guarded(
                &OPERATION,
                &format!("agent watch on {b} to deliver"),
                f.watch(),
                seen
            )
            .await,
            "the agent's watch on {b} closed without delivering the update"
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
    let (st, resp) = f
        .api(
            "POST",
            "/api/notifications",
            Some("application/json"),
            &body,
        )
        .await;
    assert_eq!(st, 200, "notification publish: {resp}");
    guarded(
        &OPERATION,
        "notification to reach the agent role",
        f.watch(),
        note.next(),
    )
    .await
    .expect("notification subscription closed");
    let (st, list) = f.api("GET", "/api/notifications", None, "").await;
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
        let replay_watch = f.watch();
        let found = guarded(
            &OPERATION,
            &format!("replay on {name}"),
            replay_watch,
            async {
                while let Some(next) = batch.next().await {
                    match next {
                        Ok(m) => {
                            if m.subject.as_str().contains(needle)
                                || String::from_utf8_lossy(&m.payload).contains(needle)
                            {
                                return true;
                            }
                        }
                        Err(e) => panic!("replay on {name} delivery error: {e}"),
                    }
                }
                false
            },
        )
        .await;
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
    let mut obj = guarded(
        &OPERATION,
        "agent object get (flow control?)",
        f.watch(),
        agent_scripts.get("conformance/big"),
    )
    .await
    .expect("agent object get");
    let mut got = Vec::new();
    guarded(
        &OPERATION,
        "agent object read (flow control?)",
        f.watch(),
        obj.read_to_end(&mut got),
    )
    .await
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
    let mut obj = guarded(
        &OPERATION,
        "agent_releases get",
        f.watch(),
        agent_releases.get("conformance/rel"),
    )
    .await
    .expect("agent_releases get");
    let mut got = Vec::new();
    guarded(
        &OPERATION,
        "agent_releases read",
        f.watch(),
        obj.read_to_end(&mut got),
    )
    .await
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
    let (st, body) = f.api("POST", "/api/jobs", yaml, &manifest).await;
    assert!(st == 200 || st == 201, "job create: {st} {body}");
    let mut collected = backend.client.subscribe("results.*").await.unwrap();
    backend.settle().await;
    let plan = serde_json::json!({"target": {"pcs": [f.pc_id]}}).to_string();
    let (st, body) = f
        .api(
            "POST",
            "/api/exec/conformance-collect",
            Some("application/json"),
            &plan,
        )
        .await;
    assert!(st == 200 || st == 201, "exec: {st} {body}");
    let key = wait_collect_object(&f, &mut collected).await;
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
    let t0 = Instant::now();
    f.broker.restart(Auth::Users).await;
    recover(
        &f,
        "broker restart",
        Auth::Users,
        t0,
        std::future::ready(()),
    )
    .await;
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
        acked(&f, stream, durable).await;
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
        f.backend.as_ref().unwrap().alive(),
        f.agent.as_ref().unwrap().alive(),
    );
    assert!(
        backend_ok && agent_ok,
        "a process exited during the allowed flows"
    );
}

/// Wait until a durable consumer has acknowledged something.
async fn acked(f: &Fleet, stream: &str, durable: &str) {
    wait_until(
        Waiter::new(&CATCH_UP, &format!("{durable} acknowledges"), f.watch()),
        || async move {
            match f.broker.ack_floor(stream, durable).await {
                0 => Err(format!("{stream}/{durable} ack floor is still 0")),
                _ => Ok(()),
            }
        },
    )
    .await;
}

/// The `collect_object` key of the first result from the agent that has one.
async fn wait_collect_object(f: &Fleet, sub: &mut async_nats::Subscriber) -> String {
    let mut w = Waiter::new(&OPERATION, "a collect result from the agent", f.watch());
    loop {
        let msg = next_message(&mut w, sub).await;
        let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap_or_default();
        if v["pc_id"] == f.pc_id.as_str()
            && let Some(k) = v["collect_object"].as_str()
        {
            w.done();
            return k.to_string();
        }
        w.observed("results arrived, none with a collect object from the agent");
    }
}

async fn wait_result_containing(f: &Fleet, sub: &mut async_nats::Subscriber, needle: &str) {
    let mut w = Waiter::new(
        &OPERATION,
        &format!("a result containing {needle:?}"),
        f.watch(),
    );
    loop {
        let msg = next_message(&mut w, sub).await;
        if String::from_utf8_lossy(&msg.payload).contains(needle) {
            w.done();
            return;
        }
        w.observed("results arrived, none with the marker");
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
        (
            Agent,
            Request("$JS.API.STREAM.PURGE.OBJ_result_output"),
            Denied,
        ),
        (
            Agent,
            Request("$JS.API.STREAM.PURGE.OBJ_collections"),
            Denied,
        ),
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
            // The error is on its way: wait for it, stop at the first, and
            // if the bound runs out the verdict below reports what the broker
            // did say.
            let mut w = Waiter::new(
                &VIOLATION_ARRIVAL,
                &format!("{role:?} {op:?} to be refused"),
                f.watch(),
            );
            while !c.violated_since(mark, op.kind(), op.subject()) {
                w.observed(format!("{:?}", c.violations_since(mark)));
                if w.left().is_zero() {
                    break;
                }
                w.tick().await;
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
    let got = guarded(
        &OPERATION,
        "positive control: break-glass publish to reach the observer",
        f.watch(),
        seen.next(),
    )
    .await
    .expect("observer subscription closed");
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
        .filter(|(n, _, _)| n.starts_with("kanade-"))
        .map(|(n, u, _)| (n, u))
        .collect()
}

/// Ids of the kanade connections the broker has right now.
#[cfg(unix)]
async fn kanade_cids(b: &Broker) -> BTreeSet<u64> {
    b.connections()
        .await
        .into_iter()
        .filter(|(n, _, _)| n.starts_with("kanade-"))
        .map(|(_, _, cid)| cid)
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
    // The agent is connected and heartbeating at 1 s: this is a steady wait,
    // not a reconnect one.
    heartbeat_via_fresh_observer(
        f,
        observe_as,
        &HEARTBEAT_STEADY,
        "agent heartbeat",
        stage,
        Instant::now(),
    )
    .await;
    let (st, body) = f
        .api("POST", &format!("/api/agents/{}/ping", f.pc_id), None, "")
        .await;
    assert_eq!(st, 200, "[{stage}] backend → agent ping: {body}");
    let (st, _) = f
        .api(
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
        f.agent.as_ref().unwrap().alive(),
        "[{stage}] the agent exited"
    );
    assert!(
        f.backend.as_ref().unwrap().alive(),
        "[{stage}] the backend exited"
    );
}

/// The CLI that was started before a switch must still receive its result.
#[cfg(unix)]
async fn assert_slow_cli_completes(f: &Fleet, child: Child, marker: &str, stage: &str) {
    let out = guarded(
        &OPERATION,
        &format!("[{stage}] the waiting kanade run to finish"),
        f.watch(),
        child.wait_with_output(),
    )
    .await
    .expect("kanade run output");
    assert!(
        out.status.success() && stdout_of(&out).contains(marker),
        "[{stage}] a kanade run kept across the switch lost its result: {}\n{}",
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Wait until the broker reports both the agent and the backend on the
/// credentials `ready` accepts, counted from `t0`.
#[cfg(unix)]
async fn principals_settle(
    f: &Fleet,
    switch: &str,
    t0: Instant,
    ready: fn(&[(String, String)], &str) -> bool,
) {
    wait_until(
        Waiter::new(&RECONNECT, "processes authenticated as expected", f.watch())
            .switch(switch)
            .since(t0),
        || async {
            let p = principals(&f.broker).await;
            if ready(&p, &f.pc_id) {
                Ok(())
            } else {
                Err(format!("the broker reports {p:?}"))
            }
        },
    )
    .await;
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
    heartbeat_via_fresh_observer(
        &f,
        Auth::Token,
        &AGENT_FIRST_HEARTBEAT,
        "agent first heartbeat",
        "token start",
        f.agent_spawned.unwrap(),
    )
    .await;
    every_role_works(&mut f, "token", Auth::Token).await;
    principals_settle(&f, "token start", Instant::now(), on_token).await;

    // Stage 2 — reload as `users`, processes untouched. The broker stops
    // admitting the token; the clients pick the user at their next attempt.
    let t0 = Instant::now();
    f.broker.reload(Auth::Users);
    proxy.cut();
    recover(
        &f,
        "token to users reload",
        Auth::Users,
        t0,
        principals_settle(&f, "token to users reload", t0, on_users),
    )
    .await;
    every_role_works(&mut f, "users", Auth::Users).await;

    // Stage 3 — reload back to the token, processes still untouched. A reload
    // back leaves the earlier users accepted on this broker, so the clients
    // (whose probe still succeeds) stay on their users. That is recorded, not
    // assumed away: the evidence is the principal the broker reports after a
    // forced reconnect, and the processes keep working either way.
    //
    // A command is left running on the agent across the switch, and is held
    // there until the clients are back: how long a reconnect takes is
    // measured here, so it cannot also be guessed at by a fixed sleep.
    let slow_marker = fresh("slow");
    let slow_dir = TempDir::new().expect("slow command dir");
    let (started, release) = (
        slow_dir.path().join("started"),
        slow_dir.path().join("release"),
    );
    let slow = f.cli_spawn_slow(&slow_marker, &started, &release);
    wait_until(
        Waiter::new(
            &OPERATION,
            "the slow command starts on the agent",
            f.watch(),
        ),
        || {
            let up = started.exists();
            async move {
                if up {
                    Ok(())
                } else {
                    Err("the agent has not started the command".to_string())
                }
            }
        },
    )
    .await;
    let before = kanade_cids(&f.broker).await;
    let t0 = Instant::now();
    f.broker.reload(Auth::Token);
    proxy.cut();
    // Decided by the broker's own report, not by a window that runs out: every
    // process is on a connection opened after the cut, and that connection is
    // authenticated either as the token or still as the role user.
    let settled = async {
        let mut w = Waiter::new(
            &RECONNECT,
            "processes re-authenticated after the reload",
            f.watch(),
        )
        .switch("users to token reload")
        .since(t0);
        loop {
            let p = principals(&f.broker).await;
            let now = kanade_cids(&f.broker).await;
            let fresh_only = now.is_disjoint(&before) && !now.is_empty();
            if fresh_only && on_token(&p, &f.pc_id) {
                w.done_as("reverted_to_token");
                return true;
            }
            if fresh_only && on_users(&p, &f.pc_id) {
                w.done_as("stayed_on_users");
                return false;
            }
            w.observed(format!("the broker reports {p:?}"));
            w.tick().await;
        }
    };
    let reverted_by_reload = recover(&f, "users to token reload", Auth::Token, t0, settled).await;
    eprintln!("revert by reload alone returned the clients to the token: {reverted_by_reload}");
    // Release the held command once a break-glass connection that is newer
    // than the cut is on the broker again.
    wait_until(
        Waiter::new(
            &RECONNECT,
            "the waiting break-glass run reconnected",
            f.watch(),
        )
        .switch("users to token reload")
        .since(t0),
        || async {
            let seen = f.broker.connections().await;
            if seen
                .iter()
                .any(|(n, _, cid)| n.starts_with("kanade-cli") && !before.contains(cid))
            {
                Ok(())
            } else {
                Err(format!("no new break-glass connection among {seen:?}"))
            }
        },
    )
    .await;
    std::fs::write(&release, b"").expect("release the slow command");
    assert_slow_cli_completes(&f, slow, &slow_marker, "users → token (reload)").await;
    if !reverted_by_reload {
        every_role_works(&mut f, "reload-back (users still accepted)", Auth::Users).await;
        // Rollback therefore needs a broker restart; it has to work under the
        // running processes and bring every role back onto the token.
        let t0 = Instant::now();
        f.broker.restart(Auth::Token).await;
        proxy.cut();
        recover(
            &f,
            "users to token restart",
            Auth::Token,
            t0,
            principals_settle(&f, "users to token restart", t0, on_token),
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
    let got = guarded(
        &OPERATION,
        "positive control to arrive",
        f.watch(),
        seen.next(),
    )
    .await
    .expect("victim subscription closed");
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
    let reply = guarded(
        &OPERATION,
        "the consumer create request to be answered",
        f.watch(),
        attacker.client.request(
            "$JS.API.CONSUMER.CREATE.KV_notifications_read",
            serde_json::to_vec(&cfg).unwrap().into(),
        ),
    )
    .await;
    let created = match reply {
        Ok(m) => {
            let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap_or_default();
            v.get("error").is_none()
        }
        Err(_) => false,
    };
    // When the consumer exists, the push is on its way and gets the whole
    // operation bound to show up. When it does not, there is nothing to wait
    // for: a short window is enough to see that nothing arrives.
    let delivered = if created {
        guarded_until_bound(
            &OPERATION,
            "the consumer's push to arrive",
            f.watch(),
            seen.next(),
        )
        .await
        .ok()
        .flatten()
    } else {
        tokio::time::timeout(Duration::from_secs(3), seen.next())
            .await
            .ok()
            .flatten()
    }
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
