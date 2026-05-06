//! Axum route handlers.
//!
//! Three routes:
//!   `GET /resolve?src=<url>[&verify_ssl=false]`          — format probe & routing decision.
//!   `GET /image.xva?src=<url>&format=<gzip|raw>[&verify_ssl=false]` — streaming proxy.
//!   `*`                                                  — fallback 404.
//!
//! ## Routing decision matrix
//!
//! | Scheme | Format | Action                                              |
//! |--------|--------|-----------------------------------------------------|
//! | http   | xva    | **direct** — VM.import can consume HTTP+raw natively |
//! | http   | xva.gz | **proxy**  — decompress gzip, stream raw over HTTP   |
//! | https  | xva    | **proxy**  — relay HTTPS→HTTP, stream raw            |
//! | https  | xva.gz | **proxy**  — decompress gzip, relay HTTPS→HTTP       |

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, Uri, Version},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use url::form_urlencoded;

use crate::{
    error::ProxyError,
    state::AppState,
    stream::{fetch_xva_stream, ImageFormat},
};

// ── Query parameter structs ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ResolveParams {
    /// Source image URL (`http://` or `https://`).
    src: Option<String>,
    /// Whether to verify the upstream TLS certificate when probing.
    /// Defaults to `true`.
    verify_ssl: Option<bool>,
}

#[derive(Deserialize)]
pub struct ImageParams {
    /// Upstream `.xva` or `.xva.gz` URL (`http://` or `https://`).
    src: Option<String>,
    /// Image format: `gzip` or `raw`.  Set by `/resolve`; required here.
    format: Option<String>,
    /// Whether to verify the upstream TLS certificate.  Defaults to `true`.
    verify_ssl: Option<bool>,
}

// ── /resolve response ─────────────────────────────────────────────────────────

/// The routing decision returned by `GET /resolve`.
///
/// The Vue frontend calls `/resolve` before starting an import.
/// Depending on the result, it either passes `url` directly to `VM.import`
/// (action = `"direct"`) or uses the proxy URL (action = `"proxy"`).
#[derive(Serialize, Debug)]
pub struct ResolveResponse {
    /// `"direct"` — pass `url` straight to `VM.import`.
    /// `"proxy"`  — let the proxy handle fetching / decompression.
    pub action: &'static str,
    /// The URL to give to `VM.import`.
    pub url: String,
    /// Detected image format (`"gzip"` or `"raw"`).  Informational.
    pub format: String,
}

// ── Format detection helpers ──────────────────────────────────────────────────

/// Try to determine the image format from the URL's path extension alone.
///
/// Strips query strings before checking so URLs like
/// `http://host/image.xva?token=abc` are handled correctly.
fn detect_format_from_extension(src_url: &str) -> Option<ImageFormat> {
    // Work on path only, ignoring `?query` and `#fragment`.
    let path = src_url
        .split('?')
        .next()
        .unwrap_or(src_url)
        .split('#')
        .next()
        .unwrap_or(src_url)
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
/// - anything else (including `application/octet-stream`) → [`ImageFormat::Raw`]
///
/// If the HEAD request itself fails (server doesn't support HEAD, network
/// error, etc.) the error is propagated as [`ProxyError::ProbeFailed`].
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
        src           = %src_url,
        content_type  = %content_type,
        content_encoding = %content_encoding,
        detected      = %format,
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

// ── GET /resolve ──────────────────────────────────────────────────────────────

/// Inspect `src` and return the routing decision the Vue frontend should follow.
///
/// The caller (Vue) should use the returned `url` as-is for `VM.import`.
/// No import lock is acquired here — this is a lightweight probe only.
pub async fn handle_resolve(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ResolveParams>,
) -> Result<Json<ResolveResponse>, ProxyError> {
    // ── Validate src ───────────────────────────────────────────────────────
    let src_url = params
        .src
        .ok_or_else(|| ProxyError::BadRequest("Missing required query parameter: src".into()))?;

    let is_http = src_url.starts_with("http://");
    let is_https = src_url.starts_with("https://");

    if !is_http && !is_https {
        return Err(ProxyError::BadRequest(format!(
            "src must start with http:// or https://, got: {}",
            src_url.chars().take(40).collect::<String>()
        )));
    }

    // ── Select TLS client for the probe ───────────────────────────────────
    let ssl_verify = params.verify_ssl.unwrap_or(true);
    let client = if ssl_verify {
        &state.client_verify
    } else {
        &state.client_no_verify
    };

    // ── Detect image format ────────────────────────────────────────────────
    let format = detect_format(client, &src_url).await?;

    // ── Routing decision ───────────────────────────────────────────────────
    //
    //   http  + raw  → direct  (XAPI handles plain HTTP + raw XVA natively)
    //   http  + gzip → proxy   (XAPI cannot decompress gzip)
    //   https + raw  → proxy   (XAPI cannot do HTTPS; proxy relays to HTTP)
    //   https + gzip → proxy   (XAPI cannot do HTTPS or decompress gzip)
    //
    let response = if is_http && format == ImageFormat::Raw {
        info!(
            src = %src_url,
            "Routing: DIRECT — plain HTTP + raw XVA is natively supported by VM.import"
        );
        ResolveResponse {
            action: "direct",
            url: src_url,
            format: format.to_string(),
        }
    } else {
        // All other combinations require the proxy.
        let proxy_url = build_proxy_url(&state.proxy_base_addr, &src_url, format, ssl_verify);

        info!(
            src       = %src_url,
            format    = %format,
            proxy_url = %proxy_url,
            "Routing: PROXY — building proxy URL"
        );

        ResolveResponse {
            action: "proxy",
            url: proxy_url,
            format: format.to_string(),
        }
    };

    Ok(Json(response))
}

/// Construct the local proxy URL for a given source and format.
///
/// Uses `url::form_urlencoded` to percent-encode the `src` parameter so that
/// complex source URLs (with their own query strings) survive round-tripping
/// through the XAPI→proxy HTTP request without corruption.
fn build_proxy_url(
    proxy_base_addr: &str,
    src_url: &str,
    format: ImageFormat,
    ssl_verify: bool,
) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("src", src_url)
        .append_pair("format", &format.to_string())
        .append_pair("verify_ssl", &ssl_verify.to_string())
        .finish();

    format!("http://{proxy_base_addr}/image.xva?{query}")
}

// ── GET /image.xva ────────────────────────────────────────────────────────────

/// Main proxy handler.
///
/// 1. Validates the `src` and `format` query parameters.
/// 2. Selects the TLS client based on `verify_ssl` (default: `true`).
/// 3. Acquires the single-import lock (409 if already held).
/// 4. Fetches the upstream image and wraps it in the appropriate pipeline
///    (gzip decompression or raw pass-through).
/// 5. Returns a **streaming HTTP/1.0 200 OK** — see note inside.
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

    // ── Validate `format` ─────────────────────────────────────────────────
    let format: ImageFormat = params
        .format
        .ok_or_else(|| ProxyError::BadRequest("Missing required query parameter: format".into()))?
        .parse()
        .map_err(|e: String| ProxyError::BadRequest(e))?;

    // ── TLS client selection ───────────────────────────────────────────────
    // `verify_ssl` defaults to true; only opt-out is explicit `false`.
    let ssl_verify = params.verify_ssl.unwrap_or(true);
    let client = if ssl_verify {
        &state.client_verify
    } else {
        &state.client_no_verify
    };

    info!(
        src        = %src_url,
        format     = %format,
        ssl_verify = ssl_verify,
        "Import request received — selecting TLS client"
    );

    // ── Import lock ────────────────────────────────────────────────────────
    // try_lock_owned() returns an OwnedMutexGuard (no lifetime tied to `state`)
    // so we can move it into the GuardedStream below.
    let guard = Arc::clone(&state.import_lock)
        .try_lock_owned()
        .map_err(|_| ProxyError::ImportInProgress)?;

    info!(src = %src_url, "Import lock acquired — starting stream");

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
            // Builder only fails on invalid header values; all ours are static
            // string literals, so this branch is unreachable in practice.
            error!(error = %e, "Failed to build response");
            ProxyError::UpstreamFailed(format!("Internal response builder error: {e}"))
        })
}

// ── Fallback ──────────────────────────────────────────────────────────────────

/// Catches any path that is not `/resolve` or `/image.xva`.
pub async fn handle_not_found(uri: Uri) -> impl IntoResponse {
    ProxyError::NotFound(format!(
        "Unknown path '{}'. Expected /resolve or /image.xva",
        uri.path()
    ))
}
