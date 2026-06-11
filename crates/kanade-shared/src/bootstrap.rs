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
use tracing::{info, warn};

use crate::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_FLEET_CONFIG,
    BUCKET_JOBS, BUCKET_JOBS_YAML, BUCKET_SCHEDULES, BUCKET_SCHEDULES_YAML, BUCKET_SCRIPT_CURRENT,
    BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, OBJECT_APP_PACKAGES, OBJECT_RESULT_OUTPUT,
    OBJECT_SCRIPTS, STREAM_AUDIT, STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_OBS_EVENTS,
    STREAM_RESULTS,
};

/// Create-or-update an Object Store, but never let it wedge backend
/// startup. `create_object_store` neither reconciles an existing
/// store's config nor has a `create_or_update` form in async-nats
/// 0.49, so a store whose desired config drifted — e.g. the #518
/// `max_bytes` cap added after the bucket was first created uncapped,
/// which the broker then rejects with error 10058 ("stream name
/// already in use with a different configuration") — would otherwise
/// fail `ensure_jetstream_resources` and crash the backend on boot
/// (production outage 2026-06-11). Fall back to the existing store
/// (uncapped, as it already was) and warn. #506 tracks real
/// reconciliation of object-store config.
async fn ensure_object_store(js: &jetstream::Context, cfg: ObjectStoreConfig) -> Result<()> {
    let name = cfg.bucket.clone();
    if let Err(e) = js.create_object_store(cfg).await {
        // The fallback is deliberately broad — any create error is
        // tolerated AS LONG AS the store already exists, because the
        // alternative is a wedged backend and "never crash on boot"
        // wins over "surface this specific error". The expected error
        // is 10058 (config drift, the incident), but auth/network
        // blips on an already-bootstrapped broker take this path too;
        // they remain visible via the `warn!`. Only a genuine
        // "can't create AND doesn't exist" is fatal.
        if js.get_object_store(&name).await.is_err() {
            return Err(e).with_context(|| {
                format!("create_object_store {name} (and no existing store to fall back to)")
            });
        }
        warn!(
            store = %name, error = %e,
            "object store exists with a different config; using it as-is (cap not reconciled)",
        );
    }
    info!(store = %name, "ready");
    Ok(())
}

/// Idempotently create every NATS JetStream resource the kanade
/// fleet relies on. Calling repeatedly is safe — `create_*` returns
/// the existing resource if it's already configured.
///
/// Returns once every resource is in place. The function is async
/// so backends can `await` it as part of their startup sequence
/// (one round-trip per resource — ~10 RTTs total).
pub async fn ensure_jetstream_resources(js: &jetstream::Context) -> Result<()> {
    // ── Streams ──────────────────────────────────────────────────
    // #518: every stream carries a `max_bytes` cap with
    // `Discard::Old` on top of its `max_age` window. Within their
    // age windows the streams used to be unbounded by size, and
    // JetStream's file store shares a disk with SQLite on the
    // backend host — one job printing 200 KB per run fleet-wide
    // could exhaust the store, at which point EVERY publish fails
    // (results, obs, audit, KV puts). With the caps, worst-case
    // degradation is "shorter history on the offending stream"
    // instead of "broker down".
    //
    // Sizing: JetStream RESERVES each `max_bytes` against its
    // available storage (min of max_file_store and free disk) at
    // create/update time and fails with error 10047 when the sum
    // doesn't fit, so these must stay small enough for modest
    // hosts. That's fine: every stream here is a transport +
    // replay buffer — the durable record is the backend's SQLite
    // (results/inventory/obs/audit are all projected within
    // seconds) — so the caps are runaway-output backstops, not
    // history budgets. Total reservation ≈ 5.3 GiB including the
    // result_output object store below.
    const MIB: i64 = 1024 * 1024;
    const GIB: i64 = 1024 * MIB;

    // INVENTORY — 90-day rolling history (spec §2.3.1).
    js.create_or_update_stream(StreamConfig {
        name: STREAM_INVENTORY.into(),
        subjects: vec!["inventory.>".into()],
        max_age: Duration::from_secs(90 * 24 * 60 * 60),
        max_bytes: GIB,
        discard: DiscardPolicy::Old,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_INVENTORY}"))?;
    info!(stream = STREAM_INVENTORY, "ready");

    // RESULTS — 30-day rolling history. The biggest producer by
    // far (every job run on every PC, with up to 256 KB of inline
    // stdout/stderr per message), so it gets the largest slice of
    // the disk budget.
    js.create_or_update_stream(StreamConfig {
        name: STREAM_RESULTS.into(),
        subjects: vec!["results.>".into()],
        max_age: Duration::from_secs(30 * 24 * 60 * 60),
        max_bytes: 2 * GIB,
        discard: DiscardPolicy::Old,
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
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        // Latest-per-subject keeps this tiny (one Command per
        // group/pc subject); the cap is a backstop against subject
        // cardinality bugs, not a working budget.
        max_bytes: 64 * MIB,
        discard: DiscardPolicy::Old,
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
        max_bytes: 256 * MIB,
        discard: DiscardPolicy::Old,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_stream {STREAM_EVENTS}"))?;
    info!(stream = STREAM_EVENTS, "ready");

    // AUDIT — operator-action record (spec §2.3.1). The DURABLE
    // copy is the backend's SQLite `audit_log` table (the projector
    // INSERTs each message, idempotently since #501; 365-day
    // retention since #486) — the stream is transport + replay
    // buffer, not the archive, so it can be bounded like the rest.
    // 90 days / 512 MiB is far more than the projector ever lags;
    // previously this stream had NO limits at all, making it an
    // unbounded disk leak on the broker host.
    js.create_or_update_stream(StreamConfig {
        name: STREAM_AUDIT.into(),
        subjects: vec!["audit.>".into()],
        max_age: Duration::from_secs(90 * 24 * 60 * 60),
        max_bytes: 512 * MIB,
        discard: DiscardPolicy::Old,
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
        max_bytes: 512 * MIB,
        discard: DiscardPolicy::Old,
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

    // fleet_config — #418 Phase 5 fleet-wide singletons (the global
    // change-freeze under KEY_FREEZE). history: 1 — only the current
    // state matters; both schedulers watch it.
    js.create_or_update_key_value(KvConfig {
        bucket: BUCKET_FLEET_CONFIG.into(),
        history: 1,
        ..Default::default()
    })
    .await
    .with_context(|| format!("create_or_update_key_value {BUCKET_FLEET_CONFIG}"))?;
    info!(bucket = BUCKET_FLEET_CONFIG, "ready");

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
    ensure_object_store(
        js,
        ObjectStoreConfig {
            bucket: OBJECT_AGENT_RELEASES.into(),
            ..Default::default()
        },
    )
    .await?;

    // app_packages — generic operator-uploaded binary distribution
    // (kanade-client today; third-party installers like Webex /
    // Teams once those flows land). Object keys are
    // `<name>/<version>`; see `kanade-shared::kv::OBJECT_APP_PACKAGES`
    // for the full rationale.
    ensure_object_store(
        js,
        ObjectStoreConfig {
            bucket: OBJECT_APP_PACKAGES.into(),
            ..Default::default()
        },
    )
    .await?;

    // scripts — manifest script bodies referenced by
    // `Execute::script_object` (SPEC §2.4.1). Sibling of
    // `app_packages`; see `kanade-shared::kv::OBJECT_SCRIPTS` for
    // the bucket-split rationale (smaller payloads + manifest-
    // coupled lifecycle vs operator-curated installers).
    ensure_object_store(
        js,
        ObjectStoreConfig {
            bucket: OBJECT_SCRIPTS.into(),
            ..Default::default()
        },
    )
    .await?;

    // result_output — overflow stdout / stderr blobs for the
    // `ExecResult` wire kind (#227). Anything larger than the agent's
    // 256 KB inline threshold gets uploaded here under
    // `<request_id>/{stdout,stderr}`; the backend's results
    // projector derefs the pointer fields before INSERT so SQLite
    // + the SPA see the full text inline. 30-day max_age matches
    // STREAM_RESULTS so the lifetimes stay in lockstep — a row still
    // resolvable in execution_results never points at a missing
    // blob.
    // #518: capped like the streams — a job whose output overflows
    // the inline threshold writes blobs HERE instead of
    // STREAM_RESULTS, so without its own cap this store bypasses
    // the stream budget entirely and can still fill the file store.
    // The projector derefs blobs within seconds of publish, so
    // eviction only ever hits already-projected (or expired)
    // output.
    ensure_object_store(
        js,
        ObjectStoreConfig {
            bucket: OBJECT_RESULT_OUTPUT.into(),
            max_age: Duration::from_secs(SECS_PER_DAY * 30),
            max_bytes: GIB,
            ..Default::default()
        },
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Throwaway `nats-server -js` on a random port, like the
    /// kv_cas_live / offline_boot harnesses. Ignored tests only.
    struct Broker {
        js: jetstream::Context,
        _server: tokio::process::Child,
        _storage: tempfile::TempDir,
    }

    async fn spawn_broker() -> Broker {
        let port = portpicker::pick_unused_port().expect("pick port");
        let storage = tempfile::TempDir::new().expect("storage tempdir");
        let server = tokio::process::Command::new("nats-server")
            .arg("-js")
            .arg("-p")
            .arg(port.to_string())
            .arg("-sd")
            .arg(storage.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server (is it in PATH?)");
        let url = format!("nats://127.0.0.1:{port}");
        let mut client = None;
        for _ in 0..50 {
            if let Ok(c) = async_nats::connect(&url).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Broker {
            js: jetstream::new(client.expect("nats-server did not come up in 5s")),
            _server: server,
            _storage: storage,
        }
    }

    /// #506 / 2026-06-11 incident: `create_object_store` neither
    /// reconciles config nor has a create-or-update form, so adding
    /// the #518 `max_bytes` cap to a store first created uncapped made
    /// the broker reject the create (error 10058 "name already in use
    /// with a different configuration") and crashed the backend on
    /// boot. `ensure_object_store` must instead accept the existing
    /// store and let startup proceed.
    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn ensure_object_store_accepts_config_drift() {
        let b = spawn_broker().await;
        // First create: uncapped, as the pre-#518 backend did.
        ensure_object_store(
            &b.js,
            ObjectStoreConfig {
                bucket: "result_output".into(),
                ..Default::default()
            },
        )
        .await
        .expect("fresh create");

        // Second create with a conflicting config (now capped) must
        // NOT error — it accepts the existing store.
        ensure_object_store(
            &b.js,
            ObjectStoreConfig {
                bucket: "result_output".into(),
                max_bytes: 1024 * 1024 * 1024,
                ..Default::default()
            },
        )
        .await
        .expect("config drift must not wedge startup");

        // The store is still usable.
        let store = b.js.get_object_store("result_output").await.expect("store");
        store
            .put("k", &mut &b"hi"[..])
            .await
            .expect("put after drift");
    }

    /// A fresh create with a cap succeeds on a broker with room (the
    /// normal first-boot path).
    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn ensure_object_store_fresh_create_with_cap() {
        let b = spawn_broker().await;
        ensure_object_store(
            &b.js,
            ObjectStoreConfig {
                bucket: "fresh".into(),
                max_bytes: 64 * 1024 * 1024,
                ..Default::default()
            },
        )
        .await
        .expect("fresh capped create");
        b.js.get_object_store("fresh").await.expect("exists");
    }

    /// The fatal path: when create fails for a store that ALSO does
    /// not exist, the error must propagate (we only swallow errors we
    /// can fall back from). An invalid bucket name fails create's
    /// charset validation and never creates a store to fall back to.
    #[tokio::test]
    #[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
    async fn ensure_object_store_propagates_when_no_fallback() {
        let b = spawn_broker().await;
        let err = ensure_object_store(
            &b.js,
            ObjectStoreConfig {
                // Spaces / '!' are rejected by the object-store name
                // rules, so create fails and get also finds nothing.
                bucket: "bad name!".into(),
                ..Default::default()
            },
        )
        .await
        .expect_err("a create failure with no existing store must be fatal");
        assert!(
            err.to_string()
                .contains("no existing store to fall back to"),
            "unexpected error: {err:#}",
        );
    }
}
