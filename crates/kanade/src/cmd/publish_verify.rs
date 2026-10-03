//! Post-publish read-back verification for `app publish`: the package is read
//! back through the backend's download endpoint, i.e. the same path agents
//! fetch it by. (`agent publish` has no such route, so it only compares the
//! size the backend reports.)
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
use sha2::{Digest, Sha256};
use tracing::warn;

/// Maximum verification attempts (= 1 try + 4 retries). Per-attempt
/// timing: ~1 s for a 40 MB download + hash plus the backoff sleep.
/// Five attempts = ~7-15 s worst case before bail-out, which is small
/// next to the typical `cargo build` ahead of the publish.
const MAX_ATTEMPTS: u32 = 5;

/// Run `once` up to `MAX_ATTEMPTS` times with a short, growing sleep between
/// tries; `true` as soon as one attempt reports success.
async fn with_backoff<F, Fut>(mut once: F) -> bool
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut delay = Duration::from_millis(200);
    for attempt in 1..=MAX_ATTEMPTS {
        if once(attempt).await {
            return true;
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(3));
        }
    }
    false
}

/// Render a finished hasher as the NATS object-store digest string.
fn nats_digest(hasher: Sha256) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE.encode(hasher.finalize());
    format!("SHA-256={b64}")
}

/// Download `url` (authenticated, streamed, never buffered) and
/// compare size and, when the backend reported one, the digest. The backend
/// reports the digest the broker computed, in the same NATS format, so the
/// comparison stays a string equality. Without a digest only the byte count
/// is checked.
pub async fn verify_http_readback(
    client: &reqwest::Client,
    url: &reqwest::Url,
    key: &str,
    expected_digest: Option<&str>,
    expected_size: u64,
) -> Result<()> {
    let ok = with_backoff(|attempt| async move {
        match http_read_and_hash(client, url).await {
            Ok((got_digest, got_size)) => {
                if got_size == expected_size && expected_digest.is_none_or(|d| d == got_digest) {
                    return true;
                }
                warn!(
                    attempt,
                    ?expected_digest,
                    got_digest = %got_digest,
                    expected_size,
                    got_size,
                    "publish read-back mismatch — object store not yet consistent (#277)"
                );
            }
            Err(e) => {
                warn!(attempt, error = %e, "publish read-back: download failed (transient?)");
            }
        }
        false
    })
    .await;
    if ok {
        return Ok(());
    }
    bail!(
        "publish read-back: {key:?} still inconsistent after {MAX_ATTEMPTS} attempts \
         — JetStream race (#277). Retry the publish in a few seconds; if it persists, check broker health."
    );
}

async fn http_read_and_hash(client: &reqwest::Client, url: &reqwest::Url) -> Result<(String, u64)> {
    use futures::StreamExt;

    let resp = client.get(url.clone()).send().await?;
    if !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        total += chunk.len() as u64;
    }
    Ok((nats_digest(hasher), total))
}
