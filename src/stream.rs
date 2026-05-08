//! Streaming fetch-and-decompress pipeline.
//!
//! Two modes are supported, selected by [`ImageFormat`]:
//!
//! **Gzip** (`.xva.gz` / `https` images):
//! ```text
//!  reqwest bytes_stream        (HTTP(S), raw .gz bytes)
//!      ↓  StreamReader          (Stream<Bytes> → AsyncRead)
//!      ↓  BufReader             (adds internal read buffer for the decoder)
//!      ↓  GzipDecoder           (async on-the-fly decompression)
//!      ↓  ReaderStream          (AsyncRead → Stream<Bytes>)
//!      ↓  GuardedStream         (holds import lock until body is consumed)
//!      →  axum Body::from_stream → TCP socket to XAPI
//! ```
//!
//! **Raw** (plain `.xva` over `https` — HTTP plain-xva bypasses the proxy):
//! ```text
//!  reqwest bytes_stream        (HTTPS, raw .xva bytes)
//!      ↓  StreamReader          (Stream<Bytes> → AsyncRead)
//!      ↓  ReaderStream          (AsyncRead → Stream<Bytes>  — no decompression)
//!      ↓  GuardedStream
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

// ── Image format ──────────────────────────────────────────────────────────────

/// Whether the upstream image is gzip-compressed or a plain XVA tar archive.
///
/// Detected once by the `/resolve` handler (extension → HEAD probe fallback)
/// and forwarded to `/image.xva` as a `format=gzip|raw` query parameter, so
/// the streaming handler never needs to inspect the body itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// `.xva.gz` — decompress with [`GzipDecoder`] before forwarding to XAPI.
    Gzip,
    /// `.xva` — stream raw bytes; XAPI receives a plain XVA tar directly.
    Raw,
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gzip => write!(f, "gzip"),
            Self::Raw => write!(f, "raw"),
        }
    }
}

impl std::str::FromStr for ImageFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "gzip" => Ok(Self::Gzip),
            "raw" => Ok(Self::Raw),
            other => Err(format!("Unknown image format '{other}': expected 'gzip' or 'raw'")),
        }
    }
}

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
    /// Counts bytes handed to axum / XAPI (post-decompression for gzip).
    bytes_sent: Arc<AtomicU64>,
    /// Original upstream URL — carried into Drop for the completion log line.
    src_url: String,
    /// Whether gzip decompression was applied.
    format: ImageFormat,
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
        let gib = bytes.bytes_to_gib();
        tracing::info!(
            src    = %self.src_url,
            format = %self.format,
            bytes_sent = bytes,
            "XVA stream finished — {:.2} GiB transferred; import lock released",
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

/// Connects to `src_url`, wraps the response in the appropriate pipeline for
/// `format`, and returns a `GuardedStream` that owns the import lock for its
/// lifetime.
///
/// | `format`          | pipeline                                      |
/// |-------------------|-----------------------------------------------|
/// | [`ImageFormat::Gzip`] | bytes → StreamReader → BufReader → GzipDecoder → ReaderStream |
/// | [`ImageFormat::Raw`]  | bytes → StreamReader → ReaderStream (no decompression)         |
///
/// The `guard` parameter is the `OwnedMutexGuard` obtained by the handler
/// before calling this function.  Moving it here transfers ownership of
/// "the lock is held" from the handler's stack frame into the stream itself,
/// ensuring the lock survives until the last byte is consumed by axum.
pub async fn fetch_xva_stream(
    client: &reqwest::Client,
    src_url: &str,
    format: ImageFormat,
    guard: OwnedMutexGuard<()>,
) -> Result<GuardedStream> {
    let response = client
        .get(src_url)
        // Tell the upstream server NOT to apply an additional HTTP-level gzip
        // layer on top of the already-.gz file content (or the raw .xva).
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("Failed to connect to upstream")?
        .error_for_status()
        .context("Upstream returned non-2xx status")?;

    tracing::info!(
        "Upstream connected: status={}, content_length={} GiB, content_type={}, format={}",
        response.status().as_u16(),
        response
            .content_length()
            .map(|n| n.bytes_to_gib())
            .map(|g| format!("{g:.2}"))
            .unwrap_or_else(|| "unknown".into()),
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown"),
        format,
    );

    // ── Pipeline assembly ──────────────────────────────────────────────────
    let byte_stream = response
        .bytes_stream()
        // Map reqwest errors into std::io::Error so StreamReader is happy
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));

    // ── Byte counter ───────────────────────────────────────────────────────
    let counter = Arc::new(AtomicU64::new(0));
    let inner: InnerStream = match format {
        ImageFormat::Gzip => {
            let gz = GzipDecoder::new(BufReader::with_capacity( 64 * 1024, StreamReader::new(byte_stream)));
            Box::pin(ReaderStream::new(gz))
        }
        ImageFormat::Raw => {
            let reader = StreamReader::new(byte_stream);
            Box::pin(ReaderStream::new(reader))
        }
    };

    let counter_clone = Arc::clone(&counter);
    let counted_stream = inner.map(move |result| {
        if let Ok(chunk) = &result {
            counter_clone.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        } else if let Err(e) = &result {
            tracing::error!(error = %e, " Strem read error — terminating");
        }
        result
    });

    Ok(GuardedStream {
        inner: Box::pin(counted_stream),
        _guard: guard,
        bytes_sent: counter,
        src_url: src_url.to_owned(),
        format,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    // ── ImageFormat::from_str ─────────────────────────────────────────────

    #[test]
    fn parse_gzip_str() {
        let f: ImageFormat = "gzip".parse().unwrap();
        assert_eq!(f, ImageFormat::Gzip);
    }

    #[test]
    fn parse_raw_str() {
        let f: ImageFormat = "raw".parse().unwrap();
        assert_eq!(f, ImageFormat::Raw);
    }

    #[test]
    fn parse_unknown_is_err() {
        let result: Result<ImageFormat, _> = "zstd".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_is_err() {
        let result: Result<ImageFormat, _> = "".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_is_case_sensitive() {
        // "Gzip" must not parse — the enum only accepts lowercase literals.
        let result: Result<ImageFormat, _> = "Gzip".parse();
        assert!(result.is_err());
    }

    // ── ImageFormat Display ───────────────────────────────────────────────

    #[test]
    fn display_gzip() {
        assert_eq!(ImageFormat::Gzip.to_string(), "gzip");
    }

    #[test]
    fn display_raw() {
        assert_eq!(ImageFormat::Raw.to_string(), "raw");
    }

    /// Round-trip: format!("{}", f).parse() must give back the same variant.
    #[test]
    fn display_roundtrips_gzip() {
        let original = ImageFormat::Gzip;
        let parsed: ImageFormat = original.to_string().parse().unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn display_roundtrips_raw() {
        let original = ImageFormat::Raw;
        let parsed: ImageFormat = original.to_string().parse().unwrap();
        assert_eq!(parsed, original);
    }

    // ── BytesToGib ────────────────────────────────────────────────────────

    #[test]
    fn zero_bytes_is_zero_gib() {
        assert_eq!(0u64.bytes_to_gib(), 0.0);
    }

    #[test]
    fn one_gib_in_bytes() {
        let one_gib: u64 = 1024 * 1024 * 1024;
        // bytes_to_gib truncates to 2 decimal places, so exactly 1 GiB → 1.0.
        assert_eq!(one_gib.bytes_to_gib(), 1.0);
    }

    #[test]
    fn half_gib_truncated() {
        let half_gib: u64 = 512 * 1024 * 1024;
        assert_eq!(half_gib.bytes_to_gib(), 0.5);
    }

    /// Verify truncation (not rounding) at the second decimal place.
    #[test]
    fn truncation_not_rounding() {
        // 1.999 GiB in bytes should truncate to 1.99, not round to 2.0.
        let bytes: u64 = (1.999 * 1024.0 * 1024.0 * 1024.0) as u64;
        let gib = bytes.bytes_to_gib();
        assert!(gib < 2.0, "expected truncation; got {gib}");
        assert!(gib >= 1.99, "expected at least 1.99; got {gib}");
    }
}
