//! One-time password setup / reset links (#770).
//!
//! Instead of mailing a plaintext password, account onboarding and
//! password resets send the user a one-time URL
//! (`{base}/password-setup/{token}`) they follow to set their own
//! password — the same shape as a "forgot password" flow.
//!
//! The raw token rides only in the URL; the DB stores its SHA-256 hash
//! (`password_setup_tokens`), so a read of that table can't reconstruct a
//! live link. Tokens are single-use (deleted on a successful set) and
//! one-per-user (a new issue replaces any outstanding one).
//!
//! Three entry points share this machinery:
//!  - account creation with an email (admin) → a `setup` link
//!    (`accounts::create`);
//!  - admin "send link" per account → a `reset` link
//!    (`accounts::reset_link`);
//!  - self-service `POST /api/auth/forgot-password` → a `reset` link.
//!
//! The `GET`/`POST /api/auth/password-setup/{token}` + forgot-password
//! routes are PUBLIC (allow-listed in [`crate::auth::verify`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use crate::api::AppState;
use crate::api::accounts::{MIN_PASSWORD_LEN, hash_password_async};
use crate::audit;
use crate::mail::Mailer;

/// How long a setup/reset link stays valid. Long enough that an
/// onboarding mail sent on a Friday still works Monday; short enough that
/// a leaked-but-unused link doesn't linger. Absolute `expires_at` is
/// stored, so a `-WipeDb` replay (scrambled `recorded_at`) can't revive
/// an old token — and a wipe drops the table entirely anyway.
const TOKEN_TTL_HOURS: i64 = 72;

pub(crate) const PURPOSE_SETUP: &str = "setup";
pub(crate) const PURPOSE_RESET: &str = "reset";

fn sha256_hex(s: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(s.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Absolute base URL (no trailing slash) for links in emails. Prefers the
/// configured `[server] public_url`; otherwise derives it from the
/// request that triggered the send — the `Host` header (always present on
/// HTTP/1.1) with the scheme from `X-Forwarded-Proto` (else `http`).
/// `bind` is unusable here: it's a wildcard (`0.0.0.0:8080`) with no
/// scheme/hostname.
///
/// Used by the **admin-authenticated** create / reset-link paths, where
/// trusting `Host` is acceptable (the caller already holds admin). The
/// **unauthenticated** `forgot_password` path deliberately does NOT use
/// this `Host` fallback — it requires `public_url`, because a spoofed
/// `Host` there would poison the emailed link and exfiltrate the token.
pub(crate) fn link_base(public_url: Option<&str>, headers: &HeaderMap) -> Option<String> {
    if let Some(u) = public_url {
        let u = u.trim().trim_end_matches('/');
        if !u.is_empty() {
            return Some(u.to_string());
        }
    }
    let host = headers.get("host")?.to_str().ok()?;
    if host.is_empty() {
        return None;
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    Some(format!("{scheme}://{host}"))
}

/// Mint a fresh token for `username`, replacing any outstanding one
/// (`username` is UNIQUE), and return the **raw** token for the URL.
pub(crate) async fn issue_token(
    pool: &SqlitePool,
    username: &str,
    purpose: &str,
) -> Result<String, sqlx::Error> {
    let raw = Uuid::new_v4().to_string();
    let token_hash = sha256_hex(&raw);
    let expires_at = Utc::now() + Duration::hours(TOKEN_TTL_HOURS);
    sqlx::query(
        "INSERT INTO password_setup_tokens (token_hash, username, purpose, expires_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(username) DO UPDATE SET \
            token_hash = excluded.token_hash, \
            purpose    = excluded.purpose, \
            created_at = CURRENT_TIMESTAMP, \
            expires_at = excluded.expires_at",
    )
    .bind(&token_hash)
    .bind(username)
    .bind(purpose)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(raw)
}

/// Mail a setup/reset link. Returns whether the SMTP send succeeded so
/// callers can report delivery accurately (vs. just "a token was made").
/// Best-effort at the call sites — a `false` is logged there; the token is
/// already stored so a re-send works. Bilingual (JA + EN) since the
/// backend doesn't track the user's locale. Plaintext; the URL is the
/// only secret.
pub(crate) async fn send_link(
    mailer: &Mailer,
    base: &str,
    email: &str,
    raw_token: &str,
    purpose: &str,
) -> bool {
    let url = format!("{base}/password-setup/{raw_token}");
    let (subject, body) = if purpose == PURPOSE_SETUP {
        (
            "kanade アカウントのパスワード設定 / kanade account password setup",
            format!(
                "kanade のアカウントが作成されました。\n\
                 以下のリンクからパスワードを設定してください（{TOKEN_TTL_HOURS} 時間有効）:\n\
                 {url}\n\
                 心当たりがない場合はこのメールを破棄してください。\n\n\
                 A kanade account has been created for you.\n\
                 Set your password from this link (valid for {TOKEN_TTL_HOURS} hours):\n\
                 {url}\n\
                 If you didn't expect this, you can ignore this email."
            ),
        )
    } else {
        (
            "kanade パスワードの再設定 / kanade password reset",
            format!(
                "kanade のパスワード再設定が要求されました。\n\
                 以下のリンクから新しいパスワードを設定してください（{TOKEN_TTL_HOURS} 時間有効）:\n\
                 {url}\n\
                 心当たりがない場合はこのメールを破棄してください（パスワードは変更されません）。\n\n\
                 A password reset was requested for your kanade account.\n\
                 Set a new password from this link (valid for {TOKEN_TTL_HOURS} hours):\n\
                 {url}\n\
                 If you didn't request this, you can ignore this email (your password won't change)."
            ),
        )
    };
    match mailer.send(&[email.to_owned()], subject, &body).await {
        Ok(()) => true,
        Err(e) => {
            warn!(error = %format!("{e:#}"), purpose, "password link email failed");
            false
        }
    }
}

/// Read-only token lookup (no delete) for the `GET` validation path.
enum TokenLookup {
    Valid { username: String, purpose: String },
    Expired,
    NotFound,
}

async fn lookup_token(pool: &SqlitePool, raw_token: &str) -> Result<TokenLookup, sqlx::Error> {
    let token_hash = sha256_hex(raw_token);
    let row: Option<(String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT username, purpose, expires_at FROM password_setup_tokens WHERE token_hash = ?",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        None => TokenLookup::NotFound,
        Some((_, _, expires_at)) if expires_at < Utc::now() => TokenLookup::Expired,
        Some((username, purpose, _)) => TokenLookup::Valid { username, purpose },
    })
}

#[derive(Serialize)]
pub struct TokenInfo {
    username: String,
    purpose: String,
}

/// `GET /api/auth/password-setup/{token}` — public. Tells the SPA whether
/// to render the set-password form (200), or an expired (410) / invalid
/// (404) message.
pub async fn get_token(
    State(s): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<TokenInfo>, (StatusCode, String)> {
    match lookup_token(&s.pool, &token)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("lookup: {e}")))?
    {
        TokenLookup::Valid { username, purpose } => Ok(Json(TokenInfo { username, purpose })),
        TokenLookup::Expired => Err((StatusCode::GONE, "link expired".into())),
        TokenLookup::NotFound => Err((StatusCode::NOT_FOUND, "invalid link".into())),
    }
}

#[derive(Deserialize)]
pub struct SetReq {
    password: String,
}

/// `POST /api/auth/password-setup/{token}` — public. Sets the user's
/// password and consumes the token (single-use), atomically. Does NOT
/// touch `disabled` — a reset link must not silently re-enable a disabled
/// account.
pub async fn set_password(
    State(s): State<AppState>,
    Path(token): Path<String>,
    Json(req): Json<SetReq>,
) -> Result<StatusCode, (StatusCode, String)> {
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            "password too short (min 8 chars)".into(),
        ));
    }
    let username = match lookup_token(&s.pool, &token)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("lookup: {e}")))?
    {
        TokenLookup::Valid { username, .. } => username,
        TokenLookup::Expired => return Err((StatusCode::GONE, "link expired".into())),
        TokenLookup::NotFound => return Err((StatusCode::NOT_FOUND, "invalid link".into())),
    };

    let hash = hash_password_async(req.password)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "hash failed".into()))?;
    let token_hash = sha256_hex(&token);

    // Set password + consume token in one transaction so a crash can't
    // leave the password changed with the link still live (or vice-versa).
    let mut tx = s
        .pool
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("tx: {e}")))?;
    sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_pw = 0, \
         updated_at = CURRENT_TIMESTAMP WHERE username = ?",
    )
    .bind(&hash)
    .bind(&username)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("update: {e}")))?;
    // Consume the token IN the transaction and require it to still be
    // there: two requests that both passed `lookup_token` race here, but
    // SQLite serialises the writers, so exactly one DELETE removes the row
    // (rows_affected == 1) and the loser sees 0 → roll back and 410. This
    // is what makes the token truly single-use under concurrency.
    let deleted = sqlx::query("DELETE FROM password_setup_tokens WHERE token_hash = ?")
        .bind(&token_hash)
        .execute(&mut *tx)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("consume: {e}")))?
        .rows_affected();
    if deleted == 0 {
        // Best-effort rollback; dropping `tx` also rolls back.
        let _ = tx.rollback().await;
        return Err((StatusCode::GONE, "link already used or expired".into()));
    }
    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("commit: {e}")))?;

    audit::record(
        &s.nats,
        "auth",
        "account.password_set_via_link",
        Some(&username),
        None,
        serde_json::json!({ "username": username }),
    )
    .await;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct ForgotReq {
    username: String,
}

/// `POST /api/auth/forgot-password` — public. **Always** returns 200
/// immediately so a caller can't probe which usernames exist — not by the
/// response, and not by *timing* (the DB lookup + SMTP send happen in a
/// spawned task, so the handler returns in constant time regardless of
/// whether the account exists).
///
/// Crucially this path uses ONLY the configured `[server] public_url`,
/// never the request `Host` header: this endpoint is unauthenticated, so
/// trusting `Host` would let an attacker poison the emailed link with
/// their own domain and exfiltrate the victim's one-time token. With no
/// `public_url` set, self-service reset is simply disabled (an admin can
/// still send links from the trusted Accounts page).
pub async fn forgot_password(State(s): State<AppState>, Json(req): Json<ForgotReq>) -> StatusCode {
    let username = req.username.trim().to_string();
    if !username.is_empty()
        && let Some(mailer) = s.mailer.clone()
        && let Some(base) = s.public_url.clone()
    {
        let pool = s.pool.clone();
        tokio::spawn(async move {
            match email_for_user(&pool, &username).await {
                Ok(Some(email)) => match issue_token(&pool, &username, PURPOSE_RESET).await {
                    Ok(raw) => {
                        send_link(&mailer, &base, &email, &raw, PURPOSE_RESET).await;
                    }
                    Err(e) => warn!(error = %e, "forgot-password: issue token"),
                },
                Ok(None) => { /* unknown user or no email on file — stay silent */ }
                Err(e) => warn!(error = %e, "forgot-password: lookup email"),
            }
        });
    }
    StatusCode::OK
}

/// The account's email, or `None` when the user doesn't exist or has no
/// email on file. (Both collapse to `None` so callers can't distinguish.)
pub(crate) async fn email_for_user(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT email FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(email,)| email).filter(|e| !e.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn link_base_prefers_public_url() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("internal:8080"));
        assert_eq!(
            link_base(Some("https://kanade.example.com/"), &h).as_deref(),
            Some("https://kanade.example.com"),
        );
    }

    #[test]
    fn link_base_falls_back_to_host() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("minipc:8080"));
        assert_eq!(link_base(None, &h).as_deref(), Some("http://minipc:8080"),);
    }

    #[test]
    fn link_base_honours_forwarded_proto() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("kanade.example.com"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            link_base(None, &h).as_deref(),
            Some("https://kanade.example.com"),
        );
    }

    #[test]
    fn link_base_none_without_host_or_config() {
        assert_eq!(link_base(None, &HeaderMap::new()), None);
        // Blank public_url is treated as unset.
        assert_eq!(link_base(Some("  "), &HeaderMap::new()), None);
    }

    #[test]
    fn sha256_is_stable_and_distinct() {
        assert_eq!(sha256_hex("abc"), sha256_hex("abc"));
        assert_ne!(sha256_hex("abc"), sha256_hex("abd"));
        // 32-byte digest → 64 hex chars.
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    // End-to-end token machinery against the real migrated schema:
    // issue → lookup → re-issue (replace) → consume → expiry.
    #[tokio::test]
    async fn token_lifecycle() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (username, password_hash, role) VALUES ('alice', 'x', 'viewer')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Issue + look up by the raw token.
        let raw = issue_token(&pool, "alice", PURPOSE_SETUP).await.unwrap();
        match lookup_token(&pool, &raw).await.unwrap() {
            TokenLookup::Valid { username, purpose } => {
                assert_eq!(username, "alice");
                assert_eq!(purpose, PURPOSE_SETUP);
            }
            _ => panic!("freshly-issued token should be valid"),
        }
        assert!(matches!(
            lookup_token(&pool, "not-a-real-token").await.unwrap(),
            TokenLookup::NotFound
        ));

        // Re-issuing for the same user replaces the old token (UNIQUE).
        let raw2 = issue_token(&pool, "alice", PURPOSE_RESET).await.unwrap();
        assert!(matches!(
            lookup_token(&pool, &raw).await.unwrap(),
            TokenLookup::NotFound
        ));
        assert!(matches!(
            lookup_token(&pool, &raw2).await.unwrap(),
            TokenLookup::Valid { .. }
        ));
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM password_setup_tokens WHERE username = 'alice'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "one outstanding token per user");

        // Consume (the set-password path DELETEs it) → gone.
        sqlx::query("DELETE FROM password_setup_tokens WHERE username = 'alice'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            lookup_token(&pool, &raw2).await.unwrap(),
            TokenLookup::NotFound
        ));

        // A token past its expiry reads as Expired (distinct from invalid).
        sqlx::query(
            "INSERT INTO password_setup_tokens (token_hash, username, purpose, expires_at) \
             VALUES (?, 'alice', 'reset', ?)",
        )
        .bind(sha256_hex("expired-raw"))
        .bind(Utc::now() - Duration::hours(1))
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            lookup_token(&pool, "expired-raw").await.unwrap(),
            TokenLookup::Expired
        ));
    }
}
