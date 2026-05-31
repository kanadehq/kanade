//! Shared post-`put` read-back verification for the two CLI commands
//! that publish to a NATS JetStream Object Store (`agent publish` →
//! `OBJECT_AGENT_RELEASES`, `app publish` → `OBJECT_APP_PACKAGES`).
//!
//! Works around #277 / upstream investigation in #278: on at least
//! single-node JetStream, `ObjectStore::put(...).await` can return
//! while the freshly-written chunks aren't yet readable through a
//! follow-up `get()` — so a downstream consumer that calls
//! `/api/app-packages/<name>/<version>` (the backend, then the
//! agent) just after the CLI prints "published" can fetch stale or
//! partial bytes and hash-mismatch.
//!
//! This helper closes the operator-visible window: after the CLI's
//! own `put`, it re-`get`s the same key, reads the full body, and
//! compares the body's SHA-256 against the put-time metadata digest.
//! On mismatch it sleeps a short, growing backoff and retries.
//!
//! That's an extra round-trip + a full re-read of the published
//! bytes per publish — measured cost on a 40 MB binary is ~1 s and
//! the operator pays it once. Until upstream fixes the consistency,
//! it's the cheapest way to guarantee "if `kanade publish` printed
//! success, the bytes are readable for downstream".

use std::time::Duration;

use anyhow::{Result, bail};
use async_nats::jetstream::object_store::ObjectStore;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tracing::warn;

/// Maximum verification attempts (= 1 try + 4 retries). Per-attempt
/// timing: ~1 s for a 40 MB get + read + hash plus the backoff sleep.
/// Five attempts = ~7-15 s worst case before bail-out, which is small
/// next to the typical `cargo build` ahead of the publish.
const MAX_ATTEMPTS: u32 = 5;

/// Confirm `store.get(key)` returns the same bytes `put` just wrote.
/// `expected_digest` is the digest field from `put`'s returned
/// `ObjectInfo` (the NATS-encoded `"SHA-256=<base64url>"` form);
/// `expected_size` ditto from `ObjectInfo.size`.
///
/// Returns `Ok(())` once a read-back hashes to the same digest. Bails
/// after `MAX_ATTEMPTS` if the read-back keeps disagreeing — that
/// signals the upstream race is wider than `MAX_ATTEMPTS * backoff`
/// and the operator should investigate before re-firing exec, rather
/// than the CLI silently letting a downstream consumer fetch garbage.
pub async fn verify_readback(
    store: &ObjectStore,
    key: &str,
    expected_digest: Option<&str>,
    expected_size: usize,
) -> Result<()> {
    let Some(expected_digest) = expected_digest else {
        // `ObjectInfo.digest` is `None` when the server doesn't compute one
        // (`OBJ_<bucket>` stream policy). Nothing to compare against;
        // skip the verify rather than guess. We still ensure the get
        // succeeds + returns the right byte count, which catches the
        // most catastrophic version of the race (empty / truncated).
        return verify_size_only(store, key, expected_size).await;
    };

    let mut delay = Duration::from_millis(200);
    for attempt in 1..=MAX_ATTEMPTS {
        match read_and_hash(store, key).await {
            Ok((got_digest, got_size)) => {
                if got_digest == expected_digest && got_size == expected_size {
                    return Ok(());
                }
                warn!(
                    attempt,
                    expected_digest,
                    got_digest = %got_digest,
                    expected_size,
                    got_size,
                    "publish read-back mismatch — JetStream object_store not yet consistent (#277)"
                );
            }
            Err(e) => {
                warn!(attempt, error = %e, "publish read-back: get failed (transient?)");
            }
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(3));
        }
    }
    bail!(
        "publish read-back: object_store key {key:?} still inconsistent after {MAX_ATTEMPTS} attempts \
         — JetStream race (#277). Retry the publish in a few seconds; if it persists, check broker health."
    );
}

/// Inner: get + stream + compute SHA-256 as `"SHA-256=<base64url>"`
/// matching the NATS digest format so the comparison is a string
/// equality, no decoding step.
async fn read_and_hash(store: &ObjectStore, key: &str) -> Result<(String, usize)> {
    let mut obj = store.get(key).await?;
    let mut hasher = Sha256::new();
    let mut total: usize = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = obj.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n;
    }
    // NATS uses base64url (no padding) for the digest portion. The
    // `=` separator after `SHA-256` is part of the string format, not
    // base64 padding.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
    Ok((format!("SHA-256={b64}"), total))
}

/// Fallback when the server didn't compute a digest — just confirm
/// the read-back returns the right number of bytes. Catches the
/// "empty / truncated" cases without depending on hash equality.
async fn verify_size_only(store: &ObjectStore, key: &str, expected_size: usize) -> Result<()> {
    let mut delay = Duration::from_millis(200);
    for attempt in 1..=MAX_ATTEMPTS {
        let res = async {
            let mut obj = store.get(key).await?;
            let mut buf = Vec::with_capacity(expected_size);
            obj.read_to_end(&mut buf).await?;
            Ok::<usize, anyhow::Error>(buf.len())
        }
        .await;
        match res {
            Ok(got) if got == expected_size => return Ok(()),
            Ok(got) => warn!(
                attempt,
                expected_size, got, "publish read-back size mismatch (#277, no digest)"
            ),
            Err(e) => warn!(attempt, error = %e, "publish read-back: get failed (transient?)"),
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(3));
        }
    }
    bail!(
        "publish read-back: object_store key {key:?} size still wrong after {MAX_ATTEMPTS} attempts"
    );
}
