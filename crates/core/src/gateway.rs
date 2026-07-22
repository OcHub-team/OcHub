//! Local relay gateway (中转网关).
//!
//! A standing loopback HTTP server that mimics relay-station semantics
//! entirely locally:
//!
//! - Standard inference endpoints for all three wire dialects —
//!   `/v1/messages` (+`count_tokens`), `/v1/chat/completions`,
//!   `/v1/responses` (SSE + WebSocket) and `/v1/models`.
//! - **Channels**: configured upstreams, each speaking one dialect, with model
//!   matchers, priority groups and weights ([`types::GatewayChannel`]).
//! - **Routing**: model name → weighted/priority candidate list
//!   ([`router::candidates_for_model`]), with per-channel failover and health
//!   probing ([`health`]).
//! - **Dialect conversion** between inlet and channel via `ochub-convert`.
//! - **Local API keys** for per-app usage attribution; one-click app
//!   configuration writes gateway URL + key into each app's live config
//!   ([`apply`]).

pub mod apply;
pub mod health;
pub mod pipeline;
pub mod router;
pub mod server;
pub mod service;
pub mod types;

pub use service::GatewayService;
pub use types::{ChannelHealth, Dialect, GatewayChannel, GatewayConfig, GatewayKey};

/// Generate a fresh local API key secret (`rd-` + 32 hex chars).
pub fn generate_key_secret() -> String {
    format!("rd-{}", uuid::Uuid::new_v4().simple())
}
