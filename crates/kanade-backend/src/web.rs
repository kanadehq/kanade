//! Static SPA served from rust-embed. The `web/dist/` folder is baked
//! into the binary at compile time so a single `kanade-backend` ships
//! both the JSON API and the dashboard.
//!
//! Unmatched routes fall back to `index.html` (typical SPA hash-router
//! shape) so client-side navigation survives a full reload.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct WebAssets;

pub async fn serve(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let lookup = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = WebAssets::get(lookup) {
        let mime = mime_guess::from_path(lookup).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref())],
            content.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: any unmatched non-API path → index.html.
    if let Some(idx) = WebAssets::get("index.html") {
        let mime = mime_guess::from_ext("html").first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref())],
            idx.data.into_owned(),
        )
            .into_response();
    }

    (StatusCode::NOT_FOUND, Body::empty()).into_response()
}
