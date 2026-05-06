//! xoa-proxy — HTTPS+gunzip bridge for XAPI VM.import
//!
//! XAPI's `VM.import` speaks plain HTTP and cannot consume gzip-compressed
//! streams.  XOA images are distributed as `.xva.gz` over HTTPS.
//!
//! This proxy bridges the gap:
//!   XAPI → HTTP GET http://127.0.0.1:9001/image.xva?src=<url>&format=<gzip|raw>
//!        → proxy fetches upstream (HTTP or HTTPS)
//!        → if format=gzip: decompresses on-the-fly via GzipDecoder
//!        → streams raw .xva back to XAPI over HTTP
//!
//! A lightweight `/resolve` endpoint lets the Vue frontend decide, before
//! starting an import, whether to hand the source URL directly to XAPI
//! (plain HTTP + raw XVA) or to route through this proxy:
//!
//! | Scheme | Format | Routing                                           |
//! |--------|--------|---------------------------------------------------|
//! | http   | xva    | **direct** — XAPI handles it natively             |
//! | http   | xva.gz | **proxy**  — decompress gzip                      |
//! | https  | xva    | **proxy**  — relay HTTPS → HTTP, stream raw       |
//! | https  | xva.gz | **proxy**  — relay HTTPS → HTTP + decompress gzip |
//!
//! SSL verification is controlled **per-request** via the `verify_ssl` query
//! parameter (default: `true`).  Both a verifying and a non-verifying
//! reqwest client are constructed once at startup; the handler selects the
//! appropriate one without any restart or config reload.
//!
//! # Module layout
//! ```
//! main.rs    — entry point, logging, router assembly, graceful shutdown
//! config.rs  — CLI / env-var configuration (clap derive)
//! state.rs   — shared AppState (two HTTP clients + import lock + proxy addr)
//! stream.rs  — fetch+decompress pipeline; GuardedStream RAII type
//! handler.rs — axum route handlers (/resolve, /image.xva, fallback)
//! error.rs   — ProxyError → HTTP response mapping
//! ```

mod config;
mod error;
mod handler;
mod state;
mod stream;

use std::sync::Arc;
use std::fs::OpenOptions;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::Config;
use state::AppState;
use stream::build_client;

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ────────────────────────────────────────────────────────────
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/var/log/xoa-proxy.log")
        .context("Failed to open /var/log/xoa-proxy.log")?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                EnvFilter::new("xoa_proxy=info,warn")
            }),
        )
        .with_target(false)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    // ── Configuration ──────────────────────────────────────────────────────
    let config = Config::parse();

    // ── Shared state ───────────────────────────────────────────────────────
    // Both clients are built eagerly at startup so there is no per-request
    // allocation cost when switching TLS policy.  They share no connection
    // pool — each has an independent pool, which is intentional: a caller
    // that switches from verify=true to verify=false must not reuse a
    // connection that was established under stricter settings.
    let client_verify = build_client(true)
        .context("Failed to build TLS-verifying HTTP client")?;
    let client_no_verify = build_client(false)
        .context("Failed to build TLS-non-verifying HTTP client")?;

    info!(
        "SSL clients ready — per-request selection via ?verify_ssl=<true|false> (default: true)"
    );

    // The proxy's own listen address, forwarded to /resolve so it can
    // construct fully-qualified proxy URLs without knowing the bind config.
    let proxy_base_addr = format!("{}:{}", config.bind, config.port);

    let state = Arc::new(AppState {
        client_verify,
        client_no_verify,
        import_lock: Arc::new(Mutex::new(())),
        proxy_base_addr: proxy_base_addr.clone(),
    });

    // ── Router ─────────────────────────────────────────────────────────────
    let app = axum::Router::new()
        // Lightweight probe: returns { action, url, format } — no import lock.
        .route(
            "/resolve",
            axum::routing::get(handler::handle_resolve),
        )
        // Streaming proxy: fetches src, optionally decompresses, streams to XAPI.
        .route(
            "/image.xva",
            axum::routing::get(handler::handle_image_xva),
        )
        .fallback(handler::handle_not_found)
        .with_state(state);

    // ── Listen ─────────────────────────────────────────────────────────────
    let addr = format!("{}:{}", config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!(
        bind = %proxy_base_addr,
        "Listening — endpoints:\n  \
         GET http://{}/resolve?src=<url>[&verify_ssl=false]\n  \
         GET http://{}/image.xva?src=<url>&format=<gzip|raw>[&verify_ssl=false]",
        proxy_base_addr, proxy_base_addr,
    );

    // ── Serve ─────────────────────────────────────────────────────────────
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

// ── Signal handling ───────────────────────────────────────────────────────────
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c    => { info!("Received SIGINT  — shutting down") }
        _ = sigterm   => { info!("Received SIGTERM — shutting down") }
    }
}
