//! Axum route handlers.
//!
//! There are only two routes:
//!   `GET /image.xva?src=<url>` — the proxy endpoint.
//!   `*`                        — fallback 404.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, Uri, Version},
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{error::ProxyError, state::AppState, stream::fetch_xva_stream};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ImageParams {
    /// Upstream `.xva.gz` URL.  Must start with `https://`.
    src: Option<String>,
}

// ── GET /image.xva ────────────────────────────────────────────────────────────

/// Main proxy handler.
///
/// 1. Validates the `src` query parameter.
/// 2. Acquires the single-import lock (409 if already held).
/// 3. Fetches the upstream .xva.gz and wraps it in a decompression pipeline.
/// 4. Returns a **streaming HTTP/1.0 200 OK** — see comment inside.
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

    if !src_url.starts_with("https://") {
        return Err(ProxyError::BadRequest(format!(
            "src must start with https://, got: {}",
            src_url.chars().take(40).collect::<String>()
        )));
    }

    // ── Import lock ────────────────────────────────────────────────────────
    // try_lock_owned() returns an OwnedMutexGuard (no lifetime tied to `state`)
    // so we can move it into the GuardedStream below.
    let guard = Arc::clone(&state.import_lock)
        .try_lock_owned()
        .map_err(|_| ProxyError::ImportInProgress)?;

    info!(src = %src_url, "Import lock acquired — starting stream");

    // ── Build the decompression pipeline ───────────────────────────────────
    let stream = fetch_xva_stream(&state.client, &src_url, guard)
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

/// Catches any path that is not `/image.xva`.
pub async fn handle_not_found(uri: Uri) -> impl IntoResponse {
    ProxyError::NotFound(format!(
        "Unknown path '{}'. Expected /image.xva",
        uri.path()
    ))
}
