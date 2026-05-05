//! Streaming fetch-and-decompress pipeline.
//!
//! ```text
//!  reqwest bytes_stream        (HTTPS, raw .gz bytes)
//!      ↓  StreamReader          (Stream<Bytes> → AsyncRead)
//!      ↓  BufReader             (adds internal read buffer for the decoder)
//!      ↓  GzipDecoder           (async on-the-fly decompression)
//!      ↓  ReaderStream          (AsyncRead → Stream<Bytes>)
//!      ↓  GuardedStream         (holds import lock until body is consumed)
//!      →  axum Body::from_stream → TCP socket to XAPI
//! ```
//!
//! The key invariant: the import `MutexGuard` is embedded inside
//! `GuardedStream` and therefore lives exactly as long as the HTTP body.
//! Whether axum finishes normally, the client disconnects early, or the
//! future is cancelled, Rust's drop guarantee ensures the lock is released.

use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use anyhow::{Context as AnyhowContext, Result};
use async_compression::tokio::bufread::GzipDecoder;
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use tokio::io::BufReader;
use tokio::sync::OwnedMutexGuard;
use tokio_util::io::{ReaderStream, StreamReader};

/// Type-erased inner stream: `Stream<Item = io::Result<Bytes>>`.
type InnerStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

// ── GuardedStream ─────────────────────────────────────────────────────────────

/// A byte stream that holds the import-lock guard for its entire lifetime.
///
/// When this struct is dropped — whether the transfer completed cleanly,
/// the connection was reset, or the future was cancelled — the `OwnedMutexGuard`
/// inside is dropped as well, atomically releasing the import lock.
///
/// A running byte counter is also embedded so we can log transfer statistics
/// in `Drop` without any external bookkeeping.
pub struct GuardedStream {
    inner: InnerStream,
    /// Released (lock freed) exactly when this stream is dropped.
    _guard: OwnedMutexGuard<()>,
    /// Counts decompressed bytes handed to axum / XAPI.
    bytes_sent: Arc<AtomicU64>,
    /// Original upstream URL — carried into Drop for the completion log line.
    src_url: String,
}

impl Stream for GuardedStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // GuardedStream is Unpin: Pin<Box<dyn Stream>> is Unpin (Box<T>: Unpin for all T),
        // and all other fields are Unpin, so get_mut() is safe here.
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

impl Drop for GuardedStream {
    fn drop(&mut self) {
        let bytes = self.bytes_sent.load(Ordering::Relaxed);
        let _mib = bytes as f64 / (1024.0 * 1024.0);
        let gib = bytes.bytes_to_gib();
        tracing::info!(
            src  = %self.src_url,
            bytes_sent = bytes,
            "Import of XVA finished — {} GiB transferred; import lock released",
            gib,
        );
    }
}

trait BytesToGib {
    fn bytes_to_gib(&self) -> f64;
}

impl BytesToGib for u64 {
    fn bytes_to_gib(&self) -> f64 {
        let gib = *self as f64 / (1024.0 * 1024.0 * 1024.0);
        (gib * 100.0).trunc() / 100.0
    }
}
// ── Client builder ────────────────────────────────────────────────────────────

/// Build the shared `reqwest::Client` according to TLS preferences.
///
/// - Uses rustls with Mozilla WebPKI roots (no OpenSSL dependency —
///   required for a fully static musl build targeting XCP-ng Dom0).
/// - `no_gzip()` / `no_deflate()` / `no_brotli()` disable reqwest's
///   automatic HTTP transfer-encoding decompression: the upstream .xva.gz
///   is a *content* gzip, not a transport-encoded response, and we must
///   receive the raw bytes to decompress them ourselves.
pub fn build_client(ssl_verify: bool) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent("xoa-lite-proxy/1.0")
        .no_gzip()
        .no_deflate()
        .no_brotli()
        .danger_accept_invalid_certs(!ssl_verify); // rustls: disables cert + hostname check

    builder
        .build()
        .context("Failed to build HTTP client")
}

// ── Stream factory ────────────────────────────────────────────────────────────

/// Connects to `src_url`, wraps the response in a decompression pipeline,
/// and returns a `GuardedStream` that owns the import lock for its lifetime.
///
/// The `guard` parameter is the `OwnedMutexGuard` obtained by the handler
/// before calling this function.  Moving it here transfers ownership of
/// "the lock is held" from the handler's stack frame into the stream itself,
/// ensuring the lock survives until the last byte is consumed by axum.
pub async fn fetch_xva_stream(
    client: &reqwest::Client,
    src_url: &str,
    guard: OwnedMutexGuard<()>,
) -> Result<GuardedStream> {
    let response = client
        .get(src_url)
        // Tell the upstream server NOT to apply an additional HTTP-level gzip
        // layer on top of the already-.gz file content.
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("Failed to connect to upstream")?
        .error_for_status()
        .context("Upstream returned non-2xx status")?;
    
//    tracing::info!(
//        status = response.status().as_u16(),
//        content_length_in_Gb = response.content_length().unwrap().bytes_to_gib(),
//        content_type = ?response.headers()
//            .get(reqwest::header::CONTENT_TYPE)
//            .and_then(|v| v.to_str().ok()).unwrap(),
//        status,
//        content_length_in_Gb,
//        content_type,
//    );

    tracing::info!(
        "Upstream connected: status={}, Compressed XVA image size={} GiB, content_type={}",
        response.status().as_u16(),
        response.content_length().unwrap().bytes_to_gib(),
        response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap(),
    );

    // ── Pipeline assembly ──────────────────────────────────────────────────
    // reqwest bytes_stream → StreamReader → BufReader → GzipDecoder → ReaderStream

    let byte_stream = response
        .bytes_stream()
        // Map reqwest errors into std::io::Error so StreamReader is happy
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));

    // 64 KiB internal buffer — matches Python CHUNK_SIZE; large enough to
    // keep the GzipDecoder fed without excessive syscalls.
    let gz = GzipDecoder::new(BufReader::with_capacity(64 * 1024, StreamReader::new(byte_stream)));

    // ── Byte counter ───────────────────────────────────────────────────────
    let counter = Arc::new(AtomicU64::new(0));
    let counter_clone = Arc::clone(&counter);

    let counted_stream = ReaderStream::new(gz).map(move |result| {
        match &result {
            Ok(chunk) => {
                counter_clone.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                // Log decompression / IO errors inline so they appear in the
                // journal before the "Stream ended" drop message.
                tracing::error!(error = %e, "Gzip decompression error — stream will terminate");
            }
        }
        result
    });

    Ok(GuardedStream {
        inner: Box::pin(counted_stream),
        _guard: guard,
        bytes_sent: counter,
        src_url: src_url.to_owned(),
    })
}

