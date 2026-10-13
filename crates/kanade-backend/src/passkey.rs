//! Passkey (WebAuthn) core: relying-party validation, the in-memory
//! challenge store and the credential table operations. The HTTP layer lives
//! in [`crate::api::passkeys`]; everything security-relevant is here so it can
//! be tested without a NATS broker.
//!
//! Security model, in one place:
//!
//!   * **User verification is mandatory.** Every ceremony goes through
//!     webauthn-rs's `Passkey` API, which demands UV, and sign-in re-checks
//!     `AuthenticationResult::user_verified` explicitly. A credential used
//!     without PIN/biometric is rejected.
//!   * **RP ID and origin are configuration, never request data.** See
//!     [`validate_config`]; an unsafe combination stops the backend booting.
//!   * **Challenges are server-side, single-use and short-lived**
//!     ([`CHALLENGE_TTL`]). `take` removes the entry before anything else is
//!     checked, so a challenge can be spent exactly once even if the attempt
//!     then fails. They live in memory (this backend is a single process), so
//!     a restart just invalidates in-flight ceremonies; the webauthn-rs state
//!     types are therefore never serialised.
//!   * **Replay protection** is the single-use challenge plus the signature
//!     counter / backup flags webauthn-rs verifies against the stored
//!     credential and that [`finish_login`] writes back after every sign-in.
//!   * **No account enumeration**: sign-in is by discoverable credential, so
//!     the client never submits a username and every failure is the same 401.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kanade_shared::config::WebauthnSection;
use rand::Rng as _;
use sqlx::SqlitePool;
use webauthn_rs::prelude::{
    CredentialID, DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url, Uuid,
    Webauthn, WebauthnBuilder,
};

use crate::auth::Role;
use crate::login_throttle::LoginThrottle;

/// How long a challenge may be answered. Also advertised to the browser as
/// the ceremony timeout.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(120);
/// Hard cap on outstanding (unanswered) challenges. `login/start` is
/// unauthenticated, so without a cap a flood of starts is a memory DoS.
const MAX_OUTSTANDING: usize = 2048;
/// `login/start` calls allowed per source IP per [`START_WINDOW`].
const START_MAX_PER_IP: u32 = 20;
const START_WINDOW: Duration = Duration::from_secs(60);
const START_PRUNE_AT: usize = 4096;
/// Credentials one account may hold.
pub const MAX_PASSKEYS_PER_USER: i64 = 10;
/// Longest accepted credential label.
const MAX_NAME_CHARS: usize = 64;

/// Re-authentication proof returned by `reauthenticate`. Carries the
/// `passkey_enrollment_epoch` snapshot so the subsequent enrolment is bound
/// to the exact account state the user proved ownership of. This prevents an
/// admin reset (which rotates the epoch) from silently invalidating the
/// re-authentication but still allowing the enrolment to succeed against the
/// new epoch.
#[derive(Debug, Clone)]
pub struct ReauthProof {
    pub must_change_pw: bool,
    pub enrollment_epoch: String,
}

/// A validated relying-party identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelyingParty {
    pub rp_id: String,
    pub rp_name: String,
    pub origins: Vec<Url>,
}

fn is_localhost(host: &str) -> bool {
    host == "localhost"
}

fn valid_host_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 63
        && !l.starts_with('-')
        && !l.ends_with('-')
        && l.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Validate `[server.webauthn]`. Fail-closed: any doubt is an error, and the
/// caller aborts startup. Never consults request data.
pub fn validate_config(cfg: &WebauthnSection) -> Result<RelyingParty, String> {
    let rp_id = cfg.rp_id.as_str();
    if rp_id.is_empty() || rp_id != rp_id.trim() {
        return Err("rp_id must be a non-empty host name without surrounding whitespace".into());
    }
    if rp_id.contains("://") || rp_id.contains('/') || rp_id.contains(':') || rp_id.contains('@') {
        return Err(format!(
            "rp_id {rp_id:?} must be a bare host name (no scheme, port, path or credentials)"
        ));
    }
    if rp_id.contains('*') {
        return Err("rp_id must not contain a wildcard".into());
    }
    if rp_id != rp_id.to_ascii_lowercase() || !rp_id.split('.').all(valid_host_label) {
        return Err(format!(
            "rp_id {rp_id:?} must be a lowercase ASCII host name (use the punycode form for IDN)"
        ));
    }
    if rp_id
        .split('.')
        .all(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("rp_id must be a domain name, not an IP address".into());
    }
    if cfg.rp_origins.is_empty() {
        return Err("rp_origins must list at least one origin".into());
    }

    let mut origins: Vec<Url> = Vec::new();
    for raw in &cfg.rp_origins {
        if raw.contains('*') {
            return Err(format!(
                "rp_origins entry {raw:?} must not contain a wildcard"
            ));
        }
        let url = Url::parse(raw).map_err(|e| format!("rp_origins entry {raw:?}: {e}"))?;
        let host = url
            .domain()
            .ok_or_else(|| format!("rp_origins entry {raw:?} must use a host name, not an IP"))?
            .to_string();
        match url.scheme() {
            "https" => {}
            "http" if is_localhost(&host) => {}
            _ => {
                return Err(format!(
                    "rp_origins entry {raw:?} must be https (plain http is accepted only for localhost)"
                ));
            }
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(format!(
                "rp_origins entry {raw:?} must be a bare origin (scheme://host[:port])"
            ));
        }
        let matches =
            host == rp_id || (!is_localhost(rp_id) && host.ends_with(&format!(".{rp_id}")));
        if !matches {
            return Err(format!(
                "rp_origins entry {raw:?}: host {host:?} is neither rp_id {rp_id:?} nor a subdomain of it"
            ));
        }
        // A dotless rp_id that is not the origin host would be a TLD-style
        // parent (`com`), which every site under it could mint credentials for.
        if !rp_id.contains('.') && !is_localhost(rp_id) && host != rp_id {
            return Err(format!(
                "rp_id {rp_id:?} is a single label and must equal the origin host exactly"
            ));
        }
        if !origins.contains(&url) {
            origins.push(url);
        }
    }
    let rp_name = cfg
        .rp_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Kanade")
        .to_string();
    Ok(RelyingParty {
        rp_id: rp_id.to_string(),
        rp_name,
        origins,
    })
}

/// True when `public_url` (used for emailed links) is not one of the
/// passkey origins — almost always a typo in one of the two settings. Only a
/// warning: the two serve different purposes and may legitimately differ.
pub fn public_url_outside_origins(public_url: &str, rp: &RelyingParty) -> bool {
    match Url::parse(public_url) {
        Ok(u) => !rp.origins.iter().any(|o| o.origin() == u.origin()),
        Err(_) => true,
    }
}

// ---- challenge store ------------------------------------------------

enum Pending {
    Register {
        username: String,
        user_uuid: Uuid,
        enrollment_epoch: String,
        name: String,
        state: Box<PasskeyRegistration>,
    },
    Login {
        state: Box<DiscoverableAuthentication>,
    },
}

struct Entry {
    expires: Instant,
    pending: Pending,
}

/// Why a challenge could not be consumed. Deliberately coarse: the HTTP layer
/// maps every variant to the same message.
#[derive(Debug, PartialEq, Eq)]
pub enum ChallengeError {
    /// Unknown, already used, expired or issued for something else.
    Invalid,
    /// Store full (unauthenticated starts flooding).
    Busy,
    /// Source IP exceeded the start rate.
    RateLimited(u64),
}

pub struct PasskeyService {
    webauthn: Webauthn,
    store: Mutex<HashMap<String, Entry>>,
    starts: Mutex<HashMap<String, (u32, Instant)>>,
}

fn new_challenge_id() -> String {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

impl PasskeyService {
    pub fn new(cfg: &WebauthnSection) -> Result<Self, String> {
        let rp = validate_config(cfg)?;
        let first = rp.origins[0].clone();
        let mut builder = WebauthnBuilder::new(&rp.rp_id, &first)
            .map_err(|e| format!("webauthn configuration rejected: {e}"))?
            .rp_name(&rp.rp_name)
            .timeout(CHALLENGE_TTL);
        for o in &rp.origins[1..] {
            builder = builder.append_allowed_origin(o);
        }
        let webauthn = builder
            .build()
            .map_err(|e| format!("webauthn configuration rejected: {e}"))?;
        Ok(Self {
            webauthn,
            store: Mutex::new(HashMap::new()),
            starts: Mutex::new(HashMap::new()),
        })
    }

    fn insert(&self, pending: Pending, now: Instant) -> Result<String, ChallengeError> {
        let mut store = self.store.lock().unwrap();
        if store.len() >= MAX_OUTSTANDING {
            store.retain(|_, e| e.expires > now);
            if store.len() >= MAX_OUTSTANDING {
                return Err(ChallengeError::Busy);
            }
        }
        if let Pending::Register { username, .. } = &pending {
            // One in-flight enrolment per account: a new start replaces the
            // old challenge, which bounds authenticated growth too.
            store.retain(|_, e| {
                !matches!(&e.pending, Pending::Register { username: u, .. } if u == username)
            });
        }
        let id = new_challenge_id();
        store.insert(
            id.clone(),
            Entry {
                expires: now + CHALLENGE_TTL,
                pending,
            },
        );
        Ok(id)
    }

    /// Remove and return a challenge. Removal happens before any other
    /// check, so a challenge is spent by the first attempt, success or not.
    fn take(&self, id: &str, now: Instant) -> Option<Pending> {
        let entry = self.store.lock().unwrap().remove(id)?;
        (entry.expires > now).then_some(entry.pending)
    }

    fn start_allowed(&self, ip: &str, now: Instant) -> Result<(), u64> {
        let mut starts = self.starts.lock().unwrap();
        if starts.len() >= START_PRUNE_AT {
            starts.retain(|_, (_, t)| now.saturating_duration_since(*t) < START_WINDOW);
        }
        let slot = starts.entry(ip.to_string()).or_insert((0, now));
        if now.saturating_duration_since(slot.1) >= START_WINDOW {
            *slot = (0, now);
        }
        if slot.0 >= START_MAX_PER_IP {
            return Err((START_WINDOW - now.saturating_duration_since(slot.1)).as_secs() + 1);
        }
        slot.0 += 1;
        Ok(())
    }

    /// Begin a sign-in. Public and unauthenticated, hence the per-IP rate and
    /// the global cap.
    pub fn start_login(
        &self,
        ip: &str,
    ) -> Result<(String, RequestChallengeResponse), ChallengeError> {
        self.start_login_at(ip, Instant::now())
    }

    fn start_login_at(
        &self,
        ip: &str,
        now: Instant,
    ) -> Result<(String, RequestChallengeResponse), ChallengeError> {
        self.start_allowed(ip, now)
            .map_err(ChallengeError::RateLimited)?;
        let (mut rcr, state) = self
            .webauthn
            .start_discoverable_authentication()
            .map_err(|_| ChallengeError::Busy)?;
        // The library tags the request `mediation: conditional` (autofill
        // UI). Ours is an explicit button, which needs the modal prompt.
        rcr.mediation = None;
        let id = self.insert(
            Pending::Login {
                state: Box::new(state),
            },
            now,
        )?;
        Ok((id, rcr))
    }

    /// Test hook: pretend every outstanding challenge has outlived its TTL.
    #[cfg(test)]
    pub(crate) fn expire_all(&self) {
        let now = Instant::now();
        for e in self.store.lock().unwrap().values_mut() {
            e.expires = now;
        }
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.store.lock().unwrap().len()
    }
}

// ---- credential table -----------------------------------------------

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct PasskeyInfo {
    pub id: i64,
    pub name: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

pub async fn list(pool: &SqlitePool, username: &str) -> Result<Vec<PasskeyInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM user_passkeys \
         WHERE username = ? ORDER BY id",
    )
    .bind(username)
    .fetch_all(pool)
    .await
}

/// Delete one of `username`'s credentials. `false` = no such credential
/// (also what asking for somebody else's id yields).
pub async fn remove(pool: &SqlitePool, username: &str, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM user_passkeys WHERE id = ? AND username = ?")
        .bind(id)
        .bind(username)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Delete every credential of `username` (admin recovery). Returns how many.
pub async fn remove_all(pool: &SqlitePool, username: &str) -> Result<u64, sqlx::Error> {
    // Serialize revocation with finish_register's conditional INSERT. If a
    // finish writes first, this delete removes it; if reset writes first,
    // the old challenge's epoch can never match again. This also covers a
    // finish that has already consumed its in-memory challenge.
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET passkey_enrollment_epoch = ? WHERE username = ?")
        .bind(Uuid::new_v4().to_string())
        .bind(username)
        .execute(&mut *tx)
        .await?;
    let res = sqlx::query("DELETE FROM user_passkeys WHERE username = ?")
        .bind(username)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected())
}

fn cred_key(id: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(id)
}

/// Why an enrolment step failed.
#[derive(Debug)]
pub enum RegisterError {
    Challenge(ChallengeError),
    /// The authenticator response did not verify (origin, RP ID, UV, ...).
    Rejected,
    /// This authenticator is already registered (to this or another account).
    Duplicate,
    LimitReached,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for RegisterError {
    fn from(e: sqlx::Error) -> Self {
        RegisterError::Db(e)
    }
}

/// Sign-in is usernameless, so the credential must be *discoverable*
/// (resident) or the authenticator cannot offer it without an allow-list.
/// webauthn-rs asks for `residentKey: discouraged` on plain passkeys, which
/// platform authenticators ignore but roaming keys honour — patch the options
/// to `required` so every credential we store can actually sign in.
fn require_discoverable(options: &mut serde_json::Value) {
    if let Some(sel) = options
        .pointer_mut("/publicKey/authenticatorSelection")
        .and_then(|v| v.as_object_mut())
    {
        sel.insert("residentKey".into(), "required".into());
        sel.insert("requireResidentKey".into(), true.into());
    }
}

/// Normalise a user-supplied label; `None` when unusable.
pub fn clean_name(raw: &str) -> Option<String> {
    let n: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    let n = n.trim();
    (!n.is_empty() && n.chars().count() <= MAX_NAME_CHARS).then(|| n.to_string())
}

impl PasskeyService {
    /// Begin enrolling a credential for `username`. The caller must already
    /// have re-authenticated the user and obtained a `ReauthProof` (see
    /// `api::passkeys::reauthenticate`).
    pub async fn start_register(
        &self,
        pool: &SqlitePool,
        username: &str,
        name: &str,
        proof: &ReauthProof,
    ) -> Result<(String, serde_json::Value), RegisterError> {
        self.start_register_at(pool, username, name, proof, Instant::now())
            .await
    }

    async fn start_register_at(
        &self,
        pool: &SqlitePool,
        username: &str,
        name: &str,
        proof: &ReauthProof,
        now: Instant,
    ) -> Result<(String, serde_json::Value), RegisterError> {
        // The re-authentication proof already captured the enrollment_epoch
        // and verified account restrictions. Use the captured epoch instead
        // of re-reading it, so an admin reset between re-auth and start
        // cannot silently invalidate the proof while allowing the enrolment.
        let enrollment_epoch = proof.enrollment_epoch.clone();
        if proof.must_change_pw {
            return Err(RegisterError::Rejected);
        }
        let existing: Vec<(String, String)> =
            sqlx::query_as("SELECT user_uuid, credential_id FROM user_passkeys WHERE username = ?")
                .bind(username)
                .fetch_all(pool)
                .await?;
        if existing.len() as i64 >= MAX_PASSKEYS_PER_USER {
            return Err(RegisterError::LimitReached);
        }
        let user_uuid = existing
            .first()
            .and_then(|(u, _)| Uuid::parse_str(u).ok())
            .unwrap_or_else(Uuid::new_v4);
        let exclude: Vec<CredentialID> = existing
            .iter()
            .filter_map(|(_, c)| URL_SAFE_NO_PAD.decode(c).ok())
            .collect();
        let (ccr, state) = self
            .webauthn
            .start_passkey_registration(
                user_uuid,
                username,
                username,
                (!exclude.is_empty()).then_some(exclude),
            )
            .map_err(|_| RegisterError::Rejected)?;
        let id = self
            .insert(
                Pending::Register {
                    username: username.to_string(),
                    user_uuid,
                    enrollment_epoch,
                    name: name.to_string(),
                    state: Box::new(state),
                },
                now,
            )
            .map_err(RegisterError::Challenge)?;
        let mut options = serde_json::to_value(&ccr).map_err(|_| RegisterError::Rejected)?;
        require_discoverable(&mut options);
        Ok((id, options))
    }

    /// Finish enrolment. The challenge must have been issued to `username`.
    pub async fn finish_register(
        &self,
        pool: &SqlitePool,
        username: &str,
        challenge_id: &str,
        cred: &RegisterPublicKeyCredential,
    ) -> Result<PasskeyInfo, RegisterError> {
        let pending = self.take(challenge_id, Instant::now());
        let Some(Pending::Register {
            username: owner,
            user_uuid,
            enrollment_epoch,
            name,
            state,
        }) = pending
        else {
            return Err(RegisterError::Challenge(ChallengeError::Invalid));
        };
        if owner != username {
            return Err(RegisterError::Challenge(ChallengeError::Invalid));
        }
        let passkey = self
            .webauthn
            .finish_passkey_registration(cred, &state)
            .map_err(|e| {
                tracing::warn!(error = %e, %username, "passkey registration rejected");
                RegisterError::Rejected
            })?;
        let json = serde_json::to_string(&passkey).map_err(|_| RegisterError::Rejected)?;
        let key = cred_key(passkey.cred_id().as_ref());
        // Recheck account restrictions and revocation in the same statement
        // that stores the key, so an admin state change cannot race a check.
        // The cap likewise cannot be exceeded by racing enrolments.
        let mut tx = pool.begin().await?;
        let res = sqlx::query(
            "INSERT INTO user_passkeys (username, user_uuid, credential_id, passkey, name) \
             SELECT ?, ?, ?, ?, ? \
             WHERE (SELECT COUNT(*) FROM user_passkeys WHERE username = ?) < ? \
             AND EXISTS (SELECT 1 FROM users WHERE username = ? AND disabled = 0 \
                         AND must_change_pw = 0 AND passkey_enrollment_epoch = ?)",
        )
        .bind(username)
        .bind(user_uuid.to_string())
        .bind(&key)
        .bind(&json)
        .bind(&name)
        .bind(username)
        .bind(MAX_PASSKEYS_PER_USER)
        .bind(username)
        .bind(enrollment_epoch)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(r) if r.rows_affected() == 0 => Err(RegisterError::Rejected),
            Ok(r) => {
                let info = sqlx::query_as(
                    "SELECT id, name, created_at, last_used_at FROM user_passkeys WHERE id = ?",
                )
                .bind(r.last_insert_rowid())
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(info)
            }
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(RegisterError::Duplicate)
            }
            Err(e) => Err(RegisterError::Db(e)),
        }
    }
}

// ---- sign-in ----------------------------------------------------------

/// Why a passkey sign-in failed. Everything but `Throttled`/`Challenge`/
/// `Internal` is the same opaque 401 to the client; the audit trail keeps the
/// distinction through [`LoginFailure::reason`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoginErrorKind {
    Throttled(u64),
    Challenge,
    Invalid,
    Internal,
}

#[derive(Debug)]
pub struct LoginFailure {
    pub kind: LoginErrorKind,
    /// Account the attempt resolved to, when it got that far (audit only).
    pub username: Option<String>,
    /// Short machine-readable cause for the audit event.
    pub reason: &'static str,
}

impl LoginFailure {
    fn new(kind: LoginErrorKind, username: Option<&str>, reason: &'static str) -> Self {
        Self {
            kind,
            username: username.map(str::to_string),
            reason,
        }
    }
}

#[derive(Debug)]
pub struct LoginSuccess {
    pub username: String,
    pub role: Role,
    pub must_change_pw: bool,
}

impl PasskeyService {
    /// Verify a discoverable-credential assertion and resolve it to an account
    /// that is allowed to sign in.
    ///
    /// Everything the password path enforces is enforced here: the per-IP and
    /// per-account throttle, the `disabled` flag (re-read from the DB), and the
    /// role comes from the DB, not from anything the client sent.
    /// `must_change_pw` is returned so the session carries the same forced
    /// password-change gate as a password sign-in. TOTP is intentionally *not*
    /// asked for: a user-verified passkey is itself two factors (possession +
    /// PIN/biometric). The enrolment re-authentication in `api::passkeys` is
    /// what stops that from being a way around TOTP.
    pub async fn finish_login(
        &self,
        pool: &SqlitePool,
        throttle: &LoginThrottle,
        ip: &str,
        challenge_id: &str,
        cred: &PublicKeyCredential,
    ) -> Result<LoginSuccess, LoginFailure> {
        use LoginErrorKind as K;
        if let Err(secs) = throttle.check_ip(ip) {
            return Err(LoginFailure::new(K::Throttled(secs), None, "ip_throttled"));
        }
        let ip_fail = |kind, reason| {
            throttle.record_ip_failure(ip);
            LoginFailure::new(kind, None, reason)
        };
        let Some(Pending::Login { state }) = self.take(challenge_id, Instant::now()) else {
            return Err(ip_fail(K::Challenge, "bad_challenge"));
        };
        let Ok((user_uuid, cred_id)) = self.webauthn.identify_discoverable_authentication(cred)
        else {
            return Err(ip_fail(K::Invalid, "no_user_handle"));
        };
        type Row = (i64, String, String, String, i64, i64);
        let row: Option<Row> = sqlx::query_as(
            "SELECT p.id, p.username, p.passkey, u.role, u.disabled, u.must_change_pw \
             FROM user_passkeys p JOIN users u ON u.username = p.username \
             WHERE p.credential_id = ? AND p.user_uuid = ?",
        )
        .bind(cred_key(cred_id))
        .bind(user_uuid.to_string())
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "passkey login query failed");
            LoginFailure::new(K::Internal, None, "db_error")
        })?;
        let Some((row_id, username, passkey_json, role, disabled, must_change_pw)) = row else {
            return Err(ip_fail(K::Invalid, "unknown_credential"));
        };
        if let Err(secs) = throttle.check(&username, ip) {
            return Err(LoginFailure::new(
                K::Throttled(secs),
                Some(&username),
                "account_throttled",
            ));
        }
        let fail = |reason| {
            throttle.record_failure(&username, ip);
            LoginFailure::new(K::Invalid, Some(&username), reason)
        };
        if disabled != 0 {
            return Err(fail("account_disabled"));
        }
        let Some(role) = Role::parse(&role) else {
            return Err(LoginFailure::new(
                K::Internal,
                Some(&username),
                "corrupt_role",
            ));
        };
        let Ok(mut passkey) = serde_json::from_str::<Passkey>(&passkey_json) else {
            return Err(LoginFailure::new(
                K::Internal,
                Some(&username),
                "corrupt_credential",
            ));
        };
        let result = match self.webauthn.finish_discoverable_authentication(
            cred,
            *state,
            &[DiscoverableKey::from(&passkey)],
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, %username, "passkey assertion rejected");
                return Err(fail("assertion_rejected"));
            }
        };
        // webauthn-rs already demands UV for this ceremony; assert it again
        // so a library regression cannot silently downgrade us to
        // presence-only.
        if !result.user_verified() {
            return Err(fail("user_not_verified"));
        }

        let updated = passkey.update_credential(&result).unwrap_or(false);
        let affected = if updated {
            // Compare-and-swap on the stored blob: if another request already
            // advanced this credential, this one is a replay/race — refuse.
            let new_json = serde_json::to_string(&passkey)
                .map_err(|_| LoginFailure::new(K::Internal, Some(&username), "serialize"))?;
            sqlx::query(
                "UPDATE user_passkeys SET passkey = ?, last_used_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND passkey = ?",
            )
            .bind(new_json)
            .bind(row_id)
            .bind(&passkey_json)
            .execute(pool)
            .await
        } else {
            sqlx::query("UPDATE user_passkeys SET last_used_at = CURRENT_TIMESTAMP WHERE id = ?")
                .bind(row_id)
                .execute(pool)
                .await
        }
        .map_err(|e| {
            tracing::warn!(error = %e, "passkey counter write failed");
            LoginFailure::new(K::Internal, Some(&username), "db_error")
        })?;
        if affected.rows_affected() == 0 {
            return Err(fail("concurrent_use"));
        }

        throttle.record_success(&username);
        Ok(LoginSuccess {
            username,
            role,
            must_change_pw: must_change_pw != 0,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;
