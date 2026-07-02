//! Shared body extractor that accepts a JSON *or* YAML request body
//! and deserialises it into a target type, while preserving the raw
//! YAML bytes when the caller sent YAML.
//!
//! Jobs / Schedules POST endpoints reuse this so an operator can
//! `kanade job create some-yaml-file.yaml` (CLI sends YAML) or
//! `POST /api/jobs` with `Content-Type: application/json` (CLI / SPA
//! pre-existing path). When YAML comes in, the raw bytes are kept so
//! the parallel `*_yaml` KV bucket stores the operator's exact source
//! (comments, block scalar indentation, key order) instead of a
//! re-serialised normalisation.

use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;

use super::AppState;

/// Best-effort write of the operator's raw YAML source into the
/// parallel `*_yaml` mirror bucket. The bucket must already exist
/// (bootstrap creates it at backend startup), so `get_key_value` is
/// enough — `create_key_value` would add a round-trip per request.
/// Callers warn-log a failure and continue; the JSON catalog write
/// has already succeeded and is what every consumer reads.
pub async fn mirror_yaml(
    state: &AppState,
    bucket: &str,
    id: &str,
    yaml: &str,
) -> anyhow::Result<()> {
    let kv = state.jetstream.get_key_value(bucket).await?;
    kv.put(id, yaml.to_owned().into_bytes().into()).await?;
    Ok(())
}

/// Shared response headers for the `/api/{jobs,schedules}/{id}/yaml`
/// endpoints. `application/yaml` is what RFC 9512 settled on for the
/// MIME type, but `monaco-yaml` / `js-yaml` happily consume any of
/// the historical aliases this PR's request extractor accepts.
pub fn yaml_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/yaml"));
    h
}

/// Extracted body + optional original YAML source.
///
/// `raw_yaml` is `Some` only when the caller used a YAML-flavoured
/// `Content-Type`; JSON callers get `None`, and the API layer falls
/// back to a `serde_yaml::to_string(&value)` dump when it needs a
/// YAML representation.
pub struct YamlOrJson<T> {
    pub value: T,
    pub raw_yaml: Option<String>,
}

impl<S, T> FromRequest<S> for YamlOrJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + kanade_shared::strict::StrictSchema,
{
    type Rejection = (StatusCode, String);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_yaml = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| {
                let head = ct.split(';').next().unwrap_or("").trim();
                matches!(
                    head,
                    "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml"
                )
            })
            .unwrap_or(false);

        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("read request body: {e}")))?;

        if is_yaml {
            let raw = String::from_utf8(bytes.to_vec()).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("YAML body must be UTF-8: {e}"),
                )
            })?;
            // #492: strict parse — this extractor IS the create
            // boundary (POST /api/jobs, /api/schedules), so unknown
            // keys are operator/SPA typos and get rejected with
            // their paths. The same types are read tolerantly
            // fleet-wide (deny_unknown_fields moved off them).
            let value: T = kanade_shared::strict::from_yaml_str(&raw)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse YAML body: {e}")))?;
            Ok(Self {
                value,
                raw_yaml: Some(raw),
            })
        } else {
            let value: T = kanade_shared::strict::from_json_slice(&bytes)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("parse JSON body: {e}")))?;
            Ok(Self {
                value,
                raw_yaml: None,
            })
        }
    }
}
