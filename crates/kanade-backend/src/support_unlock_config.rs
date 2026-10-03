//! Keeps the agent-readable support-code projection in step with
//! `server_settings`.
//!
//! Agents verify typed support codes locally against argon2id hashes, but the
//! `server_settings` document that stores them also holds secrets (the
//! installer's NATS token, SMTP relay settings) which are in clear at the KV
//! layer. So the backend republishes just the support codes to
//! `fleet_config` / [`KEY_SUPPORT_CODES`] and agents read that instead.
//! `server_settings` itself is left untouched: agents that predate the
//! projection still read it.
//!
//! The projection is always *derived from a fresh read* of the settings
//! document, never from whatever copy the caller happens to hold. A stale
//! copy written back would undo a concurrent revoke or disable.
//!
//! An empty code list is written as an empty projection and the key is never
//! deleted: its presence is what tells an upgraded agent the backend has
//! reconciled, so it can stop consulting the legacy document.

use std::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::Context;
use async_nats::jetstream::kv::{Operation, Store};
use kanade_shared::kv::{
    BUCKET_FLEET_CONFIG, BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS, KEY_SUPPORT_CODES,
};
use kanade_shared::wire::{ServerSettings, SupportCodesProjection};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// How often the projection is re-derived regardless of API activity. Bounds
/// how long a revoked code can survive on agents after a sync that failed
/// (broker blip) or after a hand edit of the settings document.
const RESYNC_INTERVAL: Duration = Duration::from_secs(300);

/// Bound on re-read rounds when the projection key moves under us.
const MAX_ATTEMPTS: usize = 8;

/// Serialises syncs within this process so two settings changes cannot
/// interleave their read-derive-write rounds. Across replicas the revision
/// guard on the projection key is what converges them.
static SYNC_LOCK: Mutex<()> = Mutex::const_new(());

/// What a sync should do given the current settings document and projection.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Already correct.
    Unchanged,
    /// Write this body.
    Write(Vec<u8>),
}

/// Decide what to write. `settings` is the raw `server_settings/current`
/// value (`None` ⇒ never configured ⇒ no codes); `current` is the raw
/// projection value (`None` ⇒ key absent). A settings document that does not
/// decode is an error: publishing an empty projection over it would silently
/// revoke every code on every agent.
fn plan(settings: Option<&[u8]>, current: Option<&[u8]>) -> anyhow::Result<Plan> {
    let settings: ServerSettings = match settings {
        Some(bytes) => serde_json::from_slice(bytes).context("decode server_settings")?,
        None => ServerSettings::default(),
    };
    let desired = SupportCodesProjection::from_settings(&settings);
    // Compare decoded values, not bytes, so formatting never causes a write;
    // an undecodable current value is simply rewritten.
    if let Some(cur) = current
        && serde_json::from_slice::<SupportCodesProjection>(cur).is_ok_and(|c| c == desired)
    {
        return Ok(Plan::Unchanged);
    }
    Ok(Plan::Write(
        serde_json::to_vec(&desired).context("encode support codes projection")?,
    ))
}

async fn live_bytes(kv: &Store, key: &str) -> anyhow::Result<(Option<Vec<u8>>, Option<u64>)> {
    match kv
        .entry(key)
        .await
        .with_context(|| format!("kv entry '{key}'"))?
    {
        Some(e) if e.operation == Operation::Put => Ok((Some(e.value.to_vec()), Some(e.revision))),
        // A delete marker's revision still guards the CAS.
        Some(e) => Ok((None, Some(e.revision))),
        None => Ok((None, None)),
    }
}

/// Re-derive the projection from the current `server_settings` and publish it
/// if it differs. Returns whether a write happened.
pub async fn sync_support_codes_projection(js: &Context) -> anyhow::Result<bool> {
    let _guard = SYNC_LOCK.lock().await;
    let settings_kv = js
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .context("open server_settings bucket")?;
    let fleet_kv = js
        .get_key_value(BUCKET_FLEET_CONFIG)
        .await
        .context("open fleet_config bucket")?;

    let mut last_err = None;
    for _ in 0..MAX_ATTEMPTS {
        // Revision first, settings second: if the settings change in between,
        // the next sync (which that change triggers) corrects it.
        let (current, revision) = live_bytes(&fleet_kv, KEY_SUPPORT_CODES).await?;
        let (settings, _) = live_bytes(&settings_kv, KEY_SERVER_SETTINGS).await?;
        let body = match plan(settings.as_deref(), current.as_deref())? {
            Plan::Unchanged => return Ok(false),
            Plan::Write(b) => b,
        };
        let res = match revision {
            Some(rev) => fleet_kv
                .update(KEY_SUPPORT_CODES, body.into(), rev)
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
            None => fleet_kv
                .create(KEY_SUPPORT_CODES, body.into())
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
        };
        match res {
            Ok(()) => {
                info!("support code projection published");
                return Ok(true);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .expect("loop ran at least once")
        .context("publish support codes projection"))
}

/// Sync and log failure instead of propagating it. Used where the caller's
/// own outcome must not change (the settings API) and at boot; the periodic
/// task retries.
pub async fn sync_or_warn(js: &Context) {
    if let Err(e) = sync_support_codes_projection(js).await {
        warn!(error = %format!("{e:#}"), "support codes projection sync failed; will retry");
    }
}

/// Periodic re-sync (its first tick runs immediately, which doubles as the
/// startup reconcile for a deployment that configured codes earlier).
pub fn spawn(js: Context) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RESYNC_INTERVAL);
        loop {
            interval.tick().await;
            sync_or_warn(&js).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::config::{MailEncryption, MailSection};
    use kanade_shared::wire::{AgentInstallSection, SupportCode};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn code(scope: &str, disabled: bool) -> SupportCode {
        SupportCode {
            scope: scope.into(),
            hash: format!("$argon2id$hash-{scope}"),
            label: Some("desk".into()),
            ttl_minutes: Some(30),
            disabled,
        }
    }

    fn secret_settings(codes: Vec<SupportCode>) -> ServerSettings {
        ServerSettings {
            support_codes: codes,
            agent_install: Some(AgentInstallSection {
                nats_url: Some("nats://x:4222".into()),
                nats_token: Some("super-secret-token".into()),
                ..Default::default()
            }),
            mail: Some(MailSection {
                host: "smtp.example".into(),
                port: 587,
                encryption: MailEncryption::Starttls,
                username: Some("mailer".into()),
                from: "kanade@example".into(),
            }),
            agent_prune_days: Some(9),
            ..Default::default()
        }
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn projection_carries_only_support_code_fields() {
        let settings = secret_settings(vec![code("support", false), code("admin", true)]);
        let raw = serde_json::to_string(&SupportCodesProjection::from_settings(&settings)).unwrap();
        // Fails if any other settings field (or its value) ever reaches it.
        assert!(!raw.contains("super-secret-token"));
        assert!(!raw.contains("smtp.example"));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(keys(&v), BTreeSet::from(["support_codes".to_string()]));
        let allowed: BTreeSet<String> = ["scope", "hash", "label", "ttl_minutes", "disabled"]
            .map(String::from)
            .into();
        for c in v["support_codes"].as_array().unwrap() {
            assert!(
                keys(c).is_subset(&allowed),
                "unexpected keys: {:?}",
                keys(c)
            );
        }
        // And the disabled flag survives so the agent can honour it.
        assert_eq!(v["support_codes"][1]["disabled"], json!(true));
    }

    #[test]
    fn startup_reconcile_creates_the_projection_when_absent() {
        let settings = serde_json::to_vec(&secret_settings(vec![code("support", false)])).unwrap();
        let Plan::Write(body) = plan(Some(&settings), None).unwrap() else {
            panic!("expected a write");
        };
        let p: SupportCodesProjection = serde_json::from_slice(&body).unwrap();
        assert_eq!(p.support_codes, vec![code("support", false)]);
    }

    #[test]
    fn unconfigured_deployment_gets_an_empty_projection_not_nothing() {
        let Plan::Write(body) = plan(None, None).unwrap() else {
            panic!("expected a write");
        };
        assert_eq!(body, br#"{"support_codes":[]}"#);
    }

    #[test]
    fn a_change_updates_and_clearing_empties_it() {
        let before = serde_json::to_vec(&secret_settings(vec![code("a", false)])).unwrap();
        let Plan::Write(cur) = plan(Some(&before), None).unwrap() else {
            panic!()
        };
        let after = serde_json::to_vec(&secret_settings(vec![code("a", true)])).unwrap();
        let Plan::Write(updated) = plan(Some(&after), Some(&cur)).unwrap() else {
            panic!("a changed code must rewrite the projection");
        };
        assert!(
            serde_json::from_slice::<SupportCodesProjection>(&updated)
                .unwrap()
                .support_codes[0]
                .disabled
        );
        let cleared = serde_json::to_vec(&secret_settings(vec![])).unwrap();
        let Plan::Write(empty) = plan(Some(&cleared), Some(&updated)).unwrap() else {
            panic!("clearing must rewrite, not skip");
        };
        assert_eq!(empty, br#"{"support_codes":[]}"#);
    }

    #[test]
    fn unchanged_projection_is_not_rewritten() {
        let settings = serde_json::to_vec(&secret_settings(vec![code("a", false)])).unwrap();
        let Plan::Write(cur) = plan(Some(&settings), None).unwrap() else {
            panic!()
        };
        assert_eq!(plan(Some(&settings), Some(&cur)).unwrap(), Plan::Unchanged);
    }

    #[test]
    fn a_corrupt_settings_document_never_clobbers_the_projection() {
        assert!(plan(Some(b"not json"), Some(br#"{"support_codes":[]}"#)).is_err());
    }
}
