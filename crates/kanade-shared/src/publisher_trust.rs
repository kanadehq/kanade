//! Managed package-publisher trust (SPEC §2.13.4.3, stage 2b): who may sign
//! package manifests *now*, with explicit rotation, revocation, freshness and
//! anti-rollback.
//!
//! # Trust model
//!
//! ```text
//! administrator ──(1) pins──▶ TrustRoot (public key, root.json, never replaced)
//! root key holder ─(2) signs▶ TrustState snapshot (sequence N)
//! anyone/any channel ─(3)────▶ apply: verified against the PINNED root only
//! agent ──(4) admit──▶ manifest key_id ∈ active ∧ ∉ revoked ∧ inside windows
//! ```
//!
//! * The **trust root** is a purpose-bound Ed25519 public key
//!   ([`ROOT_PURPOSE`]) an administrator installs with an explicit step
//!   ([`PublisherTrustStore::provision_root`]). It is never learned from NATS,
//!   an object store, a package or a manifest, never replaced by `apply`, and
//!   `provision_root` refuses to overwrite a different pinned root.
//! * A **trust state** is a signed snapshot (`state_version` 1.0): a trust set
//!   id, a strictly monotonic `sequence`, issue/expiry times, the active
//!   publisher keys with validity windows, and the cumulative revoked key ids.
//!   Its signing bytes are [`STATE_SIGNING_DOMAIN`] + compact JSON in a fixed
//!   field order, signed with the same strict Ed25519 primitive as manifests
//!   ([`TrustedPublisherKey`]); the domain differs from the manifest domain,
//!   so neither signature type can be replayed as the other.
//! * Publisher keys, the trust root and command-signing keys are separate
//!   authorities. A state that lists the root key as a publisher is rejected.
//!   Reusing one secret for two roles cannot be detected beyond that.
//!
//! # Lifecycle rules (enforced before anything is persisted)
//!
//! * Same `trust_set_id`, `sequence` strictly greater than the stored one;
//!   an identical re-apply of the stored sequence is an idempotent no-op, a
//!   lower sequence is a rollback and a same-sequence different state is a
//!   conflict. `issued_at` may not go backwards.
//! * The revoked set is cumulative: a successor must contain every revoked id
//!   of its predecessor, so revocation is permanent (a revoked key is never
//!   re-activated; rotate to a fresh key instead).
//! * **Rotation** is an overlap: a state with old and new key both active
//!   (overlapping windows), then a later state that revokes the old key.
//! * A state is valid for at most [`MAX_VALIDITY_SECS`] and is unusable at its
//!   `not_after`: an agent that cannot obtain a newer state stops admitting
//!   new packages (fail closed) while everything else keeps running.
//!
//! # Persistence and clock uncertainty
//!
//! `state.json` holds the signed state plus a time high-water mark in one
//! record, replaced atomically (private temp file → fsync → rename → fsync of
//! the directory). A crash leaves the old or the new record; an orphaned temp
//! file is ignored. Every operation re-reads and re-verifies the record
//! against the pinned root, so a corrupt, unauthenticated or tampered record
//! fails closed ([`TrustError::Broken`]) and is never replaced silently by an
//! initial state. Admission refuses when the wall clock is behind the
//! persisted high-water mark, drifts from the monotonic clock by more than
//! [`CLOCK_TOLERANCE_SECS`], or the high-water mark cannot be persisted.
//!
//! Not defended: an administrator who restores an older copy of the whole
//! trust directory (the high-water mark and sequence go back with it), or who
//! sets the clock *and* the files. The directory must be admin-owned,
//! private and not backed up to or synced from untrusted storage.
//!
//! Delivery is out of scope here: any channel may carry a signed state to
//! [`PublisherTrustStore::apply`], because authenticity comes only from the
//! pinned root. No management transport is wired to it yet.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::package_manifest::{
    ManifestError, PublisherKeyId, PublisherSigningKey, TrustedPublisherKey, decode_b64_exact,
};

pub const STATE_VERSION: &str = "1.0";
/// Prefix of every byte string signed or verified as a trust state.
pub const STATE_SIGNING_DOMAIN: &[u8] = b"kanade/package-trust-state/ed25519/v1\0";
/// `purpose` required of a trust-root record.
pub const ROOT_PURPOSE: &str = "kanade-package-trust-root-v1";
/// Longest `not_after - issued_at` of a state.
pub const MAX_VALIDITY_SECS: u64 = 90 * 24 * 60 * 60;
/// Allowed disagreement between the wall clock and the monotonic clock /
/// the persisted high-water mark.
pub const CLOCK_TOLERANCE_SECS: u64 = 5 * 60;
/// The high-water mark is persisted when the clock is this far past it.
pub const HIGH_WATER_PERSIST_SECS: u64 = 60 * 60;
const MAX_PUBLISHERS: usize = 16;
const MAX_REVOKED: usize = 256;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const RECORD_SCHEMA: u32 = 1;
const ROOT_FILE: &str = "root.json";
const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("malformed trust state: {0}")]
    Malformed(String),
    #[error("unsupported state_version {0:?} (supported: \"1.0\")")]
    UnsupportedVersion(String),
    #[error("trust state is not in canonical v1 form (compact JSON, fixed field order)")]
    NonCanonical,
    #[error("invalid trust state: {0}")]
    Invalid(String),
    #[error("trust state names a different root than the pinned trust root")]
    RootMismatch,
    #[error("trust state signature verification failed")]
    SignatureInvalid,
    #[error("trust state is stale or a rollback: {0}")]
    Rollback(String),
    #[error("trust state conflicts with the stored state: {0}")]
    Conflict(String),
    #[error("trust state is expired")]
    Expired,
    #[error("trust state is not yet valid: {0}")]
    NotYetValid(String),
    #[error("no trust root is pinned (administrator provisioning required)")]
    NoRoot,
    #[error("a different trust root is already pinned; refusing to replace it")]
    RootAlreadyPinned,
    #[error("no trust state has been applied")]
    NoState,
    #[error("stored trust data is unusable, refusing to continue: {0}")]
    Broken(String),
    #[error("clock is uncertain, refusing: {0}")]
    ClockUncertain(String),
    #[error("trust storage error: {0}")]
    Storage(String),
    #[error("publisher key is not in the managed trust set")]
    UnknownPublisher,
    #[error("publisher key is revoked")]
    PublisherRevoked,
    #[error("publisher key is outside its validity window")]
    PublisherOutsideWindow,
    #[error("invalid trust input: {0}")]
    Trust(String),
}

impl From<ManifestError> for TrustError {
    fn from(e: ManifestError) -> Self {
        Self::Trust(e.to_string())
    }
}

fn invalid(m: impl Into<String>) -> TrustError {
    TrustError::Invalid(m.into())
}

fn storage(what: &str, e: impl std::fmt::Display) -> TrustError {
    TrustError::Storage(format!("{what}: {e}"))
}

// ------------------------------------------------------------------ root

/// The administrator-pinned public key that authenticates trust states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRoot(TrustedPublisherKey);

impl TrustRoot {
    pub fn from_root_json(bytes: &[u8]) -> Result<Self, TrustError> {
        Ok(Self(TrustedPublisherKey::from_record_json(
            bytes,
            ROOT_PURPOSE,
        )?))
    }
    /// The root record for the holder of `key` (public material only).
    pub fn from_signing_key(key: &PublisherSigningKey) -> Self {
        Self(key.trusted_key())
    }
    pub fn to_root_json(&self) -> Vec<u8> {
        self.0.to_record_json(ROOT_PURPOSE)
    }
    pub fn key_id(&self) -> &PublisherKeyId {
        self.0.key_id()
    }
}

// ---------------------------------------------------------------- claims

/// One active publisher key and the window in which it may sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherEntry {
    key: TrustedPublisherKey,
    not_before: u64,
    not_after: u64,
}

impl PublisherEntry {
    pub fn new(
        key: TrustedPublisherKey,
        not_before: u64,
        not_after: u64,
    ) -> Result<Self, TrustError> {
        if not_before >= not_after {
            return Err(invalid("publisher not_before must be before not_after"));
        }
        Ok(Self {
            key,
            not_before,
            not_after,
        })
    }
    pub fn key(&self) -> &TrustedPublisherKey {
        &self.key
    }
    pub fn not_before(&self) -> u64 {
        self.not_before
    }
    pub fn not_after(&self) -> u64 {
        self.not_after
    }
}

/// Everything the root signature covers besides the root's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStateClaims {
    trust_set_id: String,
    sequence: u64,
    issued_at: u64,
    not_after: u64,
    publishers: Vec<PublisherEntry>,
    revoked: Vec<String>,
}

impl TrustStateClaims {
    /// `publishers` and `revoked` must be strictly ascending by key id (so a
    /// duplicate is an error and one byte string stands for one state).
    pub fn new(
        trust_set_id: String,
        sequence: u64,
        issued_at: u64,
        not_after: u64,
        publishers: Vec<PublisherEntry>,
        revoked: Vec<String>,
    ) -> Result<Self, TrustError> {
        let ok_id = !trust_set_id.is_empty()
            && trust_set_id.len() <= 64
            && trust_set_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
        if !ok_id {
            return Err(invalid("trust_set_id must be 1-64 bytes of [a-z0-9.-]"));
        }
        if sequence == 0 {
            return Err(invalid("sequence must be at least 1"));
        }
        if issued_at >= not_after {
            return Err(invalid("issued_at must be before not_after"));
        }
        if not_after - issued_at > MAX_VALIDITY_SECS {
            return Err(invalid("state validity exceeds the 90 day maximum"));
        }
        if publishers.len() > MAX_PUBLISHERS || revoked.len() > MAX_REVOKED {
            return Err(invalid("too many publishers or revoked key ids"));
        }
        if !publishers
            .windows(2)
            .all(|w| w[0].key.key_id().as_str() < w[1].key.key_id().as_str())
        {
            return Err(invalid(
                "publishers must be strictly ascending by key_id (no duplicates)",
            ));
        }
        if !revoked.iter().all(|k| valid_key_id(k)) {
            return Err(invalid("revoked entry is not a publisher key id"));
        }
        if !revoked.windows(2).all(|w| w[0] < w[1]) {
            return Err(invalid(
                "revoked must be strictly ascending (no duplicates)",
            ));
        }
        if let Some(p) = publishers
            .iter()
            .find(|p| revoked.iter().any(|r| r == p.key.key_id().as_str()))
        {
            return Err(invalid(format!(
                "key {} is both active and revoked",
                p.key.key_id().as_str()
            )));
        }
        Ok(Self {
            trust_set_id,
            sequence,
            issued_at,
            not_after,
            publishers,
            revoked,
        })
    }

    /// Build from an administrator-written draft; sorts, then validates.
    pub fn from_draft(draft: TrustStateDraft) -> Result<Self, TrustError> {
        let mut publishers = draft
            .publishers
            .iter()
            .map(PublisherJson::to_entry)
            .collect::<Result<Vec<_>, _>>()?;
        publishers.sort_by(|a, b| a.key.key_id().as_str().cmp(b.key.key_id().as_str()));
        let mut revoked = draft.revoked;
        revoked.sort();
        Self::new(
            draft.trust_set_id,
            draft.sequence,
            draft.issued_at,
            draft.not_after,
            publishers,
            revoked,
        )
    }

    pub fn trust_set_id(&self) -> &str {
        &self.trust_set_id
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }
    pub fn not_after(&self) -> u64 {
        self.not_after
    }
    pub fn publishers(&self) -> &[PublisherEntry] {
        &self.publishers
    }
    pub fn revoked(&self) -> &[String] {
        &self.revoked
    }
}

/// `ed25519:` + 64 lowercase hex.
fn valid_key_id(s: &str) -> bool {
    s.strip_prefix("ed25519:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// ------------------------------------------------------------------ wire

/// Public wire shape of one publisher entry (also used by drafts).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherJson {
    pub key_id: String,
    pub public_key: String,
    pub not_before: u64,
    pub not_after: u64,
}

impl PublisherJson {
    fn to_entry(&self) -> Result<PublisherEntry, TrustError> {
        let key = TrustedPublisherKey::from_public_key_b64(&self.public_key)?;
        if key.key_id().as_str() != self.key_id {
            return Err(invalid("publisher key_id does not match its public_key"));
        }
        PublisherEntry::new(key, self.not_before, self.not_after)
    }
    fn of(e: &PublisherEntry) -> Self {
        Self {
            key_id: e.key.key_id().as_str().to_owned(),
            public_key: e.key.public_key_b64(),
            not_before: e.not_before,
            not_after: e.not_after,
        }
    }
}

/// What an administrator writes before signing (`trust-state-sign`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustStateDraft {
    pub trust_set_id: String,
    pub sequence: u64,
    pub issued_at: u64,
    pub not_after: u64,
    pub publishers: Vec<PublisherJson>,
    pub revoked: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireState {
    state_version: String,
    trust_set_id: String,
    sequence: u64,
    issued_at: u64,
    not_after: u64,
    root_key_id: String,
    publishers: Vec<PublisherJson>,
    revoked: Vec<String>,
    signature: String,
}

/// Same field order as [`WireState`], minus `signature`.
#[derive(Serialize)]
struct SigningView<'a> {
    state_version: &'a str,
    trust_set_id: &'a str,
    sequence: u64,
    issued_at: u64,
    not_after: u64,
    root_key_id: &'a str,
    publishers: &'a [PublisherJson],
    revoked: &'a [String],
}

#[derive(Deserialize)]
struct VersionProbe {
    state_version: String,
}

fn wire_publishers(c: &TrustStateClaims) -> Vec<PublisherJson> {
    c.publishers.iter().map(PublisherJson::of).collect()
}

fn signing_bytes(c: &TrustStateClaims, root_key_id: &str) -> Vec<u8> {
    let publishers = wire_publishers(c);
    let view = SigningView {
        state_version: STATE_VERSION,
        trust_set_id: &c.trust_set_id,
        sequence: c.sequence,
        issued_at: c.issued_at,
        not_after: c.not_after,
        root_key_id,
        publishers: &publishers,
        revoked: &c.revoked,
    };
    let mut out = STATE_SIGNING_DOMAIN.to_vec();
    serde_json::to_writer(&mut out, &view).expect("signing view serializes");
    out
}

fn canonical_bytes(c: &TrustStateClaims, root_key_id: &str, signature: &[u8; 64]) -> Vec<u8> {
    let wire = WireState {
        state_version: STATE_VERSION.to_owned(),
        trust_set_id: c.trust_set_id.clone(),
        sequence: c.sequence,
        issued_at: c.issued_at,
        not_after: c.not_after,
        root_key_id: root_key_id.to_owned(),
        publishers: wire_publishers(c),
        revoked: c.revoked.clone(),
        signature: STANDARD.encode(signature),
    };
    let mut out = serde_json::to_vec(&wire).expect("trust state serializes");
    out.push(b'\n');
    out
}

/// Sign `claims` as the holder of the (separate) trust-root key. Returns the
/// canonical file bytes.
pub fn sign_trust_state(claims: &TrustStateClaims, root_key: &PublisherSigningKey) -> Vec<u8> {
    let root_key_id = root_key.trusted_key().key_id().as_str().to_owned();
    let signature = root_key.sign_bytes(&signing_bytes(claims, &root_key_id));
    canonical_bytes(claims, &root_key_id, &signature)
}

// -------------------------------------------------------------- verified

/// A trust state whose signature was checked against the pinned root and
/// whose content passed every structural rule. Only [`verify_trust_state`]
/// constructs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTrustState {
    claims: TrustStateClaims,
    bytes: Vec<u8>,
}

/// A publisher that is authorised at one instant under one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedPublisher {
    key: TrustedPublisherKey,
    trust_set_id: String,
    trust_sequence: u64,
}

impl AuthorizedPublisher {
    pub fn key(&self) -> &TrustedPublisherKey {
        &self.key
    }
    pub fn trust_set_id(&self) -> &str {
        &self.trust_set_id
    }
    pub fn trust_sequence(&self) -> u64 {
        self.trust_sequence
    }
}

impl VerifiedTrustState {
    pub fn claims(&self) -> &TrustStateClaims {
        &self.claims
    }
    /// SHA-256 (lowercase hex) of the canonical state bytes.
    pub fn digest_hex(&self) -> String {
        Sha256::digest(&self.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Is `key_id` allowed to sign at `now`? Fresh, active, unrevoked, inside
    /// its own window.
    pub fn authorize(&self, key_id: &str, now: u64) -> Result<AuthorizedPublisher, TrustError> {
        let c = &self.claims;
        if now >= c.not_after {
            return Err(TrustError::Expired);
        }
        if now + CLOCK_TOLERANCE_SECS < c.issued_at {
            return Err(TrustError::NotYetValid("state issued in the future".into()));
        }
        if c.revoked.iter().any(|r| r == key_id) {
            return Err(TrustError::PublisherRevoked);
        }
        let entry = c
            .publishers
            .iter()
            .find(|p| p.key.key_id().as_str() == key_id)
            .ok_or(TrustError::UnknownPublisher)?;
        if now < entry.not_before || now >= entry.not_after {
            return Err(TrustError::PublisherOutsideWindow);
        }
        Ok(AuthorizedPublisher {
            key: entry.key.clone(),
            trust_set_id: c.trust_set_id.clone(),
            trust_sequence: c.sequence,
        })
    }
}

/// Strict parse + signature check against the pinned `root`.
pub fn verify_trust_state(
    bytes: &[u8],
    root: &TrustRoot,
) -> Result<VerifiedTrustState, TrustError> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(TrustError::Malformed("too large".into()));
    }
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|e| TrustError::Malformed(e.to_string()))?;
    if probe.state_version != STATE_VERSION {
        return Err(TrustError::UnsupportedVersion(probe.state_version));
    }
    let wire: WireState =
        serde_json::from_slice(bytes).map_err(|e| TrustError::Malformed(e.to_string()))?;
    let signature = decode_b64_exact::<64>(&wire.signature)
        .ok_or_else(|| invalid("signature must be padded base64 of exactly 64 bytes"))?;
    let publishers = wire
        .publishers
        .iter()
        .map(PublisherJson::to_entry)
        .collect::<Result<Vec<_>, _>>()?;
    let claims = TrustStateClaims::new(
        wire.trust_set_id,
        wire.sequence,
        wire.issued_at,
        wire.not_after,
        publishers,
        wire.revoked,
    )?;
    let canonical = canonical_bytes(&claims, &wire.root_key_id, &signature);
    if canonical != bytes && canonical.strip_suffix(b"\n") != Some(bytes) {
        return Err(TrustError::NonCanonical);
    }
    if wire.root_key_id != root.key_id().as_str() {
        return Err(TrustError::RootMismatch);
    }
    if !root
        .0
        .verify_strict_bytes(&signing_bytes(&claims, &wire.root_key_id), &signature)
    {
        return Err(TrustError::SignatureInvalid);
    }
    // Key-role separation: the root never doubles as a publisher.
    if claims
        .publishers
        .iter()
        .any(|p| p.key.key_id() == root.key_id())
        || claims.revoked.iter().any(|r| r == root.key_id().as_str())
    {
        return Err(invalid(
            "the trust root key must not appear as a publisher key",
        ));
    }
    Ok(VerifiedTrustState {
        claims,
        bytes: canonical,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// A strictly newer state was persisted.
    Advanced,
    /// The identical stored state: nothing changed.
    Unchanged,
}

/// Anti-rollback rules between the stored state and a candidate.
fn check_successor(
    current: Option<&VerifiedTrustState>,
    new: &VerifiedTrustState,
) -> Result<ApplyOutcome, TrustError> {
    let Some(cur) = current else {
        return Ok(ApplyOutcome::Advanced);
    };
    let (c, n) = (&cur.claims, &new.claims);
    if c.trust_set_id != n.trust_set_id {
        return Err(TrustError::Conflict(
            "trust_set_id differs from the stored state".into(),
        ));
    }
    if n.sequence < c.sequence {
        return Err(TrustError::Rollback(format!(
            "sequence {} is lower than the stored {}",
            n.sequence, c.sequence
        )));
    }
    if n.sequence == c.sequence {
        return if cur.bytes == new.bytes {
            Ok(ApplyOutcome::Unchanged)
        } else {
            Err(TrustError::Conflict(format!(
                "a different state with the same sequence {}",
                n.sequence
            )))
        };
    }
    if n.issued_at < c.issued_at {
        return Err(TrustError::Rollback("issued_at went backwards".into()));
    }
    if let Some(r) = c.revoked.iter().find(|r| !n.revoked.contains(r)) {
        return Err(TrustError::Conflict(format!(
            "revocation of {r} would be undone"
        )));
    }
    Ok(ApplyOutcome::Advanced)
}

// ----------------------------------------------------------------- clock

/// Source of wall and monotonic time; injectable so clock faults are testable.
pub trait WallClock: Send + Sync {
    fn unix_now(&self) -> u64;
    fn monotonic(&self) -> Duration;
}

pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl WallClock for SystemClock {
    fn unix_now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }
}

// ----------------------------------------------------------------- store

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    high_water: u64,
    /// Standard base64 of the canonical signed state bytes.
    state: String,
}

struct Loaded {
    state: VerifiedTrustState,
    high_water: u64,
}

/// Public, secret-free view for `trust-store-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStatus {
    pub root_key_id: Option<String>,
    pub state: Option<StateSummary>,
    /// `ok`, `no-root`, `no-state`, `expired`, `clock-uncertain`, or `broken`.
    pub health: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSummary {
    pub trust_set_id: String,
    pub sequence: u64,
    pub issued_at: u64,
    pub not_after: u64,
    pub digest: String,
    pub publishers: Vec<(String, u64, u64)>,
    pub revoked: Vec<String>,
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
thread_local! {
    static CRASH_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Directory-backed managed publisher trust. Holds no secrets; every call
/// re-reads and re-verifies the files, so a change (including revocation)
/// takes effect without a restart.
pub struct PublisherTrustStore {
    dir: PathBuf,
    clock: Arc<dyn WallClock>,
    base_wall: u64,
    base_mono: Duration,
}

impl PublisherTrustStore {
    pub fn open(dir: impl Into<PathBuf>, clock: Arc<dyn WallClock>) -> Self {
        let base_wall = clock.unix_now();
        let base_mono = clock.monotonic();
        Self {
            dir: dir.into(),
            clock,
            base_wall,
            base_mono,
        }
    }

    pub fn open_system(dir: impl Into<PathBuf>) -> Self {
        Self::open(dir, Arc::new(SystemClock::new()))
    }

    /// Wall time, or an error when it cannot be relied on.
    fn checked_now(&self, high_water: u64) -> Result<u64, TrustError> {
        let wall = self.clock.unix_now();
        let elapsed = self
            .clock
            .monotonic()
            .saturating_sub(self.base_mono)
            .as_secs();
        let expected = self.base_wall.saturating_add(elapsed);
        if wall.abs_diff(expected) > CLOCK_TOLERANCE_SECS {
            return Err(TrustError::ClockUncertain(
                "wall clock jumped relative to the monotonic clock".into(),
            ));
        }
        if wall.saturating_add(CLOCK_TOLERANCE_SECS) < high_water {
            return Err(TrustError::ClockUncertain(
                "wall clock is behind the persisted high-water mark".into(),
            ));
        }
        if wall == 0 {
            return Err(TrustError::ClockUncertain("wall clock unavailable".into()));
        }
        Ok(wall)
    }

    /// Administrator step: pin the trust root. Never replaces a different
    /// root; re-pinning the identical root succeeds.
    pub fn provision_root(&self, root_json: &[u8]) -> Result<TrustRoot, TrustError> {
        let root = TrustRoot::from_root_json(root_json)?;
        let _g = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        ensure_private_dir(&self.dir)?;
        let path = self.dir.join(ROOT_FILE);
        if path.exists() {
            let existing = self.load_root()?;
            return if existing == root {
                Ok(root)
            } else {
                Err(TrustError::RootAlreadyPinned)
            };
        }
        if self.dir.join(STATE_FILE).exists() {
            return Err(TrustError::Broken(
                "a state exists without a pinned root; remove it explicitly first".into(),
            ));
        }
        write_new(&path, &root.to_root_json())?;
        Ok(root)
    }

    pub fn load_root(&self) -> Result<TrustRoot, TrustError> {
        let path = self.dir.join(ROOT_FILE);
        match read_private(&path)? {
            None => Err(TrustError::NoRoot),
            Some(b) => TrustRoot::from_root_json(&b)
                .map_err(|e| TrustError::Broken(format!("pinned root: {e}"))),
        }
    }

    fn load(&self, root: &TrustRoot) -> Result<Option<Loaded>, TrustError> {
        let Some(bytes) = read_private(&self.dir.join(STATE_FILE))? else {
            return Ok(None);
        };
        let broken = |e: &dyn std::fmt::Display| TrustError::Broken(format!("state record: {e}"));
        let rec: Record = serde_json::from_slice(&bytes).map_err(|e| broken(&e))?;
        if rec.schema != RECORD_SCHEMA {
            return Err(broken(&"unsupported record schema"));
        }
        let state_bytes = STANDARD
            .decode(&rec.state)
            .map_err(|_| broken(&"state is not base64"))?;
        let state = verify_trust_state(&state_bytes, root).map_err(|e| broken(&e))?;
        Ok(Some(Loaded {
            state,
            high_water: rec.high_water,
        }))
    }

    fn persist(&self, state: &VerifiedTrustState, high_water: u64) -> Result<(), TrustError> {
        let rec = Record {
            schema: RECORD_SCHEMA,
            high_water,
            state: STANDARD.encode(&state.bytes),
        };
        let mut out = serde_json::to_vec(&rec).expect("record serializes");
        out.push(b'\n');
        write_atomic(&self.dir.join(STATE_FILE), &out)
    }

    /// Verify (against the pinned root only) and persist a candidate state.
    /// Nothing is written unless every check passes.
    pub fn apply(&self, state_bytes: &[u8]) -> Result<ApplyOutcome, TrustError> {
        let _g = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = self.load_root()?;
        let new = verify_trust_state(state_bytes, &root)?;
        let current = self.load(&root)?;
        let outcome = check_successor(current.as_ref().map(|l| &l.state), &new)?;
        if outcome == ApplyOutcome::Unchanged {
            return Ok(outcome);
        }
        let hw = current.as_ref().map_or(0, |l| l.high_water);
        let now = self.checked_now(hw)?;
        if now >= new.claims.not_after {
            return Err(TrustError::Expired);
        }
        if new.claims.issued_at > now + CLOCK_TOLERANCE_SECS {
            return Err(TrustError::NotYetValid("state issued in the future".into()));
        }
        self.persist(&new, hw.max(now).max(new.claims.issued_at))?;
        Ok(outcome)
    }

    /// Is `key_id` (the claim of a manifest, not a trusted fact) authorised
    /// to publish right now? Fails closed on every uncertainty.
    pub fn authorize(&self, key_id: &str) -> Result<AuthorizedPublisher, TrustError> {
        let root = self.load_root()?;
        let loaded = self.load(&root)?.ok_or(TrustError::NoState)?;
        let now = self.checked_now(loaded.high_water)?;
        if now >= loaded.high_water.saturating_add(HIGH_WATER_PERSIST_SECS) {
            let _g = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            // Re-read under the lock so a concurrent apply is not clobbered.
            let again = self.load(&root)?.ok_or(TrustError::NoState)?;
            if now > again.high_water {
                self.persist(&again.state, now).map_err(|e| {
                    TrustError::ClockUncertain(format!("high-water mark not persisted: {e}"))
                })?;
            }
        }
        loaded.state.authorize(key_id, now)
    }

    pub fn status(&self) -> TrustStatus {
        let root = match self.load_root() {
            Ok(r) => r,
            Err(TrustError::NoRoot) => {
                return TrustStatus {
                    root_key_id: None,
                    state: None,
                    health: "no-root".into(),
                };
            }
            Err(_) => {
                return TrustStatus {
                    root_key_id: None,
                    state: None,
                    health: "broken".into(),
                };
            }
        };
        let root_key_id = Some(root.key_id().as_str().to_owned());
        let loaded = match self.load(&root) {
            Ok(Some(l)) => l,
            Ok(None) => {
                return TrustStatus {
                    root_key_id,
                    state: None,
                    health: "no-state".into(),
                };
            }
            Err(_) => {
                return TrustStatus {
                    root_key_id,
                    state: None,
                    health: "broken".into(),
                };
            }
        };
        let c = loaded.state.claims();
        let summary = StateSummary {
            trust_set_id: c.trust_set_id.clone(),
            sequence: c.sequence,
            issued_at: c.issued_at,
            not_after: c.not_after,
            digest: loaded.state.digest_hex(),
            publishers: c
                .publishers
                .iter()
                .map(|p| {
                    (
                        p.key.key_id().as_str().to_owned(),
                        p.not_before,
                        p.not_after,
                    )
                })
                .collect(),
            revoked: c.revoked.clone(),
        };
        let health = match self.checked_now(loaded.high_water) {
            Err(_) => "clock-uncertain",
            Ok(now) if now >= c.not_after => "expired",
            Ok(_) => "ok",
        };
        TrustStatus {
            root_key_id,
            state: Some(summary),
            health: health.into(),
        }
    }
}

// ------------------------------------------------------- private storage

fn ensure_private_dir(dir: &Path) -> Result<(), TrustError> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(dir).map_err(|e| storage("create trust dir", e))?;
        }
        Err(e) => return Err(storage("stat trust dir", e)),
    }
    check_private(dir, true)
}

/// Reject symlinks / reparse points and (unix) group/world access.
fn check_private(path: &Path, dir: bool) -> Result<(), TrustError> {
    let md = std::fs::symlink_metadata(path).map_err(|e| storage("stat", e))?;
    if md.file_type().is_symlink() {
        return Err(TrustError::Broken(format!(
            "{} is a symbolic link",
            path.display()
        )));
    }
    if dir != md.is_dir() {
        return Err(TrustError::Broken(format!(
            "{} has the wrong file type",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o077 != 0 {
            return Err(TrustError::Broken(format!(
                "{} is accessible to group/others (require owner-only)",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return Err(TrustError::Broken(format!(
                "{} is a reparse point",
                path.display()
            )));
        }
    }
    Ok(())
}

/// `Ok(None)` when absent; a bounded read of a private regular file.
fn read_private(path: &Path) -> Result<Option<Vec<u8>>, TrustError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(storage("stat", e)),
    }
    if let Some(parent) = path.parent() {
        check_private(parent, true)?;
    }
    check_private(path, false)?;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| storage("open", e))?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| storage("read", e))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(TrustError::Broken("file too large".into()));
    }
    Ok(Some(buf))
}

fn temp_path(path: &Path) -> Result<PathBuf, TrustError> {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).map_err(|e| storage("random", e))?;
    let suffix: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("x");
    Ok(path.with_file_name(format!(".{name}.{suffix}.tmp")))
}

fn write_tmp(tmp: &Path, bytes: &[u8]) -> Result<(), TrustError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp).map_err(|e| storage("create temp file", e))?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        storage("write temp file", e)
    })
}

fn sync_dir(path: &Path) -> Result<(), TrustError> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| storage("sync directory", e))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// tmp → fsync → rename over `path` → fsync dir. Old or new, never partial.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), TrustError> {
    let tmp = temp_path(path)?;
    write_tmp(&tmp, bytes)?;
    #[cfg(test)]
    if CRASH_BEFORE_RENAME.with(|c| c.get()) {
        return Err(TrustError::Storage("simulated crash before rename".into()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        storage("replace file", e)
    })?;
    sync_dir(path)
}

/// Create `path` only if it does not exist (hard-link of a synced temp file,
/// so a concurrent or earlier file is never overwritten).
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), TrustError> {
    let tmp = temp_path(path)?;
    write_tmp(&tmp, bytes)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked.map_err(|e| storage("create file (refusing to overwrite)", e))?;
    sync_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const T0: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    struct TestClock {
        wall: AtomicU64,
        mono: AtomicU64,
    }
    impl TestClock {
        fn new(wall: u64) -> Arc<Self> {
            Arc::new(Self {
                wall: AtomicU64::new(wall),
                mono: AtomicU64::new(0),
            })
        }
        /// Real time passing: both clocks move.
        fn advance(&self, secs: u64) {
            self.wall.fetch_add(secs, Ordering::SeqCst);
            self.mono.fetch_add(secs, Ordering::SeqCst);
        }
        /// A clock fault: only the wall clock moves.
        fn set_wall(&self, wall: u64) {
            self.wall.store(wall, Ordering::SeqCst);
        }
    }
    impl WallClock for TestClock {
        fn unix_now(&self) -> u64 {
            self.wall.load(Ordering::SeqCst)
        }
        fn monotonic(&self) -> Duration {
            Duration::from_secs(self.mono.load(Ordering::SeqCst))
        }
    }

    fn random_key() -> PublisherSigningKey {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        PublisherSigningKey::from_seed(seed)
    }

    fn entry(k: &PublisherSigningKey, nb: u64, na: u64) -> PublisherEntry {
        PublisherEntry::new(k.trusted_key(), nb, na).unwrap()
    }

    fn sorted(mut v: Vec<PublisherEntry>) -> Vec<PublisherEntry> {
        v.sort_by(|a, b| a.key.key_id().as_str().cmp(b.key.key_id().as_str()));
        v
    }

    fn claims(
        seq: u64,
        issued: u64,
        pubs: Vec<PublisherEntry>,
        revoked: &[&PublisherSigningKey],
    ) -> TrustStateClaims {
        let mut r: Vec<String> = revoked
            .iter()
            .map(|k| k.trusted_key().key_id().as_str().to_owned())
            .collect();
        r.sort();
        TrustStateClaims::new(
            "fleet".into(),
            seq,
            issued,
            issued + 30 * DAY,
            sorted(pubs),
            r,
        )
        .unwrap()
    }

    struct Env {
        _dir: tempfile::TempDir,
        path: PathBuf,
        clock: Arc<TestClock>,
        root: PublisherSigningKey,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("package-trust");
            Self {
                _dir: dir,
                path,
                clock: TestClock::new(T0),
                root: random_key(),
            }
        }
        fn store(&self) -> PublisherTrustStore {
            PublisherTrustStore::open(&self.path, self.clock.clone())
        }
        fn provisioned(&self) -> PublisherTrustStore {
            let s = self.store();
            s.provision_root(&TrustRoot::from_signing_key(&self.root).to_root_json())
                .unwrap();
            s
        }
        fn sign(&self, c: &TrustStateClaims) -> Vec<u8> {
            sign_trust_state(c, &self.root)
        }
    }

    fn kid(k: &PublisherSigningKey) -> String {
        k.trusted_key().key_id().as_str().to_owned()
    }

    #[test]
    fn unpinned_store_refuses_everything_and_never_learns_a_root() {
        let env = Env::new();
        let s = env.store();
        let p = random_key();
        let state = env.sign(&claims(1, T0, vec![entry(&p, T0 - DAY, T0 + DAY)], &[]));
        assert_eq!(s.apply(&state).unwrap_err(), TrustError::NoRoot);
        assert_eq!(s.authorize(&kid(&p)).unwrap_err(), TrustError::NoRoot);
        assert_eq!(s.status().health, "no-root");
        assert!(!env.path.join(ROOT_FILE).exists());
    }

    #[test]
    fn provisioned_root_without_state_fails_closed_for_authorize() {
        let env = Env::new();
        let s = env.provisioned();
        assert_eq!(
            s.authorize(&kid(&random_key())).unwrap_err(),
            TrustError::NoState
        );
        assert_eq!(s.status().health, "no-state");
    }

    #[test]
    fn root_cannot_be_replaced_and_purpose_is_enforced() {
        let env = Env::new();
        let s = env.provisioned();
        let other = TrustRoot::from_signing_key(&random_key()).to_root_json();
        assert_eq!(
            s.provision_root(&other).unwrap_err(),
            TrustError::RootAlreadyPinned
        );
        // Identical re-provision is fine.
        s.provision_root(&TrustRoot::from_signing_key(&env.root).to_root_json())
            .unwrap();
        // A publisher-purpose record is not a root record.
        let publisher_rec = random_key().trusted_key().to_trust_json();
        assert!(matches!(
            TrustRoot::from_root_json(&publisher_rec),
            Err(TrustError::Trust(_))
        ));
        // ...and a root record is not a publisher trust record.
        let root_rec = TrustRoot::from_signing_key(&random_key()).to_root_json();
        assert!(TrustedPublisherKey::from_trust_json(&root_rec).is_err());
    }

    #[test]
    fn active_key_is_authorized_unknown_and_outside_window_are_not() {
        let env = Env::new();
        let s = env.provisioned();
        let (a, future, unknown) = (random_key(), random_key(), random_key());
        let c = claims(
            1,
            T0,
            vec![
                entry(&a, T0 - DAY, T0 + 20 * DAY),
                entry(&future, T0 + DAY, T0 + 20 * DAY),
            ],
            &[],
        );
        assert_eq!(s.apply(&env.sign(&c)).unwrap(), ApplyOutcome::Advanced);
        let ok = s.authorize(&kid(&a)).unwrap();
        assert_eq!(ok.trust_sequence(), 1);
        assert_eq!(ok.key(), &a.trusted_key());
        assert_eq!(
            s.authorize(&kid(&unknown)).unwrap_err(),
            TrustError::UnknownPublisher
        );
        assert_eq!(
            s.authorize(&kid(&future)).unwrap_err(),
            TrustError::PublisherOutsideWindow
        );
        env.clock.advance(2 * DAY);
        s.authorize(&kid(&future)).unwrap();
        env.clock.advance(19 * DAY);
        assert_eq!(
            s.authorize(&kid(&a)).unwrap_err(),
            TrustError::PublisherOutsideWindow
        );
    }

    #[test]
    fn rotation_overlap_then_permanent_revocation() {
        let env = Env::new();
        let s = env.provisioned();
        let (old, new) = (random_key(), random_key());
        s.apply(&env.sign(&claims(
            1,
            T0,
            vec![entry(&old, T0 - DAY, T0 + 29 * DAY)],
            &[],
        )))
        .unwrap();
        // Overlap: both active.
        s.apply(&env.sign(&claims(
            2,
            T0 + 1,
            vec![
                entry(&old, T0 - DAY, T0 + 29 * DAY),
                entry(&new, T0, T0 + 29 * DAY),
            ],
            &[],
        )))
        .unwrap();
        s.authorize(&kid(&old)).unwrap();
        s.authorize(&kid(&new)).unwrap();
        // Cut-over: old revoked.
        s.apply(&env.sign(&claims(
            3,
            T0 + 2,
            vec![entry(&new, T0, T0 + 29 * DAY)],
            &[&old],
        )))
        .unwrap();
        assert_eq!(
            s.authorize(&kid(&old)).unwrap_err(),
            TrustError::PublisherRevoked
        );
        s.authorize(&kid(&new)).unwrap();
        // Un-revoking (even with a higher sequence) is refused.
        let undo = env.sign(&claims(
            4,
            T0 + 3,
            vec![
                entry(&old, T0 - DAY, T0 + 29 * DAY),
                entry(&new, T0, T0 + 29 * DAY),
            ],
            &[],
        ));
        assert!(matches!(
            s.apply(&undo).unwrap_err(),
            TrustError::Conflict(_)
        ));
        assert_eq!(
            s.authorize(&kid(&old)).unwrap_err(),
            TrustError::PublisherRevoked
        );
    }

    #[test]
    fn replay_downgrade_and_same_sequence_conflict() {
        let env = Env::new();
        let s = env.provisioned();
        let (a, b) = (random_key(), random_key());
        let s1 = env.sign(&claims(1, T0, vec![entry(&a, T0 - DAY, T0 + DAY)], &[]));
        let s2 = env.sign(&claims(2, T0 + 1, vec![entry(&b, T0 - DAY, T0 + DAY)], &[]));
        assert_eq!(s.apply(&s1).unwrap(), ApplyOutcome::Advanced);
        assert_eq!(s.apply(&s1).unwrap(), ApplyOutcome::Unchanged);
        assert_eq!(s.apply(&s2).unwrap(), ApplyOutcome::Advanced);
        assert!(matches!(s.apply(&s1).unwrap_err(), TrustError::Rollback(_)));
        let same_seq = env.sign(&claims(2, T0 + 1, vec![entry(&a, T0 - DAY, T0 + DAY)], &[]));
        assert!(matches!(
            s.apply(&same_seq).unwrap_err(),
            TrustError::Conflict(_)
        ));
        // Different trust set cannot hijack the sequence space.
        let other = TrustStateClaims::new(
            "other".into(),
            9,
            T0 + 2,
            T0 + DAY,
            vec![entry(&a, T0 - DAY, T0 + DAY)],
            vec![],
        )
        .unwrap();
        assert!(matches!(
            s.apply(&env.sign(&other)).unwrap_err(),
            TrustError::Conflict(_)
        ));
        assert_eq!(s.status().state.unwrap().sequence, 2);
    }

    #[test]
    fn unauthenticated_states_are_rejected_before_persisting() {
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        let c = claims(1, T0, vec![entry(&a, T0 - DAY, T0 + DAY)], &[]);
        // Signed by a different (attacker) root key.
        let forged = sign_trust_state(&c, &random_key());
        assert_eq!(s.apply(&forged).unwrap_err(), TrustError::RootMismatch);
        // Signed by a publisher key claiming the pinned root id: bad signature.
        let good = String::from_utf8(env.sign(&c)).unwrap();
        let sig_start = good.find("\"signature\":\"").unwrap() + 13;
        let mut tampered = good.clone().into_bytes();
        tampered[sig_start] = if tampered[sig_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert_eq!(
            s.apply(&tampered).unwrap_err(),
            TrustError::SignatureInvalid
        );
        // Content tamper (extend validity of the window) breaks the signature.
        let edited = good.replace(&format!("{}", T0 + DAY), &format!("{}", T0 + 2 * DAY));
        assert_ne!(edited, good);
        assert_eq!(
            s.apply(edited.as_bytes()).unwrap_err(),
            TrustError::SignatureInvalid
        );
        assert!(!env.path.join(STATE_FILE).exists());
        // Non-canonical spelling and unknown fields are rejected.
        let spaced = good.replacen("\"sequence\":1", "\"sequence\": 1", 1);
        assert_eq!(
            s.apply(spaced.as_bytes()).unwrap_err(),
            TrustError::NonCanonical
        );
        let extra = good.replacen("{", "{\"extra\":1,", 1);
        assert!(matches!(
            s.apply(extra.as_bytes()).unwrap_err(),
            TrustError::Malformed(_)
        ));
        assert!(matches!(
            s.apply(b"{").unwrap_err(),
            TrustError::Malformed(_)
        ));
    }

    #[test]
    fn manifest_signing_domain_cannot_stand_in_for_a_state() {
        // A publisher-domain signature over the same JSON does not verify.
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        let c = claims(1, T0, vec![entry(&a, T0 - DAY, T0 + DAY)], &[]);
        let root_id = kid(&env.root);
        let mut forged_msg = crate::package_manifest::SIGNING_DOMAIN.to_vec();
        forged_msg.extend_from_slice(&signing_bytes(&c, &root_id)[STATE_SIGNING_DOMAIN.len()..]);
        let sig = env.root.sign_bytes(&forged_msg);
        let bytes = canonical_bytes(&c, &root_id, &sig);
        assert_eq!(s.apply(&bytes).unwrap_err(), TrustError::SignatureInvalid);
    }

    #[test]
    fn root_key_must_not_double_as_a_publisher() {
        let env = Env::new();
        let s = env.provisioned();
        let c = claims(1, T0, vec![entry(&env.root, T0 - DAY, T0 + DAY)], &[]);
        assert!(matches!(
            s.apply(&env.sign(&c)).unwrap_err(),
            TrustError::Invalid(_)
        ));
    }

    #[test]
    fn structural_rules_reject_duplicates_overlap_and_bad_windows() {
        let a = random_key();
        let e = || entry(&a, T0, T0 + DAY);
        let mk = |pubs, revoked: Vec<String>| {
            TrustStateClaims::new("fleet".into(), 1, T0, T0 + DAY, pubs, revoked)
        };
        assert!(mk(vec![e(), e()], vec![]).is_err(), "duplicate key");
        assert!(mk(vec![e()], vec![kid(&a)]).is_err(), "active and revoked");
        assert!(
            mk(vec![], vec![kid(&a), kid(&a)]).is_err(),
            "duplicate revoked"
        );
        assert!(mk(vec![], vec!["nope".into()]).is_err(), "bad revoked id");
        assert!(
            PublisherEntry::new(a.trusted_key(), T0, T0).is_err(),
            "empty window"
        );
        assert!(
            TrustStateClaims::new("fleet".into(), 0, T0, T0 + DAY, vec![], vec![]).is_err(),
            "sequence 0"
        );
        assert!(
            TrustStateClaims::new("fleet".into(), 1, T0, T0 + 91 * DAY, vec![], vec![]).is_err(),
            "validity too long"
        );
        assert!(
            TrustStateClaims::new("Fleet!".into(), 1, T0, T0 + DAY, vec![], vec![]).is_err(),
            "bad set id"
        );
    }

    #[test]
    fn stale_and_future_states_are_refused() {
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        // Already expired when applied.
        let old = TrustStateClaims::new(
            "fleet".into(),
            1,
            T0 - 10 * DAY,
            T0 - DAY,
            vec![entry(&a, T0 - 10 * DAY, T0 - DAY)],
            vec![],
        )
        .unwrap();
        assert_eq!(s.apply(&env.sign(&old)).unwrap_err(), TrustError::Expired);
        // Issued in the future.
        let fut = claims(1, T0 + DAY, vec![entry(&a, T0, T0 + 2 * DAY)], &[]);
        assert!(matches!(
            s.apply(&env.sign(&fut)).unwrap_err(),
            TrustError::NotYetValid(_)
        ));
        // A valid state goes stale: admission stops at not_after.
        let ok = claims(1, T0, vec![entry(&a, T0 - DAY, T0 + 100 * DAY)], &[]);
        s.apply(&env.sign(&ok)).unwrap();
        s.authorize(&kid(&a)).unwrap();
        env.clock.advance(31 * DAY);
        assert_eq!(s.authorize(&kid(&a)).unwrap_err(), TrustError::Expired);
        assert_eq!(s.status().health, "expired");
    }

    #[test]
    fn restart_reloads_state_and_keeps_revocation() {
        let env = Env::new();
        let (a, b) = (random_key(), random_key());
        {
            let s = env.provisioned();
            s.apply(&env.sign(&claims(
                1,
                T0,
                vec![
                    entry(&a, T0 - DAY, T0 + 10 * DAY),
                    entry(&b, T0 - DAY, T0 + 10 * DAY),
                ],
                &[],
            )))
            .unwrap();
            s.apply(&env.sign(&claims(
                2,
                T0 + 1,
                vec![entry(&b, T0 - DAY, T0 + 10 * DAY)],
                &[&a],
            )))
            .unwrap();
        }
        let restarted = env.store();
        assert_eq!(
            restarted.authorize(&kid(&a)).unwrap_err(),
            TrustError::PublisherRevoked
        );
        restarted.authorize(&kid(&b)).unwrap();
        // Rollback after restart is still refused.
        let s1 = env.sign(&claims(
            1,
            T0,
            vec![entry(&a, T0 - DAY, T0 + 10 * DAY)],
            &[],
        ));
        assert!(matches!(
            restarted.apply(&s1).unwrap_err(),
            TrustError::Rollback(_)
        ));
    }

    #[test]
    fn interrupted_persistence_leaves_the_old_state_intact() {
        let env = Env::new();
        let s = env.provisioned();
        let (a, b) = (random_key(), random_key());
        s.apply(&env.sign(&claims(
            1,
            T0,
            vec![entry(&a, T0 - DAY, T0 + 10 * DAY)],
            &[],
        )))
        .unwrap();
        let s2 = env.sign(&claims(
            2,
            T0 + 1,
            vec![entry(&b, T0 - DAY, T0 + 10 * DAY)],
            &[],
        ));
        CRASH_BEFORE_RENAME.with(|c| c.set(true));
        assert!(matches!(s.apply(&s2).unwrap_err(), TrustError::Storage(_)));
        CRASH_BEFORE_RENAME.with(|c| c.set(false));
        // The orphaned temp file is ignored; the old state is still in force.
        let restarted = env.store();
        restarted.authorize(&kid(&a)).unwrap();
        assert_eq!(
            restarted.authorize(&kid(&b)).unwrap_err(),
            TrustError::UnknownPublisher
        );
        assert_eq!(restarted.apply(&s2).unwrap(), ApplyOutcome::Advanced);
        restarted.authorize(&kid(&b)).unwrap();
    }

    #[test]
    fn corrupt_or_tampered_record_fails_closed_without_reset() {
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        let st = env.sign(&claims(
            1,
            T0,
            vec![entry(&a, T0 - DAY, T0 + 10 * DAY)],
            &[],
        ));
        s.apply(&st).unwrap();
        let path = env.path.join(STATE_FILE);
        let good = std::fs::read(&path).unwrap();
        for bad in [
            b"{\"schema\":1".to_vec(),
            Vec::new(),
            good.iter().map(|b| b.wrapping_add(1)).collect(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(matches!(
                s.authorize(&kid(&a)).unwrap_err(),
                TrustError::Broken(_)
            ));
            assert!(matches!(s.apply(&st).unwrap_err(), TrustError::Broken(_)));
            assert_eq!(s.status().health, "broken");
        }
        // A record whose embedded state is signed by another root is rejected.
        let foreign = sign_trust_state(
            &claims(5, T0, vec![entry(&a, T0 - DAY, T0 + 10 * DAY)], &[]),
            &random_key(),
        );
        let rec = Record {
            schema: 1,
            high_water: T0,
            state: STANDARD.encode(foreign),
        };
        std::fs::write(&path, serde_json::to_vec(&rec).unwrap()).unwrap();
        assert!(matches!(
            s.authorize(&kid(&a)).unwrap_err(),
            TrustError::Broken(_)
        ));
    }

    #[test]
    fn clock_going_backwards_or_jumping_blocks_admission() {
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        s.apply(&env.sign(&claims(
            1,
            T0,
            vec![entry(&a, T0 - DAY, T0 + 20 * DAY)],
            &[],
        )))
        .unwrap();
        // Wall clock jumps (monotonic does not): uncertain, fails closed.
        env.clock.set_wall(T0 - 10 * DAY);
        assert!(matches!(
            s.authorize(&kid(&a)).unwrap_err(),
            TrustError::ClockUncertain(_)
        ));
        assert_eq!(s.status().health, "clock-uncertain");
        env.clock.set_wall(T0 + 10 * DAY);
        assert!(matches!(
            s.authorize(&kid(&a)).unwrap_err(),
            TrustError::ClockUncertain(_)
        ));
        // Back to truth: works again.
        env.clock.set_wall(T0);
        s.authorize(&kid(&a)).unwrap();
        // After a restart the monotonic baseline is fresh, so the persisted
        // high-water mark is what catches a clock that is now far behind.
        env.clock.advance(5 * DAY);
        s.authorize(&kid(&a)).unwrap(); // persists the high-water mark
        let restarted = PublisherTrustStore::open(&env.path, TestClock::new(T0));
        assert!(matches!(
            restarted.authorize(&kid(&a)).unwrap_err(),
            TrustError::ClockUncertain(_)
        ));
        assert!(matches!(
            restarted
                .apply(&env.sign(&claims(2, T0 + 1, vec![], &[])))
                .unwrap_err(),
            TrustError::ClockUncertain(_)
        ));
    }

    #[test]
    fn unpersistable_high_water_mark_blocks_admission() {
        let env = Env::new();
        let s = env.provisioned();
        let a = random_key();
        s.apply(&env.sign(&claims(
            1,
            T0,
            vec![entry(&a, T0 - DAY, T0 + 20 * DAY)],
            &[],
        )))
        .unwrap();
        env.clock.advance(2 * HIGH_WATER_PERSIST_SECS);
        CRASH_BEFORE_RENAME.with(|c| c.set(true));
        let err = s.authorize(&kid(&a)).unwrap_err();
        CRASH_BEFORE_RENAME.with(|c| c.set(false));
        assert!(matches!(err, TrustError::ClockUncertain(_)), "{err:?}");
        s.authorize(&kid(&a)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn insecure_permissions_and_symlinks_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let s = env.provisioned();
        let root_path = env.path.join(ROOT_FILE);
        std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(s.load_root().unwrap_err(), TrustError::Broken(_)));
        std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        s.load_root().unwrap();
        std::fs::set_permissions(&env.path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(s.load_root().unwrap_err(), TrustError::Broken(_)));
        std::fs::set_permissions(&env.path, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A symlinked root file is refused.
        let target = env.path.join("elsewhere");
        std::fs::rename(&root_path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &root_path).unwrap();
        assert!(matches!(s.load_root().unwrap_err(), TrustError::Broken(_)));
    }

    #[test]
    fn draft_is_sorted_and_signed_state_round_trips() {
        let env = Env::new();
        let (a, b) = (random_key(), random_key());
        let draft = TrustStateDraft {
            trust_set_id: "fleet".into(),
            sequence: 1,
            issued_at: T0,
            not_after: T0 + DAY,
            publishers: vec![
                PublisherJson::of(&entry(&a, T0 - DAY, T0 + DAY)),
                PublisherJson::of(&entry(&b, T0 - DAY, T0 + DAY)),
            ],
            revoked: vec![],
        };
        let c = TrustStateClaims::from_draft(draft).unwrap();
        let bytes = env.sign(&c);
        let root = TrustRoot::from_signing_key(&env.root);
        let v = verify_trust_state(&bytes, &root).unwrap();
        assert_eq!(v.claims(), &c);
        // Optional trailing newline, like manifests.
        verify_trust_state(bytes.strip_suffix(b"\n").unwrap(), &root).unwrap();
    }
}
