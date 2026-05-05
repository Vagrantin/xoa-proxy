//! xoa-proxy — HTTPS+gunzip bridge for XAPI VM.import
//!
//! XAPI's `VM.import` speaks plain HTTP and cannot consume gzip-compressed
//! streams.  XOA images are distributed as `.xva.gz` over HTTPS.
//!
//! This proxy bridges the gap:
//!   XAPI → HTTP GET http://127.0.0.1:9001/image.xva?src=<https://…xva.gz>
//!        → proxy fetches upstream (HTTPS), decompresses on-the-fly
//!        → streams raw .xva back to XAPI over HTTP
//!
//! # Module layout
//! ```
//! main.rs    — entry point, logging, router assembly, graceful shutdown
//! config.rs  — CLI / env-var configuration (clap derive)
//! state.rs   — shared AppState (config + HTTP client + import lock)
//! stream.rs  — fetch+decompress pipeline; GuardedStream RAII type
//! handler.rs — axum route handlers
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

    if !config.ssl_verification() {
        tracing::warn!(
            "SSL certificate verification DISABLED — \
             use only with self-signed / private-CA upstreams"
        );
    }

    // ── Shared state ───────────────────────────────────────────────────────
    let client = build_client(config.ssl_verification())?;

    let state = Arc::new(AppState {
        client,
        import_lock: Arc::new(Mutex::new(())),
    });

    // ── Router ─────────────────────────────────────────────────────────────
    let app = axum::Router::new()
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
        "Listening on  http://{}/image.xva?src=<https://…xva.gz>   \
         ssl_verification={}",
        addr,
        config.ssl_verification(),
    );

    // ── Serve ─────────────────
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
