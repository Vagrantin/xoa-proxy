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


#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http::StatusCode;

    /// Helper: convert a ProxyError into a response and return just the status code.
    fn status(e: ProxyError) -> StatusCode {
        e.into_response().status()
    }

    #[test]
    fn bad_request_is_400() {
        assert_eq!(status(ProxyError::BadRequest("oops".into())), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn not_found_is_404() {
        assert_eq!(status(ProxyError::NotFound("nope".into())), StatusCode::NOT_FOUND);
    }

    #[test]
    fn import_in_progress_is_409() {
        assert_eq!(status(ProxyError::ImportInProgress), StatusCode::CONFLICT);
    }

    #[test]
    fn upstream_failed_is_502() {
        assert_eq!(status(ProxyError::UpstreamFailed("net err".into())), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn probe_failed_is_502() {
        assert_eq!(status(ProxyError::ProbeFailed("timeout".into())), StatusCode::BAD_GATEWAY);
    }

    /// The body of ImportInProgress must mention the word "import" so callers
    /// know why they were rejected.
    #[tokio::test]
    async fn import_in_progress_body_is_informative() {
        use http_body_util::BodyExt;
        let resp = ProxyError::ImportInProgress.into_response();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&bytes).unwrap().to_lowercase();
        assert!(body.contains("import"), "expected 'import' in body: {body}");
    }

    /// The body of ProbeFailed must include the caller-supplied reason string.
    #[tokio::test]
    async fn probe_failed_body_contains_reason() {
        use http_body_util::BodyExt;
        let resp = ProxyError::ProbeFailed("DNS timeout".into()).into_response();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("DNS timeout"), "expected reason in body: {body}");
    }
}
