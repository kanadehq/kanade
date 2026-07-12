//! Named permission groups (#1008 Phase 3) — a reusable, **live-referenced**
//! page allow-list shared by many accounts. Assigning an account to a group
//! makes the group's feature set the account's effective page access
//! (resolved in [`crate::auth::verify`] via a join, so editing a group updates
//! every member at once).
//!
//! Admin-only CRUD: the router applies `route_layer(require_admin)` to this
//! group of routes, so the handlers assume the caller already cleared that
//! gate. A group is always a concrete set (`'[]'` = commons only); there is no
//! "unrestricted" group — un-restricting an account clears its group instead.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::api::AppState;
use crate::audit::{self, Caller};
use kanade_shared::feature::Feature;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_owned()).into_response()
}

fn db_err(e: sqlx::Error) -> Response {
    warn!(error = %e, "permission_groups db error");
    err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
}

/// Lenient read of a group's stored features for display — drop unknown /
/// retired keys, malformed JSON → empty. Mirrors the account allow-list's
/// display path so the two agree on what a stored value means.
fn parse_features(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw)
        .ok()
        .map(|keys| {
            keys.iter()
                .filter_map(|k| Feature::parse(k).map(|f| f.as_str().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(sqlx::FromRow)]
struct GroupRowDb {
    name: String,
    features: String,
    member_count: i64,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize)]
pub struct GroupRow {
    name: String,
    features: Vec<String>,
    /// How many accounts are assigned to this group — the SPA shows it and
    /// uses it to warn before a delete (which the API blocks anyway).
    member_count: i64,
    created_at: String,
    updated_at: String,
}

/// `GET /api/permission-groups` — admin. Lists groups with their member count.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<GroupRow>>, Response> {
    let rows = sqlx::query_as::<_, GroupRowDb>(
        "SELECT g.name, g.features, \
                (SELECT COUNT(*) FROM users u WHERE u.permission_group = g.name) AS member_count, \
                g.created_at, g.updated_at \
         FROM permission_groups g ORDER BY g.name",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(db_err)?;
    let out = rows
        .into_iter()
        .map(|r| GroupRow {
            name: r.name,
            features: parse_features(&r.features),
            member_count: r.member_count,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct CreateReq {
    name: String,
    /// Feature keys the group grants (validated against the catalog). Empty is
    /// valid — a group restricted to the always-open commons.
    #[serde(default)]
    features: Vec<String>,
}

/// `POST /api/permission-groups` — admin. `409` on a duplicate name.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(req): Json<CreateReq>,
) -> Result<StatusCode, Response> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "group name required"));
    }
    let features = Feature::canonicalize(&req.features)
        .map_err(|k| err(StatusCode::BAD_REQUEST, &format!("unknown feature: {k}")))?;
    let features_json = serde_json::to_string(&features)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "serialize features"))?;
    let res = sqlx::query("INSERT INTO permission_groups (name, features) VALUES (?, ?)")
        .bind(name)
        .bind(&features_json)
        .execute(&state.pool)
        .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "group already exists"));
        }
        Err(e) => return Err(db_err(e)),
    }
    audit::record(
        &state.nats,
        "admin",
        "permission_group.create",
        Some(name),
        Some(&caller),
        serde_json::json!({ "features": features }),
    )
    .await;
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
pub struct UpdateReq {
    features: Vec<String>,
}

/// `PATCH /api/permission-groups/{name}` — admin. Replaces the feature set.
/// Takes effect for every member on their next request (`verify` re-resolves).
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    caller: Caller,
    Json(req): Json<UpdateReq>,
) -> Result<StatusCode, Response> {
    let features = Feature::canonicalize(&req.features)
        .map_err(|k| err(StatusCode::BAD_REQUEST, &format!("unknown feature: {k}")))?;
    let features_json = serde_json::to_string(&features)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "serialize features"))?;
    let res = sqlx::query(
        "UPDATE permission_groups SET features = ?, updated_at = CURRENT_TIMESTAMP WHERE name = ?",
    )
    .bind(&features_json)
    .bind(&name)
    .execute(&state.pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such group"));
    }
    audit::record(
        &state.nats,
        "admin",
        "permission_group.update",
        Some(&name),
        Some(&caller),
        serde_json::json!({ "features": features }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/permission-groups/{name}` — admin. Refuses (`409`) while any
/// account still references the group, so no member is left with a dangling
/// group pointer — reassign them first.
pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    caller: Caller,
) -> Result<StatusCode, Response> {
    // Existence check for the 404. The member guard lives INSIDE the delete
    // statement (a `NOT EXISTS` predicate) so the count and the delete are
    // evaluated atomically — a concurrent `PATCH /api/accounts/{u}` that
    // assigns a member between a separate check and act can't slip through and
    // leave a dangling `permission_group` (matches the atomic last-admin guard
    // in `accounts.rs`). `rows_affected == 0` on a confirmed-existing group
    // therefore means the guard fired (it has members).
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM permission_groups WHERE name = ?")
        .bind(&name)
        .fetch_one(&state.pool)
        .await
        .map_err(db_err)?;
    if exists == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such group"));
    }
    let res = sqlx::query(
        "DELETE FROM permission_groups WHERE name = ? \
           AND NOT EXISTS (SELECT 1 FROM users WHERE permission_group = ?)",
    )
    .bind(&name)
    .bind(&name)
    .execute(&state.pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "group still has member(s); reassign them first",
        ));
    }
    audit::record(
        &state.nats,
        "admin",
        "permission_group.delete",
        Some(&name),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&p).await.unwrap();
        p
    }

    // Mirrors the member guard embedded in `delete`'s statement: the count and
    // the delete are one atomic statement, so a member assigned concurrently
    // still blocks the delete.
    const GUARDED_DELETE: &str = "DELETE FROM permission_groups WHERE name = ?1 \
        AND NOT EXISTS (SELECT 1 FROM users WHERE permission_group = ?1)";

    #[tokio::test]
    async fn delete_blocked_while_members_exist() {
        let p = pool().await;
        sqlx::query("INSERT INTO permission_groups (name, features) VALUES ('sec', '[]')")
            .execute(&p)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, permission_group) \
             VALUES ('u', 'x', 'viewer', 'sec')",
        )
        .execute(&p)
        .await
        .unwrap();
        let res = sqlx::query(GUARDED_DELETE)
            .bind("sec")
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            res.rows_affected(),
            0,
            "a group with a member is not deletable"
        );
    }

    #[tokio::test]
    async fn delete_succeeds_with_no_members() {
        let p = pool().await;
        sqlx::query("INSERT INTO permission_groups (name, features) VALUES ('sec', '[]')")
            .execute(&p)
            .await
            .unwrap();
        let res = sqlx::query(GUARDED_DELETE)
            .bind("sec")
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(res.rows_affected(), 1, "an empty group is deletable");
    }
}
