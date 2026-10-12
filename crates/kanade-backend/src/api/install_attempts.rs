//! Bounded, write-only installation telemetry independent of agent registration.
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use super::AppState;
use crate::audit::Caller;

type Error = (StatusCode, String);
const TTL: i64 = 3600;
const RETENTION: i64 = 30 * 86400;
const MAX_EVENTS: i64 = 128;

fn hash(key: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(key.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut result, byte| {
            let _ = write!(result, "{byte:02x}");
            result
        })
}
fn db_error(_: sqlx::Error) -> Error {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "installation log storage unavailable".into(),
    )
}
async fn prune(pool: &SqlitePool) -> Result<(), Error> {
    sqlx::query("DELETE FROM install_events WHERE attempt_id IN (SELECT id FROM install_attempts WHERE created_at < ?)")
        .bind(chrono::Utc::now().timestamp() - RETENTION).execute(pool).await.map_err(db_error)?;
    sqlx::query("DELETE FROM install_attempts WHERE created_at < ?")
        .bind(chrono::Utc::now().timestamp() - RETENTION)
        .execute(pool)
        .await
        .map_err(db_error)?;
    Ok(())
}

pub async fn issue(
    pool: &SqlitePool,
    caller: &Caller,
    os: &str,
) -> Result<(String, String), Error> {
    prune(pool).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let now = chrono::Utc::now().timestamp();
    sqlx::query("INSERT INTO install_attempts(id,issuer,os,key_hash,created_at,expires_at) VALUES(?,?,?,?,?,?)")
        .bind(&id).bind(&caller.sub).bind(os).bind(hash(&key)).bind(now).bind(now + TTL)
        .execute(pool).await.map_err(db_error)?;
    Ok((id, key))
}

// Enumerations are also the storage sanitization boundary: arbitrary output,
// exception text, source lines and credentials cannot enter an event.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Platform,
    TemporaryDirectory,
    Download,
    Extract,
    Elevate,
    Install,
    Placement,
    ServiceStart,
    Complete,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Progress,
    Succeeded,
    Failed,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    PermissionDenied,
    FileNotFound,
    InvalidArchive,
    IoError,
    ServiceError,
    ElevationCancelled,
    HttpError,
    CommandFailed,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    seq: i64,
    stage: Stage,
    outcome: Outcome,
    exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<ErrorKind>,
}
fn label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

async fn append(pool: &SqlitePool, id: &str, key: &str, event: &Event) -> Result<(), Error> {
    if (event.outcome != Outcome::Failed && event.error_kind.is_some())
        || !(1..=MAX_EVENTS).contains(&event.seq)
        || (event.outcome == Outcome::Succeeded
            && (event.stage != Stage::Complete || event.exit_code != 0))
        || (event.outcome == Outcome::Failed && event.exit_code == 0)
        || (event.outcome == Outcome::Progress && event.exit_code != 0)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid installation event".into()));
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = pool.begin().await.map_err(db_error)?;
    let row = sqlx::query(
        "SELECT status,seq FROM install_attempts WHERE id=? AND key_hash=? AND expires_at>?",
    )
    .bind(id)
    .bind(hash(key))
    .bind(now)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?
    .ok_or((
        StatusCode::UNAUTHORIZED,
        "invalid or expired report key".into(),
    ))?;
    let stage = label(&event.stage);
    let outcome = label(&event.outcome);
    let error_kind = event.error_kind.as_ref().map(label);
    let status: String = row.get("status");
    let seq: i64 = row.get("seq");
    if matches!(status.as_str(), "succeeded" | "failed") && event.outcome == Outcome::Progress {
        return Err((StatusCode::CONFLICT, "attempt is terminal".into()));
    }
    if let Some(old) = sqlx::query(
        "SELECT stage,outcome,exit_code,error_kind FROM install_events WHERE attempt_id=? AND seq=?",
    )
    .bind(id)
    .bind(event.seq)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?
    {
        if old.get::<String, _>("stage") == stage
            && old.get::<String, _>("outcome") == outcome
            && old.get::<i32, _>("exit_code") == event.exit_code
            && old.get::<Option<String>, _>("error_kind") == error_kind
        {
            return Ok(());
        }
        return Err((StatusCode::CONFLICT, "sequence already used".into()));
    }
    if matches!(status.as_str(), "succeeded" | "failed") || event.seq <= seq {
        return Err((
            StatusCode::CONFLICT,
            "attempt is terminal or sequence does not advance".into(),
        ));
    }
    let next = if event.outcome == Outcome::Progress {
        "running"
    } else {
        &outcome
    };
    sqlx::query("UPDATE install_attempts SET status=?,seq=? WHERE id=?")
        .bind(next)
        .bind(event.seq)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    sqlx::query("INSERT INTO install_events(attempt_id,seq,stage,outcome,exit_code,received_at,error_kind) VALUES(?,?,?,?,?,?,?)")
        .bind(id)
        .bind(event.seq)
        .bind(stage)
        .bind(outcome)
        .bind(event.exit_code)
        .bind(now)
        .bind(error_kind)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)
}

pub async fn report(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(event): Json<Event>,
) -> Result<StatusCode, Error> {
    let key = headers
        .get("x-install-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64)
        .ok_or((StatusCode::UNAUTHORIZED, "report key required".into()))?;
    append(&state.pool, &id, key, &event).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, Error> {
    prune(&state.pool).await?;
    let rows = sqlx::query("SELECT id,issuer,os,created_at,expires_at,status,seq FROM install_attempts ORDER BY created_at DESC LIMIT 100")
        .fetch_all(&state.pool).await.map_err(db_error)?;
    let now = chrono::Utc::now().timestamp();
    Ok(Json(rows.into_iter().map(|r| {
        let status: String = r.get("status");
        let expired = r.get::<i64,_>("expires_at") <= now && !matches!(status.as_str(),"succeeded"|"failed");
        serde_json::json!({"id":r.get::<String,_>("id"),"issuer":r.get::<Option<String>,_>("issuer"),"os":r.get::<String,_>("os"),"created_at":r.get::<i64,_>("created_at"),"expires_at":r.get::<i64,_>("expires_at"),"status":if expired {"unknown"} else {&status},"seq":r.get::<i64,_>("seq")})
    }).collect::<Vec<_>>().into()))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, Error> {
    let rows = sqlx::query("SELECT seq,stage,outcome,exit_code,received_at,error_kind FROM install_events WHERE attempt_id=? AND received_at>=? ORDER BY seq")
        .bind(id).bind(chrono::Utc::now().timestamp()-RETENTION).fetch_all(&state.pool).await.map_err(db_error)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({"seq":r.get::<i64,_>("seq"),"stage":r.get::<String,_>("stage"),"outcome":r.get::<String,_>("outcome"),"exit_code":r.get::<i32,_>("exit_code"),"received_at":r.get::<i64,_>("received_at"),"error_kind":r.get::<Option<String>,_>("error_kind")})).collect::<Vec<_>>().into()))
}

/// Bind archive reachability to the authenticated issuer, never a supplied identity.
pub async fn check_download(pool: &SqlitePool, id: &str, caller: &Caller) -> Result<(), Error> {
    let available: i64 = sqlx::query_scalar("SELECT count(*) FROM install_attempts WHERE id=? AND issuer IS ? AND expires_at>? AND download_claimed=0 AND status NOT IN ('succeeded','failed')")
        .bind(id).bind(&caller.sub).bind(chrono::Utc::now().timestamp()).fetch_one(pool).await.map_err(db_error)?;
    if available == 0 {
        return Err((StatusCode::FORBIDDEN, "attempt does not belong to caller, expired, or already downloaded; obtain a fresh one-liner".into()));
    }
    // Validation does not consume the capability. The caller commits the claim
    // only after the complete response payload has been built successfully.
    Ok(())
}

pub async fn claim_download(
    pool: &SqlitePool,
    id: &str,
    caller: &Caller,
    os: &str,
) -> Result<(), Error> {
    let changed = sqlx::query("UPDATE install_attempts SET download_claimed=1,os=? WHERE id=? AND issuer IS ? AND expires_at>? AND download_claimed=0 AND status NOT IN ('succeeded','failed')")
        .bind(os).bind(id).bind(&caller.sub).bind(chrono::Utc::now().timestamp()).execute(pool).await.map_err(db_error)?.rows_affected();
    if changed == 0 {
        return Err((
            StatusCode::FORBIDDEN,
            "attempt already consumed, terminal, or expired".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261012000000_install_attempts.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261012010000_install_error_kinds.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        pool
    }
    fn event(seq: i64, outcome: Outcome) -> Event {
        Event {
            seq,
            stage: if outcome == Outcome::Succeeded {
                Stage::Complete
            } else {
                Stage::Download
            },
            exit_code: if outcome == Outcome::Failed { 22 } else { 0 },
            outcome,
            error_kind: None,
        }
    }
    #[tokio::test]
    async fn install_keys_identity_expiry_and_retention() {
        let p = pool().await;
        let caller = Caller {
            sub: Some("issuer".into()),
            source: None,
        };
        let (id, key) = issue(&p, &caller, "unix").await.unwrap();
        let stored: String = sqlx::query_scalar("SELECT key_hash FROM install_attempts")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_ne!(stored, key);
        assert_eq!(
            append(&p, &id, "wrong", &event(1, Outcome::Progress))
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            check_download(&p, &id, &Caller::default())
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        append(&p, &id, &key, &event(1, Outcome::Progress))
            .await
            .unwrap();
        let (other_id, _) = issue(&p, &caller, "linux").await.unwrap();
        assert_eq!(
            append(&p, &other_id, &key, &event(1, Outcome::Progress))
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        check_download(&p, &id, &caller).await.unwrap();
        // A failed archive build leaves the same attempt available for retry.
        check_download(&p, &id, &caller).await.unwrap();
        claim_download(&p, &id, &caller, "linux").await.unwrap();
        assert_eq!(
            check_download(&p, &id, &caller).await.unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            check_download(&p, &id, &Caller::default())
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        sqlx::query("UPDATE install_attempts SET expires_at=0")
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            append(&p, &id, &key, &event(1, Outcome::Failed))
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        sqlx::query("UPDATE install_attempts SET created_at=0")
            .execute(&p)
            .await
            .unwrap();
        prune(&p).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM install_attempts")
                .fetch_one(&p)
                .await
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn install_events_are_idempotent_bounded_and_terminal() {
        for outcome in [Outcome::Succeeded, Outcome::Failed] {
            let p = pool().await;
            let (id, key) = issue(&p, &Caller::default(), "windows").await.unwrap();
            let first = event(1, Outcome::Progress);
            append(&p, &id, &key, &first).await.unwrap();
            append(&p, &id, &key, &first).await.unwrap();
            assert_eq!(
                append(&p, &id, &key, &event(1, Outcome::Failed))
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                append(&p, &id, &key, &event(129, Outcome::Progress))
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::BAD_REQUEST
            );
            // Missing intermediate reports do not prevent delivery of the final result.
            let final_event = event(4, outcome);
            append(&p, &id, &key, &final_event).await.unwrap();
            append(&p, &id, &key, &final_event).await.unwrap();
            assert_eq!(
                append(&p, &id, &key, &first).await.unwrap_err().0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                append(&p, &id, &key, &event(5, Outcome::Failed))
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM install_events")
                    .fetch_one(&p)
                    .await
                    .unwrap(),
                2
            );
        }
    }
    #[test]
    fn install_events_reject_arbitrary_diagnostics_and_secrets() {
        for body in [
            r#"{"seq":1,"stage":"Bearer secret","outcome":"progress","exit_code":0}"#,
            r#"{"seq":1,"stage":"download","outcome":"progress","exit_code":0,"message":"password=secret"}"#,
        ] {
            assert!(serde_json::from_str::<Event>(body).is_err());
        }
    }

    #[tokio::test]
    async fn install_diagnostic_causes_are_stored_and_checked_for_duplicate_conflicts() {
        let p = pool().await;
        for kind in [ErrorKind::PermissionDenied, ErrorKind::FileNotFound] {
            let (id, key) = issue(&p, &Caller::default(), "windows").await.unwrap();
            let mut failure = event(1, Outcome::Failed);
            failure.stage = Stage::Placement;
            failure.error_kind = Some(kind);
            append(&p, &id, &key, &failure).await.unwrap();
            append(&p, &id, &key, &failure).await.unwrap();
            let stored: Option<String> =
                sqlx::query_scalar("SELECT error_kind FROM install_events WHERE attempt_id=?")
                    .bind(&id)
                    .fetch_one(&p)
                    .await
                    .unwrap();
            assert_eq!(stored, failure.error_kind.as_ref().map(label));
            failure.error_kind = Some(ErrorKind::IoError);
            assert_eq!(
                append(&p, &id, &key, &failure).await.unwrap_err().0,
                StatusCode::CONFLICT
            );
        }
        assert!(serde_json::from_str::<Event>(r#"{"seq":1,"stage":"install","outcome":"failed","exit_code":1,"error_kind":"token-secret"}"#).is_err());
    }

    #[tokio::test]
    async fn install_reports_use_only_attempt_capability_and_enforce_body_limit() {
        use axum::{Router, body::Body, http::Request, middleware, routing::post};
        use tower::ServiceExt;
        async fn ingest(
            State(pool): State<SqlitePool>,
            Path(id): Path<String>,
            headers: HeaderMap,
            Json(event): Json<Event>,
        ) -> Result<StatusCode, Error> {
            let key = headers
                .get("x-install-key")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            append(&pool, &id, key, &event).await?;
            Ok(StatusCode::NO_CONTENT)
        }
        let p = pool().await;
        let (id, key) = issue(&p, &Caller::default(), "unix").await.unwrap();
        let app = Router::new()
            .route(
                "/api/install-reports/{id}",
                post(ingest).layer(axum::extract::DefaultBodyLimit::max(1024)),
            )
            .with_state(p.clone())
            .layer(middleware::from_fn(crate::auth::require_features))
            .layer(middleware::from_fn_with_state(p, crate::auth::verify));
        for (method, url, key, body, expected) in [
            (
                "POST",
                format!("/api/install-reports/{id}"),
                key.clone(),
                serde_json::to_string(&event(1, Outcome::Progress)).unwrap(),
                StatusCode::NO_CONTENT,
            ),
            (
                "POST",
                format!("/api/install-reports/{id}"),
                "wrong".into(),
                serde_json::to_string(&event(2, Outcome::Failed)).unwrap(),
                StatusCode::UNAUTHORIZED,
            ),
            (
                "POST",
                format!("/api/install-reports/{id}"),
                key.clone(),
                " ".repeat(1025),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                "GET",
                format!("/api/install-reports/{id}"),
                key.clone(),
                String::new(),
                StatusCode::UNAUTHORIZED,
            ),
            (
                "GET",
                "/api/agents/install-attempts".into(),
                key,
                String::new(),
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(url)
                .header("content-type", "application/json")
                .header("x-install-key", key)
                .header("authorization", "Bearer expired-session")
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn install_read_requires_operator_and_install_feature() {
        use axum::{
            Router,
            body::Body,
            http::{Method, Request},
            middleware,
            routing::get,
        };
        use tower::ServiceExt;
        for (roles, features, expected) in [
            (
                vec!["viewer"],
                vec![kanade_shared::feature::Feature::AgentInstall],
                StatusCode::FORBIDDEN,
            ),
            (
                vec!["operator"],
                vec![kanade_shared::feature::Feature::Audit],
                StatusCode::FORBIDDEN,
            ),
            (
                vec!["operator"],
                vec![kanade_shared::feature::Feature::AgentInstall],
                StatusCode::OK,
            ),
        ] {
            let claims = crate::auth::Claims {
                sub: "test".into(),
                exp: 0,
                aud: None,
                roles: roles.into_iter().map(String::from).collect(),
                allowed_features: Some(features),
            };
            let app = Router::new()
                .route(
                    "/api/agents/install-attempts",
                    get(|| async { StatusCode::OK }),
                )
                .layer(middleware::from_fn(crate::auth::require_operator))
                .layer(middleware::from_fn(crate::auth::require_features));
            let mut request = Request::builder()
                .method(Method::GET)
                .uri("/api/agents/install-attempts")
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(claims);
            assert_eq!(app.oneshot(request).await.unwrap().status(), expected);
        }
    }
}
