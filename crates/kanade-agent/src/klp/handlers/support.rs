//! `support.*` method handlers — the helpdesk unlock gate.
//!
//! - `support.unlock` — redeem an operator-issued code, revealing the calling
//!   OS user's `client.unlock`-scoped jobs for a limited window.
//! - `support.lock` — drop every grant that user holds, now.
//! - `support.status` — what they hold (so a reconnecting client can restore
//!   its banner without re-asking for the code).
//!
//! The grant gates **`jobs.list` only** — it decides what the Client App
//! offers, not what the agent will run (see `ClientHint::unlock`). So this is
//! a UX affordance, not an authorization boundary: the guard rails below
//! (rate limiting, local hashes, audit) exist to keep helpdesk-only buttons
//! from appearing in front of end users, not to protect a capability.
//!
//! Verification is **local**: the argon2id hashes live in the
//! `support_codes` key of the `fleet_config` KV bucket — a projection the
//! backend derives from `server_settings` carrying nothing but the codes — so
//! a desk can still unlock a machine while the backend is down, which is
//! exactly when they are most likely to need to. The agent deliberately does
//! not read `server_settings` itself: that document also holds the installer's
//! NATS token and other operator secrets. (Only while the projection key is
//! absent — a backend that has not published it yet — does it fall back to
//! that legacy document, so the broker cannot deny it until every agent and
//! the backend are upgraded.) The KV is reachable only with
//! the NATS token, which lives under an HKLM key ACL'd to SYSTEM +
//! Administrators, so an ordinary end user cannot read the hashes at all.
//! (A local *administrator* can, and already holds far stronger capabilities
//! than any unlock scope confers — see #1155. This gate is a control on
//! standard users, not on machine admins.)
//!
//! Every attempt is audited to the per-PC observability timeline, success
//! and failure alike: the operator value of this feature is as much "who
//! opened this machine, when, and what did they run" as it is the unlocking.

use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::support::{
    SupportLockParams, SupportLockResult, SupportStatusParams, SupportStatusResult,
    SupportUnlockParams, SupportUnlockResult,
};
use kanade_shared::kv::{
    BUCKET_FLEET_CONFIG, BUCKET_SERVER_SETTINGS, KEY_SERVER_SETTINGS, KEY_SUPPORT_CODES,
};
use kanade_shared::wire::{ObsEvent, ServerSettings, SupportCode, SupportCodesProjection};
use tracing::{info, warn};

use super::super::connection::ConnectionState;
use super::super::unlock;
use super::system::HandlerResult;
use crate::obs_outbox;

/// `support.unlock` — verify a typed support code and grant its scope.
///
/// Failure modes are deliberately indistinguishable to the caller: a wrong
/// code, a code for a disabled scope, and no codes configured at all every
/// answer the same `Unauthorized`. Telling them apart would let someone at
/// the keyboard enumerate which scopes exist before guessing at one.
pub async fn handle_support_unlock(
    conn: &ConnectionState,
    params: SupportUnlockParams,
) -> HandlerResult<SupportUnlockResult> {
    let sid = conn.peer.user_sid.clone();

    // Rate limit BEFORE touching KV or argon2: a locked-out caller must cost
    // us nothing (argon2 is deliberately expensive, which makes an unbounded
    // attempt loop a local DoS as well as a guessing oracle).
    if let Some(remaining) = unlock::lockout_remaining(&sid) {
        warn!(
            user = %conn.peer.user,
            secs = remaining.as_secs(),
            "support.unlock: refused, caller is rate-limited",
        );
        return Err(RpcError::new(
            ErrorKind::RateLimit,
            format!(
                "too many failed attempts; try again in {} seconds",
                remaining.as_secs().max(1)
            ),
        ));
    }

    let code = params.code.trim().to_string();
    if code.is_empty() {
        // Not counted as a failure: an empty box is a UI slip, not a guess.
        return Err(RpcError::new(
            ErrorKind::InvalidParams,
            "support.unlock: code must not be empty",
        ));
    }

    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "support.unlock: NATS client not wired into the connection",
        )
    })?;
    let codes = read_support_codes(client).await?;

    let usable: Vec<SupportCode> = codes.iter().filter(|c| c.is_usable()).cloned().collect();

    // argon2 is CPU-bound by design (tens of ms per verify). Off the async
    // worker it would stall every other connection's handlers on this
    // thread, so the whole match loop goes to a blocking thread.
    let matched = tokio::task::spawn_blocking(move || match_code(&code, &usable))
        .await
        .map_err(|e| {
            warn!(error = %e, "support.unlock: verify task failed");
            RpcError::new(ErrorKind::InternalError, "support.unlock: verify failed")
        })?;

    let Some(code) = matched else {
        let lockout = unlock::record_failure(&sid);
        warn!(
            user = %conn.peer.user,
            locked_out = lockout.is_some(),
            "support.unlock: rejected an unrecognised code",
        );
        audit(conn, "support_unlock_failed", serde_json::json!({}));
        return Err(RpcError::new(
            ErrorKind::Unauthorized,
            "support.unlock: code not recognised",
        ));
    };

    unlock::clear_failures(&sid);
    let ttl = code.effective_ttl_minutes();
    let grants = unlock::grant(&sid, &code.scope, code.label.clone(), ttl);
    info!(
        user = %conn.peer.user,
        scope = %code.scope,
        ttl_minutes = ttl,
        "support.unlock: granted",
    );
    audit(
        conn,
        "support_unlock",
        serde_json::json!({ "scope": code.scope, "ttl_minutes": ttl, "label": code.label }),
    );
    Ok(SupportUnlockResult { grants })
}

/// `support.lock` — drop every grant the caller holds. Never an error: the
/// desk pressing "終了" on an already-lapsed session should see it close, not
/// see a failure.
pub fn handle_support_lock(
    conn: &ConnectionState,
    _params: SupportLockParams,
) -> HandlerResult<SupportLockResult> {
    let released = unlock::lock(&conn.peer.user_sid);
    if released > 0 {
        info!(user = %conn.peer.user, released, "support.lock: grants released");
        audit(
            conn,
            "support_lock",
            serde_json::json!({ "released": released }),
        );
    }
    Ok(SupportLockResult { released })
}

/// `support.status` — the caller's live grants (empty ⇒ locked). Read-only
/// and unaudited: it reports state the caller already caused.
pub fn handle_support_status(
    conn: &ConnectionState,
    _params: SupportStatusParams,
) -> HandlerResult<SupportStatusResult> {
    Ok(SupportStatusResult {
        grants: unlock::grants(&conn.peer.user_sid),
    })
}

/// The first configured code whose hash matches `code`, or `None`.
///
/// Every candidate is verified even after a match, so the work done is a
/// function of how many codes are configured and not of which one was typed
/// — a timing side channel here would leak scope ordering to someone who can
/// measure it, which is anyone sitting at the machine.
fn match_code(code: &str, codes: &[SupportCode]) -> Option<SupportCode> {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};

    let mut hit: Option<SupportCode> = None;
    for candidate in codes {
        let parsed = match PasswordHash::new(&candidate.hash) {
            Ok(p) => p,
            Err(e) => {
                // A malformed stored hash can never match anything, so this
                // fails closed on its own — but it also means a scope the
                // operator believes is configured silently opens for nobody,
                // which is worth a log line.
                warn!(scope = %candidate.scope, error = %e, "support code hash is unparseable");
                continue;
            }
        };
        if Argon2::default()
            .verify_password(code.as_bytes(), &parsed)
            .is_ok()
            && hit.is_none()
        {
            hit = Some(candidate.clone());
        }
    }
    hit
}

/// Read the configured support codes. A missing projection *and* a missing
/// legacy document both mean "never configured" (no codes ⇒ nothing
/// unlockable); a read or decode failure is surfaced so the desk sees
/// "couldn't check" rather than an indistinguishable "wrong code".
async fn read_support_codes(client: &async_nats::Client) -> HandlerResult<Vec<SupportCode>> {
    let js = async_nats::jetstream::new(client.clone());
    let projection = read_projection_bytes(&js).await;
    select_codes(projection, || read_legacy_codes(&js)).await
}

/// Choose the code source from what the projection read produced.
///
/// The legacy `server_settings` read happens **only** when the projection key
/// is strictly absent: an unreadable or corrupt projection is an outage to
/// report, not a licence to reach for the broader document, and a present
/// projection — even an empty one — is the backend's authoritative answer.
async fn select_codes<F, Fut>(
    projection: HandlerResult<Option<Vec<u8>>>,
    legacy: F,
) -> HandlerResult<Vec<SupportCode>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = HandlerResult<Vec<SupportCode>>>,
{
    match projection? {
        Some(bytes) => serde_json::from_slice::<SupportCodesProjection>(&bytes)
            .map(|p| p.support_codes)
            .map_err(|e| {
                warn!(error = %e, "support.unlock: decode support_codes projection");
                RpcError::new(
                    ErrorKind::InternalError,
                    "support.unlock: server settings are corrupt",
                )
            }),
        None => legacy().await,
    }
}

/// `Ok(None)` ⇒ the projection key does not exist.
async fn read_projection_bytes(
    js: &async_nats::jetstream::Context,
) -> HandlerResult<Option<Vec<u8>>> {
    let kv = js.get_key_value(BUCKET_FLEET_CONFIG).await.map_err(|e| {
        warn!(error = %e, "support.unlock: open fleet_config bucket");
        RpcError::new(
            ErrorKind::InternalError,
            "support.unlock: server settings unavailable",
        )
    })?;
    match kv.get(KEY_SUPPORT_CODES).await {
        Ok(v) => Ok(v.map(|b| b.to_vec())),
        Err(e) => {
            warn!(error = %e, "support.unlock: read support_codes projection");
            Err(RpcError::new(
                ErrorKind::InternalError,
                "support.unlock: server settings unavailable",
            ))
        }
    }
}

/// Pre-projection source, kept only for a backend that has not published the
/// projection yet. Same failure semantics the agent always had.
async fn read_legacy_codes(js: &async_nats::jetstream::Context) -> HandlerResult<Vec<SupportCode>> {
    let kv = js
        .get_key_value(BUCKET_SERVER_SETTINGS)
        .await
        .map_err(|e| {
            warn!(error = %e, "support.unlock: open server_settings bucket");
            RpcError::new(
                ErrorKind::InternalError,
                "support.unlock: server settings unavailable",
            )
        })?;
    match kv.get(KEY_SERVER_SETTINGS).await {
        Ok(Some(bytes)) => serde_json::from_slice::<ServerSettings>(&bytes)
            .map(|s| s.support_codes)
            .map_err(|e| {
                warn!(error = %e, "support.unlock: decode server_settings");
                RpcError::new(
                    ErrorKind::InternalError,
                    "support.unlock: server settings are corrupt",
                )
            }),
        Ok(None) => Ok(Vec::new()),
        Err(e) => {
            warn!(error = %e, "support.unlock: read server_settings");
            Err(RpcError::new(
                ErrorKind::InternalError,
                "support.unlock: server settings unavailable",
            ))
        }
    }
}

/// Queue one audit event onto the per-PC observability timeline.
///
/// Best-effort and fire-and-forget: the outbox makes delivery durable across
/// a broker outage, but a failure to *queue* must not fail the unlock itself
/// — refusing to open a machine because an audit file couldn't be written
/// would turn a disk hiccup into "the helpdesk can't help you".
///
/// `payload` never carries the code, hashed or otherwise. It records who
/// (SID + friendly name), which scope, and how long — the questions an
/// operator reviewing the timeline actually asks.
fn audit(conn: &ConnectionState, kind: &str, mut payload: serde_json::Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("user".into(), conn.peer.user.clone().into());
        obj.insert("user_sid".into(), conn.peer.user_sid.clone().into());
    }
    let event = ObsEvent {
        pc_id: conn.pc_id.clone(),
        at: chrono::Utc::now(),
        kind: kind.to_string(),
        source: "agent:support".to_string(),
        // Every attempt is its own event — nothing here should ever dedup
        // (two identical unlocks a minute apart are two facts, not one).
        event_record_id: Some(format!("support_{}", uuid::Uuid::new_v4().simple())),
        payload,
    };
    let dir = obs_outbox::default_dir();
    let res = obs_outbox::ensure_outbox_dir(&dir)
        .and_then(|()| obs_outbox::enqueue(&dir, &event).map(|_| ()));
    if let Err(e) = res {
        warn!(error = %e, kind, "failed to queue support audit event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(code: &str) -> String {
        use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
        let salt = SaltString::generate(&mut OsRng);
        argon2::Argon2::default()
            .hash_password(code.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    fn code(scope: &str, plain: &str) -> SupportCode {
        SupportCode {
            scope: scope.into(),
            hash: hash_of(plain),
            label: None,
            ttl_minutes: None,
            disabled: false,
        }
    }

    #[test]
    fn matches_the_right_scope() {
        let codes = vec![code("support", "hunter2"), code("admin", "correct-horse")];
        assert_eq!(match_code("hunter2", &codes).unwrap().scope, "support");
        assert_eq!(match_code("correct-horse", &codes).unwrap().scope, "admin");
    }

    #[test]
    fn rejects_a_wrong_code() {
        let codes = vec![code("support", "hunter2")];
        assert!(match_code("hunter3", &codes).is_none());
        assert!(match_code("", &codes).is_none());
    }

    #[test]
    fn no_configured_codes_matches_nothing() {
        // The fresh-deployment state: every `client.unlock` job stays hidden
        // from everyone until an operator sets a code.
        assert!(match_code("anything", &[]).is_none());
    }

    #[test]
    fn skips_an_unparseable_hash_without_matching() {
        // A hand-corrupted (or API-redacted, i.e. blank) hash must fail
        // closed rather than panic or match. `is_usable()` already filters
        // blanks upstream; this covers the garbage case.
        let codes = vec![SupportCode {
            scope: "broken".into(),
            hash: "not-a-phc-string".into(),
            ..Default::default()
        }];
        assert!(match_code("not-a-phc-string", &codes).is_none());
        assert!(match_code("anything", &codes).is_none());
    }

    #[test]
    fn a_later_scope_still_matches_after_an_earlier_miss() {
        // Guards the "verify every candidate" loop against an early return
        // sneaking back in and shadowing scopes past the first.
        let codes = vec![code("a", "aaa"), code("b", "bbb"), code("c", "ccc")];
        assert_eq!(match_code("ccc", &codes).unwrap().scope, "c");
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn projection_bytes(codes: Vec<SupportCode>) -> Vec<u8> {
        serde_json::to_vec(&SupportCodesProjection {
            support_codes: codes,
        })
        .unwrap()
    }

    /// Run `select_codes` and report how many times the legacy source was hit.
    async fn select(
        projection: HandlerResult<Option<Vec<u8>>>,
        legacy: HandlerResult<Vec<SupportCode>>,
    ) -> (HandlerResult<Vec<SupportCode>>, usize) {
        let hits = AtomicUsize::new(0);
        let out = select_codes(projection, || async {
            hits.fetch_add(1, Ordering::SeqCst);
            legacy
        })
        .await;
        (out, hits.load(Ordering::SeqCst))
    }

    fn unavailable() -> RpcError {
        RpcError::new(ErrorKind::InternalError, "unavailable")
    }

    /// Select, then run the same filter + match the handler does.
    async fn unlock_with(typed: &str, projection: Vec<SupportCode>) -> Option<String> {
        let (codes, _) = select(Ok(Some(projection_bytes(projection))), Err(unavailable())).await;
        let usable: Vec<SupportCode> = codes
            .unwrap()
            .into_iter()
            .filter(|c| c.is_usable())
            .collect();
        match_code(typed, &usable).map(|c| c.scope)
    }

    #[tokio::test]
    async fn projection_unlocks_with_the_right_code_and_refuses_a_wrong_one() {
        let codes = vec![code("support", "hunter2")];
        assert_eq!(
            unlock_with("hunter2", codes.clone()).await.as_deref(),
            Some("support")
        );
        assert_eq!(unlock_with("hunter3", codes).await, None);
    }

    #[tokio::test]
    async fn a_disabled_scope_does_not_unlock() {
        let mut c = code("support", "hunter2");
        c.disabled = true;
        assert_eq!(unlock_with("hunter2", vec![c]).await, None);
    }

    #[tokio::test]
    async fn present_projection_never_touches_the_legacy_document() {
        let (out, hits) = select(
            Ok(Some(projection_bytes(vec![code("a", "aaa")]))),
            Ok(vec![code("old", "old")]),
        )
        .await;
        assert_eq!(out.unwrap()[0].scope, "a");
        assert_eq!(hits, 0);
    }

    #[tokio::test]
    async fn empty_projection_is_authoritative() {
        // Cleared codes must not resurrect from the legacy document.
        let (out, hits) = select(
            Ok(Some(projection_bytes(vec![]))),
            Ok(vec![code("old", "o")]),
        )
        .await;
        assert!(out.unwrap().is_empty());
        assert_eq!(hits, 0);
    }

    #[tokio::test]
    async fn unreadable_projection_is_unavailable_without_fallback() {
        let (out, hits) = select(Err(unavailable()), Ok(vec![code("old", "o")])).await;
        assert!(out.is_err());
        assert_eq!(hits, 0);
    }

    #[tokio::test]
    async fn corrupt_projection_is_surfaced_without_fallback() {
        let (out, hits) = select(Ok(Some(b"{not json".to_vec())), Ok(vec![code("old", "o")])).await;
        assert!(out.is_err());
        assert_eq!(hits, 0);
    }

    #[tokio::test]
    async fn absent_projection_falls_back_to_the_legacy_document() {
        let (out, hits) = select(Ok(None), Ok(vec![code("old", "o")])).await;
        assert_eq!(out.unwrap()[0].scope, "old");
        assert_eq!(hits, 1);
        // The legacy source keeps its own failure semantics.
        let (out, hits) = select(Ok(None), Err(unavailable())).await;
        assert!(out.is_err());
        assert_eq!(hits, 1);
    }
}
