//! `GET /api/jetstream/status` — health snapshot of every JetStream
//! resource the kanade fleet expects. Useful from the web UI's
//! debug page and as a smoke check that `kanade-backend`'s
//! startup-time auto-bootstrap actually fired.
//!
//! For now we just report which resources are present + absent;
//! detailed stats (message count, bytes, last seq) can come in a
//! follow-up when there's an actual UI consumer for them.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, BUCKET_AGENTS_STATE, BUCKET_SCHEDULES,
    BUCKET_SCRIPT_CURRENT, BUCKET_SCRIPT_STATUS, OBJECT_AGENT_RELEASES, STREAM_AUDIT,
    STREAM_EVENTS, STREAM_EXEC, STREAM_INVENTORY, STREAM_RESULTS,
};
use serde::Serialize;

use super::AppState;

#[derive(Serialize)]
pub struct ResourceProbe {
    pub name: String,
    pub exists: bool,
}

#[derive(Serialize)]
pub struct JetstreamSnapshot {
    pub streams: Vec<ResourceProbe>,
    pub kv_buckets: Vec<ResourceProbe>,
    pub object_stores: Vec<ResourceProbe>,
}

pub async fn status(
    State(state): State<AppState>,
) -> Result<Json<JetstreamSnapshot>, (StatusCode, String)> {
    let mut snap = JetstreamSnapshot {
        streams: Vec::new(),
        kv_buckets: Vec::new(),
        object_stores: Vec::new(),
    };

    for name in [
        STREAM_INVENTORY,
        STREAM_RESULTS,
        STREAM_EXEC,
        STREAM_EVENTS,
        STREAM_AUDIT,
    ] {
        let exists = state.jetstream.get_stream(name).await.is_ok();
        snap.streams.push(ResourceProbe {
            name: name.to_string(),
            exists,
        });
    }

    for name in [
        BUCKET_SCRIPT_CURRENT,
        BUCKET_SCRIPT_STATUS,
        BUCKET_AGENTS_STATE,
        BUCKET_AGENT_CONFIG,
        BUCKET_AGENT_GROUPS,
        BUCKET_SCHEDULES,
    ] {
        let exists = state.jetstream.get_key_value(name).await.is_ok();
        snap.kv_buckets.push(ResourceProbe {
            name: name.to_string(),
            exists,
        });
    }

    for name in [OBJECT_AGENT_RELEASES] {
        let exists = state.jetstream.get_object_store(name).await.is_ok();
        snap.object_stores.push(ResourceProbe {
            name: name.to_string(),
            exists,
        });
    }

    Ok(Json(snap))
}
