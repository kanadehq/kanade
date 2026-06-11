//! Optimistic-concurrency read-modify-write over a JetStream KV key.
//!
//! A blind `kv.get` → mutate → `kv.put` loses updates when two
//! writers race: both read the same snapshot, both put, and the
//! second put silently erases the first writer's change. JetStream
//! KV's `update(key, value, revision)` is a compare-and-swap — it
//! fails unless the key is still at the revision the caller read —
//! and `create(key, value)` is the matching CAS for "key must not
//! exist yet". [`read_modify_write`] wraps both behind a re-read
//! retry loop so callers only supply the mutation.

use anyhow::Context as _;
use async_nats::jetstream::kv::{Operation, Store};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Upper bound on CAS retries before giving up. Each conflict round
/// means another writer won — global progress is guaranteed — but a
/// single writer can keep losing under a burst (N lockstep writers
/// need up to N rounds for the unluckiest one), so the bound is
/// sized for "an operator script fanning out over one key", not
/// just two racing humans. Hitting it means something is rewriting
/// the key continuously and deserves the error.
const MAX_CAS_ATTEMPTS: usize = 32;

/// Per-round backoff cap. The backoff grows linearly with the
/// attempt number — enough to spread lockstep writers apart (their
/// differing broker round-trip times do the rest) without adding
/// meaningful latency to the 1-conflict common case.
const BACKOFF_STEP_MS: u64 = 2;
const BACKOFF_CAP_MS: u64 = 25;

/// Read `key` (missing or deleted → `T::default()`), apply `mutate`,
/// and write the result back guarded by the revision the read
/// observed, retrying the whole round on a CAS conflict.
///
/// `mutate` returning `false` means "no change needed" — the current
/// value is returned without a write (so no-op adds/removes don't
/// bump the key's revision or wake watchers).
///
/// Returns the value as written (or as read, for a no-op).
pub async fn read_modify_write<T, F>(kv: &Store, key: &str, mut mutate: F) -> anyhow::Result<T>
where
    T: Serialize + DeserializeOwned + Default,
    F: FnMut(&mut T) -> bool,
{
    let mut last_err = None;
    for attempt in 0..MAX_CAS_ATTEMPTS {
        if attempt > 0 {
            let ms = (attempt as u64 * BACKOFF_STEP_MS).min(BACKOFF_CAP_MS);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        // A transient read failure spends an attempt like a write
        // conflict does — the loop is the retry policy for the
        // whole round-trip, not just the CAS write.
        let entry = match kv.entry(key).await {
            Ok(e) => e,
            Err(e) => {
                last_err = Some(anyhow::Error::from(e).context(format!("kv entry '{key}'")));
                continue;
            }
        };
        let (mut value, revision) = match entry {
            Some(e) if e.operation == Operation::Put => {
                let v: T = serde_json::from_slice(&e.value)
                    .with_context(|| format!("decode kv '{key}'"))?;
                (v, Some(e.revision))
            }
            // Delete / purge marker: the value is gone but the
            // marker's revision still guards the CAS, so a writer
            // racing a delete loses cleanly instead of resurrecting
            // over it unguarded.
            Some(e) => (T::default(), Some(e.revision)),
            None => (T::default(), None),
        };
        if !mutate(&mut value) {
            return Ok(value);
        }
        let bytes = serde_json::to_vec(&value).with_context(|| format!("encode kv '{key}'"))?;
        let written = match revision {
            Some(rev) => kv
                .update(key, bytes.into(), rev)
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
            None => kv
                .create(key, bytes.into())
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
        };
        match written {
            Ok(()) => return Ok(value),
            // Revision conflict (or AlreadyExists for the create
            // arm) — somebody won the race; re-read their value and
            // re-apply the mutation on top. Transient broker errors
            // take the same path; they just spend an attempt.
            Err(e) => last_err = Some(e),
        }
    }
    // Not necessarily a conflict — transient broker errors ride the
    // same retry loop — so the context stays cause-neutral and the
    // chained source error carries the specifics.
    Err(last_err.expect("loop ran at least once").context(format!(
        "kv read-modify-write '{key}': failed after {MAX_CAS_ATTEMPTS} attempts"
    )))
}
