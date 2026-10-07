//! A real broker switched between `token` and `users` (by restart) under
//! running clients, with a fault proxy between them, to prove the invariant
//! the operations book promises: after any restart or authentication-mode
//! switch, every role's client either resumes talking within
//! [`RESUME_BOUND`] or its process exits non-zero within [`EXIT_BOUND`] so the
//! service manager restarts it. A client that is alive but silent and not
//! restarted is the failure this file exists to catch.
//!
//! Ignored by default — it spawns throwaway `nats-server` processes (must be
//! in PATH) on random ports and skips cleanly when the binary is absent:
//!
//! ```text
//! cargo test -p kanade-shared --test nats_auth_switch -- --ignored
//! ```
//!
//! The proxy is what makes the overlap deterministic. A real restart makes
//! the credential probe fail only by luck of timing; here the probe (and only
//! the probe, recognised by the connection name in its CONNECT frame) can be
//! dropped or held for a chosen interval while the real attempt reaches a
//! broker that already runs the other mode.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use kanade_shared::nats_client::{
    EXIT_BOUND, NatsCredentials, NatsRole, RESUME_BOUND, connect_with_credentials, wait_until_dead,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

const TOKEN: &str = "fleet-token";

/// Pause between a process exiting and its replacement starting. Stands in for
/// the service manager's restart delay, which the book adds on top of the
/// bounds.
const SUPERVISOR_DELAY: Duration = Duration::from_millis(500);

const ROLES: [NatsRole; 3] = [NatsRole::Agent, NatsRole::Backend, NatsRole::Cli];

fn user_of(role: NatsRole) -> (String, String) {
    (
        format!("{}-user", role.as_str()),
        format!("{}-password", role.as_str()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Token,
    Users,
}

impl Mode {
    fn other(self) -> Self {
        match self {
            Mode::Token => Mode::Users,
            Mode::Users => Mode::Token,
        }
    }

    fn auth_block(self) -> String {
        match self {
            Mode::Token => format!("authorization {{ token: \"{TOKEN}\" }}"),
            Mode::Users => {
                let users: Vec<String> = ROLES
                    .iter()
                    .map(|r| {
                        let (u, p) = user_of(*r);
                        format!("{{ user: \"{u}\", password: \"{p}\" }}")
                    })
                    .collect();
                format!("authorization {{ users: [ {} ] }}", users.join(", "))
            }
        }
    }
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

// ─────────────────────────────── the broker ───────────────────────────────

struct Broker {
    child: Option<tokio::process::Child>,
    dir: tempfile::TempDir,
    port: u16,
}

impl Broker {
    async fn start(mode: Mode) -> Self {
        let mut b = Self {
            child: None,
            dir: tempfile::TempDir::new().expect("tempdir"),
            port: portpicker::pick_unused_port().expect("broker port"),
        };
        b.up(mode).await;
        b
    }

    async fn up(&mut self, mode: Mode) {
        std::fs::write(
            self.dir.path().join("nats.conf"),
            format!(
                "host: 127.0.0.1\nport: {}\n{}\n",
                self.port,
                mode.auth_block()
            ),
        )
        .expect("write nats.conf");
        let child = tokio::process::Command::new("nats-server")
            .arg("-c")
            .arg(self.dir.path().join("nats.conf"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server");
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", self.port)).await.is_err() {
            assert!(Instant::now() < deadline, "nats-server did not come up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn down(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }
}

// ───────────────────────────── the fault proxy ─────────────────────────────

/// What the proxy does to a connection that arrives while the fault is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Forward everything.
    None,
    /// The port is closed: connecting is refused outright.
    Closed,
    /// Accept every connection and drop it before the broker's INFO — the
    /// client reports "expected INFO, got nothing".
    DropAll,
    /// Accept every connection and hold it silent.
    HoldAll,
    /// Drop only credential probes (recognised by name in CONNECT).
    DropProbes,
    /// Hold only credential probes silent until they time out.
    HoldProbes,
    /// Let every connection through, but only after the broker's greeting has
    /// been delayed this many milliseconds (a broker busy recovering).
    SlowInfo(u64),
}

struct ProxyState {
    fault: Mutex<Fault>,
    upstream: Mutex<u16>,
}

struct Proxy {
    port: u16,
    state: Arc<ProxyState>,
    accept: Option<tokio::task::JoinHandle<()>>,
}

impl Proxy {
    async fn start(upstream: u16) -> Self {
        let port = portpicker::pick_unused_port().expect("proxy port");
        let mut p = Self {
            port,
            state: Arc::new(ProxyState {
                fault: Mutex::new(Fault::None),
                upstream: Mutex::new(upstream),
            }),
            accept: None,
        };
        p.listen().await;
        p
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    async fn listen(&mut self) {
        let listener = {
            let mut tries = 0;
            loop {
                match TcpListener::bind(("127.0.0.1", self.port)).await {
                    Ok(l) => break l,
                    Err(e) => {
                        tries += 1;
                        assert!(tries < 100, "rebind proxy port: {e}");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        };
        let state = self.state.clone();
        self.accept = Some(tokio::spawn(async move {
            // Held in the accept task so aborting it severs every pipe too:
            // a closed port and a dead broker both end open connections.
            let mut pipes = JoinSet::new();
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let state = state.clone();
                pipes.spawn(pipe(sock, state));
            }
        }));
    }

    async fn set(&mut self, fault: Fault) {
        let was_closed = *self.state.fault.lock().unwrap() == Fault::Closed;
        *self.state.fault.lock().unwrap() = fault;
        match fault {
            Fault::Closed => {
                if let Some(a) = self.accept.take() {
                    a.abort();
                    let _ = a.await;
                }
            }
            // A held or dropped port severs what is already open, as a
            // restarting broker would.
            Fault::DropAll | Fault::HoldAll => {
                if let Some(a) = self.accept.take() {
                    a.abort();
                    let _ = a.await;
                }
                self.listen().await;
            }
            _ => {
                if was_closed || self.accept.is_none() {
                    self.listen().await;
                }
            }
        }
    }
}

async fn pipe(mut client: TcpStream, state: Arc<ProxyState>) {
    let fault = *state.fault.lock().unwrap();
    match fault {
        Fault::DropAll => return,
        Fault::HoldAll => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            return;
        }
        _ => {}
    }
    let upstream = *state.upstream.lock().unwrap();
    let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
        return;
    };
    if let Fault::SlowInfo(ms) = fault {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let (mut cr, mut cw) = client.split();
    let (mut sr, mut sw) = server.split();

    // Server → client is forwarded untouched. Client → server is read up to
    // the first line (CONNECT) so the connection can be recognised.
    let down = async { tokio::io::copy(&mut sr, &mut cw).await.map(|_| ()) };
    let up = async {
        let mut first = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = cr.read(&mut buf).await?;
            if n == 0 {
                return std::io::Result::Ok(());
            }
            first.extend_from_slice(&buf[..n]);
            if first.contains(&b'\n') {
                break;
            }
        }
        let line = String::from_utf8_lossy(&first).to_string();
        if line.starts_with("CONNECT") && line.contains("\"name\":\"auth-probe\"") {
            let fault = *state.fault.lock().unwrap();
            match fault {
                Fault::DropProbes => return Ok(()),
                Fault::HoldProbes => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return Ok(());
                }
                _ => {}
            }
        }
        sw.write_all(&first).await?;
        tokio::io::copy(&mut cr, &mut sw).await.map(|_| ())
    };
    tokio::select! {
        _ = down => {}
        _ = up => {}
    }
}

// ───────────────────────── a client standing in for a process ─────────────────────────

/// What the test learns about one role's process.
struct Process {
    role: NatsRole,
    /// Times the process exited (non-zero) and was started again.
    exits: Arc<AtomicUsize>,
    /// When the process last got a message of its own through the broker.
    last_talk: Arc<Mutex<Option<Instant>>>,
    task: tokio::task::JoinHandle<()>,
}

/// Run the way the shipped binaries do: connect, talk, and treat
/// [`wait_until_dead`] returning as fatal. A supervisor loop restarts it.
fn spawn_process(role: NatsRole, url: String) -> Process {
    let exits = Arc::new(AtomicUsize::new(0));
    let last_talk = Arc::new(Mutex::new(None));
    let (e, l) = (exits.clone(), last_talk.clone());
    let task = tokio::spawn(async move {
        loop {
            let creds = NatsCredentials::new(Some(TOKEN.into()), Some(user_of(role)));
            let client = connect_with_credentials(role, &url, creds)
                .await
                .expect("connect");
            let subject = format!("echo.{}", role.as_str());
            let mut sub = client.subscribe(subject.clone()).await.expect("subscribe");
            let l2 = l.clone();
            let talk = async {
                let mut tick = tokio::time::interval(Duration::from_millis(250));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            // Publish to ourselves: a message coming back
                            // proves the whole path, both directions.
                            let _ = client.publish(subject.clone(), "x".into()).await;
                        }
                        m = sub.next() => {
                            if m.is_none() { return; }
                            *l2.lock().unwrap() = Some(Instant::now());
                        }
                    }
                }
            };
            tokio::select! {
                () = talk => {}
                () = wait_until_dead(role, &client) => {
                    e.fetch_add(1, Ordering::SeqCst);
                    eprintln!("[{}] process exits non-zero", role.as_str());
                }
            }
            tokio::time::sleep(SUPERVISOR_DELAY).await;
        }
    });
    Process {
        role,
        exits,
        last_talk,
        task,
    }
}

impl Process {
    fn talked_since(&self, t: Instant) -> bool {
        matches!(*self.last_talk.lock().unwrap(), Some(x) if x > t)
    }
}

/// One scenario: switch the broker to the other mode, restart it, and keep a
/// fault in front of the credential probe while it comes back.
struct Scenario {
    name: &'static str,
    fault: Fault,
    /// How long the broker is gone before it starts again.
    down: Duration,
    /// How long the fault stays after the broker is up in the new mode.
    fault_after_up: Duration,
}

async fn run_scenarios(start: Mode, scenarios: &[Scenario]) {
    if !nats_server_available() {
        eprintln!("skipping: nats-server not found in PATH");
        return;
    }
    let mut broker = Broker::start(start).await;
    let mut proxy = Proxy::start(broker.port).await;
    let mut mode = start;
    let processes: Vec<Process> = ROLES
        .iter()
        .map(|r| spawn_process(*r, proxy.url()))
        .collect();
    for p in &processes {
        wait_talking(p, Instant::now(), Duration::from_secs(20))
            .await
            .unwrap_or_else(|| panic!("{:?} never talked at the start", p.role));
    }

    let mut report: HashMap<&str, Vec<String>> = HashMap::new();
    for s in scenarios {
        mode = mode.other();
        eprintln!("=== {} → {:?} (fault {:?}) ===", s.name, mode, s.fault);
        proxy.set(s.fault).await;
        broker.down().await;
        tokio::time::sleep(s.down).await;
        broker.up(mode).await;
        tokio::time::sleep(s.fault_after_up).await;
        proxy.set(Fault::None).await;
        // From here the broker answers in its new mode and nothing is in the
        // way: the bounds start now.
        let t0 = Instant::now();
        let before: Vec<usize> = processes
            .iter()
            .map(|p| p.exits.load(Ordering::SeqCst))
            .collect();
        for (p, was) in processes.iter().zip(before) {
            let resumed = wait_talking(p, t0, EXIT_BOUND + Duration::from_secs(5)).await;
            let at = t0.elapsed();
            let exited = p.exits.load(Ordering::SeqCst) > was;
            report.entry(s.name).or_default().push(format!(
                "{:?}: resumed={:?} exited={exited} at {at:?}",
                p.role, resumed
            ));
            // Resumed in time, or exited in time and its replacement then
            // resumed in time.
            match resumed {
                Some(took) if took <= RESUME_BOUND || exited => {}
                Some(took) => panic!(
                    "{}: {:?} took {took:?} to resume without exiting (bound {RESUME_BOUND:?})",
                    s.name, p.role
                ),
                None => panic!(
                    "{}: {:?} is alive but silent {:?} after the broker came back (exited={exited}): {:?}",
                    s.name,
                    p.role,
                    t0.elapsed(),
                    report
                ),
            }
        }
    }
    for (k, v) in &report {
        eprintln!("{k}: {v:?}");
    }
    for p in processes {
        p.task.abort();
    }
}

/// Wait for `p` to receive a message of its own published after `since`.
async fn wait_talking(p: &Process, since: Instant, within: Duration) -> Option<Duration> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if p.talked_since(since) {
            return Some(since.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

fn s(name: &'static str, fault: Fault, down_ms: u64, fault_after_up_ms: u64) -> Scenario {
    Scenario {
        name,
        fault,
        down: Duration::from_millis(down_ms),
        fault_after_up: Duration::from_millis(fault_after_up_ms),
    }
}

fn matrix() -> Vec<Scenario> {
    vec![
        s("plain restart", Fault::None, 0, 0),
        s(
            "probe dropped while the broker returns",
            Fault::DropProbes,
            200,
            6000,
        ),
        s(
            "probe held while the broker returns",
            Fault::HoldProbes,
            200,
            6000,
        ),
        s("all dropped", Fault::DropAll, 500, 3000),
        s("port closed", Fault::Closed, 1000, 3000),
        s("probe dropped long", Fault::DropProbes, 0, 20000),
        s(
            "greeting slower than the broker's auth timeout",
            Fault::SlowInfo(2500),
            0,
            4000,
        ),
        s("all held", Fault::HoldAll, 200, 6000),
    ]
}

/// The scenarios share the per-role credential state inside `nats_client`, so
/// they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn token_to_users_and_back_under_probe_faults() {
    let _one_at_a_time = SERIAL.lock().await;
    run_scenarios(Mode::Token, &matrix()).await;
}

#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn users_to_token_and_back_under_probe_faults() {
    let _one_at_a_time = SERIAL.lock().await;
    run_scenarios(Mode::Users, &matrix()).await;
}

/// A refusal must not end the client's connection task: the same `Client`
/// that was refused for longer than the sustained-refusal window talks as soon
/// as the broker accepts it. This is what lets the switch procedure rely on a
/// retry instead of a process restart, and it fails if a version of the
/// connection library ever treats an authorization violation as terminal.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn a_refused_client_is_not_dead_and_talks_once_accepted() {
    if !nats_server_available() {
        eprintln!("skipping: nats-server not found in PATH");
        return;
    }
    let _one_at_a_time = SERIAL.lock().await;
    let mut broker = Broker::start(Mode::Token).await;
    // A user and no token: refused for as long as the broker runs `token`.
    let creds = NatsCredentials::new(None, Some(user_of(NatsRole::Backend)));
    let client = connect_with_credentials(
        NatsRole::Backend,
        &format!("nats://127.0.0.1:{}", broker.port),
        creds,
    )
    .await
    .expect("connect");
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(
        !kanade_shared::nats_client::is_dead(&client).await,
        "a refusal ended the connection task"
    );
    broker.down().await;
    broker.up(Mode::Users).await;
    let deadline = Instant::now() + RESUME_BOUND;
    loop {
        if let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(2), client.flush()).await {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the refused client never talked after the broker accepted it"
        );
    }
}
