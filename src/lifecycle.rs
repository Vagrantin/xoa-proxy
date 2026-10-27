//! Socket activation and request/body lifetime tracking. No systemd crate needed.

use std::{
    env,
    os::fd::FromRawFd,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use axum::{
    body::{Body, Bytes, HttpBody},
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use tokio::{sync::watch, time::Instant};

/// Own systemd's single listening descriptor, or bind normally outside systemd.
/// Invalid activation metadata is an error, never a fallback bind on another port.
pub async fn listener(bind: &str, port: u16) -> Result<(tokio::net::TcpListener, bool)> {
    let pid = env::var("LISTEN_PID").ok();
    let fds = env::var("LISTEN_FDS").ok();
    if pid.is_none() && fds.is_none() {
        return Ok((tokio::net::TcpListener::bind((bind, port)).await?, false));
    }
    if pid.as_deref().and_then(|p| p.parse::<u32>().ok()) != Some(std::process::id())
        || fds.as_deref() != Some("1")
    {
        bail!("Expected LISTEN_PID for this process and exactly one LISTEN_FDS socket");
    }
    // SAFETY: the systemd activation protocol transfers fd 3 to this process.
    // This is the only ownership conversion; the descriptor is closed on drop.
    let socket = unsafe { std::net::TcpListener::from_raw_fd(3) };
    socket
        .local_addr()
        .context("Activated fd 3 is not an IP socket")?;
    socket.set_nonblocking(true)?;
    Ok((tokio::net::TcpListener::from_std(socket)?, true))
}

#[derive(Clone)]
pub struct Activity(Arc<watch::Sender<usize>>);

impl Default for Activity {
    fn default() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }
}

impl Activity {
    fn enter(&self) -> ActiveRequest {
        self.0.send_modify(|active| *active += 1);
        ActiveRequest(self.clone())
    }

    /// A fresh idle window starts after the last body is completed or dropped.
    /// There is no periodic polling while the proxy is streaming an import.
    pub async fn idle(&self, timeout: Duration) {
        let mut changes = self.0.subscribe();
        loop {
            let active = *changes.borrow_and_update();
            if active == 0 {
                let deadline = Instant::now() + timeout;
                tokio::select! {
                    biased;
                    _ = changes.changed() => {},
                    _ = tokio::time::sleep_until(deadline) => return,
                }
            } else {
                let _ = changes.changed().await;
            }
        }
    }
}

struct ActiveRequest(Activity);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.0.send_modify(|active| *active -= 1);
    }
}

/// Cover validation, HEAD probes, upstream headers and the entire response body.
/// Dropping a cancelled handler or disconnected body releases activity via RAII.
pub async fn track(State(activity): State<Activity>, request: Request, next: Next) -> Response {
    let guard = activity.enter();
    let response = next.run(request).await;
    let (parts, inner) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(ActiveBody {
            inner,
            _guard: guard,
        }),
    )
}

struct ActiveBody {
    inner: Body,
    _guard: ActiveRequest,
}

impl HttpBody for ActiveBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test(start_paused = true)]
    async fn response_body_lifetime_is_active_even_after_handler_returns() {
        let activity = Activity::default();
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "image bytes" }))
            .layer(axum::middleware::from_fn_with_state(activity.clone(), track));
        let response = app.oneshot(Request::new(Body::empty())).await.unwrap();
        assert_eq!(response.body().size_hint().exact(), Some(11));
        let idle_activity = activity.clone();
        let idle = tokio::spawn(async move { idle_activity.idle(Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(!idle.is_finished());
        drop(response);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(!idle.is_finished());
        tokio::time::sleep(Duration::from_secs(1)).await;
        idle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_expires_without_requests() {
        let activity = Activity::default();
        let start = Instant::now();
        activity.idle(Duration::from_secs(60)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn active_request_prevents_shutdown_and_drop_starts_full_window() {
        let activity = Activity::default();
        let guard = activity.enter();
        let idle_activity = activity.clone();
        let idle = tokio::spawn(async move { idle_activity.idle(Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(!idle.is_finished());
        drop(guard);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(!idle.is_finished());
        tokio::time::sleep(Duration::from_secs(1)).await;
        idle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn new_request_resets_idle_window_even_if_it_finishes_immediately() {
        let activity = Activity::default();
        let idle_activity = activity.clone();
        let idle = tokio::spawn(async move { idle_activity.idle(Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_secs(59)).await;
        drop(activity.enter());
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(!idle.is_finished());
        tokio::time::sleep(Duration::from_secs(1)).await;
        idle.await.unwrap();
    }
}
