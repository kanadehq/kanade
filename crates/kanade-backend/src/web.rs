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
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

/// #1215②: Vite content-hashes every filename under `assets/`, so a new
/// build is a new URL — safe to cache forever. Everything else
/// (`index.html`, root icons) can change under the same URL, so it must
/// revalidate; the ETag below turns that revalidation into a cheap 304.
const ASSETS_CACHE: &str = "public, max-age=31536000, immutable";
const SHELL_CACHE: &str = "no-cache";

pub async fn serve(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let lookup = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = WebAssets::get(lookup) {
        return embedded_response(lookup, &content, req.headers());
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
        return embedded_response("index.html", &idx, req.headers());
    }

    (StatusCode::NOT_FOUND, Body::empty()).into_response()
}

/// `assets/*` is immutable-by-hash; anything else must revalidate.
fn cache_control_for(lookup: &str) -> &'static str {
    if lookup.starts_with("assets/") {
        ASSETS_CACHE
    } else {
        SHELL_CACHE
    }
}

/// Quoted hex of rust-embed's content SHA-256 — a strong validator.
fn etag_of(file: &EmbeddedFile) -> String {
    etag_hex(file.metadata.sha256_hash())
}

fn etag_hex(hash: [u8; 32]) -> String {
    let mut s = String::with_capacity(2 + 64);
    s.push('"');
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('"');
    s
}

/// Weak comparison per RFC 9110 §8.8.3.2: both sides with their `W/`
/// prefix stripped must be byte-identical. `If-None-Match` may carry a
/// comma-separated list (or `*`).
fn etag_matches(if_none_match: Option<&HeaderValue>, etag: &str) -> bool {
    fn strip_weak(t: &str) -> &str {
        let t = t.trim();
        t.strip_prefix("W/").unwrap_or(t)
    }
    let Some(v) = if_none_match else { return false };
    let Ok(v) = v.to_str() else { return false };
    v.split(',')
        .any(|t| t.trim() == "*" || strip_weak(t) == etag)
}

fn embedded_response(lookup: &str, file: &EmbeddedFile, req_headers: &HeaderMap) -> Response {
    let etag = etag_of(file);
    // Check BEFORE touching the body: on a match the 304 goes out
    // without cloning the file out of the embedded archive — the
    // conditional-request path (every `index.html` revalidation) must
    // not pay the full memcpy it exists to avoid.
    if etag_matches(req_headers.get(header::IF_NONE_MATCH), &etag) {
        return not_modified(lookup, etag);
    }
    respond_ok(lookup, file.data.clone().into_owned(), etag)
}

/// 304 — validator + cache headers, no body.
fn not_modified(lookup: &str, etag: String) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [
            (header::CACHE_CONTROL, cache_control_for(lookup).to_string()),
            (header::ETAG, etag),
        ],
        Body::empty(),
    )
        .into_response()
}

/// Split from `embedded_response` so tests can drive it without
/// constructing an `EmbeddedFile` (rust-embed's `Metadata` has private
/// fields and no public constructor).
fn respond_ok(lookup: &str, data: Vec<u8>, etag: String) -> Response {
    let mime = mime_guess::from_path(lookup).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache_control_for(lookup).to_string()),
            (header::ETAG, etag),
        ],
        data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_control_splits_hashed_assets_from_shell() {
        assert_eq!(cache_control_for("assets/index-abc123.js"), ASSETS_CACHE);
        assert_eq!(cache_control_for("assets/app.def456.css"), ASSETS_CACHE);
        assert_eq!(cache_control_for("index.html"), SHELL_CACHE);
        assert_eq!(cache_control_for("icon.svg"), SHELL_CACHE);
    }

    #[test]
    fn etag_is_quoted_hex_of_the_content_hash() {
        assert_eq!(etag_hex([0xab; 32]), format!("\"{}\"", "ab".repeat(32)));
    }

    #[test]
    fn if_none_match_accepts_exact_weak_list_and_star() {
        let etag = "\"abc\"";
        let v = |s: &str| HeaderValue::from_str(s).unwrap();
        assert!(etag_matches(Some(&v("\"abc\"")), etag));
        assert!(etag_matches(Some(&v("W/\"abc\"")), etag));
        assert!(etag_matches(Some(&v("\"other\", \"abc\"")), etag));
        assert!(etag_matches(Some(&v("*")), etag));
        assert!(!etag_matches(Some(&v("\"abcd\"")), etag));
        assert!(!etag_matches(None, etag));
    }

    #[test]
    fn full_response_carries_mime_cache_and_etag() {
        let res = respond_ok(
            "assets/index-abc.js",
            b"body".to_vec(),
            etag_hex([0x00; 32]),
        );
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(h[header::CACHE_CONTROL], ASSETS_CACHE);
        assert_eq!(h[header::CONTENT_TYPE], "text/javascript");
        assert!(h[header::ETAG].to_str().unwrap().starts_with('"'));
    }

    #[test]
    fn not_modified_carries_cache_and_etag_but_no_mime() {
        let etag = etag_hex([0x00; 32]);
        let res = not_modified("index.html", etag.clone());
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(res.headers()[header::CACHE_CONTROL], SHELL_CACHE);
        assert_eq!(res.headers()[header::ETAG], etag.as_str());
        assert!(res.headers().get(header::CONTENT_TYPE).is_none());
    }
}
