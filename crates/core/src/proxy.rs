//! Local streaming reverse-proxy subsystem.
//!
//! Faithful port of cc-switch `src-tauri/src/proxy/`, restructured to be
//! transport-agnostic (no Tauri). The proxy binds a local axum/hyper server to
//! loopback, takes over the selected app's live config (rewriting it to point at
//! the local proxy and backing up the real config to the DB `live_backup`
//! table), and transparently forwards incoming requests to the selected
//! provider's real upstream, streaming responses back.
//!
//! The request forwarding path includes same-format passthrough, Claude
//! cross-format transforms, failover/circuit-breaker routing, managed-account
//! token injection, usage logging, request body filtering, Bedrock cache /
//! thinking optimization, media fallback, and Copilot request shaping.

pub mod body_filter;
pub mod cache_injector;
pub mod circuit_breaker;
pub mod copilot_optimizer;
pub mod error;
pub mod error_mapper;
pub mod failover_switch;
pub mod forward;
pub mod forward_transform;
pub mod gemini_url;
pub mod http_client;
pub mod json_canonical;
pub mod log_codes;
pub mod managed_auth;
pub mod media_sanitizer;
pub mod model_mapper;
pub mod provider_router;
pub mod providers;
pub mod server;
pub mod session;
pub mod sse;
pub mod thinking_budget_rectifier;
pub mod thinking_optimizer;
pub mod thinking_rectifier;
pub mod usage;

#[allow(unused_imports)]
pub use error::ProxyError;

#[allow(unused_imports)]
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerStats, CircuitState,
};
#[allow(unused_imports)]
pub use provider_router::ProviderRouter;
#[allow(unused_imports)]
pub use server::{ProxyServer, ProxyState};

// Proxy config/status types live in the already-ported db layer; re-export them
// under `crate::proxy::types` so the port matches cc-switch's `proxy::types::*`.
pub mod types {
    pub use crate::db::proxy_types::{
        ActiveTarget, AppProxyConfig, CopilotOptimizerConfig, GlobalProxyConfig, OptimizerConfig,
        ProxyConfig, ProxyServerInfo, ProxyStatus, ProxyTakeoverStatus, RectifierConfig,
    };
}

#[allow(unused_imports)]
pub use crate::db::proxy_types::{
    ActiveTarget, AppProxyConfig, CopilotOptimizerConfig, GlobalProxyConfig, OptimizerConfig,
    ProxyConfig, ProxyServerInfo, ProxyStatus, ProxyTakeoverStatus, RectifierConfig,
};

/// Placeholder token written into a live config when the proxy takes it over.
/// Keeps clients from prompting for a missing key while never leaking the real
/// token (the proxy injects the real token upstream).
pub const PROXY_TOKEN_PLACEHOLDER: &str = "PROXY_MANAGED";
