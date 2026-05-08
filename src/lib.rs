pub mod config;
pub mod error;
pub mod handler;
pub mod state;
pub mod stream;

use std::sync::Arc;

pub fn build_router(state: Arc<state::AppState>) -> axum::Router {
    axum::Router::new()
        .route(
            "/image.xva",
            axum::routing::get(handler::handle_image_xva),
        )
        .fallback(handler::handle_not_found)
        .with_state(state)
}
