//! RouteDeck axum server.
//!
//! Exposes the cc-switch command surface as an HTTP/JSON control API and (later)
//! hosts the local provider proxy. The GPUI app hosts this in-process; it can
//! also run headless.

pub mod api;
pub mod api_apps;
pub mod api_data;
pub mod api_more;
pub mod api_proxy;
pub mod error;
pub mod state;

use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;

pub use error::{ApiError, ApiResult};
pub use state::ServerState;

/// Build the full control-API router with state applied.
pub fn build_router(state: ServerState) -> Router {
    api::router()
        .merge(api_apps::router())
        .merge(api_proxy::router())
        .merge(api_data::router())
        .merge(api_more::router())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Serve the control API on the given loopback address until the process exits.
pub async fn serve(state: ServerState, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("RouteDeck control API listening on http://{addr}");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

/// Build state from an existing in-process `AppState` and serve.
pub async fn serve_with_app(
    app: Arc<routedeck_core::app_state::AppState>,
    addr: std::net::SocketAddr,
) -> anyhow::Result<()> {
    serve(ServerState::from_app(app), addr).await
}
