//! Idempotent JetStream bootstrap (Sprint 6.x follow-up).
//!
//! Lists every NATS JetStream resource the kanade fleet expects —
//! streams, KV buckets, Object Stores — and asks the broker to
//! create them. `create_*` is a no-op when the resource already
//! exists, so this is safe to call from every backend startup and
//! from the operator-side `kanade jetstream setup` CLI.
//!
//! Centralising the list here means a future "we added a new
//! bucket" change touches one place and both the operator CLI +
//! the auto-bootstrap path pick it up.

use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{
    self,
    kv::Config as KvConfig,
    object_store::Config as ObjectStoreConfig,
    stream::{Config as StreamConfig, DiscardPolicy},
};
use tracing::info;

use crate::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_JOBS, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_RESULTS,
};

/// Idempotently create every NATS JetStream resource the kanade
/// fleet relies on. Calling repeatedly is safe — `create_*` returns
/// the existing resource if it's already configured.
///
/// Returns once every resource is in place. The function is async
/// so backends can `await` it as part of their startup sequence
/// (one round-trip per resource — ~10 RTTs total).
pub async fn ensure_jetstream_resources(js: &jetstream::Context) -> Result<()> {
    // ── Streams ──────────────────────────────────────────────────
    // INVENTORY — 90-day rolling history (spec §2.3.1).
    js.create_stream(StreamConfig {
        name: STREAM_INVENTORY.into(),
        subjects: vec!["inventory.>".into()],
        max_age: Duration::from_secs(90 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_stream {STREAM_INVENTORY}"))?;
    info!(stream = STREAM_INVENTORY, "ready");

    // RESULTS — 30-day rolling history.
    js.create_stream(StreamConfig {
        name: STREAM_RESULTS.into(),
        subjects: vec!["results.>".into()],
        max_age: Duration::from_secs(30 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_stream {STREAM_RESULTS}"))?;
    info!(stream = STREAM_RESULTS, "ready");

    // EXEC — latest-per-subject only (spec §2.6 Layer 1). Carries the
    // `commands.exec.<job_id>` fan-out from `kanade exec` /
    // scheduler fires.
    js.create_stream(StreamConfig {
        name: STREAM_EXEC.into(),
        subjects: vec!["commands.exec.>".into()],
        max_messages_per_subject: 1,
        discard: DiscardPolicy::Old,
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_stream {STREAM_EXEC}"))?;
    info!(stream = STREAM_EXEC, "ready");

    // EVENTS — short-lived broadcast bus for kill / revoke / etc.
    // 7-day window matches the EXEC spec window.
    js.create_stream(StreamConfig {
        name: STREAM_EVENTS.into(),
        subjects: vec!["events.>".into()],
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_stream {STREAM_EVENTS}"))?;
    info!(stream = STREAM_EVENTS, "ready");

    // AUDIT — permanent record of operator actions (spec §2.3.1).
    js.create_stream(StreamConfig {
        name: STREAM_AUDIT.into(),
        subjects: vec!["audit.>".into()],
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_stream {STREAM_AUDIT}"))?;
    info!(stream = STREAM_AUDIT, "ready");

    // ── KV buckets ───────────────────────────────────────────────
    // script_current — cmd_id → version (spec §2.6 Layer 2).
    js.create_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_CURRENT.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_SCRIPT_CURRENT}"))?;
    info!(bucket = BUCKET_SCRIPT_CURRENT, "ready");

    // script_status — cmd_id → ACTIVE / REVOKED.
    js.create_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_STATUS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_SCRIPT_STATUS}"))?;
    info!(bucket = BUCKET_SCRIPT_STATUS, "ready");

    // agents_state — pc_id → latest hw snapshot (history=1).
    js.create_key_value(KvConfig {
        bucket: BUCKET_AGENTS_STATE.into(),
        history: 1,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_AGENTS_STATE}"))?;
    info!(bucket = BUCKET_AGENTS_STATE, "ready");

    // agent_config — Sprint 6 layered scopes (global / groups.* /
    // pcs.*) plus the legacy target_version key.
    js.create_key_value(KvConfig {
        bucket: BUCKET_AGENT_CONFIG.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_AGENT_CONFIG}"))?;
    info!(bucket = BUCKET_AGENT_CONFIG, "ready");

    // agent_groups — Sprint 5 per-pc group membership.
    js.create_key_value(KvConfig {
        bucket: BUCKET_AGENT_GROUPS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_AGENT_GROUPS}"))?;
    info!(bucket = BUCKET_AGENT_GROUPS, "ready");

    // schedules — admin-API CRUD'd cron table (spec §2.5.3).
    // Backend's scheduler.rs also creates this on startup; calling
    // twice is harmless.
    js.create_key_value(KvConfig {
        bucket: BUCKET_SCHEDULES.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_SCHEDULES}"))?;
    info!(bucket = BUCKET_SCHEDULES, "ready");

    // jobs — v0.15 operator-registered Manifest catalog. Schedules
    // reference rows here by id; editing a job rewrites what future
    // schedule fires exec.
    js.create_key_value(KvConfig {
        bucket: BUCKET_JOBS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_key_value {BUCKET_JOBS}"))?;
    info!(bucket = BUCKET_JOBS, "ready");

    // ── Object Store ─────────────────────────────────────────────
    // agent_releases — one object per version, raw exe bytes.
    js.create_object_store(ObjectStoreConfig {
        bucket: OBJECT_AGENT_RELEASES.into(),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_object_store {OBJECT_AGENT_RELEASES}"))?;
    info!(store = OBJECT_AGENT_RELEASES, "ready");

    Ok(())
}
