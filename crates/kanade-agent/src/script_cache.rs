//! Local on-disk cache of script bodies referenced by
//! `Command::script_object` (kanadehq/kanade#210).
//!
//! ## Layout
//!
//! `<data_dir>/script_cache/<sha256>` — files keyed by digest, not
//! by `(name, version)`. An operator re-upload that produces new
//! bytes also produces a new sha256, so the new digest naturally
//! misses the cache and triggers a fresh fetch. Re-uploads that
//! restore previous bytes (rollback) reuse an existing cache file
//! for free.
//!
//! ## Verification posture
//!
//! Every read path — cache hit AND fresh fetch — hashes the bytes
//! and compares against `expected_sha` before returning. A digest
//! mismatch aborts the run; the agent treats this as "operator
//! re-uploaded the script between exec submission and agent fire,
//! and we won't silently execute different bytes than the operator
//! signed off on".
//!
//! The expected digest comes from the backend resolver at
//! `kanade-backend::api::exec::resolve_script_source`, which
//! snapshots the Object Store digest at exec submission. The wire
//! format is plain lowercase hex (SPEC §2.4.1) — backend converts
//! NATS' base64url digest representation to hex once at submission
//! to keep this module dep-free of base64.
//!
//! ## Size cap
//!
//! Bounded at 4 MB to match the backend's
//! `SCRIPT_OBJECT_BODY_LIMIT`. A corrupted / oversized object
//! errors before draining RAM.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use async_nats::jetstream;
use base64::Engine as _;
use kanade_shared::kv::OBJECT_SCRIPTS;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

/// Mirror of `kanade_backend::api::SCRIPT_OBJECT_BODY_LIMIT`. The
/// two cap each other so a body that fits the backend's POST also
/// fits the agent's fetch.
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Resolver + on-disk cache for OBJECT_SCRIPTS-backed Command bodies.
///
/// Cheap to clone — only holds the jetstream `Context` handle
/// (Arc-internally) and the cache directory PathBuf.
#[derive(Clone)]
pub struct ScriptCache {
    js: jetstream::Context,
    dir: PathBuf,
}

impl ScriptCache {
    pub fn new(js: jetstream::Context, dir: PathBuf) -> Self {
        Self { js, dir }
    }

    /// Resolve a `Command::script_object` reference into the script
    /// body the dispatcher hands to the shell.
    ///
    /// `expected_sha` is the lowercase hex digest the backend
    /// snapshotted at exec submission. Both cache hits and fresh
    /// fetches re-hash and compare against it — see module docs.
    pub async fn resolve(&self, key: &str, expected_sha: &str) -> Result<String> {
        let path = self.cache_path(expected_sha);

        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let actual = hex_sha256(&bytes);
                if actual == expected_sha {
                    debug!(%key, %expected_sha, size = bytes.len(), "script_cache: hit");
                    return bytes_to_string(bytes, key);
                }
                // A digest collision on disk means either bit-rot
                // or someone wrote the wrong file under the sha
                // name. Re-fetch; the new fetch will overwrite the
                // bad file atomically.
                warn!(
                    %key, %expected_sha, %actual,
                    "script_cache: file digest mismatch — refetching",
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(%key, %expected_sha, "script_cache: miss");
            }
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("read cache file {}", path.display()));
            }
        }

        let body = self.fetch(key, expected_sha).await?;
        self.write_atomic(&path, body.as_bytes()).await?;
        info!(
            %key, %expected_sha, size = body.len(),
            "script_cache: populated",
        );
        Ok(body)
    }

    fn cache_path(&self, sha: &str) -> PathBuf {
        self.dir.join(sha)
    }

    /// Look up the broker-side digest for `key` and return it in
    /// lowercase hex — the wire format used by Command's
    /// `script_object_sha256` field.
    ///
    /// Used by the agent-local scheduler (`runs_on: agent`) to
    /// build Commands with the same shape the backend would build,
    /// so they flow through the unified `handle_command` resolver
    /// path uniformly.
    pub async fn digest_of(&self, key: &str) -> Result<String> {
        let store = self
            .js
            .get_object_store(OBJECT_SCRIPTS)
            .await
            .with_context(|| format!("get_object_store {OBJECT_SCRIPTS}"))?;
        let info = store
            .info(key)
            .await
            .with_context(|| format!("object_store.info {key}"))?;
        let raw = info
            .digest
            .as_deref()
            .ok_or_else(|| anyhow!("script_object '{key}' has no broker digest"))?;
        let b64 = raw.strip_prefix("SHA-256=").unwrap_or(raw);
        // NATS async-nats emits URL-safe base64 WITH `=` padding —
        // `URL_SAFE_NO_PAD` rejects it. Use a GeneralPurpose with
        // `DecodePaddingMode::Indifferent` so a future broker emitting
        // unpadded digests doesn't break us either way (Gemini #225
        // — the stock `URL_SAFE` engine uses `RequireCanonical`,
        // which would re-introduce the fragility in the other
        // direction). Symmetric with the backend fix in
        // `exec::resolve_script_source`.
        use base64::alphabet::URL_SAFE;
        use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
        static DECODER: GeneralPurpose = GeneralPurpose::new(
            &URL_SAFE,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        let bytes = DECODER
            .decode(b64)
            .with_context(|| format!("decode digest for '{key}' (raw='{raw}')"))?;
        Ok(hex_lower(&bytes))
    }

    async fn fetch(&self, key: &str, expected_sha: &str) -> Result<String> {
        let store = self
            .js
            .get_object_store(OBJECT_SCRIPTS)
            .await
            .with_context(|| format!("get_object_store {OBJECT_SCRIPTS}"))?;
        let mut obj = store
            .get(key)
            .await
            .with_context(|| format!("object_store.get {key}"))?;

        // Cap the upfront allocation by the broker-reported size
        // (clipped to MAX_BODY_BYTES) so a misreported size from
        // the broker can't blow the agent's heap. The chunk loop
        // also enforces the cap.
        let reported = obj.info().size;
        let cap_hint = reported.min(MAX_BODY_BYTES);
        let mut buf: Vec<u8> = Vec::with_capacity(cap_hint);
        let mut hasher = Sha256::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = obj
                .read(&mut chunk)
                .await
                .with_context(|| format!("read script object '{key}'"))?;
            if n == 0 {
                break;
            }
            if buf.len() + n > MAX_BODY_BYTES {
                bail!(
                    "script_object '{key}' exceeds {MAX_BODY_BYTES} byte cap \
                     (broker reported size {reported})",
                );
            }
            hasher.update(&chunk[..n]);
            buf.extend_from_slice(&chunk[..n]);
        }
        let actual = hex_lower(&hasher.finalize());
        if actual != expected_sha {
            return Err(anyhow!(
                "script_object '{key}' sha256 mismatch: expected={expected_sha} \
                 actual={actual} — operator likely re-uploaded mid-rollout, \
                 refusing to execute unverified bytes",
            ));
        }
        bytes_to_string(buf, key)
    }

    async fn write_atomic(&self, final_path: &Path, bytes: &[u8]) -> Result<()> {
        write_atomic_into(&self.dir, final_path, bytes).await
    }
}

/// Free-function form of the atomic-write so the disk-side unit
/// tests can exercise it without spinning up a real jetstream
/// `Context`.
///
/// Tmp file uses a per-call random suffix (vs a fixed `.tmp`) so
/// two concurrent fetches resolving the same digest don't clobber
/// each other's in-flight write before the rename — Gemini #214
/// MED finding.
async fn write_atomic_into(dir: &Path, final_path: &Path, bytes: &[u8]) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("mkdir {}", dir.display()))?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let tmp = final_path.with_extension(format!("{suffix}.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, final_path)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), final_path.display()))
}

fn bytes_to_string(bytes: Vec<u8>, key: &str) -> Result<String> {
    String::from_utf8(bytes)
        .with_context(|| format!("script_object '{key}' bytes are not valid UTF-8"))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_matches_known_vector() {
        // SHA-256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(hex_sha256(b"hello world"), expected);
    }

    #[test]
    fn hex_lower_pads_each_byte_to_two_chars() {
        // Easy to lose a leading zero with `{:x}` instead of `{:02x}`.
        assert_eq!(hex_lower(&[0x00, 0x0a, 0xff]), "000aff");
    }

    #[tokio::test]
    async fn write_atomic_creates_dir_then_renames() {
        // Exercise the disk-side helpers without a broker — the
        // free-function form makes that possible. The fetch path
        // is covered by the integration tests against a real
        // nats-server (see tests/script_object_fetch.rs).
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("nested").join("script_cache");
        let path = dir.join("deadbeef");
        write_atomic_into(&dir, &path, b"echo hi")
            .await
            .expect("write_atomic");
        assert_eq!(tokio::fs::read(&path).await.expect("read back"), b"echo hi");
        // After the rename, no `<sha>.<uuid>.tmp` scratch file
        // should remain — walk the dir and assert nothing matches
        // that pattern (per-call random suffix means we can't
        // construct the exact expected name here).
        let mut rd = tokio::fs::read_dir(&dir).await.expect("read_dir");
        while let Some(entry) = rd.next_entry().await.expect("next_entry") {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".tmp"), "leftover tmp file: {name}");
        }
    }

    #[tokio::test]
    async fn write_atomic_overwrites_existing_file() {
        // Re-resolving the same sha after a corruption-driven
        // refetch needs to replace the on-disk file atomically.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let path = dir.join("aa");
        write_atomic_into(&dir, &path, b"first")
            .await
            .expect("first");
        write_atomic_into(&dir, &path, b"second")
            .await
            .expect("second");
        assert_eq!(tokio::fs::read(&path).await.expect("read back"), b"second");
    }

    #[tokio::test]
    async fn write_atomic_concurrent_same_sha_does_not_corrupt() {
        // Gemini #214 MED: two commands fetching the same script
        // (same sha) could previously clobber each other's `.tmp`
        // mid-write before rename. With per-call random suffix the
        // races are isolated; the final file is one of the two
        // candidate bodies in full (never a half-written mix).
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::sync::Arc::new(tmp.path().to_path_buf());
        let path = dir.join("samesha");

        let n_writers = 8usize;
        let body_a = vec![b'A'; 4096];
        let body_b = vec![b'B'; 4096];

        let mut handles = Vec::with_capacity(n_writers);
        for i in 0..n_writers {
            let dir = dir.clone();
            let path = path.clone();
            let bytes = if i.is_multiple_of(2) {
                body_a.clone()
            } else {
                body_b.clone()
            };
            handles.push(tokio::spawn(async move {
                write_atomic_into(&dir, &path, &bytes).await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("write_atomic_into");
        }

        let final_bytes = tokio::fs::read(&path).await.expect("final read");
        assert_eq!(final_bytes.len(), 4096, "final length matches one body");
        let all_a = final_bytes.iter().all(|&b| b == b'A');
        let all_b = final_bytes.iter().all(|&b| b == b'B');
        assert!(
            all_a || all_b,
            "final file is one of the candidate bodies, not a mix",
        );
    }
}
