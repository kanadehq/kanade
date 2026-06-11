//! Self-update watcher (spec §2.10.5). Sprint 6: target_version
//! arrives via the layered agent_config path now, resolved per-pc /
//! per-group / global by the config_supervisor and pushed on a
//! [`tokio::sync::watch`] channel. Whenever that resolved value
//! drifts from `AGENT_VERSION`, the watcher pulls the new binary
//! from the `agent_releases` Object Store, hashes it (SHA-256),
//! atomically swaps it into the running exe's location, and exits
//! — SCM's failure-actions then restart the service on the new
//! binary.
//!
//! The swap is the cross-volume-safe three-step (copy to `<exe>.new`,
//! rename `<exe>` to `<exe>.old`, rename `.new` to `<exe>`) so the
//! window in which the running exe path holds a partially-written file
//! is zero. Cleanup of `.old` / `.new` from any interrupted attempt
//! happens at startup in `main.rs::cleanup_stale_upgrade_artifacts`.
//!
//! `deploy-agent.ps1` is responsible for configuring `sc.exe failure`
//! and `sc.exe failureflag 1` on the service so SCM treats the
//! self-update exit (code 64) as a recoverable failure and restarts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream;
use base64::Engine as _;
use kanade_shared::kv::OBJECT_AGENT_RELEASES;
use kanade_shared::wire::EffectiveConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tracing::{error, info, warn};

/// Persisted across the exit(64) / SCM restart cycle so we can spot a
/// self-update loop: if the agent boots, sees the same `target_version`
/// it just tried to swap to, AND its `running_version` is identical to
/// what was running before the swap (i.e. the swap didn't actually
/// change the embedded `CARGO_PKG_VERSION`), the binary uploaded under
/// that label has a different version baked in than the label claims.
/// Refuse to keep swapping; surface in the log.
///
/// Written under `<data_dir>/last_swap.json`. Discarded once a
/// successful swap (one where `running_version` afterwards matches
/// the target) clears the loop.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct LastSwap {
    target: String,
    running_before: String,
}

pub async fn run(
    client: async_nats::Client,
    pc_id: String,
    running_version: String,
    mut cfg_rx: watch::Receiver<EffectiveConfig>,
    tracker: crate::staleness::Tracker,
) {
    let js = jetstream::new(client.clone());

    // Pre-fix used `match get_object_store ... Err => return;` which
    // permanently killed the self-update subsystem when the bucket
    // wasn't provisioned at boot. Live test found a fleet of agents
    // booted at T0 with no `OBJECT_AGENT_RELEASES` had self-update
    // dead-as-doorknob even after the bucket was provisioned at T1
    // — only an agent restart unstuck them.
    //
    // Retry with backoff. `wait_for_object_store` returns as soon as
    // the bucket is reachable (which the broker reconnect path wakes
    // the tracker for), so recovery is essentially instant once the
    // operator runs `kanade jetstream setup`.
    let store = crate::nats_retry::wait_for_object_store(
        &js,
        &client,
        &tracker,
        OBJECT_AGENT_RELEASES,
        "self_update",
    )
    .await;

    // Boot-time loop check: if last_swap.json says we already tried
    // this target with the same running_version, the binary at that
    // label has a label/version mismatch — refuse to retry.
    let last_swap = read_last_swap();
    if let Some(prev) = &last_swap {
        info!(?prev, "recovered last_swap.json from prior cycle");
        // We're the binary that prior cycle swapped IN — running_version
        // matching the swap target is the definitive "self-update
        // succeeded" signal (vs. is_loop below, where it did NOT change).
        // Surface it on the SPA Events timeline so a rollout's progress
        // is observable per-PC. Durable obs-outbox: spawn_drain ships it.
        if prev.target == running_version && prev.running_before != running_version {
            emit_update_event(&pc_id, &prev.running_before, &running_version);
            // Clear the marker NOW: a crash/restart before the normal
            // clearance further down would re-emit a duplicate timeline
            // event on next boot (the jitter sleep ahead of an already-
            // queued next rollout can hold that window open for minutes).
            // The loop-detector doesn't need it anymore — success means
            // running_version DID change.
            clear_last_swap();
        }
    }

    // Initial check against whatever the supervisor's first push
    // (its initial_sync) populated.
    let (mut current_target, jitter) = {
        let cfg = cfg_rx.borrow();
        (
            cfg.target_version.clone(),
            cfg.target_version_jitter_duration(),
        )
    };
    let mut loop_blocked_target: Option<String> = None;
    if let Some(target) = current_target.as_deref()
        && target != running_version
    {
        if is_quarantined(target) {
            warn!(
                target,
                "self-update: target is quarantined (it crash-looped on a prior boot and was \
                 rolled back). Refusing to re-deploy it — this is what stops a bad rollout from \
                 looping rollout↔rollback. Republish a fixed binary under a new version, or clear \
                 the quarantine.",
            );
        } else if is_loop(&last_swap, target, &running_version) {
            loop_blocked_target = Some(target.to_string());
            warn!(
                target,
                running = %running_version,
                "self-update LOOP detected — previous swap to this target produced the same running_version. \
                 Refusing to swap again. The binary under this label has a label/version mismatch; \
                 republish it or clear target_version (`kanade config unset target_version`)."
            );
        } else {
            sleep_jitter(jitter).await;
            if let Err(e) = attempt_swap(&store, target, &running_version).await {
                warn!(error = %e, target, "initial self-update fetch failed");
            }
        }
    } else if last_swap.is_some() {
        // We're past a loop: clear the marker so a future legit
        // rollout to a same-named target isn't falsely blocked.
        clear_last_swap();
    }

    // React to every supervisor push; trigger only when
    // target_version actually changed (cadence-only updates land
    // here too and should be ignored).
    loop {
        if cfg_rx.changed().await.is_err() {
            return;
        }
        let (new_target, jitter) = {
            let cfg = cfg_rx.borrow();
            (
                cfg.target_version.clone(),
                cfg.target_version_jitter_duration(),
            )
        };
        if new_target == current_target {
            continue;
        }
        current_target = new_target.clone();

        // Any target_version change clears a previous loop block —
        // a new operator action means a fresh attempt is in order.
        if loop_blocked_target.is_some() && loop_blocked_target.as_deref() != new_target.as_deref()
        {
            info!("target_version changed; clearing loop block");
            loop_blocked_target = None;
            clear_last_swap();
        }

        if let Some(target) = new_target.as_deref()
            && target != running_version
        {
            if loop_blocked_target.as_deref() == Some(target) {
                warn!(target, "still loop-blocked on this target; ignoring");
                continue;
            }
            if is_quarantined(target) {
                warn!(
                    target,
                    "self-update: target is quarantined (crash-looped on a prior boot); refusing \
                     to re-deploy. Republish a fixed version or clear the quarantine.",
                );
                continue;
            }
            sleep_jitter(jitter).await;
            if let Err(e) = attempt_swap(&store, target, &running_version).await {
                warn!(error = %e, target, "self-update fetch failed");
            }
        }
    }
}

/// Best-effort "agent self-updated from→to" ObsEvent, enqueued to the
/// durable obs-outbox (the drain task ships it to the OBS stream, the
/// backend projects it, the SPA Events page shows it under the
/// `agent_update` kind). Failures only warn — observability must never
/// block the update path.
fn emit_update_event(pc_id: &str, from: &str, to: &str) {
    let event = kanade_shared::wire::ObsEvent {
        pc_id: pc_id.to_string(),
        at: chrono::Utc::now(),
        kind: "agent_update".to_string(),
        source: "agent:self_update".to_string(),
        // UUID, not a from→to pair: the same upgrade path can legally
        // repeat (downgrade + retry) and must show up again.
        event_record_id: Some(format!("self_update_{}", uuid::Uuid::new_v4().as_simple())),
        payload: serde_json::json!({ "from": from, "to": to }),
    };
    let dir = kanade_shared::default_paths::data_dir().join("obs-outbox");
    let res = crate::obs_outbox::ensure_outbox_dir(&dir)
        .and_then(|()| crate::obs_outbox::enqueue(&dir, &event).map(|_| ()));
    match res {
        Ok(()) => info!(from, to, "queued agent_update obs event"),
        Err(e) => warn!(error = %e, from, to, "failed to queue agent_update obs event"),
    }
}

fn is_loop(last: &Option<LastSwap>, target: &str, running: &str) -> bool {
    last.as_ref()
        .map(|p| p.target == target && p.running_before == running)
        .unwrap_or(false)
}

/// #582: true if `target` was rolled back after a failed boot (it
/// crash-looped). The boot sentinel quarantines such versions; the
/// self-update path must refuse to re-deploy them, otherwise a bad
/// rollout target loops rollout → crash → rollback → rollout forever.
/// Cleared automatically when the operator pushes a different (fixed)
/// version, or explicitly via `clear_quarantine`.
fn is_quarantined(target: &str) -> bool {
    use kanade_shared::boot_sentinel::BootSentinel;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // The sentinel's version field is irrelevant here (is_quarantined
    // only reads the quarantine list), so the package version suffices.
    BootSentinel::new(
        &kanade_shared::default_paths::data_dir(),
        exe,
        env!("CARGO_PKG_VERSION"),
    )
    .is_quarantined(target)
}

fn last_swap_path() -> Option<PathBuf> {
    use kanade_shared::default_paths;
    Some(default_paths::data_dir().join("last_swap.json"))
}

fn read_last_swap() -> Option<LastSwap> {
    let path = last_swap_path()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_last_swap(target: &str, running_before: &str) {
    let Some(path) = last_swap_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = LastSwap {
        target: target.to_string(),
        running_before: running_before.to_string(),
    };
    match serde_json::to_vec(&payload) {
        Ok(b) => {
            if let Err(e) = std::fs::write(&path, b) {
                warn!(error = %e, ?path, "write last_swap.json");
            }
        }
        Err(e) => warn!(error = %e, "encode last_swap.json"),
    }
}

fn clear_last_swap() {
    if let Some(path) = last_swap_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// #489: bounded download retry. `last_swap.json` is no longer
/// written here — it used to be recorded BEFORE the download, so a
/// transient fetch failure (broker blip mid-rollout, exactly when
/// thousands of agents pull at once) left a marker claiming a swap
/// happened when none did. Two consequences: the in-process watch
/// loop never retried (same `target` ⇒ skipped), and the next boot's
/// `is_loop()` saw `prev.target == target && running unchanged` and
/// permanently refused the target — a one-off network error stranded
/// the agent on the old version until an operator changed
/// target_version. The marker now gets written inside
/// `swap_and_restart`, after the renames succeed (the only point
/// where "we swapped to this target" is actually true).
///
/// The retry is deliberately small (3 attempts, 15 s / 45 s gaps):
/// it self-heals blips, while a long outage is left to the next
/// config push or agent restart (whose initial check re-attempts).
async fn attempt_swap(
    store: &jetstream::object_store::ObjectStore,
    target: &str,
    running: &str,
) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    let mut delay = Duration::from_secs(15);
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match maybe_download(store, target, running).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(
                    attempt,
                    max_attempts = ATTEMPTS,
                    target,
                    error = ?e,
                    "self-update download attempt failed",
                );
                last_err = Some(e);
                if attempt < ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay *= 3;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

/// Random pause in `0..=max` before the download fires. The point is
/// to de-synchronise a fleet-wide rollout — `kanade agent rollout
/// <v> --global` fans the same KV update out to every agent within
/// milliseconds, and without jitter every agent would hit the Object
/// Store at the same instant. `max == 0` means "fire now" (default
/// for the empty-fleet / dev case and for canary smoke tests).
async fn sleep_jitter(max: Duration) {
    if max.is_zero() {
        return;
    }
    let secs = max.as_secs();
    let pick = if secs == 0 {
        0
    } else {
        use rand::RngExt;
        rand::rng().random_range(0..=secs)
    };
    info!(
        jitter_max_secs = secs,
        sleep_secs = pick,
        "self-update jitter — pausing before download"
    );
    tokio::time::sleep(Duration::from_secs(pick)).await;
}

async fn maybe_download(
    store: &jetstream::object_store::ObjectStore,
    target: &str,
    running: &str,
) -> Result<()> {
    if target == running {
        info!(target, "target_version matches running — no self-update");
        return Ok(());
    }
    info!(
        target,
        running, "target_version drift — downloading new binary"
    );

    let mut object = store
        .get(target)
        .await
        .with_context(|| format!("object store get '{target}'"))?;

    let staging = staging_path(target)?;
    if let Some(parent) = staging.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut file = tokio::fs::File::create(&staging)
        .await
        .with_context(|| format!("create {staging:?}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut object, &mut buf)
            .await
            .context("read object chunk")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .await
            .context("write staged exe")?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    // The staged bytes are about to become the service binary —
    // fsync before the digest gate / swap so a power-cut can't leave
    // a torn file that the rename then promotes (review PR #546:
    // flush() is a no-op on the unbuffered tokio File and its error
    // was swallowed anyway).
    file.sync_all().await.context("sync staged exe")?;
    drop(file);
    let digest = hasher.finalize();

    // #490: verify the staged bytes against the Object Store's own
    // recorded digest (`SHA-256=<base64url>`) before swapping it into
    // the service binary path. This catches transfer truncation /
    // corruption — the previous code computed the hash but only logged
    // it. (Authenticity — a publisher-signed expected hash — is a
    // separate concern and out of scope here; the store digest is
    // still integrity, not trust.)
    if let Some(expected) = object.info.digest.as_deref() {
        if !digest_matches(expected, digest.as_slice()) {
            let _ = tokio::fs::remove_file(&staging).await;
            let actual = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
            anyhow::bail!(
                "staged binary digest mismatch for '{target}': object store records {expected}, downloaded bytes hash to SHA-256={actual} — discarding staged file"
            );
        }
    } else {
        warn!(
            target,
            "object store entry carries no digest; proceeding without verification"
        );
    }

    info!(
        target,
        path = ?staging,
        bytes = total,
        sha256 = %hex(&digest),
        "staged new agent binary (digest verified) — beginning atomic swap",
    );

    swap_and_restart(&staging, target, running).await?;
    // Unreachable: swap_and_restart calls std::process::exit on success.
    Ok(())
}

/// Replace the running exe with the staged one and exit so SCM's
/// failure-actions can restart the service on the new binary.
///
/// Sequence (cross-volume safe: staging is under `%ProgramData%`,
/// the running exe under `%ProgramFiles%`):
///   1. Copy `<staged>` to `<exe>.new` in the exe's directory.
///   2. Rename `<exe>` to `<exe>.old`. Allowed even though the file
///      is mapped — Windows blocks delete-while-loaded, not rename.
///   3. Rename `<exe>.new` to `<exe>` — atomic within the same dir.
///   4. `std::process::exit(64)`. With `sc.exe failureflag <svc> 1`
///      configured on the service, SCM treats this as a recoverable
///      failure and applies the configured restart action.
///
/// Startup-time cleanup of `<exe>.old` lives in `main.rs` so the
/// stale binary doesn't accumulate.
async fn swap_and_restart(staged: &Path, target_version: &str, running: &str) -> Result<()> {
    let current = std::env::current_exe().context("current_exe")?;
    let exe_dir = current
        .parent()
        .context("current_exe has no parent directory")?;
    let exe_name = current
        .file_name()
        .and_then(|s| s.to_str())
        .context("current_exe has no UTF-8 file name")?
        .to_string();
    let new_path = exe_dir.join(format!("{exe_name}.new"));
    let old_path = exe_dir.join(format!("{exe_name}.old"));

    // Tidy any leftover .new / .old from a previous interrupted run
    // so the renames below always have a clean target.
    let _ = tokio::fs::remove_file(&new_path).await;
    let _ = tokio::fs::remove_file(&old_path).await;

    tokio::fs::copy(staged, &new_path)
        .await
        .with_context(|| format!("copy {staged:?} -> {new_path:?}"))?;

    tokio::fs::rename(&current, &old_path)
        .await
        .with_context(|| format!("rename {current:?} -> {old_path:?}"))?;
    if let Err(e) = tokio::fs::rename(&new_path, &current).await {
        // #490: compensating rollback. At this point the service
        // binary path is EMPTY — if we bail here without restoring,
        // the next service start (reboot, SCM failure action,
        // operator stop/start) finds no exe and the endpoint is
        // permanently agent-less until manual repair. Put the
        // original back; a transient lock on `.new` (AV scan) then
        // degrades to "this update attempt failed", not a brick.
        match tokio::fs::rename(&old_path, &current).await {
            Ok(()) => warn!(
                error = %e,
                "second rename failed; rolled the original exe back into place",
            ),
            Err(restore_err) => error!(
                error = %e,
                restore_error = %restore_err,
                exe = ?current,
                backup = ?old_path,
                "second rename failed AND rollback failed — service binary path is empty; \
                 manual repair required (rename the .old file back)",
            ),
        }
        return Err(e).with_context(|| format!("rename {new_path:?} -> {current:?}"));
    }

    // #489: record the loop-detection marker only now — after both
    // renames succeeded — so it can never claim a swap that didn't
    // happen. (A crash in the microseconds before this write loses
    // only the success obs-event / loop marker, which is safe; the
    // old placement before the download falsely loop-blocked targets
    // on transient fetch failures.)
    write_last_swap(target_version, running);

    // #582: arm the boot sentinel. `old_path` is the outgoing binary
    // that just booted fine — it's the rollback target if `target_version`
    // crash-loops. Writes the sentinel so the next boot is gated.
    {
        use kanade_shared::boot_sentinel::BootSentinel;
        // The `version` arg here is irrelevant: `arm_for_swap` writes a
        // sentinel for `target_version` and never reads `self.version`.
        // Pass `running` (the outgoing version) for honesty.
        let sentinel = BootSentinel::new(
            &kanade_shared::default_paths::data_dir(),
            current.clone(),
            running,
        );
        if let Err(e) = sentinel.arm_for_swap(&old_path, target_version) {
            warn!(
                error = %e, target = target_version,
                "boot sentinel: arm_for_swap failed — crash-loop rollback disabled for this swap",
            );
        }
    }

    info!(
        target = target_version,
        replaced = ?current,
        backup   = ?old_path,
        "swap complete — exiting (code 64); SCM failure-actions take over",
    );

    // Let the tracing subscriber flush its buffer before SCM kills us.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    std::process::exit(64);
}

fn staging_path(version: &str) -> Result<PathBuf> {
    use kanade_shared::default_paths;
    let exe = std::env::current_exe().context("current_exe")?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kanade-agent")
        .to_string();
    // Spec §2.11.3 — staged binaries live in the data dir, never next
    // to the running exe (Program Files is read-only for LocalSystem
    // services after MSI install).
    Ok(default_paths::data_dir()
        .join("staging")
        .join(format!("{stem}.{version}.staged")))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Does the Object Store's recorded `SHA-256=<base64url>` digest match
/// the freshly-hashed staged bytes? The algorithm prefix is accepted in
/// either of the two casings NATS emits (`SHA-256=` / `sha-256=`); the
/// payload is compared as decoded *bytes* so a base64 padding difference
/// (NATS records WITH `=`, we'd encode without) or a url-safe/standard
/// alphabet split can't trigger a false mismatch — the string-compare
/// that #546 shipped wedged every self-update from 0.43.46 on for
/// exactly that padding reason. A malformed / undecodable recorded
/// digest fails closed (no match).
fn digest_matches(expected: &str, actual: &[u8]) -> bool {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    expected
        .strip_prefix("SHA-256=")
        .or_else(|| expected.strip_prefix("sha-256="))
        .and_then(|b64| {
            // NATS records url-safe-with-padding today; trim the pad and
            // try url-safe first, then the standard alphabet, so neither
            // a padding nor an alphabet difference can false-mismatch.
            let payload = b64.trim_end_matches('=');
            URL_SAFE_NO_PAD
                .decode(payload)
                .or_else(|_| STANDARD_NO_PAD.decode(payload))
                .ok()
        })
        .as_deref()
        == Some(actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};

    // A 32-byte SHA-256 digest with a high bit set so its base64
    // encoding exercises a url-safe character (`-`/`_`).
    const DIGEST: [u8; 32] = [
        0x21, 0x3e, 0x9b, 0xbd, 0xfc, 0x8e, 0x5c, 0x44, 0x6d, 0x51, 0x44, 0x24, 0xd0, 0xfe, 0xd3,
        0x98, 0x63, 0x24, 0xd7, 0xa0, 0xaa, 0x9e, 0x9a, 0x0c, 0xf8, 0x68, 0x71, 0x91, 0x1a, 0xc4,
        0xd2, 0x1f,
    ];

    #[test]
    fn matches_padded_url_safe_digest() {
        // The shape NATS actually records: url-safe, WITH `=` padding —
        // the exact case that wedged self-update (regression guard).
        let recorded = format!("SHA-256={}", URL_SAFE.encode(DIGEST));
        assert!(recorded.ends_with('='), "fixture must carry padding");
        assert!(digest_matches(&recorded, &DIGEST));
    }

    #[test]
    fn matches_unpadded_and_standard_alphabet() {
        // No-pad url-safe and padded standard-alphabet both decode to
        // the same bytes — all accepted.
        assert!(digest_matches(
            &format!("SHA-256={}", URL_SAFE_NO_PAD.encode(DIGEST)),
            &DIGEST
        ));
        assert!(digest_matches(
            &format!("SHA-256={}", STANDARD.encode(DIGEST)),
            &DIGEST
        ));
    }

    #[test]
    fn prefix_is_case_insensitive() {
        assert!(digest_matches(
            &format!("sha-256={}", URL_SAFE.encode(DIGEST)),
            &DIGEST
        ));
    }

    #[test]
    fn rejects_wrong_bytes_missing_prefix_and_garbage() {
        // A genuinely different digest still fails (integrity preserved).
        let mut other = DIGEST;
        other[0] ^= 0xff;
        assert!(!digest_matches(
            &format!("SHA-256={}", URL_SAFE.encode(DIGEST)),
            &other
        ));
        // No `SHA-256=` prefix → fail closed.
        assert!(!digest_matches(&URL_SAFE.encode(DIGEST), &DIGEST));
        // Undecodable payload → fail closed.
        assert!(!digest_matches("SHA-256=not*valid*base64", &DIGEST));
    }
}
