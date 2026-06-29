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
use kanade_shared::kv::{ALL_KV_BUCKETS, ALL_OBJECT_STORES, ALL_STREAMS};
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

    // Probe the full bootstrap contract from the canonical lists in
    // `kanade_shared::kv` (the same source the health rollup uses). This
    // page used to list a hand-maintained subset — most visibly only
    // 1 of the 5 object stores — so the SPA's "Resource detail" looked
    // suspiciously sparse next to what bootstrap actually creates.
    for name in ALL_STREAMS {
        let exists = state.jetstream.get_stream(*name).await.is_ok();
        snap.streams.push(ResourceProbe {
            name: (*name).to_string(),
            exists,
        });
    }

    for name in ALL_KV_BUCKETS {
        let exists = state.jetstream.get_key_value(*name).await.is_ok();
        snap.kv_buckets.push(ResourceProbe {
            name: (*name).to_string(),
            exists,
        });
    }

    for name in ALL_OBJECT_STORES {
        let exists = state.jetstream.get_object_store(*name).await.is_ok();
        snap.object_stores.push(ResourceProbe {
            name: (*name).to_string(),
            exists,
        });
    }

    Ok(Json(snap))
}
