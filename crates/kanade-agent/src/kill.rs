//! One kill switch per run, fed by two sources: the broker's
//! `kill.<exec_id>` subject (the backend API and the CLI stop runs
//! remotely this way) and an in-process registry the Client App's
//! `jobs.kill` triggers directly, so a local cancel needs neither a
//! broker connection nor publish rights on `kill.*`.
//!
//! The switch is created once at the outermost point of a run and held
//! until the run ends. It is a latch: once a kill has arrived it stays
//! set, so a kill that lands between phases (jitter, slot wait, child,
//! retry backoff) is seen by whichever phase comes next instead of being
//! lost in the gap where no listener is subscribed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use kanade_shared::subject;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long arming waits for the broker to confirm the subscription.
/// Local kill is already live by then; this only bounds how long a
/// broker outage can delay the run's start.
const ARM_TIMEOUT: Duration = Duration::from_secs(2);

type Entry = (u64, Arc<watch::Sender<bool>>);

fn registry() -> &'static Mutex<HashMap<String, Vec<Entry>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Vec<Entry>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<String, Vec<Entry>>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Kill the runs registered under `exec_id` in this process. Returns
/// whether any live run was reached; an unknown or finished id is a
/// harmless `false`. Only the Windows-only KLP handler calls it outside tests.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn trigger_local(exec_id: &str) -> bool {
    let senders: Vec<Arc<watch::Sender<bool>>> = lock_registry()
        .get(exec_id)
        .map(|v| v.iter().map(|(_, tx)| tx.clone()).collect())
        .unwrap_or_default();
    for tx in &senders {
        tx.send_replace(true);
    }
    !senders.is_empty()
}

/// RAII handle for one run's kill state. Dropping it unregisters the run
/// and stops the broker forwarder.
pub struct KillSwitch {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
    registration: Option<(String, u64)>,
    forwarder: Option<JoinHandle<()>>,
}

impl KillSwitch {
    /// A switch nothing can trigger, for runs with no `exec_id`.
    pub fn inert() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
            registration: None,
            forwarder: None,
        }
    }

    /// Register `exec_id` in the local registry (synchronously, so a local
    /// kill is live before this returns) and, when a client is given,
    /// subscribe to `kill.<exec_id>`. A broker that is unreachable or slow
    /// costs only remote kill for this run; the local path is unaffected.
    pub async fn arm(client: Option<&async_nats::Client>, exec_id: Option<&str>) -> Self {
        let Some(exec_id) = exec_id else {
            return Self::inert();
        };
        let mut switch = Self::inert();
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        lock_registry()
            .entry(exec_id.to_owned())
            .or_default()
            .push((token, switch.tx.clone()));
        switch.registration = Some((exec_id.to_owned(), token));

        if let Some(client) = client {
            let subject = subject::kill(exec_id);
            let armed = tokio::time::timeout(ARM_TIMEOUT, async {
                let sub = client.subscribe(subject.clone()).await?;
                // Flush so the server has registered our SUB before any
                // publish can race past us.
                client.flush().await.ok();
                Ok::<_, async_nats::SubscribeError>(sub)
            })
            .await;
            match armed {
                Ok(Ok(mut sub)) => {
                    debug!(exec_id, %subject, "kill listener armed");
                    let tx = switch.tx.clone();
                    let id = exec_id.to_owned();
                    switch.forwarder = Some(tokio::spawn(async move {
                        // A closed subscription is not a kill.
                        if sub.next().await.is_some() {
                            info!(exec_id = %id, "kill arm fired (broker)");
                            tx.send_replace(true);
                        }
                    }));
                }
                Ok(Err(e)) => {
                    warn!(exec_id, %subject, error = %e, "kill subscribe failed; only local kill is available for this run");
                }
                Err(_) => {
                    warn!(exec_id, %subject, "kill subscribe timed out; only local kill is available for this run");
                }
            }
        }
        switch
    }

    /// Whether a kill has already arrived.
    pub fn is_killed(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once a kill has arrived; never resolves for an inert
    /// switch. Cancel-safe, and returns immediately if already killed.
    pub async fn killed(&self) {
        wait_killed(self.rx.clone()).await;
    }

    /// An independent receiver, for bridging into a spawned task.
    #[cfg(target_os = "windows")]
    pub fn receiver(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }
}

/// Resolve once `rx` observes a kill.
pub async fn wait_killed(mut rx: watch::Receiver<bool>) {
    // `wait_for` errs only when the sender is gone, which a live switch
    // never lets happen; park instead of reporting a kill that didn't occur.
    if rx.wait_for(|k| *k).await.is_err() {
        std::future::pending::<()>().await;
    }
}

impl Drop for KillSwitch {
    fn drop(&mut self) {
        if let Some(f) = self.forwarder.take() {
            f.abort();
        }
        if let Some((id, token)) = self.registration.take() {
            let mut reg = lock_registry();
            if let Some(v) = reg.get_mut(&id) {
                v.retain(|(t, _)| *t != token);
                if v.is_empty() {
                    reg.remove(&id);
                }
            }
        }
    }
}

/// Broker helpers for the `#[ignore]`d remote-kill tests, which need a
/// `nats-server` (default `127.0.0.1:4222`, override with
/// `KANADE_TEST_NATS_URL`). Run them with
/// `cargo test -p kanade-agent -- --ignored remote_kill`.
#[cfg(test)]
pub mod broker_test {
    pub async fn connect() -> async_nats::Client {
        let url = std::env::var("KANADE_TEST_NATS_URL").unwrap_or("127.0.0.1:4222".into());
        async_nats::connect(url)
            .await
            .expect("connect to nats-server")
    }

    /// What the backend API / CLI does to stop an execution remotely.
    pub async fn publish_kill(client: &async_nats::Client, exec_id: &str) {
        client
            .publish(kanade_shared::subject::kill(exec_id), bytes::Bytes::new())
            .await
            .unwrap();
        client.flush().await.unwrap();
    }
}

/// Number of exec ids currently registered (tests only).
#[cfg(test)]
pub fn registered(exec_id: &str) -> bool {
    lock_registry().contains_key(exec_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_trigger_latches_without_a_broker() {
        let sw = KillSwitch::arm(None, Some("k-local")).await;
        assert!(!sw.is_killed());
        assert!(trigger_local("k-local"));
        tokio::time::timeout(Duration::from_secs(1), sw.killed())
            .await
            .expect("kill observed");
        // Latch: still set, and a second wait returns at once.
        assert!(sw.is_killed());
        tokio::time::timeout(Duration::from_secs(1), sw.killed())
            .await
            .expect("latched");
    }

    #[tokio::test]
    async fn entry_is_removed_when_the_run_ends() {
        let sw = KillSwitch::arm(None, Some("k-leak")).await;
        assert!(registered("k-leak"));
        drop(sw);
        assert!(!registered("k-leak"));
        // Killing a finished run is a no-op.
        assert!(!trigger_local("k-leak"));
        assert!(!trigger_local("k-never-existed"));
    }

    #[tokio::test]
    async fn same_id_switches_are_independent() {
        let a = KillSwitch::arm(None, Some("k-shared")).await;
        let b = KillSwitch::arm(None, Some("k-shared")).await;
        drop(a);
        assert!(registered("k-shared"));
        assert!(trigger_local("k-shared"));
        assert!(b.is_killed());
        drop(b);
        assert!(!registered("k-shared"));
    }

    #[tokio::test]
    async fn inert_switch_never_fires() {
        let sw = KillSwitch::arm(None, None).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sw.killed())
                .await
                .is_err()
        );
    }

    /// The agent must never publish on the kill subject: a Client App
    /// cancel is delivered in-process, and publish rights on `kill.*`
    /// would let a compromised host stop any execution in the fleet.
    /// Scans every source file (comments and the test-only broker helper excluded) except this file's own
    /// test module, which publishes to prove the broker path.
    #[test]
    fn agent_sources_never_publish_to_the_kill_subject() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let mut offenders = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let own = f.file_name().is_some_and(|n| n == "kill.rs");
            let lines: Vec<&str> = text
                .lines()
                .take_while(|l| !(own && l.trim() == "#[cfg(test)]"))
                .filter(|l| !l.trim_start().starts_with("//"))
                // The ignored broker tests stop runs the way the backend
                // does, through the test-only helper.
                .filter(|l| !l.contains("broker_test::publish_kill"))
                .collect();
            for (i, l) in lines.iter().enumerate() {
                if !l.contains("publish") {
                    continue;
                }
                let window = lines[i..lines.len().min(i + 4)].join("\n");
                if window.contains("subject::kill")
                    || window.contains("kill(")
                    || window.contains("\"kill.")
                {
                    offenders.push(format!("{}:{}", f.display(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "publish on the kill subject: {offenders:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a live nats-server"]
    async fn remote_kill_latches_the_switch() {
        let client = broker_test::connect().await;
        let sw = KillSwitch::arm(Some(&client), Some("k-nats")).await;
        broker_test::publish_kill(&client, "k-nats").await;
        tokio::time::timeout(Duration::from_secs(2), sw.killed())
            .await
            .expect("broker kill observed");
    }
}
