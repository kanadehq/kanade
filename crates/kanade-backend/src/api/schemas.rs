//! JSON Schema endpoints. The SPA's Monaco editor wires
//! `monaco-yaml` to these URLs so operators get field-name
//! completion, hover docs, and inline validation against the
//! authoritative `Manifest` / `Schedule` Rust types instead of a
//! hand-curated schema that would inevitably rot.
//!
//! `schemars::schema_for!` walks the derived `JsonSchema` impls
//! once per request. The cost is negligible for an editor-open
//! workflow; if it ever shows up in a profile, wrap in `OnceLock`.

use axum::Json;
use axum::http::StatusCode;
use kanade_shared::manifest::{GroupDef, Manifest, Schedule, View};

pub async fn manifest_schema() -> Result<Json<serde_json::Value>, StatusCode> {
    let schema = schemars::schema_for!(Manifest);
    serde_json::to_value(&schema)
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn schedule_schema() -> Result<Json<serde_json::Value>, StatusCode> {
    let schema = schemars::schema_for!(Schedule);
    serde_json::to_value(&schema)
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn view_schema() -> Result<Json<serde_json::Value>, StatusCode> {
    let schema = schemars::schema_for!(View);
    serde_json::to_value(&schema)
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn group_def_schema() -> Result<Json<serde_json::Value>, StatusCode> {
    let schema = schemars::schema_for!(GroupDef);
    serde_json::to_value(&schema)
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
