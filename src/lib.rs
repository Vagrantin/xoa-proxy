pub mod config;
pub mod error;
pub mod handler;
pub mod state;
pub mod stream;

pub fn build_router(state: state::AppState) -> axum::Router {
    axum::Router::new()
        .route("/image.xva", axum::routing::get(handler::handle_image_xva))
        .fallback(handler::handle_not_found)
        .with_state(state)
}

// QA demo (xcp-hl#147), never to be merged: deliberately misformatted.
pub fn   qa_demo( )->u8{1}
