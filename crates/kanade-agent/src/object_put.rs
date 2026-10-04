//! Object-store writes that never replace an existing object.
//!
//! async-nats' `put` over a name that already has metadata purges the old
//! object's chunks with a whole-stream purge request. The filter travels in
//! the request body, which a subject permission cannot constrain, so the
//! right to purge would let a compromised endpoint empty the stream. The
//! agent therefore looks before it writes: an identical object is reused, a
//! different one is left alone and a distinct key is used instead, so `put`
//! only ever sees a name with no metadata and has nothing to purge.
//!
//! The look-then-write pair is not atomic. Keys carry the pc id / request id
//! and only this agent writes them, so two writers racing on one key does not
//! happen in practice; if it did, the purge would be refused by the broker
//! while the metadata could still be replaced, which is the accepted residual.

use anyhow::{Context, Result, bail};
use async_nats::jetstream::object_store::{InfoErrorKind, ObjectStore};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Candidate keys tried after the base key before giving up.
const MAX_ALT_ATTEMPTS: u32 = 8;

/// What a key currently holds, as far as `put` is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// No metadata. Orphan chunks from an interrupted upload do not count:
    /// without metadata the object does not exist.
    Absent,
    Live {
        size: usize,
        /// `SHA-256=<base64url>` as recorded by the store.
        digest: Option<String>,
    },
    /// A deletion marker. `put` would purge over it, so it is treated as taken.
    Deleted,
}

/// The two store operations the rule needs; faked in unit tests.
pub trait PutStore {
    async fn slot(&self, key: &str) -> Result<Slot>;
    async fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<()>;
}

impl PutStore for ObjectStore {
    async fn slot(&self, key: &str) -> Result<Slot> {
        match self.info(key).await {
            Ok(i) if i.deleted => Ok(Slot::Deleted),
            Ok(i) => Ok(Slot::Live {
                size: i.size,
                digest: i.digest,
            }),
            Err(e) if e.kind() == InfoErrorKind::NotFound => Ok(Slot::Absent),
            Err(e) => Err(anyhow::Error::new(e).context(format!("object_store.info {key}"))),
        }
    }

    async fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let mut cursor = std::io::Cursor::new(bytes);
        self.put(key, &mut cursor)
            .await
            .with_context(|| format!("object_store.put {key}"))?;
        Ok(())
    }
}

fn digest_matches(recorded: &str, want: &[u8]) -> bool {
    let Some(b64) = recorded.strip_prefix("SHA-256=") else {
        return false;
    };
    let b64 = b64.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(b64)
        .is_ok_and(|d| d.as_slice() == want)
}

/// Store `bytes` under `base_key`, or under `alt_key(n)` for n = 1.. when an
/// earlier candidate holds different content. Candidates are tried in a fixed
/// order, so a retry of the same payload converges on the key it used before.
/// Returns the key that holds the bytes.
pub async fn put_no_overwrite<S: PutStore>(
    store: &S,
    base_key: &str,
    bytes: &[u8],
    alt_key: impl Fn(u32) -> String,
) -> Result<String> {
    let want = Sha256::digest(bytes);
    for n in 0..=MAX_ALT_ATTEMPTS {
        let key = if n == 0 {
            base_key.to_string()
        } else {
            alt_key(n)
        };
        match store.slot(&key).await? {
            Slot::Absent => {
                store.put_bytes(&key, bytes).await?;
                return Ok(key);
            }
            Slot::Live { size, digest }
                if size == bytes.len()
                    && digest.as_deref().is_some_and(|d| digest_matches(d, &want)) =>
            {
                return Ok(key);
            }
            Slot::Live { .. } | Slot::Deleted => {}
        }
    }
    bail!("object_store: no free key for {base_key} after {MAX_ALT_ATTEMPTS} alternatives")
}

/// `<base>.r<n>` for keys without an extension that matters (outbox output).
pub fn suffix_key(base: &str) -> impl Fn(u32) -> String + '_ {
    move |n| format!("{base}.r{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        objects: RefCell<HashMap<String, Slot>>,
        /// keys that have chunks but no metadata
        orphans: RefCell<Vec<String>>,
        puts: RefCell<Vec<String>>,
        info_err: bool,
    }

    fn live(bytes: &[u8], pad: bool) -> Slot {
        let d = Sha256::digest(bytes);
        let enc = if pad {
            URL_SAFE.encode(d)
        } else {
            URL_SAFE_NO_PAD.encode(d)
        };
        Slot::Live {
            size: bytes.len(),
            digest: Some(format!("SHA-256={enc}")),
        }
    }

    impl PutStore for Fake {
        async fn slot(&self, key: &str) -> Result<Slot> {
            if self.info_err {
                bail!("broker down");
            }
            Ok(self
                .objects
                .borrow()
                .get(key)
                .cloned()
                .unwrap_or(Slot::Absent))
        }
        async fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<()> {
            // The real `put` purges when metadata exists; the rule must
            // never reach that.
            assert!(
                !self.objects.borrow().contains_key(key),
                "overwrite of {key}"
            );
            self.puts.borrow_mut().push(key.to_string());
            self.objects
                .borrow_mut()
                .insert(key.into(), live(bytes, true));
            Ok(())
        }
    }

    async fn run(f: &Fake, bytes: &[u8]) -> Result<String> {
        put_no_overwrite(f, "r/stdout", bytes, suffix_key("r/stdout")).await
    }

    #[tokio::test]
    async fn first_upload_uses_base_key() {
        let f = Fake::default();
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout");
    }

    #[tokio::test]
    async fn identical_retry_is_a_noop_referencing_existing_key() {
        let f = Fake::default();
        run(&f, b"a").await.unwrap();
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout");
        assert_eq!(f.puts.borrow().len(), 1);
    }

    #[tokio::test]
    async fn digest_padding_is_ignored() {
        let f = Fake::default();
        f.objects
            .borrow_mut()
            .insert("r/stdout".into(), live(b"a", false));
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout");
        assert!(f.puts.borrow().is_empty());
    }

    #[tokio::test]
    async fn different_content_picks_alt_key_and_converges() {
        let f = Fake::default();
        run(&f, b"a").await.unwrap();
        assert_eq!(run(&f, b"b").await.unwrap(), "r/stdout.r1");
        // retry of b converges, original a untouched
        assert_eq!(run(&f, b"b").await.unwrap(), "r/stdout.r1");
        assert_eq!(run(&f, b"c").await.unwrap(), "r/stdout.r2");
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout");
        assert_eq!(f.puts.borrow().len(), 3);
    }

    #[tokio::test]
    async fn same_size_different_digest_is_a_collision() {
        let f = Fake::default();
        run(&f, b"a").await.unwrap();
        assert_eq!(run(&f, b"b").await.unwrap(), "r/stdout.r1");
    }

    #[tokio::test]
    async fn tombstone_counts_as_taken() {
        let f = Fake::default();
        f.objects
            .borrow_mut()
            .insert("r/stdout".into(), Slot::Deleted);
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout.r1");
    }

    #[tokio::test]
    async fn exhausted_candidates_error() {
        let f = Fake::default();
        f.objects
            .borrow_mut()
            .insert("r/stdout".into(), Slot::Deleted);
        for n in 1..=MAX_ALT_ATTEMPTS {
            f.objects
                .borrow_mut()
                .insert(format!("r/stdout.r{n}"), Slot::Deleted);
        }
        assert!(run(&f, b"a").await.is_err());
        assert!(f.puts.borrow().is_empty());
    }

    #[tokio::test]
    async fn info_error_propagates_without_put() {
        let f = Fake {
            info_err: true,
            ..Default::default()
        };
        assert!(run(&f, b"a").await.is_err());
        assert!(f.puts.borrow().is_empty());
    }

    #[tokio::test]
    async fn interrupted_upload_does_not_block_later_one() {
        let f = Fake::default();
        f.orphans.borrow_mut().push("r/stdout".into());
        assert_eq!(run(&f, b"a").await.unwrap(), "r/stdout");
        assert_eq!(f.puts.borrow().len(), 1);
    }
}
