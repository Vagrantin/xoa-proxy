//! Axum route handlers.
//!
//! Two routes:
//!   `GET /image.xva?src=<url>[&verify_ssl=false]` — streaming proxy.
//!   `*`                                            — fallback 404.
//!
//! ## Format detection
//!
//! The image format (gzip-compressed vs plain XVA) is detected automatically:
//!   1. URL path extension (`.xva.gz` / `.xva.gzip` → Gzip; `.xva` → Raw).
//!   2. HEAD probe as fallback when the extension is ambiguous.
//!
//! ## Routing decision matrix
//!
//! | Scheme | Format | Proxy action                               |
//! |--------|--------|--------------------------------------------|
//! | http   | xva    | relay HTTP → HTTP, stream raw bytes        |
//! | http   | xva.gz | relay HTTP → HTTP, decompress gzip         |
//! | https  | xva    | relay HTTPS → HTTP, stream raw bytes       |
//! | https  | xva.gz | relay HTTPS → HTTP, decompress gzip        |
//!
//! All four cases are handled transparently by the single `/image.xva`
//! endpoint. The Vue frontend always builds the same proxy URL regardless of
//! the source scheme or format.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, Uri, Version},
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::{
    error::ProxyError,
    state::AppState,
    stream::{fetch_xva_stream, ImageFormat},
};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ImageParams {
    /// Upstream `.xva` or `.xva.gz` URL (`http://` or `https://`).
    src: Option<String>,

    /// Whether to verify the upstream TLS certificate.
    ///
    /// Defaults to `true` (verification on). Pass `verify_ssl=false` to skip
    /// certificate checks — use only with self-signed / private-CA upstreams.
    verify_ssl: Option<bool>,
}

// ── Format detection ──────────────────────────────────────────────────────────

/// Try to determine the image format from the URL path extension alone.
///
/// Strips query strings and fragments before checking, so URLs like
/// `http://host/image.xva?token=abc` are handled correctly.
fn detect_format_from_extension(src_url: &str) -> Option<ImageFormat> {
    let path = src_url
        .split('?').next().unwrap_or(src_url)
        .split('#').next().unwrap_or(src_url)
        .to_lowercase();

    if path.ends_with(".xva.gz") || path.ends_with(".xva.gzip") {
        Some(ImageFormat::Gzip)
    } else if path.ends_with(".xva") {
        Some(ImageFormat::Raw)
    } else {
        None
    }
}

/// Fall back to a HEAD request when the URL extension is ambiguous.
///
/// Inspects `Content-Type` and `Content-Encoding` response headers:
/// - `application/gzip`, `application/x-gzip`, or `Content-Encoding: gzip`
///   → [`ImageFormat::Gzip`]
/// - anything else → [`ImageFormat::Raw`]
async fn detect_format_via_head(
    client: &reqwest::Client,
    src_url: &str,
) -> Result<ImageFormat, ProxyError> {
    warn!(src = %src_url, "URL extension is ambiguous — sending HEAD probe");

    let response = client
        .head(src_url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|e| ProxyError::ProbeFailed(format!("HEAD request failed: {e}")))?;

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let content_encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let format = if content_type.contains("gzip") || content_encoding.contains("gzip") {
        ImageFormat::Gzip
    } else {
        ImageFormat::Raw
    };

    info!(
        src              = %src_url,
        content_type     = %content_type,
        content_encoding = %content_encoding,
        detected         = %format,
        "HEAD probe complete"
    );

    Ok(format)
}

/// Detect image format: try extension first, HEAD probe as fallback.
async fn detect_format(
    client: &reqwest::Client,
    src_url: &str,
) -> Result<ImageFormat, ProxyError> {
    if let Some(format) = detect_format_from_extension(src_url) {
        info!(src = %src_url, detected = %format, "Format detected from URL extension");
        return Ok(format);
    }
    detect_format_via_head(client, src_url).await
}

// ── GET /image.xva ────────────────────────────────────────────────────────────

/// Main proxy handler.
///
/// 1. Validates the `src` query parameter.
/// 2. Selects the TLS client based on `verify_ssl` (default: `true`).
/// 3. Detects image format (extension → HEAD probe fallback).
/// 4. Acquires the single-import lock (409 if already held).
/// 5. Fetches the upstream image and wraps it in the appropriate pipeline
///    (gzip decompression or raw pass-through).
/// 6. Returns a **streaming HTTP/1.0 200 OK** — see note inside.
///
/// The import lock is released only when axum finishes writing the body —
/// `GuardedStream::drop` fires regardless of success or client disconnect.
pub async fn handle_image_xva(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ImageParams>,
) -> Result<axum::http::Response<Body>, ProxyError> {
    // ── Validate `src` ─────────────────────────────────────────────────────
    let src_url = params
        .src
        .ok_or_else(|| ProxyError::BadRequest("Missing required query parameter: src".into()))?;

    if !src_url.starts_with("http://") && !src_url.starts_with("https://") {
        return Err(ProxyError::BadRequest(format!(
            "src must start with http:// or https://, got: {}",
            src_url.chars().take(40).collect::<String>()
        )));
    }

    // ── TLS client selection ───────────────────────────────────────────────
    let ssl_verify = params.verify_ssl.unwrap_or(true);
    let client = if ssl_verify {
        &state.client_verify
    } else {
        &state.client_no_verify
    };

    info!(src = %src_url, ssl_verify, "Import request received");

    // ── Format detection ───────────────────────────────────────────────────
    let format = detect_format(client, &src_url).await?;

    // ── Import lock ────────────────────────────────────────────────────────
    // try_lock_owned() returns an OwnedMutexGuard (no lifetime tied to `state`)
    // so we can move it into the GuardedStream below.
    let guard = Arc::clone(&state.import_lock)
        .try_lock_owned()
        .map_err(|_| ProxyError::ImportInProgress)?;

    info!(src = %src_url, %format, "Import lock acquired — starting stream");

    // ── Build the pipeline ─────────────────────────────────────────────────
    let stream = fetch_xva_stream(client, &src_url, format, guard)
        .await
        .map_err(|e| {
            error!(error = %e, src = %src_url, "Upstream fetch failed");
            ProxyError::UpstreamFailed(format!("Failed to fetch upstream image: {e}"))
        })?;

    // ── Return a streaming HTTP/1.0 response ───────────────────────────────
    // CRITICAL: must use HTTP/1.0, NOT HTTP/1.1.
    //
    // HTTP/1.1 without a known Content-Length requires chunked transfer
    // encoding (RFC 7230 §4.1). Hyper applies it automatically, framing
    // every chunk as:
    //
    //   ffff\r\n
    //   <65535 bytes of XVA tar data>\r\n
    //   ...
    //
    // XAPI's `open_uri` HTTP client reads raw socket bytes without decoding
    // HTTP/1.1 chunk framing. The first bytes it sees are therefore the hex
    // chunk-size string "ffff\r\n", not a tar magic header. XAPI then:
    //   1. Decides the data is not a plain tar → "Failed to directly open"
    //   2. Forks `nice`/gunzip to try gzip decompression
    //   3. gunzip rejects non-gzip bytes → "nice failed to decompress: exit code 1"
    //   4. Closes the connection → proxy sees BrokenPipe → stream ends at 0.2 MiB
    //
    // HTTP/1.0 has no chunked encoding: body bytes are written verbatim and
    // the connection close signals EOF. XAPI reads until EOF — correct for
    // a stream whose decompressed length is unknown at response time.
    axum::http::Response::builder()
        .version(Version::HTTP_10)
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .header("Connection", "close")
        .body(Body::from_stream(stream))
        .map_err(|e| {
            error!(error = %e, "Failed to build response");
            ProxyError::UpstreamFailed(format!("Internal response builder error: {e}"))
        })
}

// ── Fallback ──────────────────────────────────────────────────────────────────

/// Catches any path that is not `/image.xva`.
pub async fn handle_not_found(uri: Uri) -> impl IntoResponse {
    ProxyError::NotFound(format!(
        "Unknown path '{}'. Expected /image.xva",
        uri.path()
    ))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ImageFormat;

    // ── detect_format_from_extension ──────────────────────────────────────

    // --- positive: gzip variants ---

    #[test]
    fn xva_gz_is_gzip() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva.gz"),
            Some(ImageFormat::Gzip)
        );
    }

    #[test]
    fn xva_gzip_is_gzip() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva.gzip"),
            Some(ImageFormat::Gzip)
        );
    }

    #[test]
    fn uppercase_xva_gz_is_gzip() {
        // Function lowercases before matching, so .XVA.GZ must still work.
        assert_eq!(
            detect_format_from_extension("http://host/IMAGE.XVA.GZ"),
            Some(ImageFormat::Gzip)
        );
    }

    #[test]
    fn mixed_case_xva_gzip() {
        assert_eq!(
            detect_format_from_extension("http://host/image.XvA.GzIp"),
            Some(ImageFormat::Gzip)
        );
    }

    // --- positive: raw variants ---

    #[test]
    fn xva_is_raw() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva"),
            Some(ImageFormat::Raw)
        );
    }

    #[test]
    fn uppercase_xva_is_raw() {
        assert_eq!(
            detect_format_from_extension("http://host/image.XVA"),
            Some(ImageFormat::Raw)
        );
    }

    // --- query strings and fragments are stripped before matching ---

    #[test]
    fn xva_with_query_string_is_raw() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva?token=abc&foo=bar"),
            Some(ImageFormat::Raw)
        );
    }

    #[test]
    fn xva_gz_with_query_string_is_gzip() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva.gz?v=1"),
            Some(ImageFormat::Gzip)
        );
    }

    #[test]
    fn xva_with_fragment_is_raw() {
        assert_eq!(
            detect_format_from_extension("http://host/image.xva#section"),
            Some(ImageFormat::Raw)
        );
    }

    // --- ambiguous / unknown extensions return None ---

    #[test]
    fn no_extension_returns_none() {
        assert_eq!(detect_format_from_extension("http://host/image"), None);
    }

    #[test]
    fn dot_gz_only_returns_none() {
        // ".gz" without ".xva" prefix should NOT match.
        assert_eq!(detect_format_from_extension("http://host/image.gz"), None);
    }

    #[test]
    fn tar_extension_returns_none() {
        assert_eq!(
            detect_format_from_extension("http://host/image.tar"), None);
    }

    #[test]
    fn tar_gz_returns_none() {
        assert_eq!(
            detect_format_from_extension("http://host/image.tar.gz"), None);
    }

    #[test]
    fn empty_url_returns_none() {
        assert_eq!(detect_format_from_extension(""), None);
    }

    // --- path components don't bleed into extension matching ---

    #[test]
    fn xva_in_directory_name_does_not_match_gz() {
        // The file itself has no XVA extension; only the directory name does.
        assert_eq!(detect_format_from_extension("http://host/archive.xva/image.tar"), None);
    }
}
