//! Integration tests for xoa-proxy.
//!
//! These tests drive the complete axum router through `tower::ServiceExt::oneshot`,
//! which exercises every code path — handler, format detection, stream pipeline,
//! lock logic — without binding a real TCP socket.
//!
//! A `wiremock::MockServer` acts as the upstream XVA host so every HTTP exchange
//! is deterministic and offline.
//!
//! # Running
//! ```
//! cargo test
//! cargo test -- --nocapture          # show tracing output
//! cargo test integration             # only integration tests
//! ```
//!
//! # Required source changes (see TEST_SETUP.md)
//! - `detect_format_from_extension` → `pub(crate)`
//! - `pub fn build_router(state: Arc<AppState>) -> axum::Router` added to main.rs

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, Version},
};
use bytes::Bytes;
use flate2::{write::GzEncoder, Compression};
use http_body_util::BodyExt;
use std::io::Write;
use tokio::sync::Mutex;
use tower::ServiceExt; // for `.oneshot()`
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

// ── Re-export the items the tests need from the binary crate.
// Adjust the crate name if your `[package] name` in Cargo.toml differs.
use xoa_proxy_lib::{build_router, state::AppState, stream::build_client};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// A minimal but structurally valid XVA is just a tar archive.  For proxy
/// testing purposes plain bytes are sufficient — XAPI validity is not checked
/// by the proxy, only stream integrity matters.
fn fake_xva_bytes() -> Bytes {
    // 512-byte tar EOF block (two 512-byte zero blocks) — the smallest
    // legal tar archive; XAPI would accept it as an empty XVA container.
    Bytes::from(vec![0u8; 1024])
}

/// Returns `fake_xva_bytes()` compressed with gzip.
fn fake_xva_gz_bytes() -> Bytes {
    let raw = fake_xva_bytes();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).unwrap();
    Bytes::from(encoder.finish().unwrap())
}

/// Build a test `AppState` with real HTTP clients and a fresh import lock.
fn test_state() -> Arc<AppState> {
    Arc::new(AppState {
        client_verify: build_client(true).unwrap(),
        client_no_verify: build_client(false).unwrap(),
        import_lock: Arc::new(Mutex::new(())),
    })
}

/// Fire a single GET request through the router without a TCP round-trip.
async fn send(state: Arc<AppState>, uri: &str) -> axum::response::Response {
    let app = build_router(state);
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap()
}

/// Drain a response body into a `Bytes` value.
async fn body_bytes(resp: axum::response::Response) -> Bytes {
    resp.into_body().collect().await.unwrap().to_bytes()
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Parameter validation
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_src_returns_400() {
    let resp = send(test_state(), "/image.xva").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_src_returns_400() {
    let resp = send(test_state(), "/image.xva?src=").await;
    // An empty src is caught by the scheme check (doesn't start with http:// or https://).
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ftp_scheme_returns_400() {
    let resp = send(
        test_state(),
        "/image.xva?src=ftp://host/image.xva",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn file_scheme_returns_400() {
    let resp = send(
        test_state(),
        "/image.xva?src=file:///etc/passwd",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Unknown paths
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_path_returns_404() {
    let resp = send(test_state(), "/not-a-real-path").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn root_path_returns_404() {
    let resp = send(test_state(), "/").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Raw XVA streaming (HTTP upstream)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_xva_streams_correctly() {
    let server = MockServer::start().await;
    let payload = fake_xva_bytes();

    Mock::given(method("GET"))
        .and(path("/image.xva"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(payload.clone())
                .insert_header("Content-Type", "application/octet-stream"),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = body_bytes(resp).await;
    assert_eq!(received, payload, "proxy must stream raw bytes unchanged");
}

#[tokio::test]
async fn raw_xva_response_is_http10() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/image.xva"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(
        resp.version(),
        Version::HTTP_10,
        "XAPI requires HTTP/1.0 — chunked transfer encoding must not be used"
    );
}

#[tokio::test]
async fn raw_xva_content_type_is_octet_stream() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/image.xva"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    let ct = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "application/octet-stream");
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Gzip XVA streaming — decompression pipeline
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gzip_xva_is_decompressed_before_forwarding() {
    let server = MockServer::start().await;
    let raw = fake_xva_bytes();
    let compressed = fake_xva_gz_bytes();

    Mock::given(method("GET"))
        .and(path("/image.xva.gz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(compressed)
                .insert_header("Content-Type", "application/gzip"),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva.gz", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = body_bytes(resp).await;
    assert_eq!(
        received, raw,
        "proxy must decompress gzip before streaming to XAPI"
    );
}

#[tokio::test]
async fn gzip_response_is_http10() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/image.xva.gz"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fake_xva_gz_bytes()),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva.gz", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.version(), Version::HTTP_10);
}

#[tokio::test]
async fn xva_gzip_extension_also_decompresses() {
    // `.xva.gzip` is a valid alias — must decompress identically to `.xva.gz`.
    let server = MockServer::start().await;
    let raw = fake_xva_bytes();

    Mock::given(method("GET"))
        .and(path("/image.xva.gzip"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fake_xva_gz_bytes()),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva.gzip", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, raw);
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. HEAD probe fallback (ambiguous URL extension)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn head_probe_detects_gzip_via_content_type() {
    let server = MockServer::start().await;
    let raw = fake_xva_bytes();

    // HEAD probe — proxy expects Content-Type: application/gzip
    Mock::given(method("HEAD"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/gzip"),
        )
        .mount(&server)
        .await;

    // GET — proxy has now determined format=gzip and will decompress
    Mock::given(method("GET"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fake_xva_gz_bytes()),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(resp).await,
        raw,
        "HEAD probe detected gzip — body must be decompressed"
    );
}

#[tokio::test]
async fn head_probe_detects_raw_via_content_type() {
    let server = MockServer::start().await;
    let raw = fake_xva_bytes();

    Mock::given(method("HEAD"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/octet-stream"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/image"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(raw.clone()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(resp).await,
        raw,
        "HEAD probe detected raw — body must pass through unchanged"
    );
}

#[tokio::test]
async fn head_probe_detects_gzip_via_content_encoding() {
    let server = MockServer::start().await;
    let raw = fake_xva_bytes();

    // Some servers signal gzip via Content-Encoding rather than Content-Type.
    Mock::given(method("HEAD"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/octet-stream")
                .insert_header("Content-Encoding", "gzip"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fake_xva_gz_bytes()),
        )
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, raw);
}

#[tokio::test]
async fn head_probe_failure_returns_502() {
    let server = MockServer::start().await;

    // No mock registered → wiremock returns 500 for unmatched requests,
    // or we explicitly register a 503.
    Mock::given(method("HEAD"))
        .and(path("/image"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Upstream errors
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_404_returns_502() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/missing.xva"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/missing.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn upstream_500_returns_502() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/image.xva"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn unreachable_host_returns_502() {
    // Port 1 is reserved and will refuse connections on all platforms.
    let uri = "/image.xva?src=http://127.0.0.1:1/image.xva";
    let resp = send(test_state(), uri).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. Import lock — concurrency gate
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn second_concurrent_import_returns_409() {
    let server = MockServer::start().await;

    // Serve a deliberately slow response so the first request holds the lock
    // while the second one arrives.  wiremock supports response delays.
    Mock::given(method("GET"))
        .and(path("/slow.xva"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(fake_xva_bytes())
                .set_delay(std::time::Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    let state = test_state();
    let uri = format!("/image.xva?src={}/slow.xva", server.uri());

    // Spawn first request — it will hold the import lock for ~200 ms.
    let state1 = Arc::clone(&state);
    let uri1 = uri.clone();
    let first = tokio::spawn(async move { send(state1, &uri1).await });

    // Give the first request just enough time to acquire the lock.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request must be rejected immediately.
    let resp2 = send(Arc::clone(&state), &uri).await;
    assert_eq!(
        resp2.status(),
        StatusCode::CONFLICT,
        "concurrent import must return 409 Conflict"
    );

    // Let the first request finish cleanly.
    let resp1 = first.await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
}

#[tokio::test]
async fn lock_released_after_first_import_completes() {
    // After the first import finishes, the lock must be free for a second request.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()))
        .mount(&server)
        .await;

    let state = test_state();
    let uri = format!("/image.xva?src={}/image.xva", server.uri());

    // First import — consume the full body to release the lock.
    let resp1 = send(Arc::clone(&state), &uri).await;
    assert_eq!(resp1.status(), StatusCode::OK);
    body_bytes(resp1).await; // drain body → GuardedStream::drop → lock released

    // Second import must succeed, not 409.
    let resp2 = send(Arc::clone(&state), &uri).await;
    assert_eq!(resp2.status(), StatusCode::OK);
}

// ─────────────────────────────────────────────────────────────────────────────
// 8. SSL verification flag
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn verify_ssl_defaults_to_true() {
    // With a plain HTTP upstream this flag has no visible effect, but we can
    // confirm the request succeeds with the default (verify_ssl not specified).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva", server.uri());
    let resp = send(test_state(), &uri).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn verify_ssl_false_is_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva&verify_ssl=false", server.uri());
    let resp = send(test_state(), &uri).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn verify_ssl_true_is_accepted_explicitly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_xva_bytes()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/image.xva&verify_ssl=true", server.uri());
    let resp = send(test_state(), &uri).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ─────────────────────────────────────────────────────────────────────────────
// 9. Body integrity — larger payloads
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn large_raw_payload_is_transferred_intact() {
    // 4 MiB — large enough to span multiple stream chunks.
    let payload = Bytes::from(vec![0xABu8; 4 * 1024 * 1024]);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/large.xva"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/large.xva", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let received = body_bytes(resp).await;
    assert_eq!(received.len(), payload.len(), "byte count must match");
    assert_eq!(received, payload, "every byte must be identical");
}

#[tokio::test]
async fn large_gzip_payload_decompresses_intact() {
    // Build a 4 MiB raw payload, compress it, serve the compressed bytes,
    // and verify the proxy decompresses back to the original.
    let raw = Bytes::from(vec![0xCDu8; 4 * 1024 * 1024]);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).unwrap();
    let compressed = Bytes::from(encoder.finish().unwrap());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/large.xva.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed))
        .mount(&server)
        .await;

    let uri = format!("/image.xva?src={}/large.xva.gz", server.uri());
    let resp = send(test_state(), &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, raw);
}

// ─────────────────────────────────────────────────────────────────────────────
// 10. Regression: HTTP/1.1 chunked encoding must never reach XAPI
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn response_is_never_http11() {
    let server = MockServer::start().await;

    // Test both raw and gzip paths.
    for (path_str, body) in [
        ("/a.xva", fake_xva_bytes()),
        ("/b.xva.gz", fake_xva_gz_bytes()),
    ] {
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(path_str))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let uri = format!("/image.xva?src={}{}", server.uri(), path_str);
        let resp = send(test_state(), &uri).await;

        assert_ne!(
            resp.version(),
            Version::HTTP_11,
            "HTTP/1.1 response for {path_str} would send chunked encoding to XAPI"
        );
        assert_eq!(resp.version(), Version::HTTP_10);
    }
}
