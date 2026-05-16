//! `GET /api/agents/<pc_id>/logs?tail=500` — proxies a NATS
//! `logs.fetch.<pc_id>` request/reply round-trip and returns the
//! agent's tail-N log bytes as `text/plain`.
//!
//! The agent must be online for the round trip to succeed; on
//! timeout we respond 504 so the Web UI can show "agent didn't
//! reply" without conflating it with a 500.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use kanade_shared::subject;
use kanade_shared::wire::LogsRequest;
use serde::Deserialize;
use tracing::warn;

use super::AppState;

#[derive(Deserialize)]
pub struct TailParams {
    #[serde(default = "default_tail")]
    pub tail: u32,
}

fn default_tail() -> u32 {
    500
}

pub async fn tail(
    State(state): State<AppState>,
    Path(pc_id): Path<String>,
    Query(params): Query<TailParams>,
) -> Result<Response, (StatusCode, String)> {
    let req = LogsRequest {
        tail_lines: params.tail,
    };
    let payload = serde_json::to_vec(&req).map_err(|e| {
        warn!(error = %e, "encode LogsRequest");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let subject = subject::logs_fetch(&pc_id);
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            format!("agent '{pc_id}' didn't reply within 10s"),
        )
    })?
    .map_err(|e| {
        warn!(error = %e, %pc_id, "logs.fetch request failed");
        (StatusCode::BAD_GATEWAY, format!("logs.fetch.{pc_id}: {e}"))
    })?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(reply.payload))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
