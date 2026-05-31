//! Idempotent JetStream bootstrap (Sprint 6.x follow-up).
//!
//! Lists every NATS JetStream resource the kanade fleet expects —
//! streams, KV buckets, Object Stores — and asks the broker to
//! create-or-update them. v0.25.0 switched from `create_*` to
//! `create_or_update_*`: the old form returned error 10058 ("name
//! already in use with a different configuration") when a release
//! widened a stream's subjects or changed its retention policy on
//! a broker that still held the older config. With the new form the
//! broker reconciles its definition to the one in this file, so
//! version bumps no longer require operator-side data wipes.
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
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_JOBS, BUCKET_JOBS_YAML,
    BUCKET_SCHEDULES, BUCKET_SCHEDULES_YAML, BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS,
    OBJECT_AGENT_RELEASES, OBJECT_APP_PACKAGES, OBJECT_RESULT_OUTPUT, OBJECT_SCRIPTS, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_OBS_EVENTS, STREAM_RESULTS,
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
    js.create_or_update_stream(StreamConfig {
        name: STREAM_INVENTORY.into(),
        subjects: vec!["inventory.>".into()],
        max_age: Duration::from_secs(90 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_INVENTORY}"))?;
    info!(stream = STREAM_INVENTORY, "ready");

    // RESULTS — 30-day rolling history.
    js.create_or_update_stream(StreamConfig {
        name: STREAM_RESULTS.into(),
        subjects: vec!["results.>".into()],
        max_age: Duration::from_secs(30 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_RESULTS}"))?;
    info!(stream = STREAM_RESULTS, "ready");

    // EXEC — latest-per-subject only (spec §2.6 Layer 1). v0.22.1:
    // catch the existing `commands.{all,group.X,pc.Y}` subjects so a
    // single backend publish lands in BOTH the agent's live core
    // subscription AND the stream's retention store. Reconnecting
    // agents catch up via a durable consumer with
    // `DeliverPolicy::LastPerSubject` — they receive the most
    // recent Command per subject they care about, no matter how
    // long they were offline (within `max_age`).
    js.create_or_update_stream(StreamConfig {
        name: STREAM_EXEC.into(),
        subjects: vec!["commands.>".into()],
        max_messages_per_subject: 1,
        discard: DiscardPolicy::Old,
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_EXEC}"))?;
    info!(stream = STREAM_EXEC, "ready");

    // EVENTS — short-lived broadcast bus for kill / revoke / etc.
    // 7-day window matches the EXEC spec window.
    js.create_or_update_stream(StreamConfig {
        name: STREAM_EVENTS.into(),
        subjects: vec!["events.>".into()],
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_EVENTS}"))?;
    info!(stream = STREAM_EVENTS, "ready");

    // AUDIT — permanent record of operator actions (spec §2.3.1).
    js.create_or_update_stream(StreamConfig {
        name: STREAM_AUDIT.into(),
        subjects: vec!["audit.>".into()],
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_AUDIT}"))?;
    info!(stream = STREAM_AUDIT, "ready");

    // OBS_EVENTS — per-PC observability timeline (Issue #246). The
    // 90-day window matches `obs_events` table retention so a
    // backend bootstrapping after long downtime can catch up but
    // doesn't carry data the table will discard anyway. Subject
    // filter `obs.>` catches every PC without a per-PC subscription.
    //
    // Days-to-seconds is spelt out once instead of `90 * 24 * 60 *
    // 60` open-coded across bootstrap + cleanup; the matching prune
    // window in `kanade-backend::cleanup` quotes the same number
    // separately (SQLite-relative string syntax there, not a
    // duration), so it can't share a constant — but a single
    // arithmetic spell-out here makes the relationship grep-able.
    const SECS_PER_DAY: u64 = 24 * 60 * 60;
    const OBS_EVENTS_RETENTION_DAYS: u64 = 90;
    js.create_or_update_stream(StreamConfig {
        name: STREAM_OBS_EVENTS.into(),
        subjects: vec!["obs.>".into()],
        max_age: Duration::from_secs(OBS_EVENTS_RETENTION_DAYS * SECS_PER_DAY),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_OBS_EVENTS}"))?;
    info!(stream = STREAM_OBS_EVENTS, "ready");

    // ── KV buckets ───────────────────────────────────────────────
    // script_current — cmd_id → version (spec §2.6 Layer 2).
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_CURRENT.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_SCRIPT_CURRENT}"))?;
    info!(bucket = BUCKET_SCRIPT_CURRENT, "ready");

    // script_status — cmd_id → ACTIVE / REVOKED.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_SCRIPT_STATUS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_SCRIPT_STATUS}"))?;
    info!(bucket = BUCKET_SCRIPT_STATUS, "ready");

    // agents_state — pc_id → latest hw snapshot (history=1).
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_AGENTS_STATE.into(),
        history: 1,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_AGENTS_STATE}"))?;
    info!(bucket = BUCKET_AGENTS_STATE, "ready");

    // agent_config — Sprint 6 layered scopes (global / groups.* /
    // pcs.*) plus the legacy target_version key.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_AGENT_CONFIG.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_AGENT_CONFIG}"))?;
    info!(bucket = BUCKET_AGENT_CONFIG, "ready");

    // agent_groups — Sprint 5 per-pc group membership.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_AGENT_GROUPS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_AGENT_GROUPS}"))?;
    info!(bucket = BUCKET_AGENT_GROUPS, "ready");

    // schedules — admin-API CRUD'd cron table (spec §2.5.3).
    // Backend's scheduler.rs also creates this on startup; calling
    // twice is harmless.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_SCHEDULES.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_SCHEDULES}"))?;
    info!(bucket = BUCKET_SCHEDULES, "ready");

    // jobs — v0.15 operator-registered Manifest catalog. Schedules
    // reference rows here by id; editing a job rewrites what future
    // schedule fires exec.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_JOBS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_JOBS}"))?;
    info!(bucket = BUCKET_JOBS, "ready");

    // jobs_yaml / schedules_yaml — operator source-of-truth YAML
    // alongside the JSON catalogs above. Same key shape (manifest id
    // / schedule id), but the value is the raw YAML bytes so the
    // SPA's YAML editor preserves comments + script block-scalar
    // indentation across edits. Agents/scheduler don't read these.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_JOBS_YAML.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_JOBS_YAML}"))?;
    info!(bucket = BUCKET_JOBS_YAML, "ready");

    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_SCHEDULES_YAML.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_SCHEDULES_YAML}"))?;
    info!(bucket = BUCKET_SCHEDULES_YAML, "ready");

    // ── Object Store ─────────────────────────────────────────────
    // agent_releases — one object per version, raw exe bytes.
    js.create_object_store(ObjectStoreConfig {
        bucket: OBJECT_AGENT_RELEASES.into(),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_object_store {OBJECT_AGENT_RELEASES}"))?;
    info!(store = OBJECT_AGENT_RELEASES, "ready");

    // app_packages — generic operator-uploaded binary distribution
    // (kanade-client today; third-party installers like Webex /
    // Teams once those flows land). Object keys are
    // `<name>/<version>`; see `kanade-shared::kv::OBJECT_APP_PACKAGES`
    // for the full rationale.
    js.create_object_store(ObjectStoreConfig {
        bucket: OBJECT_APP_PACKAGES.into(),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_object_store {OBJECT_APP_PACKAGES}"))?;
    info!(store = OBJECT_APP_PACKAGES, "ready");

    // scripts — manifest script bodies referenced by
    // `Execute::script_object` (SPEC §2.4.1). Sibling of
    // `app_packages`; see `kanade-shared::kv::OBJECT_SCRIPTS` for
    // the bucket-split rationale (smaller payloads + manifest-
    // coupled lifecycle vs operator-curated installers).
    js.create_object_store(ObjectStoreConfig {
        bucket: OBJECT_SCRIPTS.into(),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_object_store {OBJECT_SCRIPTS}"))?;
    info!(store = OBJECT_SCRIPTS, "ready");

    // result_output — overflow stdout / stderr blobs for the
    // `ExecResult` wire kind (#227). Anything larger than the agent's
    // 256 KB inline threshold gets uploaded here under
    // `<request_id>/{stdout,stderr}`; the backend's results
    // projector derefs the pointer fields before INSERT so SQLite
    // + the SPA see the full text inline. 30-day max_age matches
    // STREAM_RESULTS so the lifetimes stay in lockstep — a row still
    // resolvable in execution_results never points at a missing
    // blob.
    js.create_object_store(ObjectStoreConfig {
        bucket: OBJECT_RESULT_OUTPUT.into(),
        max_age: Duration::from_secs(SECS_PER_DAY * 30),
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_object_store {OBJECT_RESULT_OUTPUT}"))?;
    info!(store = OBJECT_RESULT_OUTPUT, "ready");

    Ok(())
}
