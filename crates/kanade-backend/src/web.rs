//! Static SPA served from rust-embed. The `web/dist/` folder is baked
//! into the binary at compile time so a single `kanade-backend` ships
//! both the JSON API and the dashboard.
//!
//! Two routing classes share this fallback:
//!
//! * **Hashed asset paths under `/assets/*`** are emitted by Vite at
//!   build time and named like `index-<hash>.js`. They must hit the
//!   embedded bundle exactly — a typo or a bundle ↔ binary mismatch
//!   should surface as a clean 404 so the browser shows "Failed to
//!   fetch dynamically imported module" with the real cause, not a
//!   strict-MIME error from being handed the SPA's `text/html` shell
//!   instead. Observed once when a partial PR merge committed the
//!   main bundle (referencing a lazy-loaded chunk hash) but lost the
//!   chunk file itself: the SPA fallback masked the missing file
//!   behind a misleading MIME error.
//!
//! * **Anything else** (`/`, `/jobs`, `/activity/:requestId`, …) is a
//!   client-side React Router path. Those don't exist as files on
//!   disk; they need `index.html` so React Router can pick the route
//!   up on a full reload.
//!
//! Splitting on the `/assets/` prefix gives us both behaviours
//! cleanly: assets get 404'd on miss, SPA paths get the HTML shell.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
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

    // Asset miss: hashed Vite chunks live at /assets/<name>-<hash>.<ext>
    // and any unknown path under that root means the browser asked for
    // a file the bundle doesn't have. The SPA fallback shape (return
    // index.html as text/html) actively misleads here — the browser
    // gets the wrong MIME and the operator sees "strict MIME type
    // checking is enforced for module scripts" with no hint that the
    // real cause is "the chunk file is missing from this build."
    // `path` has its leading `/` stripped above, so `assets/...`
    // catches `/assets/...` requests. Re-trim defensively in case a
    // future refactor changes the path source to keep the slash.
    if path.trim_start_matches('/').starts_with("assets/") {
        return (StatusCode::NOT_FOUND, Body::empty()).into_response();
    }

    // SPA fallback: any other unmatched path → index.html so React
    // Router can resolve `/jobs`, `/activity/:id`, etc. on reload.
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
