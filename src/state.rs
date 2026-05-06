//! Shared application state, injected into every handler by axum's `State` extractor.

use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AppState {
    /// Pre-built HTTP client with TLS certificate verification **enabled**.
    /// Used when the upstream URL is trusted (default).
    pub client_verify: reqwest::Client,

    /// Pre-built HTTP client with TLS certificate verification **disabled**.
    /// Used when the caller passes `?verify_ssl=false`, e.g. for self-signed
    /// or private-CA upstreams.  Both clients share no state — they each have
    /// their own connection pool so the security boundary is never crossed.
    pub client_no_verify: reqwest::Client,

    /// Non-reentrant import lock.
    ///
    /// At most one XVA stream may be active at a time: XAPI's `VM.import`
    /// is not designed for concurrent image download.
    /// A second concurrent request receives HTTP 409 immediately via `try_lock`.
    ///
    /// Wrapped in `Arc` so we can obtain an `OwnedMutexGuard` that is
    /// `'static` and therefore movable into the streaming body — the lock is
    /// held for the *entire duration of the HTTP body transfer*, not just
    /// until the handler function returns.
    pub import_lock: Arc<Mutex<()>>,

    /// `"host:port"` string of this proxy's own listening address.
    ///
    /// Used by `/resolve` to construct the proxy URL it returns to the Vue
    /// frontend (e.g. `"127.0.0.1:9001"`).  Kept as a plain string to avoid
    /// allocating on every resolve request.
    pub proxy_base_addr: String,
}
