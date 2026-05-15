//! Self-update watcher (spec §2.10.5). Listens to
//! `agent_config.target_version`. When it drifts from `AGENT_VERSION`
//! the watcher pulls the new binary from the `agent_releases` Object
//! Store, hashes it (SHA-256), and stages it next to the running exe.
//!
//! Sprint 4d MVP: download + hash + log "restart pending". The actual
//! exe swap + Windows-Service restart lands in a follow-up because a
//! running .exe can't be safely deleted on Windows — a clean
//! supervisor-driven restart belongs in the windows-service crate
//! wiring, not inside the long-lived agent task.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_nats::jetstream;
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, KEY_AGENT_TARGET_VERSION, OBJECT_AGENT_RELEASES};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

pub async fn run(client: async_nats::Client, running_version: String) {
    let js = jetstream::new(client);

    let kv = match js.get_key_value(BUCKET_AGENT_CONFIG).await {
        Ok(k) => k,
        Err(_) => {
            info!(
                bucket = BUCKET_AGENT_CONFIG,
                "agent_config KV missing — self-update idle"
            );
            return;
        }
    };
    let store = match js.get_object_store(OBJECT_AGENT_RELEASES).await {
        Ok(s) => s,
        Err(_) => {
            info!(
                store = OBJECT_AGENT_RELEASES,
                "agent_releases Object Store missing — self-update idle"
            );
            return;
        }
    };

    // 1. Initial sync — if the broadcast version is already different
    //    from the running version, pull straight away.
    if let Ok(Some(b)) = kv.get(KEY_AGENT_TARGET_VERSION).await {
        let target = String::from_utf8_lossy(&b).to_string();
        if let Err(e) = maybe_download(&store, &target, &running_version).await {
            warn!(error = %e, target, "initial self-update fetch failed");
        }
    }

    // 2. Watch — react to every flip of target_version.
    let mut watcher = match kv.watch(KEY_AGENT_TARGET_VERSION).await {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "kv watch target_version");
            return;
        }
    };
    while let Some(entry) = watcher.next().await {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "self-update watch entry");
                continue;
            }
        };
        let target = String::from_utf8_lossy(&entry.value).to_string();
        if let Err(e) = maybe_download(&store, &target, &running_version).await {
            warn!(error = %e, target, "self-update fetch failed");
        }
    }
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
    file.flush().await.ok();
    let digest = hasher.finalize();
    info!(
        target,
        path = ?staging,
        bytes = total,
        sha256 = %hex(&digest),
        "staged new agent binary — restart pending (windows-service supervisor swap is the next step)",
    );
    Ok(())
}

fn staging_path(version: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().context("exe parent")?.to_path_buf();
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kanade-agent");
    Ok(dir.join(format!("{stem}.{version}.staged")))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
