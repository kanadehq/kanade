//! Boxed handler error for `axum` routes.
//!
//! `axum::response::Response` is `hyper::Response<Body>` inline — 128+
//! bytes — so `Result<_, Response>` trips `clippy::result_large_err` on
//! every handler (the lint only looks at the `Err` variant's size, and the
//! `Ok` path in these handlers is typically a small `Json<T>`/`StatusCode`).
//! [`ApiError`] boxes that payload once here so the handler signatures stay
//! small; construct one from any `Response` via `.into()` (or build one
//! directly with `IntoResponse` + `.into()`, matching the existing
//! `err`/`too_many`/`db_err`-style helpers in each module).

use axum::response::{IntoResponse, Response};

pub struct ApiError(Box<Response>);

impl From<Response> for ApiError {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        *self.0
    }
}
