//! View CRUD (#743) — operator-registered [`View`] resources in
//! `BUCKET_VIEWS`. A view is a standalone declarative read/aggregation
//! over stored fleet data for the Analytics page (no `execute`, no
//! schedule). The Analytics endpoint (`api/analytics`) reads these at
//! query time and merges their widgets with the co-located `aggregate:`
//! hints on jobs.
//!
//! Mirrors the `schedules` / `jobs` resource pattern: JSON catalog in
//! `BUCKET_VIEWS` (what the backend reads), with a best-effort YAML mirror
//! in `BUCKET_VIEWS_YAML` so the SPA editor preserves comments/formatting.

use async_nats::jetstream::kv::Config as KvConfig;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use futures::TryStreamExt;
use kanade_shared::kv::{BUCKET_VIEWS, BUCKET_VIEWS_YAML};
use kanade_shared::manifest::View;
use serde::Serialize;
use tracing::{info, warn};

use crate::api::AppState;
use crate::api::yaml_body::{YamlOrJson, mirror_yaml, yaml_headers};
use crate::audit;
use crate::audit::Caller;

/// Response for `POST /api/views` — the upserted view's id plus a widget
/// count, so the caller gets a compact confirmation. (`GET /api/views`
/// returns full [`View`] objects.)
#[derive(Serialize)]
pub struct ViewSummary {
    pub id: String,
    pub description: Option<String>,
    pub widget_count: usize,
    pub tags: Vec<String>,
}

/// `GET /api/views` — list every registered view (viewer-readable).
pub async fn list(State(s): State<AppState>) -> Result<Json<Vec<View>>, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_VIEWS).await {
        Ok(k) => k,
        // Bucket not created yet (no view ever registered) ⇒ empty list,
        // not an error — matches schedules::list.
        Err(_) => return Ok(Json(Vec::new())),
    };
    let keys: Vec<String> = kv
        .keys()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?
        .try_collect()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv keys: {e}")))?;
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        if let Ok(Some(bytes)) = kv.get(&k).await
            && let Ok(view) = serde_json::from_slice::<View>(&bytes)
        {
            out.push(view);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(out))
}

/// `POST /api/views` — create/upsert a view (operator). Validates, writes
/// the JSON catalog entry, and best-effort mirrors the operator YAML.
pub async fn create(
    State(s): State<AppState>,
    caller: Caller,
    body: YamlOrJson<View>,
) -> Result<Json<ViewSummary>, (StatusCode, String)> {
    let YamlOrJson {
        value: view,
        raw_yaml,
    } = body;

    if let Err(e) = view.validate() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid view: {e}")));
    }

    let kv = s
        .jetstream
        .create_key_value(KvConfig {
            bucket: BUCKET_VIEWS.into(),
            history: 5,
            ..Default::default()
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ensure KV: {e}")))?;

    let body_bytes = serde_json::to_vec(&view)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize: {e}")))?;
    kv.put(&view.id, body_bytes.into())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV put: {e}")))?;

    // Operator-facing YAML mirror — best-effort (same as jobs/schedules);
    // the backend reads the JSON catalog, the YAML store only feeds the
    // SPA editor.
    let yaml_source = raw_yaml.unwrap_or_else(|| {
        serde_yaml::to_string(&view)
            .unwrap_or_else(|_| String::from("# YAML mirror unavailable for this entry"))
    });
    if let Err(e) = mirror_yaml(&s, BUCKET_VIEWS_YAML, &view.id, &yaml_source).await {
        warn!(error = %e, view_id = %view.id, "views: YAML mirror put failed; JSON catalog is current");
    }

    info!(view_id = %view.id, widgets = view.widgets.len(), "view upserted");
    audit::record(
        &s.nats,
        "operator",
        "view_upsert",
        Some(&view.id),
        Some(&caller),
        serde_json::json!({ "widgets": view.widgets.len() }),
    )
    .await;

    Ok(Json(ViewSummary {
        id: view.id,
        description: view.description,
        widget_count: view.widgets.len(),
        tags: view.tags,
    }))
}

/// `GET /api/views/{id}/yaml` — operator source YAML (mirror first, else
/// a serde_yaml dump of the JSON catalog entry).
pub async fn get_yaml(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, HeaderMap, String), (StatusCode, String)> {
    if let Ok(kv) = s.jetstream.get_key_value(BUCKET_VIEWS_YAML).await
        && let Ok(Some(bytes)) = kv.get(&id).await
        && let Ok(text) = String::from_utf8(bytes.to_vec())
    {
        return Ok((StatusCode::OK, yaml_headers(), text));
    }
    let kv = s
        .jetstream
        .get_key_value(BUCKET_VIEWS)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, format!("view '{id}' not found")))?;
    let bytes = kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("KV get: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("view '{id}' not found")))?;
    let view: View = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")))?;
    let yaml = serde_yaml::to_string(&view).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode YAML: {e}"),
        )
    })?;
    Ok((StatusCode::OK, yaml_headers(), yaml))
}

/// `DELETE /api/views/{id}` — remove a view (operator). Best-effort drops
/// the YAML mirror too.
pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    caller: Caller,
) -> Result<StatusCode, (StatusCode, String)> {
    let kv = match s.jetstream.get_key_value(BUCKET_VIEWS).await {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "views KV missing on delete");
            return Err((StatusCode::NOT_FOUND, "views bucket missing".into()));
        }
    };
    // NATS KV `delete` tombstones even a never-written key, so guard on
    // existence first — otherwise deleting a typo'd id would 204 and write
    // a phantom `view_delete` audit record with no matching upsert.
    if kv
        .get(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv get: {e}")))?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, format!("view '{id}' not found")));
    }
    kv.delete(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("kv delete: {e}")))?;
    if let Ok(ykv) = s.jetstream.get_key_value(BUCKET_VIEWS_YAML).await {
        let _ = ykv.delete(&id).await;
    }
    info!(view_id = %id, "view deleted");
    audit::record(
        &s.nats,
        "operator",
        "view_delete",
        Some(&id),
        Some(&caller),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
