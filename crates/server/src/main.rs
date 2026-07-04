//! Headless RouteDeck server entrypoint.
//!
//! Initializes the SQLite store and serves the control API on loopback. The
//! GPUI app uses the library entrypoints instead (it owns `AppState`).

use std::net::SocketAddr;

use routedeck_server::ServerState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port: u16 = std::env::var("MS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let state = ServerState::init().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    routedeck_server::serve(state, addr).await
}
