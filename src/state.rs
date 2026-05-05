//! Shared application state, injected into every handler by axum's `State` extractor.

use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AppState {
    /// Pre-built HTTP client — connection pool is reused across requests.
    /// Constructed once with the correct TLS settings (verify/skip).
    pub client: reqwest::Client,

    /// Non-reentrant import lock.
    ///
    /// At most one XVA stream may be active at a time: XAPI's `VM.import`
    /// is not designed for concurrent uploads consumes significant CPU.
    /// A second concurrent request receives HTTP 409 immediately via `try_lock`.
    ///
    /// Wrapped in `Arc` so we can obtain an `OwnedMutexGuard` that is
    /// `'static` and therefore movable into the streaming body — the lock is
    /// held for the *entire duration of the HTTP body transfer*, not just
    /// until the handler function returns.
    pub import_lock: Arc<Mutex<()>>,
}
