//! Authentication + RBAC middleware for `/api/*`.
//!
//! Resolution order at request time:
//!
//!   1. `KANADE_AUTH_DISABLE=1` — open access, synthesised admin
//!      identity. Dev / local-only.
//!   2. `/api/auth/login` — public (the SPA / CLI POST credentials here
//!      to obtain a JWT, so it must be reachable without one).
//!   3. Static token (service token) — shared bearer secret. Caller
//!      sends `Authorization: Bearer <secret>`; a constant-time compare
//!      grants an **admin-equivalent** identity. For CI / non-interactive
//!      automation. A non-matching bearer falls through to JWT below
//!      (so user JWTs and a service token coexist).
//!   4. JWT mode (HS256) — `aud=kanade` + `exp`. On a valid signature the
//!      caller's `sub` is looked up in the SQLite `users` table and the
//!      **DB row is authoritative**: a missing row → 401, a `disabled`
//!      row → 401 as well (**not** 403 — the SPA's expired-token path then
//!      logs the user out instead of trapping them on a page that spams
//!      permission toasts, Gemini #331), otherwise the DB `role` (not the
//!      token's claim) is injected into [`Claims`]. This makes `disable` /
//!      role changes take effect immediately rather than waiting for `exp`.
//!
//! Secret resolution is registry-first, env-second:
//!
//! ```text
//! StaticToken:  HKLM\SOFTWARE\kanade\backend\StaticToken
//!               → $KANADE_AUTH_STATIC_TOKEN
//! JwtSecret:    HKLM\SOFTWARE\kanade\backend\JwtSecret
//!               → $KANADE_JWT_SECRET   (required for account login)
//! ```
//!
//! `JwtSecret` is the fleet-wide skeleton key (anyone holding it can
//! mint admin tokens), so it lives **only on the backend host** — agents
//! and the CLI never need it.
//!
//! Steps 1, 3 and 4 live in [`verify_bearer`] rather than in the middleware
//! body, because not every route can be authenticated by a layer: a browser
//! cannot set an `Authorization` header on a WebSocket, so the
//! remote-assistance socket (#1140) verifies its `Sec-WebSocket-Protocol`
//! credential by calling [`verify_bearer`] directly. One implementation, one
//! place where the DB stays authoritative.
//!
//! Per-route role enforcement is via the [`require_operator`] /
//! [`require_admin`] `route_layer` middleware applied to the mutating /
//! admin route groups in [`crate::api::router`]; under-privileged
//! callers are rejected with `403` before the handler runs.

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use kanade_shared::feature::Feature;
use kanade_shared::secrets;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::env;
use std::sync::OnceLock;
use tracing::{error, warn};

/// Hierarchical role: `Viewer < Operator < Admin` (the derived `Ord`
/// follows declaration order). `admin ⊇ operator ⊇ viewer`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    /// True when this role satisfies a route's minimum requirement.
    pub fn allows(self, required: Role) -> bool {
        self >= required
    }
}

/// JWT claims yielded by the validator. Stashed in the request
/// extensions so downstream handlers can introspect the caller.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Claims {
    pub sub: String,
    pub exp: i64,
    #[serde(default)]
    pub aud: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    /// Per-account page allow-list, resolved from the DB
    /// (`users.allowed_features`) by [`verify`] — **not** trusted from the
    /// token. `None` = unrestricted (every page). `Some(list)` restricts the
    /// caller to those features plus the always-open commons (see
    /// [`crate::api::feature_for_path`]). `skip_serializing_if` keeps it out
    /// of minted JWTs entirely: like `roles`, the DB is authoritative and
    /// re-read on every request, so a token never carries a stale allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_features: Option<Vec<Feature>>,
}

impl Claims {
    /// The caller's effective role — the highest parseable entry in
    /// `roles`, defaulting to the least privilege (`Viewer`) when the
    /// token carries no recognised role.
    pub fn role(&self) -> Role {
        self.roles
            .iter()
            .filter_map(|r| Role::parse(r))
            .max()
            .unwrap_or(Role::Viewer)
    }

    /// Synthesised admin identity for the two table-less paths
    /// (`KANADE_AUTH_DISABLE` and the static service token).
    fn service(sub: &str) -> Self {
        Claims {
            sub: sub.to_string(),
            exp: 4_102_444_800, // 2100-01-01
            aud: Some(EXPECTED_AUDIENCE.to_string()),
            roles: vec![Role::Admin.as_str().to_string()],
            // Service / auth-disabled identities are unrestricted — they are
            // the escape hatch that can always reach every page (e.g. to
            // un-restrict an account that locked itself out of `/accounts`).
            allowed_features: None,
        }
    }
}

const ENV_DISABLE: &str = "KANADE_AUTH_DISABLE";
const ENV_STATIC_TOKEN: &str = "KANADE_AUTH_STATIC_TOKEN";
const ENV_SECRET: &str = "KANADE_JWT_SECRET";
const REG_SUBKEY: &str = r"SOFTWARE\kanade\backend";
const REG_STATIC_TOKEN: &str = "StaticToken";
const REG_JWT_SECRET: &str = "JwtSecret";
pub const EXPECTED_AUDIENCE: &str = "kanade";

/// Resolve the static service token, **cached for the process lifetime**.
/// Reading the registry / env on every request would be needless syscalls
/// at fleet scale (Gemini #331); a rotated token requires a backend
/// restart, which deploys already do.
fn resolve_static_token() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Some(t) = secrets::read_hklm_value(REG_SUBKEY, REG_STATIC_TOKEN) {
                return Some(t);
            }
            match env::var(ENV_STATIC_TOKEN) {
                Ok(t) if !t.is_empty() => Some(t),
                _ => None,
            }
        })
        .as_deref()
}

/// Resolve the HS256 signing/verification secret. Returns `None` when
/// neither the registry value nor the env var is set.
fn resolve_jwt_secret() -> Option<String> {
    if let Some(s) = secrets::read_hklm_value(REG_SUBKEY, REG_JWT_SECRET) {
        return Some(s);
    }
    match env::var(ENV_SECRET) {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// The HS256 secret used to both verify and mint tokens, **cached for the
/// process lifetime** (same registry/restart rationale as the static
/// token). Falls back to a loud dev default when unset so `cargo run`
/// works — never acceptable in production, hence the one-time warning.
pub fn signing_secret() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        resolve_jwt_secret().unwrap_or_else(|| {
            warn!(
                "no JwtSecret registry value and no $KANADE_JWT_SECRET — using a hard-coded dev fallback (NEVER in production)"
            );
            "dev-secret-please-override".to_string()
        })
    })
}

/// The authoritative account facts [`verify`] re-reads on every request.
struct UserAuth {
    role: Role,
    disabled: bool,
    /// The account's **effective** page allow-list. `None` = unrestricted;
    /// `Some(list)` = restricted to those pages. Resolved with the permission
    /// group taking precedence over the per-user list (see [`lookup_user`]).
    /// Unknown / retired feature keys are dropped (a shrunk catalog must not
    /// deny access on a key the backend no longer knows).
    allowed_features: Option<Vec<Feature>>,
}

/// Look up a user's authoritative `(role, disabled, effective allow-list)`
/// from SQLite. A row whose `role` column fails to parse is treated as absent
/// (deny).
///
/// The effective allow-list is the account's **permission group** (#1008
/// Phase 3, live-referenced via the join) when it has one, otherwise its own
/// `allowed_features` (whose `NULL` means unrestricted). A group is always a
/// concrete set (its `'[]'` means "commons only"), so it wins over the
/// per-user column.
///
/// **Fail-closed for a group-assigned account** (Gemini HIGH on #1014): once
/// an account has a `permission_group`, its effective access can never be
/// `None`/unrestricted, even if that group is missing or its JSON is corrupt.
/// A dangling group (which the atomic `DELETE` guard already prevents) resolves
/// group → per-user list → `Some(Vec::new())` (commons only), never falling
/// open to every page. Only an account with **no** group falls through to the
/// per-user `NULL` = unrestricted default.
async fn lookup_user(pool: &SqlitePool, username: &str) -> Result<Option<UserAuth>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, Option<String>, Option<String>, Option<String>)>(
        "SELECT u.role, u.disabled, u.allowed_features, g.features, u.permission_group \
         FROM users u \
         LEFT JOIN permission_groups g ON u.permission_group = g.name \
         WHERE u.username = ?",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(
        |(role, disabled, allowed, group_features, permission_group)| {
            Role::parse(&role).map(|role| {
                let allowed_features = if permission_group.is_some() {
                    // Group intended → never unrestricted. Prefer the group's
                    // set, then the per-user list, then commons-only.
                    parse_allowed_features(group_features.as_deref())
                        .or_else(|| parse_allowed_features(allowed.as_deref()))
                        .or_else(|| Some(Vec::new()))
                } else {
                    parse_allowed_features(allowed.as_deref())
                };
                UserAuth {
                    role,
                    disabled: disabled != 0,
                    allowed_features,
                }
            })
        },
    ))
}

/// Parse the stored `allowed_features` JSON (`NULL`/absent → `None` =
/// unrestricted). A malformed or non-array value is treated as `None`
/// (unrestricted) with a warning rather than locking the account out — a
/// corrupt column should fail *open* for its owner, never silently deny
/// every page. Individual unknown keys are dropped.
fn parse_allowed_features(raw: Option<&str>) -> Option<Vec<Feature>> {
    let raw = raw?;
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(keys) => Some(keys.iter().filter_map(|k| Feature::parse(k)).collect()),
        Err(e) => {
            warn!(error = %e, "malformed allowed_features JSON; treating as unrestricted");
            None
        }
    }
}

/// True for `/api/remote/{pc_id}/ws` — the one `/api/*` route [`verify`]
/// waves through for the handler to authenticate itself.
///
/// Matched against the raw request path, because this middleware runs before
/// routing and so has no `MatchedPath` to compare. Deliberately strict about
/// the shape: a `pc_id` containing `/` would let a deeper path opt itself out
/// of authentication by ending in `/ws`, and this is the only allow-list
/// entry whose route is not public.
fn is_remote_ws_path(path: &str) -> bool {
    path.strip_prefix("/api/remote/")
        .and_then(|rest| rest.strip_suffix("/ws"))
        .is_some_and(|pc_id| !pc_id.is_empty() && !pc_id.contains('/'))
}

/// Resolve the caller's authoritative identity from a bearer credential —
/// steps 1, 3 and 4 of the module doc, in that order.
///
/// This is the **single** implementation of "who is this caller", shared by
/// the `/api/*` middleware ([`verify`]) and by the routes that cannot go
/// through it. A browser cannot set an `Authorization` header on a WebSocket,
/// so the remote-assistance socket (#1140) carries its credential in
/// `Sec-WebSocket-Protocol` and verifies it here instead of via the layer.
/// Sharing one function is what keeps the DB authoritative everywhere: `role`,
/// `disabled` and `allowed_features` are re-read on every call, so a
/// second verifier can never become the one path where an account change
/// doesn't take effect.
///
/// `token` is the raw credential *without* the `Bearer ` prefix; blank and
/// whitespace-only values are treated as absent. Every `Err` on this path
/// maps to a `401` (see [`unauth`]) — including a disabled account, so the
/// SPA's expired-token path logs the user out rather than trapping them on a
/// page that spams permission toasts (Gemini #331). The string is the reason;
/// callers own the logging so each can attach its own context.
pub async fn verify_bearer(pool: &SqlitePool, token: Option<&str>) -> Result<Claims, String> {
    // 1. Auth opt-out for local development. [`verify`] has already
    //    short-circuited on this before it gets here (its check covers the
    //    untokened paths too), so this branch is what the non-middleware
    //    callers rely on — without it, a dev backend started with
    //    `KANADE_AUTH_DISABLE=1` would refuse the WebSocket for want of a
    //    credential the SPA never obtained.
    if env::var(ENV_DISABLE).is_ok() {
        return Ok(Claims::service("auth-disabled"));
    }
    verify_token(pool, token).await
}

/// The token-dependent half of [`verify_bearer`], split out so the tests can
/// exercise it without mutating process-global environment state (which would
/// race every other test in the binary).
async fn verify_token(pool: &SqlitePool, token: Option<&str>) -> Result<Claims, String> {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err("missing bearer token".to_string());
    };

    // 3. Static service token: admin-equivalent. A mismatch is NOT a
    //    rejection — fall through to JWT so user tokens coexist.
    if let Some(expected) = resolve_static_token()
        && constant_time_eq(token.as_bytes(), expected.as_bytes())
    {
        return Ok(Claims::service("service-token"));
    }

    // 4. JWT mode.
    let secret = signing_secret();
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&[EXPECTED_AUDIENCE]);

    let claims = match decode::<Claims>(token, &key, &validation) {
        Ok(data) => data.claims,
        Err(e) => return Err(format!("invalid token: {e}")),
    };

    // DB is authoritative: re-read role + disabled now so account
    // changes apply immediately rather than at the token's exp.
    match lookup_user(pool, &claims.sub).await {
        Ok(Some(user)) => {
            if user.disabled {
                return Err("account disabled".to_string());
            }
            let mut claims = claims;
            // DB is authoritative for BOTH role and the page allow-list —
            // overwrite whatever the token carried (the mint path never sets
            // allowed_features, but a hand-crafted token might).
            claims.roles = vec![user.role.as_str().to_string()];
            claims.allowed_features = user.allowed_features;
            Ok(claims)
        }
        Ok(None) => Err("unknown account".to_string()),
        Err(e) => {
            // Fail closed: a DB hiccup must not grant access. Logged here
            // rather than at the call site because it is an infrastructure
            // fault, not a rejected caller — the reason handed back is
            // deliberately vague.
            error!(error = %e, sub = %claims.sub, "user lookup failed");
            Err("auth backend unavailable".to_string())
        }
    }
}

pub async fn verify(
    State(pool): State<SqlitePool>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    // 1. Auth opt-out for local development. Checked here rather than left
    //    to [`verify_bearer`] below because this one covers *every* path —
    //    the SPA static files and /health included — while the call below is
    //    only reached by the tokened `/api/*` routes.
    if env::var(ENV_DISABLE).is_ok() {
        let mut req = req;
        req.extensions_mut()
            .insert(Claims::service("auth-disabled"));
        return Ok(next.run(req).await);
    }

    // Only /api/* is protected; the SPA static files at / and the
    // health probe at /health stay public.
    let path = req.uri().path();
    if !path.starts_with("/api/") {
        return Ok(next.run(req).await);
    }

    // 2. Public endpoints reachable without a token: the login route, the
    //    backend version probe (so the SPA can show it pre-login), and the
    //    #770 password setup/reset link + forgot-password flow (the user
    //    has no session yet — the one-time token IS the credential).
    if path == "/api/auth/login"
        || path == "/api/version"
        || path == "/api/auth/forgot-password"
        || path.starts_with("/api/auth/password-setup/")
    {
        return Ok(next.run(req).await);
    }

    // 2b. The remote-assistance WebSocket is **not** public — it is the one
    //     route this middleware cannot authenticate. A browser cannot set an
    //     `Authorization` header on a WebSocket, so its credential arrives in
    //     `Sec-WebSocket-Protocol` and the handler verifies it by calling
    //     [`verify_bearer`] directly. It also re-checks the role and the page
    //     permission, because bypassing this layer means [`require_operator`]
    //     and [`require_features`] have no [`Claims`] to read. See
    //     [`crate::api::remote`].
    if is_remote_ws_path(path) {
        return Ok(next.run(req).await);
    }

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    match verify_bearer(&pool, token).await {
        Ok(claims) => {
            let mut req = req;
            req.extensions_mut().insert(claims);
            Ok(next.run(req).await)
        }
        Err(reason) => {
            // The path is the context worth having in the log, and it is only
            // known here — which is why [`verify_token`] returns the reason
            // instead of logging it.
            warn!(path, reason, "auth rejected");
            Err(unauth(&reason))
        }
    }
}

/// Returns `Some(403)` unless the caller (identity injected by
/// [`verify`]) holds at least `required`; `None` when the request may
/// proceed. Used as a `route_layer` over the mutating / admin route
/// groups in [`crate::api::router`], so individual handlers stay free of
/// role boilerplate.
fn gate(req: &Request, required: Role) -> Option<Response> {
    let Some(claims) = req.extensions().get::<Claims>().cloned() else {
        return Some(forbidden("no authenticated identity"));
    };
    if claims.role().allows(required) {
        None
    } else {
        Some(forbidden(&format!(
            "{} role required (caller is {})",
            required.as_str(),
            claims.role().as_str()
        )))
    }
}

/// `route_layer` middleware: caller must be at least `operator`.
pub async fn require_operator(req: Request, next: Next) -> Result<Response, Response> {
    if let Some(rejection) = gate(&req, Role::Operator) {
        return Err(rejection);
    }
    Ok(next.run(req).await)
}

/// `route_layer` middleware: caller must be `admin`.
pub async fn require_admin(req: Request, next: Next) -> Result<Response, Response> {
    if let Some(rejection) = gate(&req, Role::Admin) {
        return Err(rejection);
    }
    Ok(next.run(req).await)
}

/// Per-account **page** enforcement (hard, `403`) — the horizontal axis
/// orthogonal to the vertical role gates above.
///
/// Layered over the whole API router (inside [`crate::api::router`], so it
/// runs *after* [`verify`] has injected [`Claims`] and *after* routing has
/// populated [`MatchedPath`]). The decision:
///
///   * caller unrestricted (`allowed_features == None`, or no identity /
///     service token) → allow;
///   * the matched route is **commons** (`feature_for_path` → `None`:
///     login, self-service, the Dashboard landing feeds, shared substrate)
///     → allow;
///   * otherwise allow iff the caller's allow-list intersects the route's
///     feature(s); else `403`.
///
/// Commons-by-default (an unmapped route is open to any authenticated
/// caller) is deliberate: most endpoints are shared substrate, and the
/// sensitive per-page data lives behind the mapped routes. A NEW topical
/// endpoint must be added to `feature_for_path` to be gated.
pub async fn require_features(req: Request, next: Next) -> Result<Response, Response> {
    // Decide entirely within a borrow of `req.extensions()` and yield only
    // owned data (`Feature::as_str` is `&'static str`), so no borrow — and no
    // clone of the allow-list — outlives the block. Then `req` is free to move
    // into `next.run`.
    let denied: Option<&'static str> = {
        let ext = req.extensions();
        match ext
            .get::<Claims>()
            .and_then(|c| c.allowed_features.as_ref())
        {
            // No authenticated identity (public route) or an unrestricted
            // caller (service token / NULL allow-list) → nothing to enforce.
            None => None,
            Some(allowed) => match ext
                .get::<MatchedPath>()
                .and_then(|m| crate::api::feature_for_path(m.as_str()))
            {
                // Commons route, or the caller is permitted this page.
                None => None,
                Some(feature) if allowed.contains(&feature) => None,
                Some(feature) => Some(feature.as_str()),
            },
        }
    };

    match denied {
        None => Ok(next.run(req).await),
        Some(want) => Err(forbidden(&format!(
            "account not permitted to access this page (requires {want})"
        ))),
    }
}

/// Length-checked, branch-free byte comparison. Tiny inline impl —
/// we don't want a crate dep just for this and the use site is a
/// non-adversarial config path anyway, but the constant-time shape
/// future-proofs us if the token surface ever gets exposed to
/// untrusted callers.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauth(msg: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Body::from(msg.to_owned())).into_response()
}

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, Body::from(msg.to_owned())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_hierarchy() {
        assert!(Role::Admin.allows(Role::Operator));
        assert!(Role::Admin.allows(Role::Viewer));
        assert!(Role::Operator.allows(Role::Viewer));
        assert!(!Role::Operator.allows(Role::Admin));
        assert!(!Role::Viewer.allows(Role::Operator));
        assert!(Role::Viewer.allows(Role::Viewer));
    }

    #[test]
    fn role_roundtrip() {
        for r in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn jwt_hs256_roundtrip_does_not_panic() {
        // Regression: jsonwebtoken 10.x routes HS256 through a rustls-
        // style CryptoProvider and *panics at runtime* on the first
        // encode/decode unless a provider feature is pinned (see the dep
        // note in the workspace Cargo.toml). No test minted a token
        // before, so the panic shipped in v0.43.14 and only surfaced on
        // the first SPA login. This exercises the exact crypto path so CI
        // catches a missing / ambiguous provider feature instead of it
        // blowing up at runtime.
        use jsonwebtoken::{EncodingKey, Header, encode};

        let claims = Claims {
            sub: "alice".into(),
            exp: 4_102_444_800,
            aud: Some(EXPECTED_AUDIENCE.to_string()),
            roles: vec![Role::Admin.as_str().to_string()],
            allowed_features: None,
        };
        let key = b"regression-secret";
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(key),
        )
        .expect("HS256 encode must not panic — pin a jsonwebtoken CryptoProvider feature");

        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[EXPECTED_AUDIENCE]);
        let decoded = decode::<Claims>(&token, &DecodingKey::from_secret(key), &validation)
            .expect("HS256 decode of our own token");
        assert_eq!(decoded.claims.sub, "alice");
        assert_eq!(decoded.claims.role(), Role::Admin);
    }

    #[test]
    fn allowed_features_parse() {
        // NULL / absent → unrestricted.
        assert!(parse_allowed_features(None).is_none());
        // A JSON array → those features, unknown keys dropped.
        let got = parse_allowed_features(Some(r#"["compliance","inventory","bogus"]"#)).unwrap();
        assert_eq!(got, vec![Feature::Compliance, Feature::Inventory]);
        // Empty array is a REAL restriction (commons only) — not None.
        assert_eq!(parse_allowed_features(Some("[]")), Some(vec![]));
        // Malformed → fail open (None), never lock the owner out.
        assert!(parse_allowed_features(Some("not json")).is_none());
        assert!(parse_allowed_features(Some(r#"{"x":1}"#)).is_none());
    }

    #[tokio::test]
    async fn effective_features_group_precedence() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO permission_groups (name, features) VALUES ('sec', '[\"compliance\"]')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO permission_groups (name, features) VALUES ('locked', '[]')")
            .execute(&pool)
            .await
            .unwrap();

        let eff = |name: &'static str| {
            let pool = pool.clone();
            async move {
                lookup_user(&pool, name)
                    .await
                    .unwrap()
                    .unwrap()
                    .allowed_features
            }
        };

        // Group wins over the per-user list.
        sqlx::query("INSERT INTO users (username, password_hash, role, allowed_features, permission_group) VALUES ('g', 'x', 'viewer', '[\"audit\"]', 'sec')")
            .execute(&pool).await.unwrap();
        assert_eq!(eff("g").await, Some(vec![Feature::Compliance]));

        // A group with `[]` restricts to commons only — NOT unrestricted.
        sqlx::query("INSERT INTO users (username, password_hash, role, permission_group) VALUES ('l', 'x', 'viewer', 'locked')")
            .execute(&pool).await.unwrap();
        assert_eq!(eff("l").await, Some(vec![]));

        // No group → fall back to the per-user list.
        sqlx::query("INSERT INTO users (username, password_hash, role, allowed_features) VALUES ('p', 'x', 'viewer', '[\"audit\"]')")
            .execute(&pool).await.unwrap();
        assert_eq!(eff("p").await, Some(vec![Feature::Audit]));

        // Dangling group (references a missing group) → fall back to per-user.
        sqlx::query("INSERT INTO users (username, password_hash, role, allowed_features, permission_group) VALUES ('d', 'x', 'viewer', '[\"logs\"]', 'ghost')")
            .execute(&pool).await.unwrap();
        assert_eq!(eff("d").await, Some(vec![Feature::Logs]));

        // Dangling group with NO per-user list → commons-only, NEVER
        // unrestricted (fail-closed; a group-assigned account can't escalate).
        sqlx::query("INSERT INTO users (username, password_hash, role, permission_group) VALUES ('x', 'x', 'admin', 'ghost')")
            .execute(&pool).await.unwrap();
        assert_eq!(eff("x").await, Some(vec![]));

        // No group and no per-user list → unrestricted.
        sqlx::query("INSERT INTO users (username, password_hash, role) VALUES ('u', 'x', 'admin')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(eff("u").await, None);
    }

    /// Mint a token the way `/api/auth/login` does, with a caller-chosen
    /// `roles` claim so the tests can prove the DB overrides it.
    fn mint(sub: &str, roles: &[&str]) -> String {
        use jsonwebtoken::{EncodingKey, Header, encode};
        let claims = Claims {
            sub: sub.into(),
            exp: 4_102_444_800,
            aud: Some(EXPECTED_AUDIENCE.to_string()),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            allowed_features: None,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(signing_secret().as_bytes()),
        )
        .expect("mint")
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn verify_token_rejects_absent_and_blank_credentials() {
        let pool = test_pool().await;
        for token in [None, Some(""), Some("   ")] {
            assert_eq!(
                verify_token(&pool, token).await.unwrap_err(),
                "missing bearer token"
            );
        }
    }

    #[tokio::test]
    async fn verify_token_rejects_a_bad_signature() {
        let pool = test_pool().await;
        let err = verify_token(&pool, Some("not.a.jwt")).await.unwrap_err();
        assert!(err.starts_with("invalid token: "), "{err}");
    }

    #[tokio::test]
    async fn verify_token_lets_the_db_override_the_token() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, allowed_features) \
             VALUES ('alice', 'x', 'viewer', '[\"audit\"]')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // The token claims admin and carries no allow-list; the DB row says
        // viewer, restricted to Audit. The DB wins on both — this is the
        // property the WebSocket path must not lose by verifying separately.
        let claims = verify_token(&pool, Some(&mint("alice", &["admin"])))
            .await
            .expect("accepted");
        assert_eq!(claims.role(), Role::Viewer);
        assert_eq!(claims.allowed_features, Some(vec![Feature::Audit]));
        assert_eq!(claims.sub, "alice");
    }

    #[tokio::test]
    async fn verify_token_rejects_disabled_and_unknown_accounts() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, disabled) \
             VALUES ('bob', 'x', 'admin', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // A still-valid token whose account was disabled since it was minted.
        assert_eq!(
            verify_token(&pool, Some(&mint("bob", &["admin"])))
                .await
                .unwrap_err(),
            "account disabled"
        );
        // A well-signed token for an account that no longer exists.
        assert_eq!(
            verify_token(&pool, Some(&mint("ghost", &["admin"])))
                .await
                .unwrap_err(),
            "unknown account"
        );
    }

    #[tokio::test]
    async fn verify_token_trims_surrounding_whitespace() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO users (username, password_hash, role) VALUES ('carol', 'x', 'operator')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let padded = format!("  {}\t", mint("carol", &["operator"]));
        let claims = verify_token(&pool, Some(&padded)).await.expect("accepted");
        assert_eq!(claims.role(), Role::Operator);
    }

    #[test]
    fn remote_ws_allow_list_is_exactly_one_segment() {
        assert!(is_remote_ws_path("/api/remote/PC1234/ws"));
        // Casing is not folded anywhere else in the fleet either — pc_ids
        // are OS hostnames, verbatim.
        assert!(is_remote_ws_path("/api/remote/minipc/ws"));

        // Anything that is not exactly this route keeps its authentication.
        assert!(!is_remote_ws_path("/api/remote//ws"));
        assert!(!is_remote_ws_path("/api/remote/PC1/frames"));
        assert!(!is_remote_ws_path("/api/remote/PC1/ws/extra"));
        assert!(!is_remote_ws_path("/api/agents"));
        // The one that matters: a nested path must not opt itself out of
        // auth just by ending in `/ws`.
        assert!(!is_remote_ws_path("/api/remote/../accounts/ws"));
        assert!(!is_remote_ws_path("/api/remote/a/b/ws"));
    }

    #[test]
    fn claims_role_picks_highest() {
        let c = Claims {
            sub: "x".into(),
            exp: 0,
            aud: None,
            roles: vec!["viewer".into(), "admin".into()],
            allowed_features: None,
        };
        assert_eq!(c.role(), Role::Admin);
        let none = Claims {
            sub: "x".into(),
            exp: 0,
            aud: None,
            roles: vec![],
            allowed_features: None,
        };
        assert_eq!(none.role(), Role::Viewer);
    }
}
