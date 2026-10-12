//! Core passkey tests. A software authenticator (`SoftPasskey`) produces real
//! ECDSA attestations/assertions, so these exercise the full webauthn-rs
//! verification path — origin, RP ID, challenge, user verification, counter —
//! against an in-memory SQLite, with no broker and no browser.

use super::*;
use webauthn_authenticator_rs::WebauthnAuthenticator as _;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RegisterPublicKeyCredential};

pub(crate) const ORIGIN: &str = "https://kanade.example.com";
/// A host a browser would happily bind to this RP ID (it is a subdomain of
/// it) but that the server never listed as an origin.
const EVIL: &str = "https://evil.kanade.example.com";
const IP: &str = "198.51.100.7";

pub(crate) fn section(rp_id: &str, origins: &[&str]) -> WebauthnSection {
    WebauthnSection {
        rp_id: rp_id.to_string(),
        rp_origins: origins.iter().map(|s| s.to_string()).collect(),
        rp_name: None,
    }
}

pub(crate) fn svc() -> PasskeyService {
    PasskeyService::new(&section("kanade.example.com", &[ORIGIN])).unwrap()
}

pub(crate) async fn pool() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    for (name, role) in [("alice", "operator"), ("bob", "viewer"), ("root", "admin")] {
        sqlx::query("INSERT INTO users (username, password_hash, role) VALUES (?, 'x', ?)")
            .bind(name)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// What a browser + authenticator do for `navigator.credentials.create`.
/// `tweak` edits the server's options JSON first (e.g. to ask for no UV).
pub(crate) fn create(
    authn: &mut SoftPasskey,
    origin: &str,
    mut options: serde_json::Value,
    tweak: impl FnOnce(&mut serde_json::Value),
) -> RegisterPublicKeyCredential {
    // The soft authenticator cannot hold resident keys; it stands in for one.
    let sel = options
        .pointer_mut("/publicKey/authenticatorSelection")
        .unwrap();
    assert_eq!(
        sel["residentKey"], "required",
        "server must demand discoverable"
    );
    assert_eq!(sel["userVerification"], "required", "server must demand UV");
    sel["residentKey"] = "discouraged".into();
    sel["requireResidentKey"] = false.into();
    tweak(&mut options);
    let ccr: CreationChallengeResponse = serde_json::from_value(options).unwrap();
    authn.do_registration(url(origin), ccr).unwrap()
}

async fn enroll(
    svc: &PasskeyService,
    pool: &SqlitePool,
    authn: &mut SoftPasskey,
    user: &str,
) -> PasskeyInfo {
    let (cid, options) = svc
        .start_register(pool, user, "laptop", &proof(pool, user).await)
        .await
        .unwrap();
    let reg = create(authn, ORIGIN, options, |_| {});
    svc.finish_register(pool, user, &cid, &reg).await.unwrap()
}

pub(crate) async fn handle_of(pool: &SqlitePool, user: &str) -> (Uuid, Vec<u8>) {
    let (u, c): (String, String) = sqlx::query_as(
        "SELECT user_uuid, credential_id FROM user_passkeys WHERE username = ? ORDER BY id LIMIT 1",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    (
        Uuid::parse_str(&u).unwrap(),
        URL_SAFE_NO_PAD.decode(c).unwrap(),
    )
}

/// `navigator.credentials.get` against a discoverable credential: the server
/// sent no allow-list, so the authenticator finds the credential itself and
/// returns the user handle. `tweak` edits the request options first.
pub(crate) fn sign(
    authn: &mut SoftPasskey,
    origin: &str,
    mut rcr: serde_json::Value,
    handle: &(Uuid, Vec<u8>),
    tweak: impl FnOnce(&mut serde_json::Value),
) -> PublicKeyCredential {
    assert_eq!(rcr["publicKey"]["userVerification"], "required");
    assert!(
        rcr["publicKey"]["allowCredentials"]
            .as_array()
            .is_none_or(|a| a.is_empty()),
        "sign-in must not leak or require a username/allow-list"
    );
    assert!(rcr.get("mediation").is_none(), "modal prompt, not autofill");
    rcr["publicKey"]["allowCredentials"] = serde_json::json!([
        { "type": "public-key", "id": URL_SAFE_NO_PAD.encode(&handle.1) }
    ]);
    tweak(&mut rcr);
    let rcr = serde_json::from_value(rcr).unwrap();
    let mut cred = authn.do_authentication(url(origin), rcr).unwrap();
    cred.response.user_handle = Some(handle.0.as_bytes().to_vec());
    cred
}

fn start(svc: &PasskeyService) -> (String, serde_json::Value) {
    let (id, rcr) = svc.start_login(IP).unwrap();
    (id, serde_json::to_value(rcr).unwrap())
}

async fn login(
    svc: &PasskeyService,
    pool: &SqlitePool,
    throttle: &LoginThrottle,
    authn: &mut SoftPasskey,
    handle: &(Uuid, Vec<u8>),
) -> Result<LoginSuccess, LoginFailure> {
    let (cid, rcr) = start(svc);
    let cred = sign(authn, ORIGIN, rcr, handle, |_| {});
    svc.finish_login(pool, throttle, IP, &cid, &cred).await
}

fn reason(r: Result<LoginSuccess, LoginFailure>) -> (LoginErrorKind, &'static str) {
    let f = r.expect_err("login must fail");
    (f.kind, f.reason)
}

// ---- configuration ----------------------------------------------------

#[test]
fn accepts_the_documented_configurations() {
    for (id, origins) in [
        ("kanade.example.com", vec!["https://kanade.example.com"]),
        ("example.com", vec!["https://kanade.example.com"]),
        (
            "example.com",
            vec![
                "https://kanade.example.com",
                "https://kanade.example.com:8443",
            ],
        ),
        (
            "localhost",
            vec!["http://localhost:8080", "https://localhost"],
        ),
        ("kanade", vec!["https://kanade"]),
    ] {
        let rp = validate_config(&section(id, &origins));
        assert!(rp.is_ok(), "{id} {origins:?}: {rp:?}");
        PasskeyService::new(&section(id, &origins)).unwrap();
    }
}

#[test]
fn rejects_unsafe_configurations() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("", &["https://a.example.com"], "empty rp_id"),
        (
            "https://a.example.com",
            &["https://a.example.com"],
            "rp_id with scheme",
        ),
        (
            "a.example.com:8443",
            &["https://a.example.com:8443"],
            "rp_id with port",
        ),
        (
            "a.example.com/x",
            &["https://a.example.com"],
            "rp_id with path",
        ),
        (
            "*.example.com",
            &["https://a.example.com"],
            "wildcard rp_id",
        ),
        (
            "A.Example.com",
            &["https://a.example.com"],
            "uppercase rp_id",
        ),
        ("192.0.2.10", &["https://192.0.2.10"], "IP rp_id"),
        ("a.example.com", &[], "no origins"),
        ("a.example.com", &["http://a.example.com"], "plain http"),
        (
            "a.example.com",
            &["https://*.example.com"],
            "wildcard origin",
        ),
        ("a.example.com", &["https://192.0.2.10"], "IP origin"),
        (
            "a.example.com",
            &["https://b.example.com"],
            "origin outside rp_id",
        ),
        (
            "a.example.com",
            &["https://example.com"],
            "origin is the parent, not a child",
        ),
        (
            "a.example.com",
            &["https://evila.example.com"],
            "suffix confusion",
        ),
        (
            "a.example.com",
            &["https://a.example.com/path"],
            "origin with path",
        ),
        (
            "a.example.com",
            &["https://a.example.com/?q=1"],
            "origin with query",
        ),
        (
            "a.example.com",
            &["https://user@a.example.com"],
            "origin with userinfo",
        ),
        ("a.example.com", &["not a url"], "unparseable origin"),
        (
            "com",
            &["https://example.com"],
            "TLD-style single-label parent",
        ),
        (
            "localhost",
            &["http://evil.localhost.example.com"],
            "localhost rp with foreign origin",
        ),
        (
            "example.com",
            &["http://example.com"],
            "http on a real domain",
        ),
    ];
    for (id, origins, why) in cases {
        assert!(
            validate_config(&section(id, origins)).is_err(),
            "must reject: {why}"
        );
        assert!(PasskeyService::new(&section(id, origins)).is_err(), "{why}");
    }
}

#[test]
fn public_url_mismatch_is_detected() {
    let rp = validate_config(&section("example.com", &[ORIGIN])).unwrap();
    assert!(!public_url_outside_origins(
        "https://kanade.example.com/",
        &rp
    ));
    assert!(public_url_outside_origins("https://other.example.com", &rp));
    assert!(public_url_outside_origins("http://kanade.example.com", &rp));
    assert!(public_url_outside_origins("garbage", &rp));
}

// ---- enrolment ------------------------------------------------------------

#[tokio::test]
async fn enrolled_passkey_is_listed_and_removable_only_by_its_owner() {
    let (svc, pool) = (svc(), pool().await);
    let mut authn = SoftPasskey::new(true);
    let info = enroll(&svc, &pool, &mut authn, "alice").await;
    assert_eq!(info.name, "laptop");
    assert_eq!(list(&pool, "alice").await.unwrap().len(), 1);
    assert!(list(&pool, "bob").await.unwrap().is_empty());
    // Bob cannot delete Alice's credential by guessing its id.
    assert!(!remove(&pool, "bob", info.id).await.unwrap());
    assert_eq!(list(&pool, "alice").await.unwrap().len(), 1);
    assert!(remove(&pool, "alice", info.id).await.unwrap());
    assert!(list(&pool, "alice").await.unwrap().is_empty());
}

#[tokio::test]
async fn one_account_can_hold_several_passkeys_sharing_one_user_handle() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let (mut a, mut b) = (SoftPasskey::new(true), SoftPasskey::new(true));
    enroll(&svc, &pool, &mut a, "alice").await;
    enroll(&svc, &pool, &mut b, "alice").await;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT user_uuid, credential_id FROM user_passkeys WHERE username='alice'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, rows[1].0, "one WebAuthn user handle per account");
    assert_ne!(rows[0].1, rows[1].1);
    // Each authenticator can sign in on its own.
    for (i, authn) in [&mut a, &mut b].into_iter().enumerate() {
        let (u, c) = &rows[i];
        let h = (
            Uuid::parse_str(u).unwrap(),
            URL_SAFE_NO_PAD.decode(c).unwrap(),
        );
        login(&svc, &pool, &throttle, authn, &h).await.unwrap();
    }
}

#[tokio::test]
async fn enrollment_epoch_migration_backfills_legacy_users_and_generates_new_incarnations() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE users (username TEXT PRIMARY KEY, passkey_enrollment_epoch TEXT NOT NULL DEFAULT '');
         INSERT INTO users (username) VALUES ('alice'), ('bob');
         INSERT INTO users VALUES ('reset-user', 'already-rotated');",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/20261013020000_unique_passkey_enrollment_epoch.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let epochs: Vec<String> = sqlx::query_scalar(
        "SELECT passkey_enrollment_epoch FROM users WHERE username IN ('alice', 'bob') ORDER BY username",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(epochs.iter().all(|epoch| epoch.len() == 64));
    assert_ne!(epochs[0], epochs[1]);
    let rotated: String = sqlx::query_scalar(
        "SELECT passkey_enrollment_epoch FROM users WHERE username = 'reset-user'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rotated, "already-rotated");
    sqlx::raw_sql("DELETE FROM users WHERE username = 'alice'; INSERT INTO users (username) VALUES ('alice');")
        .execute(&pool)
        .await
        .unwrap();
    let recreated: String =
        sqlx::query_scalar("SELECT passkey_enrollment_epoch FROM users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recreated.len(), 64);
    assert!(!epochs.contains(&recreated));
}

#[tokio::test]
async fn enrollment_finish_rechecks_current_account_restrictions() {
    for (must_change_pw, disabled) in [(1, 0), (0, 1)] {
        let (svc, pool) = (svc(), pool().await);
        let (id, options) = svc
            .start_register(&pool, "alice", "laptop", &proof(&pool, "alice").await)
            .await
            .unwrap();
        let cred = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        sqlx::query("UPDATE users SET must_change_pw = ?, disabled = ? WHERE username = 'alice'")
            .bind(must_change_pw)
            .bind(disabled)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            svc.finish_register(&pool, "alice", &id, &cred).await,
            Err(RegisterError::Rejected)
        ));
        assert!(list(&pool, "alice").await.unwrap().is_empty());
        // Returning the account to normal cannot replay the spent challenge.
        sqlx::query("UPDATE users SET must_change_pw = 0, disabled = 0 WHERE username = 'alice'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            svc.finish_register(&pool, "alice", &id, &cred).await,
            Err(RegisterError::Challenge(ChallengeError::Invalid))
        ));
    }
}

#[tokio::test]
async fn reset_and_enrollment_finish_cannot_leave_a_revoked_key() {
    let (svc, pool) = (svc(), pool().await);
    // SQLite's in-memory shared cache reports SQLITE_LOCKED immediately
    // for simultaneous writers. A single connection queues the transactions
    // at the SQL boundary without mistaking that harness error for a breach.
    // Cover both serial orderings explicitly before also competing the tasks.
    for reset_first in [false, true] {
        let (id, options) = svc
            .start_register(&pool, "alice", "laptop", &proof(&pool, "alice").await)
            .await
            .unwrap();
        let cred = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        if reset_first {
            remove_all(&pool, "alice").await.unwrap();
            assert!(matches!(
                svc.finish_register(&pool, "alice", &id, &cred).await,
                Err(RegisterError::Rejected)
            ));
        } else {
            svc.finish_register(&pool, "alice", &id, &cred)
                .await
                .unwrap();
            remove_all(&pool, "alice").await.unwrap();
        }
        assert!(list(&pool, "alice").await.unwrap().is_empty());
    }
    for _ in 0..10 {
        let (id, options) = svc
            .start_register(&pool, "alice", "laptop", &proof(&pool, "alice").await)
            .await
            .unwrap();
        let cred = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
        let (finished, reset) = tokio::join!(
            svc.finish_register(&pool, "alice", &id, &cred),
            remove_all(&pool, "alice"),
        );
        reset.unwrap();
        assert!(finished.is_ok() || matches!(finished, Err(RegisterError::Rejected)));
        assert!(list(&pool, "alice").await.unwrap().is_empty());
    }
    // Reset does not permanently disable new, reauthenticated enrollment.
    enroll(&svc, &pool, &mut SoftPasskey::new(true), "alice").await;
}

#[tokio::test]
async fn enrolment_caps_credentials_per_account() {
    let (svc, pool) = (svc(), pool().await);
    for _ in 0..MAX_PASSKEYS_PER_USER {
        enroll(&svc, &pool, &mut SoftPasskey::new(true), "alice").await;
    }
    let err = svc
        .start_register(&pool, "alice", "one too many", &proof(&pool, "alice").await)
        .await;
    assert!(matches!(err, Err(RegisterError::LimitReached)));
}

#[tokio::test]
async fn the_same_authenticator_cannot_be_registered_twice_or_to_two_accounts() {
    let (svc, pool) = (svc(), pool().await);
    let mut authn = SoftPasskey::new(true);
    let (cid, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut authn, ORIGIN, options, |_| {});
    svc.finish_register(&pool, "alice", &cid, &reg)
        .await
        .unwrap();

    // Replaying the very same attestation under a fresh challenge for Bob
    // fails verification (challenge mismatch) rather than attaching it.
    let (cid2, _) = svc
        .start_register(&pool, "bob", "k", &proof(&pool, "bob").await)
        .await
        .unwrap();
    let err = svc.finish_register(&pool, "bob", &cid2, &reg).await;
    assert!(matches!(err, Err(RegisterError::Rejected)), "{err:?}");
    assert!(list(&pool, "bob").await.unwrap().is_empty());

    // And the table itself refuses a duplicate credential id.
    let dup = sqlx::query(
        "INSERT INTO user_passkeys (username, user_uuid, credential_id, passkey, name) \
         SELECT 'bob', user_uuid, credential_id, passkey, 'dup' FROM user_passkeys",
    )
    .execute(&pool)
    .await;
    assert!(dup.is_err(), "credential_id must be UNIQUE across accounts");
}

#[tokio::test]
async fn registration_challenge_is_bound_to_the_account_that_started_it() {
    let (svc, pool) = (svc(), pool().await);
    let (cid, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
    let err = svc.finish_register(&pool, "bob", &cid, &reg).await;
    assert!(matches!(
        err,
        Err(RegisterError::Challenge(ChallengeError::Invalid))
    ));
    // The attempt burned the challenge, so Alice cannot use it afterwards either.
    let err = svc.finish_register(&pool, "alice", &cid, &reg).await;
    assert!(matches!(
        err,
        Err(RegisterError::Challenge(ChallengeError::Invalid))
    ));
    assert!(list(&pool, "alice").await.unwrap().is_empty());
}

#[tokio::test]
async fn registration_rejects_wrong_origin_missing_uv_and_expired_challenges() {
    let (svc, pool) = (svc(), pool().await);

    // Wrong origin: the browser would report the attacker's origin.
    let (cid, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut SoftPasskey::new(true), EVIL, options, |_| {});
    let r = svc.finish_register(&pool, "alice", &cid, &reg).await;
    assert!(matches!(r, Err(RegisterError::Rejected)), "{r:?}");

    // UV not performed: the authenticator is told UV is merely discouraged,
    // so it produces an attestation without the UV flag.
    let (cid, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut SoftPasskey::new(true), ORIGIN, options, |o| {
        o["publicKey"]["authenticatorSelection"]["userVerification"] = "discouraged".into();
    });
    let r = svc.finish_register(&pool, "alice", &cid, &reg).await;
    assert!(matches!(r, Err(RegisterError::Rejected)), "{r:?}");

    // Expired challenge.
    let (cid, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
    svc.expire_all();
    let r = svc.finish_register(&pool, "alice", &cid, &reg).await;
    assert!(matches!(
        r,
        Err(RegisterError::Challenge(ChallengeError::Invalid))
    ));
    assert!(list(&pool, "alice").await.unwrap().is_empty());
}

#[test]
fn labels_are_trimmed_and_bounded() {
    assert_eq!(
        clean_name("  Work laptop \n").as_deref(),
        Some("Work laptop")
    );
    assert_eq!(clean_name("   "), None);
    assert_eq!(clean_name(&"x".repeat(65)), None);
    assert_eq!(clean_name(&"x".repeat(64)).map(|s| s.len()), Some(64));
}

// ---- sign-in --------------------------------------------------------------

#[tokio::test]
async fn passkey_login_resolves_the_account_and_role_from_the_database() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    let ok = login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
    assert_eq!(ok.username, "alice");
    assert_eq!(ok.role, Role::Operator);
    assert!(!ok.must_change_pw);
    let used: Option<String> = sqlx::query_scalar("SELECT last_used_at FROM user_passkeys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(used.is_some(), "last_used_at is recorded");

    // A role change takes effect on the very next sign-in.
    sqlx::query("UPDATE users SET role = 'viewer' WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    let ok = login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
    assert_eq!(ok.role, Role::Viewer);
}

#[tokio::test]
async fn a_challenge_is_single_use_so_a_captured_assertion_cannot_be_replayed() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    let (cid, rcr) = start(&svc);
    let cred = sign(&mut authn, ORIGIN, rcr, &h, |_| {});
    svc.finish_login(&pool, &throttle, IP, &cid, &cred)
        .await
        .unwrap();
    // Same bytes, same challenge id: refused as a spent challenge.
    let replay = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(replay), (LoginErrorKind::Challenge, "bad_challenge"));
    assert_eq!(svc.outstanding(), 0);
}

#[tokio::test]
async fn a_challenge_is_burned_by_a_failed_attempt_too() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    let (cid, rcr) = start(&svc);
    let good = sign(&mut authn, ORIGIN, rcr.clone(), &h, |_| {});
    let bad = sign(&mut authn, EVIL, rcr, &h, |_| {});
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &bad).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));
    // The legitimate response to the same challenge is now too late.
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &good).await;
    assert_eq!(reason(r), (LoginErrorKind::Challenge, "bad_challenge"));
}

#[tokio::test]
async fn expired_and_unknown_challenges_are_rejected() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    let (cid, rcr) = start(&svc);
    let cred = sign(&mut authn, ORIGIN, rcr, &h, |_| {});
    svc.expire_all();
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(r), (LoginErrorKind::Challenge, "bad_challenge"));

    let (_, rcr) = start(&svc);
    let cred = sign(&mut authn, ORIGIN, rcr, &h, |_| {});
    let r = svc
        .finish_login(&pool, &throttle, IP, "never-issued", &cred)
        .await;
    assert_eq!(reason(r), (LoginErrorKind::Challenge, "bad_challenge"));
}

#[tokio::test]
async fn a_login_challenge_cannot_complete_an_enrolment_and_vice_versa() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    let (login_cid, _) = start(&svc);
    let (_, options) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let reg = create(&mut SoftPasskey::new(true), ORIGIN, options, |_| {});
    let r = svc.finish_register(&pool, "alice", &login_cid, &reg).await;
    assert!(matches!(
        r,
        Err(RegisterError::Challenge(ChallengeError::Invalid))
    ));

    let (reg_cid, _) = svc
        .start_register(&pool, "alice", "k", &proof(&pool, "alice").await)
        .await
        .unwrap();
    let (_, rcr) = start(&svc);
    let cred = sign(&mut authn, ORIGIN, rcr, &h, |_| {});
    let r = svc
        .finish_login(&pool, &throttle, IP, &reg_cid, &cred)
        .await;
    assert_eq!(reason(r), (LoginErrorKind::Challenge, "bad_challenge"));
}

#[tokio::test]
async fn login_rejects_wrong_origin_and_missing_user_verification() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    let (cid, rcr) = start(&svc);
    let cred = sign(&mut authn, EVIL, rcr, &h, |_| {});
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));

    // Presence-only assertion (UV flag clear) — what a stolen, unlocked
    // security key would produce.
    let (cid, rcr) = start(&svc);
    let cred = sign(&mut authn, ORIGIN, rcr, &h, |o| {
        o["publicKey"]["userVerification"] = "discouraged".into();
    });
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));

    // Same origin as configured but a different port: origins match exactly.
    let (cid, rcr) = start(&svc);
    let cred = sign(
        &mut authn,
        "https://kanade.example.com:4443",
        rcr,
        &h,
        |_| {},
    );
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));

    // An assertion made for another relying party (other RP ID and origin),
    // answering our challenge.
    let (cid, rcr) = start(&svc);
    let cred = sign(&mut authn, "https://other.example.org", rcr, &h, |o| {
        o["publicKey"]["rpId"] = "other.example.org".into();
    });
    let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));

    // None of that logged anybody in or burned the account.
    login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
}

#[tokio::test]
async fn disabled_accounts_cannot_sign_in_with_a_passkey() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    sqlx::query("UPDATE users SET disabled = 1 WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    let f = login(&svc, &pool, &throttle, &mut authn, &h)
        .await
        .unwrap_err();
    assert_eq!(
        (f.kind, f.reason),
        (LoginErrorKind::Invalid, "account_disabled")
    );
    assert_eq!(f.username.as_deref(), Some("alice"));

    // Re-enabling restores access: the credential itself was never touched.
    sqlx::query("UPDATE users SET disabled = 0 WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
}

#[tokio::test]
async fn must_change_pw_is_reported_so_the_forced_change_gate_still_applies() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    sqlx::query("UPDATE users SET must_change_pw = 1 WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    let ok = login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
    assert!(ok.must_change_pw);
}

#[tokio::test]
async fn credentials_do_not_cross_accounts_and_removed_ones_stop_working() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let (mut a, mut b) = (SoftPasskey::new(true), SoftPasskey::new(true));
    let alice = enroll(&svc, &pool, &mut a, "alice").await;
    enroll(&svc, &pool, &mut b, "bob").await;
    let ha = handle_of(&pool, "alice").await;
    let hb = handle_of(&pool, "bob").await;

    // Alice's key, claiming Bob's user handle: no row matches the pair.
    let forged = (hb.0, ha.1.clone());
    let r = login(&svc, &pool, &throttle, &mut a, &forged).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "unknown_credential"));
    // Bob's key, claiming Alice's handle.
    let forged = (ha.0, hb.1.clone());
    let r = login(&svc, &pool, &throttle, &mut b, &forged).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "unknown_credential"));

    // Removed credential.
    assert!(remove(&pool, "alice", alice.id).await.unwrap());
    let r = login(&svc, &pool, &throttle, &mut a, &ha).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "unknown_credential"));
    // Bob is unaffected.
    login(&svc, &pool, &throttle, &mut b, &hb).await.unwrap();
}

#[tokio::test]
async fn admin_reset_removes_every_credential_of_the_account_only() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let (mut a1, mut a2, mut b) = (
        SoftPasskey::new(true),
        SoftPasskey::new(true),
        SoftPasskey::new(true),
    );
    enroll(&svc, &pool, &mut a1, "alice").await;
    enroll(&svc, &pool, &mut a2, "alice").await;
    enroll(&svc, &pool, &mut b, "bob").await;
    let ha = handle_of(&pool, "alice").await;
    let hb = handle_of(&pool, "bob").await;
    assert_eq!(remove_all(&pool, "alice").await.unwrap(), 2);
    let r = login(&svc, &pool, &throttle, &mut a1, &ha).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "unknown_credential"));
    login(&svc, &pool, &throttle, &mut b, &hb).await.unwrap();
}

#[tokio::test]
async fn a_credential_whose_user_row_vanished_cannot_sign_in() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    // `foreign_keys` is off, so deleting the user leaves the row behind
    // unless the delete handler cleans it up; the JOIN is the backstop.
    sqlx::query("DELETE FROM users WHERE username = 'alice'")
        .execute(&pool)
        .await
        .unwrap();
    let r = login(&svc, &pool, &throttle, &mut authn, &h).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "unknown_credential"));
}

#[tokio::test]
async fn a_rewound_signature_counter_is_treated_as_a_cloned_credential() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    // First sign-in stores counter 1.
    login(&svc, &pool, &throttle, &mut authn, &h).await.unwrap();
    let stored: String = sqlx::query_scalar("SELECT passkey FROM user_passkeys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        stored.contains("\"counter\":1"),
        "counter persisted: {stored}"
    );

    // Pretend a clone of the key already advanced the counter to 50. The real
    // authenticator is now "behind" and must be refused.
    let bumped = stored.replace("\"counter\":1", "\"counter\":50");
    sqlx::query("UPDATE user_passkeys SET passkey = ?")
        .bind(&bumped)
        .execute(&pool)
        .await
        .unwrap();
    let r = login(&svc, &pool, &throttle, &mut authn, &h).await;
    assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));
    let after: String = sqlx::query_scalar("SELECT passkey FROM user_passkeys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after, bumped,
        "a refused sign-in must not rewrite the credential"
    );
}

#[tokio::test]
async fn failed_sign_ins_feed_the_account_and_ip_throttle() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;

    // Five bad assertions for a resolvable account lock it (shared with the
    // password form's counter).
    for _ in 0..5 {
        let (cid, rcr) = start(&svc);
        let cred = sign(&mut authn, EVIL, rcr, &h, |_| {});
        let r = svc.finish_login(&pool, &throttle, IP, &cid, &cred).await;
        assert_eq!(reason(r), (LoginErrorKind::Invalid, "assertion_rejected"));
    }
    let r = login(&svc, &pool, &throttle, &mut authn, &h).await;
    assert!(matches!(r.unwrap_err().kind, LoginErrorKind::Throttled(_)));
    assert!(throttle.check("alice", "203.0.113.1").is_err());
    // …without locking out anyone else.
    assert!(throttle.check("bob", "203.0.113.1").is_ok());
}

#[tokio::test]
async fn unidentifiable_garbage_only_counts_against_the_source_ip() {
    let (svc, pool) = (svc(), pool().await);
    let throttle = LoginThrottle::default();
    let mut authn = SoftPasskey::new(true);
    enroll(&svc, &pool, &mut authn, "alice").await;
    let h = handle_of(&pool, "alice").await;
    for i in 0..40 {
        // A fresh start source each time keeps the start limiter out of the way;
        // the finish calls all come from one attacker address.
        let (cid, rcr) = svc.start_login(&format!("10.2.0.{i}")).unwrap();
        let mut cred = sign(
            &mut authn,
            ORIGIN,
            serde_json::to_value(rcr).unwrap(),
            &h,
            |_| {},
        );
        cred.response.user_handle = None;
        let r = svc
            .finish_login(&pool, &throttle, "198.51.100.99", &cid, &cred)
            .await;
        assert!(r.is_err());
        if throttle.check_ip("198.51.100.99").is_err() {
            break;
        }
    }
    assert!(
        throttle.check_ip("198.51.100.99").is_err(),
        "IP budget exhausted"
    );
    let r = login(&svc, &pool, &throttle, &mut authn, &h).await;
    assert!(r.is_ok(), "a legitimate user on another IP is unaffected");
}

// ---- challenge store ------------------------------------------------------

#[test]
fn login_start_is_rate_limited_per_ip_and_bounded_overall() {
    let svc = svc();
    for _ in 0..START_MAX_PER_IP {
        svc.start_login("192.0.2.1").unwrap();
    }
    assert!(matches!(
        svc.start_login("192.0.2.1"),
        Err(ChallengeError::RateLimited(_))
    ));
    // Another source is independent…
    svc.start_login("192.0.2.2").unwrap();
    // …until the global cap, which fails closed rather than growing.
    let svc = self::svc();
    let now = Instant::now();
    for i in 0..MAX_OUTSTANDING {
        svc.start_login_at(&format!("10.0.{}.{}", i / 250, i % 250), now)
            .unwrap();
    }
    assert!(matches!(
        svc.start_login_at("10.9.9.9", now),
        Err(ChallengeError::Busy)
    ));
    // Expired entries are reclaimed, so the cap recovers on its own.
    svc.expire_all();
    svc.start_login_at("10.9.9.9", Instant::now() + Duration::from_secs(1))
        .unwrap();
}

#[test]
fn challenge_ids_are_unguessable_and_unique() {
    let svc = svc();
    let a = svc.start_login("192.0.2.1").unwrap().0;
    let b = svc.start_login("192.0.2.1").unwrap().0;
    assert_ne!(a, b);
    assert!(a.len() >= 43, "256 bits of entropy, base64url");
}

// ---- wire contract with the SPA ---------------------------------------------

/// `web/src/lib/passkey.ts` hand-builds these bodies from `navigator.credentials`
/// output (base64url, no `transports`, `extensions: {}`, `userHandle: null` when
/// the authenticator sent none). Pin that the server parses exactly that shape.
#[test]
fn the_spas_credential_json_deserialises() {
    let reg: RegisterPublicKeyCredential = serde_json::from_value(serde_json::json!({
        "id": "AQID", "rawId": "AQID", "type": "public-key",
        "response": { "attestationObject": "o2NmbXRkbm9uZQ", "clientDataJSON": "e30" },
        "extensions": {},
    }))
    .unwrap();
    assert_eq!(reg.raw_id, [1, 2, 3]);

    let auth: PublicKeyCredential = serde_json::from_value(serde_json::json!({
        "id": "AQID", "rawId": "AQID", "type": "public-key",
        "response": {
            "authenticatorData": "AQID", "clientDataJSON": "e30",
            "signature": "AQID", "userHandle": null,
        },
        "extensions": {},
    }))
    .unwrap();
    assert!(auth.get_user_unique_id().is_none());
    let with_handle: PublicKeyCredential = serde_json::from_value(serde_json::json!({
        "id": "AQID", "rawId": "AQID", "type": "public-key",
        "response": {
            "authenticatorData": "AQID", "clientDataJSON": "e30",
            "signature": "AQID", "userHandle": "BAUG",
        },
        "extensions": {},
    }))
    .unwrap();
    assert_eq!(with_handle.get_user_unique_id(), Some(&[4u8, 5, 6][..]));
}

async fn proof(pool: &SqlitePool, user: &str) -> ReauthProof {
    let (must_change_pw, enrollment_epoch): (i64, String) = sqlx::query_as(
        "SELECT must_change_pw, passkey_enrollment_epoch FROM users WHERE username = ?",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    ReauthProof {
        must_change_pw: must_change_pw != 0,
        enrollment_epoch,
    }
}

#[tokio::test]
async fn migration_assigns_unique_generations_to_legacy_and_recreated_accounts() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql("CREATE TABLE users (username TEXT PRIMARY KEY); INSERT INTO users VALUES ('alice'), ('bob');")
        .execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/20261013000000_passkeys.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let before: Vec<String> =
        sqlx::query_scalar("SELECT passkey_enrollment_epoch FROM users ORDER BY username")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|epoch| epoch.len() == 64));
    assert_ne!(before[0], before[1]);
    sqlx::raw_sql(
        "DELETE FROM users WHERE username = 'alice'; INSERT INTO users VALUES ('alice', '');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let after: String =
        sqlx::query_scalar("SELECT passkey_enrollment_epoch FROM users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after.len(), 64);
    assert!(!before.contains(&after));
}

#[tokio::test]
async fn factor_change_rotates_epoch_atomically_and_rollback_preserves_proof() {
    let pool = pool().await;
    let original = proof(&pool, "alice").await.enrollment_epoch;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE users SET password_hash = 'replacement', totp_secret = 'new secret' WHERE username = 'alice'")
        .execute(&mut *tx).await.unwrap();
    let changed: String =
        sqlx::query_scalar("SELECT passkey_enrollment_epoch FROM users WHERE username = 'alice'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_ne!(
        original, changed,
        "factor and epoch changes share a transaction"
    );
    tx.rollback().await.unwrap();
    assert_eq!(original, proof(&pool, "alice").await.enrollment_epoch);
    let factors: (String, Option<String>) =
        sqlx::query_as("SELECT password_hash, totp_secret FROM users WHERE username = 'alice'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(factors, ("x".into(), None));
    // An unchanged assignment should not revoke a still-valid snapshot.
    sqlx::query("UPDATE users SET password_hash = password_hash, totp_secret = totp_secret WHERE username = 'alice'")
        .execute(&pool).await.unwrap();
    assert_eq!(original, proof(&pool, "alice").await.enrollment_epoch);
}
