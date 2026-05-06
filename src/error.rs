//! Application error type.
//!
//! Every variant maps to a distinct HTTP status code and is converted directly
//! into an axum `Response` via `IntoResponse`, keeping handler return types
//! clean (`Result<Response, ProxyError>`).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug)]
pub enum ProxyError {
    /// 400 — caller omitted a required parameter or sent a bad value.
    BadRequest(String),
    /// 404 — path not recognised.
    NotFound(String),
    /// 409 — another XVA import is already streaming; only one is allowed.
    ImportInProgress,
    /// 502 — upstream HTTP(S) fetch failed.
    UpstreamFailed(String),
    /// 502 — HEAD probe to detect image format failed.
    ///
    /// Raised by `/resolve` when the URL extension is ambiguous and the
    /// fallback HEAD request cannot be completed (network error, server
    /// returns 4xx/5xx, etc.).
    ProbeFailed(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::ImportInProgress => (
                StatusCode::CONFLICT,
                "An import is already in progress. Only one concurrent import is allowed.".into(),
            ),
            Self::UpstreamFailed(msg) => (StatusCode::BAD_GATEWAY, msg),
            Self::ProbeFailed(msg) => (
                StatusCode::BAD_GATEWAY,
                format!("Format probe failed — could not determine image type: {msg}"),
            ),
        };

        tracing::warn!(status = status.as_u16(), detail = %body, "HTTP error");
        (status, body).into_response()
    }
}
