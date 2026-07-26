//! OcHub axum server.
//!
//! Exposes the OcHub HTTP/JSON control API. The GPUI app hosts this in-process;
//! it can also run headless.

pub mod api;
pub mod api_apps;
pub mod api_data;
pub mod api_gateway;
pub mod api_more;
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
        .merge(api_data::router())
        .merge(api_gateway::router())
        .merge(api_more::router())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Serve the control API on the given loopback address until the process exits.
pub async fn serve(state: ServerState, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    // Autostart is independent from the control-API listener. In particular,
    // a control-port conflict must not prevent the in-process gateway from
    // coming online for the desktop app.
    ochub_core::services::pricing_catalog::start_background_pricing_sync(state.app.db.clone());
    state.app.gateway.maybe_autostart().await;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(state, listener).await
}

async fn serve_listener(
    state: ServerState,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!("OcHub control API listening on http://{addr}");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

/// Build state from an existing in-process `AppState` and serve.
pub async fn serve_with_app(
    app: Arc<ochub_core::app_state::AppState>,
    addr: std::net::SocketAddr,
) -> anyhow::Result<()> {
    serve(ServerState::from_app(app), addr).await
}

/// Serve from a listener that the desktop shell bound synchronously. This lets
/// the UI know whether the configured port is available before it opens while
/// still keeping the long-running axum task on the server thread.
///
/// Gateway autostart is owned by the desktop shell for this entry point.
pub async fn serve_with_app_on_listener(
    app: Arc<ochub_core::app_state::AppState>,
    listener: std::net::TcpListener,
) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    serve_listener(ServerState::from_app(app), listener).await
}
