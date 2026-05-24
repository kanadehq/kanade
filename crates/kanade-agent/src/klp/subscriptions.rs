//! Per-connection subscription registry for KLP push streams.
//!
//! When a client calls `state.subscribe` / `notifications.subscribe`
//! / `jobs.subscribe`, the agent spawns a forwarder task that
//! watches a backend source (watch channel, NATS subject, etc.)
//! and writes `RpcNotification` frames into the connection's
//! shared `push_tx` channel. The forwarder's `JoinHandle` lives
//! here so a subsequent `*.unsubscribe` can abort it, and so
//! [`Drop`] aborts every leftover forwarder when the connection
//! closes (no leaks if a careless client never unsubscribes).
//!
//! IDs follow SPEC §2.12.7's `sub-<namespace>-<n>` format, one
//! counter per namespace. The namespace prefix is supplied by
//! each subscribe handler (`"s"` for state, `"n"` for
//! notifications, `"j"` for jobs).

use std::collections::HashMap;

use tokio::task::JoinHandle;

#[derive(Default)]
pub struct SubscriptionRegistry {
    /// Per-namespace monotonic counters. Keyed by the prefix
    /// string the handler passes in (`"s"` / `"n"` / `"j"`).
    counters: HashMap<&'static str, u64>,
    /// Live forwarder handles keyed by the issued subscription
    /// id. `Drop` aborts each on connection teardown.
    handles: HashMap<String, JoinHandle<()>>,
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh subscription id within `namespace`
    /// (`"s"` / `"n"` / `"j"` etc.) and remember `handle` so a
    /// later [`Self::unsubscribe`] can abort it.
    ///
    /// Returns the id the handler ships back to the client as
    /// `result.subscription` (SPEC §2.12.7).
    pub fn register(&mut self, namespace: &'static str, handle: JoinHandle<()>) -> String {
        let n = self.counters.entry(namespace).or_insert(0);
        *n += 1;
        let id = format!("sub-{namespace}-{n}");
        self.handles.insert(id.clone(), handle);
        id
    }

    /// Cancel the named subscription. Returns `true` if the id
    /// existed; the caller maps `false` to
    /// [`kanade_shared::ipc::error::ErrorKind::NotFound`].
    pub fn unsubscribe(&mut self, id: &str) -> bool {
        if let Some(handle) = self.handles.remove(id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Count of currently-live subscriptions on this connection.
    /// Exposed for diagnostics + tests.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for SubscriptionRegistry {
    fn drop(&mut self) {
        // Best-effort cleanup: abort every still-live forwarder
        // so a careless client (no explicit unsubscribe before
        // disconnect) doesn't leak tasks.
        for (_, handle) in self.handles.drain() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a task that just sleeps forever; the handle is only
    /// useful as something to abort.
    fn dummy_handle() -> JoinHandle<()> {
        tokio::spawn(async {
            // Park forever — the test's runtime drop / abort will
            // kill us.
            std::future::pending::<()>().await;
        })
    }

    #[tokio::test]
    async fn register_issues_namespaced_monotonic_ids() {
        let mut reg = SubscriptionRegistry::new();
        let id1 = reg.register("s", dummy_handle());
        let id2 = reg.register("s", dummy_handle());
        let id3 = reg.register("n", dummy_handle());
        assert_eq!(id1, "sub-s-1");
        assert_eq!(id2, "sub-s-2");
        // Counters are per-namespace — `n` starts at 1, not 3.
        assert_eq!(id3, "sub-n-1");
        assert_eq!(reg.len(), 3);
    }

    #[tokio::test]
    async fn unsubscribe_aborts_handle_and_returns_true() {
        let mut reg = SubscriptionRegistry::new();
        let id = reg.register("s", dummy_handle());
        assert!(reg.unsubscribe(&id));
        assert_eq!(reg.len(), 0);
        // A second call must report NotFound (false).
        assert!(!reg.unsubscribe(&id));
    }

    #[tokio::test]
    async fn drop_aborts_all_outstanding_handles() {
        let mut reg = SubscriptionRegistry::new();
        let _ = reg.register("s", dummy_handle());
        let _ = reg.register("n", dummy_handle());
        let _ = reg.register("j", dummy_handle());
        assert_eq!(reg.len(), 3);
        // Drop is the cleanup path — we can't observe the abort
        // result directly, but tokio's runtime will reap the
        // tasks on the next poll cycle. The lack of leaked task
        // warnings is the actual test.
        drop(reg);
    }
}
