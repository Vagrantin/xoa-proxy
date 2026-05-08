//! xoa-proxy — HTTP(S) + gunzip bridge for XAPI VM.import
//!
//! XAPI's `VM.import` speaks plain HTTP and cannot consume gzip-compressed
//! streams or reach HTTPS sources directly.  This proxy bridges the gap:
//!
//!   XAPI → HTTP GET http://127.0.0.1:9001/image.xva?src=<url>
//!        → proxy detects format (extension → HEAD probe fallback)
//!        → fetches upstream (HTTP or HTTPS)
//!        → if gzip: decompresses on-the-fly via GzipDecoder
//!        → streams raw .xva back to XAPI over plain HTTP
//!
//! The Vue frontend always builds the same proxy URL regardless of whether
//! the source is HTTP or HTTPS, compressed or plain — format detection is
//! handled entirely inside the proxy.
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
//! state.rs   — shared AppState (two HTTP clients + import lock)
//! stream.rs  — fetch pipeline; GuardedStream RAII type; ImageFormat enum
//! handler.rs — axum route handlers (/image.xva, fallback)
//! error.rs   — ProxyError → HTTP response mapping
//! ```

use std::sync::Arc;
use std::fs::OpenOptions;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

use xoa_proxy_lib::{
    build_router,
    config::Config,
    state::AppState,
    stream::build_client,
};

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
    let client_verify = build_client(true)
        .context("Failed to build TLS-verifying HTTP client")?;
    let client_no_verify = build_client(false)
        .context("Failed to build TLS-non-verifying HTTP client")?;

    info!(
        "SSL clients ready — per-request selection via ?verify_ssl=<true|false> (default: true)"
    );

    let state = Arc::new(AppState {
        client_verify,
        client_no_verify,
        import_lock: Arc::new(Mutex::new(())),
    });

    // ── Router ─────────────────────────────────────────────────────────────
    let app = build_router(state);

    // ── Listen ─────────────────────────────────────────────────────────────
    let addr = format!("{}:{}", config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!(
        "Listening on http://{}/image.xva?src=<url>[&verify_ssl=false]",
        addr,
    );

    // ── Serve ──────────────────────────────────────────────────────────────
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
        _ = ctrl_c  => { info!("Received SIGINT  — shutting down") }
        _ = sigterm => { info!("Received SIGTERM — shutting down") }
    }
}
