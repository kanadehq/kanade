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
//!      row → 403, otherwise the DB `role` (not the token's claim) is
//!      injected into [`Claims`]. This makes `disable` / role changes
//!      take effect immediately rather than waiting for `exp`.
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
    /// `None` = unrestricted; `Some(list)` = page allow-list. Unknown /
    /// retired feature keys in the stored JSON are dropped here (a shrunk
    /// catalog must not deny access on a key the backend no longer knows).
    allowed_features: Option<Vec<Feature>>,
}

/// Look up a user's authoritative `(role, disabled, allowed_features)` from
/// SQLite. A row whose `role` column fails to parse is treated as absent
/// (deny).
async fn lookup_user(pool: &SqlitePool, username: &str) -> Result<Option<UserAuth>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, Option<String>)>(
        "SELECT role, disabled, allowed_features FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(role, disabled, allowed)| {
        Role::parse(&role).map(|role| UserAuth {
            role,
            disabled: disabled != 0,
            allowed_features: parse_allowed_features(allowed.as_deref()),
        })
    }))
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

pub async fn verify(
    State(pool): State<SqlitePool>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    // 1. Auth opt-out for local development.
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

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(unauth("missing bearer token"));
    };

    // 3. Static service token: admin-equivalent. A mismatch is NOT a
    //    rejection — fall through to JWT so user tokens coexist.
    if let Some(expected) = resolve_static_token()
        && constant_time_eq(token.as_bytes(), expected.as_bytes())
    {
        let mut req = req;
        req.extensions_mut()
            .insert(Claims::service("service-token"));
        return Ok(next.run(req).await);
    }

    // 4. JWT mode.
    let secret = signing_secret();
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&[EXPECTED_AUDIENCE]);

    let claims = match decode::<Claims>(token, &key, &validation) {
        Ok(data) => data.claims,
        Err(e) => {
            warn!(error = %e, path, "JWT verify failed");
            return Err(unauth(&format!("invalid token: {e}")));
        }
    };

    // DB is authoritative: re-read role + disabled now so account
    // changes apply immediately rather than at the token's exp.
    match lookup_user(&pool, &claims.sub).await {
        Ok(Some(user)) => {
            if user.disabled {
                // 401 (not 403) so the SPA's expired-token path logs the
                // user out instead of trapping them on a page that spams
                // permission toasts (Gemini #331).
                return Err(unauth("account disabled"));
            }
            let mut claims = claims;
            // DB is authoritative for BOTH role and the page allow-list —
            // overwrite whatever the token carried (the mint path never sets
            // allowed_features, but a hand-crafted token might).
            claims.roles = vec![user.role.as_str().to_string()];
            claims.allowed_features = user.allowed_features;
            let mut req = req;
            req.extensions_mut().insert(claims);
            Ok(next.run(req).await)
        }
        Ok(None) => Err(unauth("unknown account")),
        Err(e) => {
            // Fail closed: a DB hiccup must not grant access.
            error!(error = %e, sub = %claims.sub, "user lookup failed");
            Err(unauth("auth backend unavailable"))
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
