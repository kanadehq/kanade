//! `GET /api/jetstream/status` — health + usage snapshot of every
//! JetStream resource the kanade fleet expects. Useful from the web UI's
//! debug page and as a smoke check that `kanade-backend`'s startup-time
//! auto-bootstrap actually fired.
//!
//! Beyond presence, each probe carries the backing stream's used `bytes`,
//! its `max_bytes` cap (`None` = unlimited), and `messages`, so the SPA
//! can render a usage bar — how full each stream / store is matters for
//! capacity planning and for spotting a runaway stream before
//! `discard: Old` starts trimming useful history.

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
    /// Bytes currently stored in the backing stream. `None` when the
    /// resource is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Configured `max_bytes` cap. `None` = unlimited (the curated object
    /// stores — agent_releases / app_packages / scripts — run uncapped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Message count in the backing stream (a KV / object store counts one
    /// message per live key / chunk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<u64>,
}

impl ResourceProbe {
    fn missing(name: &str) -> Self {
        Self {
            name: name.to_string(),
            exists: false,
            bytes: None,
            max_bytes: None,
            messages: None,
        }
    }
}

/// Normalise a stream's `config.max_bytes` to a usage cap. NATS uses
/// `<= 0` (typically `-1`) for "unlimited", which we surface as `None` so
/// the UI shows "no cap" instead of dividing by a zero-byte cap.
fn cap_bytes(max_bytes: i64) -> Option<u64> {
    (max_bytes > 0).then_some(max_bytes as u64)
}

#[derive(Serialize)]
pub struct JetstreamSnapshot {
    pub streams: Vec<ResourceProbe>,
    pub kv_buckets: Vec<ResourceProbe>,
    pub object_stores: Vec<ResourceProbe>,
}

/// Probe one JetStream resource by its **backing stream** name
/// (`<name>` for a stream, `OBJ_<bucket>` / `KV_<bucket>` for a store /
/// bucket) and report presence + usage. `get_stream` already fetches the
/// stream's info to build the handle, so we reuse it via `cached_info()`
/// rather than issuing a second `STREAM.INFO` round-trip per resource
/// (this endpoint probes ~20 of them). A missing resource degrades to
/// exists-only without failing the whole snapshot.
async fn probe(
    js: &async_nats::jetstream::Context,
    display_name: &str,
    stream_name: &str,
) -> ResourceProbe {
    let Ok(stream) = js.get_stream(stream_name).await else {
        return ResourceProbe::missing(display_name);
    };
    let info = stream.cached_info();
    ResourceProbe {
        name: display_name.to_string(),
        exists: true,
        bytes: Some(info.state.bytes),
        messages: Some(info.state.messages),
        max_bytes: cap_bytes(info.config.max_bytes),
    }
}

pub async fn status(
    State(state): State<AppState>,
) -> Result<Json<JetstreamSnapshot>, (StatusCode, String)> {
    let js = &state.jetstream;
    let mut snap = JetstreamSnapshot {
        streams: Vec::new(),
        kv_buckets: Vec::new(),
        object_stores: Vec::new(),
    };

    // Probe the full bootstrap contract from the canonical lists in
    // `kanade_shared::kv`. Object stores and KV buckets are backed by
    // streams named `OBJ_<bucket>` / `KV_<bucket>` — read usage off those.
    for name in ALL_STREAMS {
        snap.streams.push(probe(js, name, name).await);
    }
    for name in ALL_KV_BUCKETS {
        snap.kv_buckets
            .push(probe(js, name, &format!("KV_{name}")).await);
    }
    for name in ALL_OBJECT_STORES {
        snap.object_stores
            .push(probe(js, name, &format!("OBJ_{name}")).await);
    }

    Ok(Json(snap))
}

#[cfg(test)]
mod tests {
    use super::{ResourceProbe, cap_bytes};

    #[test]
    fn cap_bytes_treats_non_positive_as_unlimited() {
        assert_eq!(cap_bytes(-1), None, "NATS unlimited sentinel");
        assert_eq!(cap_bytes(0), None, "zero cap is not a real 0-byte limit");
        assert_eq!(cap_bytes(1024), Some(1024));
        assert_eq!(
            cap_bytes(2 * 1024 * 1024 * 1024),
            Some(2 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn missing_probe_has_no_usage() {
        let p = ResourceProbe::missing("STREAM_X");
        assert!(!p.exists);
        assert_eq!(p.name, "STREAM_X");
        assert!(p.bytes.is_none() && p.max_bytes.is_none() && p.messages.is_none());
    }
}
