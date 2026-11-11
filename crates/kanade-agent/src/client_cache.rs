//! Durable cache behind the end-user Client App, plus the offline
//! execution gate (`docs/client-offline.md`).
//!
//! Two decisions are kept deliberately separate:
//!
//! * **Viewing** — the job catalog, manifests, group membership, revocation
//!   set and per-user notification history of the last successful
//!   synchronization are persisted under `<data_dir>/client_cache/` and
//!   served when the broker is unreachable (also after an agent restart).
//! * **Executing** — only [`evaluate`] may authorize an offline run. It is a
//!   pure function over an explicit opt-in (`client.offline`), the integrity
//!   of the cached definition, revocation / visibility evidence and a
//!   freshness window measured from the START of the last successful
//!   authoritative sync. The freshness evidence ([`Evidence`]) lives in
//!   process memory only: a stored timestamp cannot prove elapsed time across
//!   a restart or clock change, so after a restart browsing is restored but
//!   execution stays denied until the next successful sync. Anything absent
//!   or uncertain denies.
//!
//! Digests here detect accidental corruption / truncation of the cache. They
//! are NOT tamper protection: the cache directory's confidentiality and
//! integrity rest on its OS permissions (owner-only, symlinks refused).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use kanade_shared::ipc::jobs::{CacheSource, CacheStatus, ExecUnavailable};
use kanade_shared::ipc::notifications::Notification;
use kanade_shared::kv::{
    BUCKET_AGENT_GROUPS, BUCKET_AGENT_GROUPS_DERIVED, BUCKET_JOBS, BUCKET_SCRIPT_STATUS,
    SCRIPT_STATUS_REVOKED,
};
use kanade_shared::manifest::Manifest;
use kanade_shared::wire::Staleness;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

const SCHEMA_VERSION: u32 = 1;
/// Capacity bounds: a larger catalog / history is refused (sync fails closed)
/// or trimmed (notifications keep the newest).
pub const MAX_MANIFESTS: usize = 2000;
pub const MAX_NOTIFICATIONS: usize = 500;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Wall clock may lag the monotonic clock by at most this before the
/// evidence is declared inconsistent (clock set backwards).
const CLOCK_TOLERANCE: Duration = Duration::from_secs(120);

/// Background sync cadence while connected, and the retry delay after a
/// failed attempt.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const SYNC_RETRY: Duration = Duration::from_secs(30);

// ---------- stored data ----------

/// One cached manifest: the raw KV bytes (so the digest is stable regardless
/// of map ordering) plus digests checked on every read.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CachedManifest {
    pub id: String,
    pub raw: String,
    pub sha256: String,
    /// Digest of the inline script body, when the manifest has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_sha256: Option<String>,
}

/// Catalog-side snapshot of one authoritative sync (per PC).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CatalogSnapshot {
    pub version: u32,
    pub scope: String,
    /// Random id of this sync; the in-memory [`Evidence`] must reference the
    /// same generation or the snapshot is not trusted for execution.
    pub generation: String,
    /// Wall clock at the START of the sync (conservative freshness basis).
    pub synced_at: DateTime<Utc>,
    pub entries: Vec<CachedManifest>,
    /// Job ids whose `script_status` is REVOKED at sync time.
    pub revoked: Vec<String>,
    /// Effective group membership (manual ∪ derived).
    pub groups: Vec<String>,
}

/// Per-user notification history snapshot.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NotificationSnapshot {
    pub version: u32,
    pub scope: String,
    pub user_key: String,
    pub synced_at: DateTime<Utc>,
    pub items: Vec<Notification>,
    pub acks: HashMap<String, DateTime<Utc>>,
}

/// In-memory proof of when the current snapshot generation was confirmed.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub generation: String,
    pub wall: DateTime<Utc>,
    pub mono: Instant,
}

// ---------- the offline gate ----------

/// Why an offline run (or cached-data execution) is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// Manifest has no `client.offline.allow: true` (the default).
    NotOptedIn,
    /// `client.offline` / `staleness` carries an unusable duration.
    InvalidPolicy,
    /// No cached definition for the job.
    DefinitionMissing,
    /// Cached manifest or script body fails its digest / parse check.
    IntegrityMismatch,
    /// The job is revoked as of the last sync.
    Revoked,
    /// `client.visible_to` excludes this PC per the cached membership.
    NotVisible,
    /// The job needs the broker/backend to complete (collect, finalize, …).
    NeedsBroker,
    /// No in-process sync evidence (never synced, or agent restarted).
    ResyncRequired,
    /// Evidence does not belong to the loaded snapshot.
    EvidenceMismatch,
    /// Wall clock moved backwards / into the future relative to evidence.
    ClockAnomaly,
    /// The validity period since the last sync has elapsed.
    SyncExpired,
}

impl Denied {
    pub fn code(self) -> &'static str {
        match self {
            Denied::NotOptedIn => "offline_not_allowed",
            Denied::InvalidPolicy => "offline_policy_invalid",
            Denied::DefinitionMissing => "definition_missing",
            Denied::IntegrityMismatch => "integrity_mismatch",
            Denied::Revoked => "revoked",
            Denied::NotVisible => "not_visible",
            Denied::NeedsBroker => "needs_broker",
            Denied::ResyncRequired => "resync_required",
            Denied::EvidenceMismatch => "evidence_mismatch",
            Denied::ClockAnomaly => "clock_anomaly",
            Denied::SyncExpired => "sync_expired",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Denied::NotOptedIn => "このジョブはオフラインでの実行が許可されていません",
            Denied::InvalidPolicy => "オフライン実行の有効期間設定が不正です",
            Denied::DefinitionMissing => "ジョブ定義がこのPCに保存されていません",
            Denied::IntegrityMismatch => "保存されたジョブ定義の整合性を確認できません",
            Denied::Revoked => "このジョブは管理者により失効されています",
            Denied::NotVisible => "このジョブはこのPCでは利用できません",
            Denied::NeedsBroker => {
                "このジョブは完了にサーバー接続が必要なためオフラインでは実行できません"
            }
            Denied::ResyncRequired => "再起動後はサーバーと再同期するまで実行できません",
            Denied::EvidenceMismatch => "同期状態を確認できないため実行できません",
            Denied::ClockAnomaly => "PCの時刻が不正な可能性があるため実行できません",
            Denied::SyncExpired => "最後の同期から有効期間が過ぎたため実行できません",
        }
    }

    pub fn to_wire(self) -> ExecUnavailable {
        ExecUnavailable {
            code: self.code().to_string(),
            message: self.message().to_string(),
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a cache entry from the raw KV bytes of a manifest; `None` when the
/// bytes are not a manifest (such a job is neither shown nor runnable).
pub fn cache_entry(id: &str, raw: &[u8]) -> Option<(CachedManifest, Manifest)> {
    let manifest: Manifest = serde_json::from_slice(raw).ok()?;
    let raw_s = String::from_utf8(raw.to_vec()).ok()?;
    let script_sha256 = manifest
        .execute
        .require_shell()
        .ok()
        .and_then(|e| e.script.as_deref().map(|s| sha256_hex(s.as_bytes())));
    Some((
        CachedManifest {
            id: id.to_string(),
            sha256: sha256_hex(raw),
            raw: raw_s,
            script_sha256,
        },
        manifest,
    ))
}

/// Re-verify an entry and decode it. This is the only way cached manifest
/// bytes become a [`Manifest`], for browsing and for execution alike.
pub fn verify_entry(entry: &CachedManifest) -> Result<Manifest, Denied> {
    if sha256_hex(entry.raw.as_bytes()) != entry.sha256 {
        return Err(Denied::IntegrityMismatch);
    }
    let m: Manifest = serde_json::from_str(&entry.raw).map_err(|_| Denied::IntegrityMismatch)?;
    if m.id != entry.id {
        return Err(Denied::IntegrityMismatch);
    }
    let script = m
        .execute
        .require_shell()
        .ok()
        .and_then(|e| e.script.as_deref().map(sha256_hex_str));
    if script != entry.script_sha256 {
        return Err(Denied::IntegrityMismatch);
    }
    Ok(m)
}

fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

/// Reasons a job can never run offline irrespective of freshness: it needs
/// the broker/backend (approval, collection upload, finalize hook, check /
/// emit / inventory reporting, feed, Object Store body, typed deployment).
fn needs_broker(m: &Manifest) -> bool {
    let Ok(exec) = m.execute.require_shell() else {
        return true;
    };
    m.require_approval
        || m.deploy_authorization.is_some()
        || m.collect.is_some()
        || m.finalize.is_some()
        || m.check.is_some()
        || m.emit.is_some()
        || m.inventory.is_some()
        || !m.feed.is_empty()
        || exec.script_object.is_some()
        || exec.script.as_deref().is_none_or(str::is_empty)
}

/// Agent-wide freshness check shared by the per-job gate and the status
/// shown to the user. Returns the elapsed time since the sync started.
pub fn freshness(
    snapshot_generation: &str,
    evidence: Option<&Evidence>,
    now_wall: DateTime<Utc>,
    now_mono: Instant,
) -> Result<Duration, Denied> {
    let ev = evidence.ok_or(Denied::ResyncRequired)?;
    if ev.generation != snapshot_generation {
        return Err(Denied::EvidenceMismatch);
    }
    let wall = (now_wall - ev.wall)
        .to_std()
        .map_err(|_| Denied::ClockAnomaly)?;
    let mono = now_mono.checked_duration_since(ev.mono).unwrap_or_default();
    if wall + CLOCK_TOLERANCE < mono {
        return Err(Denied::ClockAnomaly);
    }
    // Wall may run ahead of mono (suspend): count the larger, conservatively.
    Ok(wall.max(mono))
}

/// The pure offline execution decision. `Ok(manifest)` is the verified
/// manifest the caller may run; everything else is a typed refusal.
pub fn evaluate(
    snapshot: &CatalogSnapshot,
    evidence: Option<&Evidence>,
    job_id: &str,
    pc_id: &str,
    now_wall: DateTime<Utc>,
    now_mono: Instant,
) -> Result<Manifest, Denied> {
    let entry = snapshot
        .entries
        .iter()
        .find(|e| e.id == job_id)
        .ok_or(Denied::DefinitionMissing)?;
    let m = verify_entry(entry)?;
    // Explicit opt-in first: a job that never opted in is "not allowed",
    // whatever else is true of the cache.
    let Some(off) = m.client.as_ref().and_then(|c| c.offline.as_ref()) else {
        return Err(Denied::NotOptedIn);
    };
    if !off.allow {
        return Err(Denied::NotOptedIn);
    }
    let mut window = off.validity().ok_or(Denied::InvalidPolicy)?;
    // An existing Strict constraint applies too; the shorter bound wins.
    if let Staleness::Strict { max_cache_age } = &m.staleness {
        let strict =
            humantime::parse_duration(max_cache_age.trim()).map_err(|_| Denied::InvalidPolicy)?;
        window = window.min(strict);
    }
    if snapshot.revoked.iter().any(|r| r == job_id) {
        return Err(Denied::Revoked);
    }
    if let Some(target) = m.client.as_ref().and_then(|c| c.visible_to.as_ref())
        && !target.matches(pc_id, &snapshot.groups)
    {
        return Err(Denied::NotVisible);
    }
    if needs_broker(&m) {
        return Err(Denied::NeedsBroker);
    }
    let elapsed = freshness(&snapshot.generation, evidence, now_wall, now_mono)?;
    if elapsed > window {
        return Err(Denied::SyncExpired);
    }
    Ok(m)
}

/// Status of a response that was just read from the live broker.
pub fn live_status() -> CacheStatus {
    CacheStatus {
        source: CacheSource::Live,
        last_sync_at: store().and_then(|s| s.evidence()).map(|e| e.wall),
        exec_unavailable: None,
    }
}

/// Cache status for a list response: agent-wide, not per job.
pub fn status_for(
    source: CacheSource,
    snapshot: Option<&CatalogSnapshot>,
    evidence: Option<&Evidence>,
    now_wall: DateTime<Utc>,
    now_mono: Instant,
) -> CacheStatus {
    let exec_unavailable = match (source, snapshot) {
        (CacheSource::Live, _) => None,
        (_, None) => Some(Denied::ResyncRequired.to_wire()),
        (_, Some(s)) => freshness(&s.generation, evidence, now_wall, now_mono)
            .err()
            .map(Denied::to_wire),
    };
    CacheStatus {
        source,
        last_sync_at: snapshot.map(|s| s.synced_at),
        exec_unavailable,
    }
}

// ---------- store ----------

/// On-disk store + in-memory evidence + single-flight guard.
pub struct Store {
    dir: PathBuf,
    scope: String,
    evidence: Mutex<Option<Evidence>>,
    syncing: tokio::sync::Mutex<()>,
}

static STORE: OnceLock<Store> = OnceLock::new();

/// Install the process-wide store (agent boot). `scope` identifies the
/// broker + PC so a cache is never reused across them.
pub fn init(dir: PathBuf, scope: String) -> Result<&'static Store> {
    let store = Store::new(dir, scope)?;
    Ok(STORE.get_or_init(|| store))
}

pub fn store() -> Option<&'static Store> {
    STORE.get()
}

impl Store {
    pub fn new(dir: PathBuf, scope: String) -> Result<Self> {
        create_private_dir(&dir)?;
        Ok(Self {
            dir,
            scope,
            evidence: Mutex::new(None),
            syncing: tokio::sync::Mutex::new(()),
        })
    }

    fn catalog_path(&self) -> PathBuf {
        self.dir.join(format!(
            "catalog-{}.json",
            &sha256_hex(self.scope.as_bytes())[..16]
        ))
    }

    fn notifications_path(&self, user_key: &str) -> PathBuf {
        let h = sha256_hex(format!("{}\0{}", self.scope, user_key).as_bytes());
        self.dir.join(format!("notifications-{}.json", &h[..16]))
    }

    pub fn evidence(&self) -> Option<Evidence> {
        self.evidence.lock().ok().and_then(|g| g.clone())
    }

    /// Load the persisted catalog snapshot; any problem (missing, unsafe
    /// file, corrupt, other scope, wrong schema) reads as "no cache".
    pub fn load_catalog(&self) -> Option<CatalogSnapshot> {
        let s: CatalogSnapshot = read_json(&self.catalog_path())?;
        (s.version == SCHEMA_VERSION && s.scope == self.scope).then_some(s)
    }

    pub fn load_notifications(&self, user_key: &str) -> Option<NotificationSnapshot> {
        let s: NotificationSnapshot = read_json(&self.notifications_path(user_key))?;
        (s.version == SCHEMA_VERSION && s.scope == self.scope && s.user_key == user_key)
            .then_some(s)
    }

    /// Persist a completed authoritative sync and — only after the write
    /// succeeded — record the in-memory evidence. `started_wall` /
    /// `started_mono` were captured BEFORE the first read.
    pub fn commit_catalog(
        &self,
        started_wall: DateTime<Utc>,
        started_mono: Instant,
        entries: Vec<CachedManifest>,
        revoked: Vec<String>,
        groups: Vec<String>,
    ) -> Result<CatalogSnapshot> {
        if entries.len() > MAX_MANIFESTS {
            bail!(
                "client cache: catalog has {} manifests (cap {MAX_MANIFESTS})",
                entries.len()
            );
        }
        let snap = CatalogSnapshot {
            version: SCHEMA_VERSION,
            scope: self.scope.clone(),
            generation: uuid::Uuid::new_v4().to_string(),
            synced_at: started_wall,
            entries,
            revoked,
            groups,
        };
        write_json_atomic(&self.catalog_path(), &snap)?;
        if let Ok(mut g) = self.evidence.lock() {
            *g = Some(Evidence {
                generation: snap.generation.clone(),
                wall: started_wall,
                mono: started_mono,
            });
        }
        Ok(snap)
    }

    /// Persist a COMPLETE notification history (callers pass it only after
    /// the full stream replay succeeded, so deletions/recalls are reflected).
    pub fn commit_notifications(
        &self,
        user_key: &str,
        started_wall: DateTime<Utc>,
        mut items: Vec<Notification>,
        acks: HashMap<String, DateTime<Utc>>,
    ) -> Result<()> {
        items.sort_by(|a, b| b.issued_at.cmp(&a.issued_at).then_with(|| a.id.cmp(&b.id)));
        items.truncate(MAX_NOTIFICATIONS);
        let snap = NotificationSnapshot {
            version: SCHEMA_VERSION,
            scope: self.scope.clone(),
            user_key: user_key.to_string(),
            synced_at: started_wall,
            items,
            acks,
        };
        write_json_atomic(&self.notifications_path(user_key), &snap)
    }

    /// Full authoritative sync from the broker. Every read must succeed:
    /// a failed membership / revocation / catalog read aborts and leaves the
    /// previous snapshot and evidence untouched (never "no revocations").
    /// Single-flight: a concurrent call returns `Ok(false)` immediately.
    pub async fn sync_catalog(&self, client: &async_nats::Client, pc_id: &str) -> Result<bool> {
        let Ok(_guard) = self.syncing.try_lock() else {
            return Ok(false);
        };
        let started_wall = Utc::now();
        let started_mono = Instant::now();
        let js = async_nats::jetstream::new(client.clone());

        let jobs = js.get_key_value(BUCKET_JOBS).await.context("open jobs")?;
        let keys: Vec<String> = jobs
            .keys()
            .await
            .context("scan jobs")?
            .try_collect()
            .await
            .context("stream jobs")?;
        let mut entries = Vec::new();
        for k in keys {
            // `None`: deleted between scan and read — simply absent.
            let Some(bytes) = jobs
                .get(&k)
                .await
                .with_context(|| format!("read job {k}"))?
            else {
                continue;
            };
            match cache_entry(&k, &bytes) {
                // Only user-facing definitions are cached.
                Some((entry, m)) if m.client.is_some() => entries.push(entry),
                Some(_) => {}
                None => warn!(key = %k, "client cache: skipping unparseable manifest"),
            }
        }

        let manual = read_groups_strict(&js, BUCKET_AGENT_GROUPS, pc_id).await?;
        let derived = read_groups_strict(&js, BUCKET_AGENT_GROUPS_DERIVED, pc_id).await?;
        let groups = crate::groups::union_groups(&manual, &derived);

        let status = js
            .get_key_value(BUCKET_SCRIPT_STATUS)
            .await
            .context("open script_status")?;
        let skeys: Vec<String> = status
            .keys()
            .await
            .context("scan script_status")?
            .try_collect()
            .await
            .context("stream script_status")?;
        let mut revoked = Vec::new();
        for k in skeys {
            if let Some(v) = status
                .get(&k)
                .await
                .with_context(|| format!("read script_status {k}"))?
                && String::from_utf8_lossy(&v) == SCRIPT_STATUS_REVOKED
            {
                revoked.push(k);
            }
        }
        let n = entries.len();
        self.commit_catalog(started_wall, started_mono, entries, revoked, groups)?;
        info!(manifests = n, "client cache: catalog synchronized");
        Ok(true)
    }
}

async fn read_groups_strict(
    js: &async_nats::jetstream::Context,
    bucket: &'static str,
    pc_id: &str,
) -> Result<Vec<String>> {
    let kv = js
        .get_key_value(bucket)
        .await
        .with_context(|| format!("open {bucket}"))?;
    Ok(
        match kv
            .get(pc_id)
            .await
            .with_context(|| format!("read {bucket}"))?
        {
            Some(b) => crate::groups::parse_groups(&b),
            None => Vec::new(),
        },
    )
}

/// Background sync: runs after boot, on every reconnect, and every
/// [`SYNC_INTERVAL`] while connected (retry after [`SYNC_RETRY`] on
/// failure). Never starts user executions; only refreshes data.
pub async fn run_sync_loop(store: &'static Store, client: async_nats::Client, pc_id: String) {
    let mut was_online = false;
    let mut next_at = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let online = crate::klp::state::client_online(&client);
        let due = Instant::now() >= next_at;
        if online && (!was_online || due) {
            next_at = Instant::now()
                + match store.sync_catalog(&client, &pc_id).await {
                    Ok(_) => SYNC_INTERVAL,
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "client cache: sync failed");
                        SYNC_RETRY
                    }
                };
        }
        was_online = online;
    }
}

// ---------- file helpers ----------

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.file_type().is_file() || md.len() > MAX_FILE_BYTES {
        warn!(path = %path.display(), "client cache: refusing non-regular or oversized file");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o022 != 0 {
            warn!(path = %path.display(), "client cache: refusing group/world-writable file");
            return None;
        }
    }
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "client cache: unreadable, ignoring");
            None
        }
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write as _;
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("client cache: snapshot exceeds {MAX_FILE_BYTES} bytes");
    }
    let dir = path.parent().context("cache path has no parent")?;
    // Refuse to follow a planted symlink at the destination.
    if let Ok(md) = std::fs::symlink_metadata(path)
        && md.file_type().is_symlink()
    {
        bail!("client cache: {} is a symlink", path.display());
    }
    let tmp_path = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp_path)
        .with_context(|| format!("create {}", tmp_path.display()))?;
    let res = f
        .write_all(&bytes)
        .and_then(|_| f.sync_all())
        .and_then(|_| {
            drop(f);
            std::fs::rename(&tmp_path, path)
        });
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::Error::new(e).context(format!("write {}", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as CDuration;

    fn manifest_json(id: &str, client_extra: &str, top_extra: &str) -> Vec<u8> {
        // `client_extra` / `top_extra` are JSON object bodies (no braces).
        let wrap = |extra: &str| -> serde_json::Value {
            serde_json::from_str(&format!("{{{extra}}}")).unwrap()
        };
        let mut m = serde_json::json!({
            "id": id,
            "version": "1.0.0",
            "execute": {"shell": "powershell", "script": "echo hi", "timeout": "30s"},
            "client": {"name": "N", "category": "troubleshoot"},
        });
        for (k, v) in wrap(top_extra).as_object().unwrap() {
            m[k] = v.clone();
        }
        for (k, v) in wrap(client_extra).as_object().unwrap() {
            m["client"][k] = v.clone();
        }
        let parsed: Manifest = serde_json::from_value(m).unwrap();
        parsed.validate().unwrap();
        serde_json::to_vec(&parsed).unwrap()
    }

    const OPT_IN: &str = r#""offline": {"allow": true, "max_age": "1h"}"#;

    fn snap(
        jobs: &[(&str, Vec<u8>)],
        revoked: &[&str],
        groups: &[&str],
        at: DateTime<Utc>,
    ) -> CatalogSnapshot {
        CatalogSnapshot {
            version: SCHEMA_VERSION,
            scope: "pc1".into(),
            generation: "g1".into(),
            synced_at: at,
            entries: jobs
                .iter()
                .map(|(id, raw)| cache_entry(id, raw).unwrap().0)
                .collect(),
            revoked: revoked.iter().map(|s| s.to_string()).collect(),
            groups: groups.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn ev(at: DateTime<Utc>, mono: Instant) -> Evidence {
        Evidence {
            generation: "g1".into(),
            wall: at,
            mono,
        }
    }

    #[test]
    fn default_and_explicit_deny_never_run_offline() {
        let now = Utc::now();
        let mono = Instant::now();
        let s = snap(
            &[
                ("plain", manifest_json("plain", "", "")),
                (
                    "denied",
                    manifest_json("denied", r#""offline": {"allow": false}"#, ""),
                ),
            ],
            &[],
            &[],
            now,
        );
        let e = ev(now, mono);
        for id in ["plain", "denied"] {
            assert_eq!(
                evaluate(&s, Some(&e), id, "pc1", now, mono).unwrap_err(),
                Denied::NotOptedIn
            );
        }
    }

    #[test]
    fn opted_in_runs_within_window_and_expires_after() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        let e = ev(t0, m0);
        let ok = evaluate(
            &s,
            Some(&e),
            "j",
            "pc1",
            t0 + CDuration::minutes(59),
            m0 + Duration::from_secs(3540),
        );
        assert!(ok.is_ok());
        let late = evaluate(
            &s,
            Some(&e),
            "j",
            "pc1",
            t0 + CDuration::minutes(61),
            m0 + Duration::from_secs(3660),
        );
        assert_eq!(late.unwrap_err(), Denied::SyncExpired);
        // Wall clock held back a little: the monotonic clock still expires it.
        let held = evaluate(
            &s,
            Some(&e),
            "j",
            "pc1",
            t0 + CDuration::minutes(59),
            m0 + Duration::from_secs(3660),
        );
        assert_eq!(held.unwrap_err(), Denied::SyncExpired);
        // Wall clock far behind the monotonic clock is an anomaly, not a pass.
        let skewed = evaluate(
            &s,
            Some(&e),
            "j",
            "pc1",
            t0 + CDuration::minutes(10),
            m0 + Duration::from_secs(3000),
        );
        assert_eq!(skewed.unwrap_err(), Denied::ClockAnomaly);
    }

    #[test]
    fn restart_without_evidence_denies_but_data_remains() {
        let t0 = Utc::now();
        let s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        assert_eq!(
            evaluate(&s, None, "j", "pc1", t0, Instant::now()).unwrap_err(),
            Denied::ResyncRequired
        );
        let other = Evidence {
            generation: "other".into(),
            ..ev(t0, Instant::now())
        };
        assert_eq!(
            evaluate(&s, Some(&other), "j", "pc1", t0, Instant::now()).unwrap_err(),
            Denied::EvidenceMismatch
        );
    }

    #[test]
    fn clock_set_backwards_or_future_sync_denies() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        let e = ev(t0, m0);
        assert_eq!(
            evaluate(&s, Some(&e), "j", "pc1", t0 - CDuration::minutes(5), m0).unwrap_err(),
            Denied::ClockAnomaly
        );
    }

    #[test]
    fn strict_staleness_shortens_the_window() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let s = snap(
            &[(
                "j",
                manifest_json(
                    "j",
                    OPT_IN,
                    r#""staleness": {"mode": "strict", "max_cache_age": "1m"}"#,
                ),
            )],
            &[],
            &[],
            t0,
        );
        let e = ev(t0, m0);
        assert!(
            evaluate(
                &s,
                Some(&e),
                "j",
                "pc1",
                t0 + CDuration::seconds(30),
                m0 + Duration::from_secs(30)
            )
            .is_ok()
        );
        assert_eq!(
            evaluate(
                &s,
                Some(&e),
                "j",
                "pc1",
                t0 + CDuration::minutes(2),
                m0 + Duration::from_secs(120)
            )
            .unwrap_err(),
            Denied::SyncExpired
        );
    }

    #[test]
    fn revoked_not_visible_and_missing_are_refused() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let vis = format!(r#"{OPT_IN}, "visible_to": {{"groups": ["staff"]}}"#);
        let s = snap(
            &[
                ("rev", manifest_json("rev", OPT_IN, "")),
                ("grp", manifest_json("grp", &vis, "")),
            ],
            &["rev"],
            &["other"],
            t0,
        );
        let e = ev(t0, m0);
        assert_eq!(
            evaluate(&s, Some(&e), "rev", "pc1", t0, m0).unwrap_err(),
            Denied::Revoked
        );
        assert_eq!(
            evaluate(&s, Some(&e), "grp", "pc1", t0, m0).unwrap_err(),
            Denied::NotVisible
        );
        assert_eq!(
            evaluate(&s, Some(&e), "gone", "pc1", t0, m0).unwrap_err(),
            Denied::DefinitionMissing
        );
        let mut member = s.clone();
        member.groups = vec!["staff".into()];
        assert!(evaluate(&member, Some(&e), "grp", "pc1", t0, m0).is_ok());
    }

    #[test]
    fn broker_dependent_jobs_are_refused_even_when_allowed() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let e = ev(t0, m0);
        let s = snap(
            &[(
                "appr",
                manifest_json("appr", OPT_IN, r#""require_approval": true"#),
            )],
            &[],
            &[],
            t0,
        );
        assert_eq!(
            evaluate(&s, Some(&e), "appr", "pc1", t0, m0).unwrap_err(),
            Denied::NeedsBroker
        );
        // Object Store bodies are not locally available either.
        let raw = serde_json::json!({
            "id": "obj", "version": "1.0.0",
            "execute": {"shell": "powershell", "script_object": "k@1", "timeout": "30s"},
            "client": {"name": "N", "category": "c",
                       "offline": {"allow": true, "max_age": "1h"}},
        });
        if let Ok(m) = serde_json::from_value::<Manifest>(raw) {
            let bytes = serde_json::to_vec(&m).unwrap();
            let s = snap(&[("obj", bytes)], &[], &[], t0);
            assert_eq!(
                evaluate(&s, Some(&e), "obj", "pc1", t0, m0).unwrap_err(),
                Denied::NeedsBroker
            );
        }
    }

    #[test]
    fn corrupted_manifest_or_script_is_an_integrity_failure() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let e = ev(t0, m0);
        let mut s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        s.entries[0].raw = s.entries[0].raw.replace("echo hi", "echo evil");
        assert_eq!(
            evaluate(&s, Some(&e), "j", "pc1", t0, m0).unwrap_err(),
            Denied::IntegrityMismatch
        );
        // Digest of raw refreshed but the recorded script digest disagrees.
        let mut s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        s.entries[0].script_sha256 = Some(sha256_hex(b"other"));
        assert_eq!(
            evaluate(&s, Some(&e), "j", "pc1", t0, m0).unwrap_err(),
            Denied::IntegrityMismatch
        );
        let mut s = snap(&[("j", manifest_json("j", OPT_IN, ""))], &[], &[], t0);
        s.entries[0].script_sha256 = None;
        assert_eq!(
            evaluate(&s, Some(&e), "j", "pc1", t0, m0).unwrap_err(),
            Denied::IntegrityMismatch
        );
    }

    #[test]
    fn status_reports_cache_reason_and_live_has_none() {
        let t0 = Utc::now();
        let m0 = Instant::now();
        let s = snap(&[], &[], &[], t0);
        let live = status_for(CacheSource::Live, Some(&s), None, t0, m0);
        assert!(live.exec_unavailable.is_none());
        let cached = status_for(CacheSource::Cache, Some(&s), None, t0, m0);
        assert_eq!(cached.exec_unavailable.unwrap().code, "resync_required");
        assert_eq!(cached.last_sync_at, Some(t0));
        let fresh = status_for(CacheSource::Cache, Some(&s), Some(&ev(t0, m0)), t0, m0);
        assert!(fresh.exec_unavailable.is_none());
        assert_eq!(
            status_for(CacheSource::Cache, None, None, t0, m0)
                .exec_unavailable
                .unwrap()
                .code,
            "resync_required"
        );
    }

    #[test]
    fn store_persists_across_restart_without_restoring_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let m0 = Instant::now();
        let raw = manifest_json("j", OPT_IN, "");
        let (entry, _) = cache_entry("j", &raw).unwrap();
        let store = Store::new(dir.path().join("c"), "broker|pc1".into()).unwrap();
        store
            .commit_catalog(t0, m0, vec![entry], vec![], vec!["g".into()])
            .unwrap();
        let loaded = store.load_catalog().unwrap();
        // Same process: evidence present → runnable.
        assert!(evaluate(&loaded, store.evidence().as_ref(), "j", "pc1", t0, m0).is_ok());

        // "Restart": a new Store over the same directory.
        let again = Store::new(dir.path().join("c"), "broker|pc1".into()).unwrap();
        let loaded = again.load_catalog().expect("browsable after restart");
        assert_eq!(loaded.entries.len(), 1);
        assert!(again.evidence().is_none());
        assert_eq!(
            evaluate(&loaded, again.evidence().as_ref(), "j", "pc1", t0, m0).unwrap_err(),
            Denied::ResyncRequired
        );
        // A different broker/PC scope never sees this cache.
        let other = Store::new(dir.path().join("c"), "broker|pc2".into()).unwrap();
        assert!(other.load_catalog().is_none());
    }

    #[test]
    fn resync_replaces_catalog_so_deletions_and_revocations_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let m0 = Instant::now();
        let store = Store::new(dir.path().join("c"), "s".into()).unwrap();
        let a = cache_entry("a", &manifest_json("a", OPT_IN, "")).unwrap().0;
        let b = cache_entry("b", &manifest_json("b", OPT_IN, "")).unwrap().0;
        store
            .commit_catalog(t0, m0, vec![a.clone(), b], vec![], vec![])
            .unwrap();
        store
            .commit_catalog(t0, m0, vec![a], vec!["a".into()], vec![])
            .unwrap();
        let s = store.load_catalog().unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(
            evaluate(&s, store.evidence().as_ref(), "a", "pc1", t0, m0).unwrap_err(),
            Denied::Revoked
        );
        assert_eq!(
            evaluate(&s, store.evidence().as_ref(), "b", "pc1", t0, m0).unwrap_err(),
            Denied::DefinitionMissing
        );
    }

    #[test]
    fn notifications_are_isolated_per_user_and_capped() {
        use kanade_shared::ipc::notifications::NotificationPriority;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("c"), "s".into()).unwrap();
        let now = Utc::now();
        let mk = |i: usize| Notification {
            id: format!("n{i}"),
            priority: NotificationPriority::Info,
            require_ack: false,
            title: "t".into(),
            body: "b".into(),
            toast: false,
            issued_at: now + CDuration::seconds(i as i64),
            issued_by: None,
            expires_at: None,
            acked_at: None,
            edited_at: None,
            acks_reset_at: None,
        };
        let many: Vec<_> = (0..MAX_NOTIFICATIONS + 10).map(mk).collect();
        let mut acks = HashMap::new();
        acks.insert("n1".to_string(), now);
        store
            .commit_notifications("alice", now, many, acks)
            .unwrap();
        store
            .commit_notifications("bob", now, vec![mk(1)], HashMap::new())
            .unwrap();
        let alice = store.load_notifications("alice").unwrap();
        assert_eq!(alice.items.len(), MAX_NOTIFICATIONS);
        assert!(alice.acks.contains_key("n1"));
        let bob = store.load_notifications("bob").unwrap();
        assert_eq!(bob.items.len(), 1);
        assert!(bob.acks.is_empty());
        assert!(store.load_notifications("carol").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_cache_files_are_ignored() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("c"), "s".into()).unwrap();
        let t0 = Utc::now();
        store
            .commit_catalog(t0, Instant::now(), vec![], vec![], vec![])
            .unwrap();
        let p = store.catalog_path();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o077,
            0,
            "owner-only"
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(store.load_catalog().is_none(), "world-writable refused");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load_catalog().is_some());
        // Symlink at the destination: neither read nor overwritten.
        let target = dir.path().join("elsewhere.json");
        std::fs::copy(&p, &target).unwrap();
        std::fs::remove_file(&p).unwrap();
        symlink(&target, &p).unwrap();
        assert!(store.load_catalog().is_none());
        assert!(
            store
                .commit_catalog(t0, Instant::now(), vec![], vec![], vec![])
                .is_err()
        );
        // Corrupt bytes read as no cache.
        std::fs::remove_file(&p).unwrap();
        std::fs::write(&p, b"{not json").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load_catalog().is_none());
    }
}
