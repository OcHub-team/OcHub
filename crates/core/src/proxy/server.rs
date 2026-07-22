//! Local HTTP proxy server (axum).
//!
//! Binds an axum server to loopback and routes incoming CLI requests to the
//! selected provider upstream via the transparent passthrough forwarder.
//!
//! The cc-switch reference used a manual hyper HTTP/1.1 accept loop with
//! `preserve_header_case(true)` to reproduce direct CLI wire casing. This port
//! intentionally uses axum's standard `serve`; HTTP header names are
//! case-insensitive, and the managed CLIs this proxy targets do not depend on
//! byte-identical header casing.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, RawQuery, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

use crate::app_type::AppType;
use crate::db::Database;
use crate::proxy::providers::codex_chat_history::CodexChatHistoryStore;
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::providers::copilot_auth::CopilotAuthManager;

use super::circuit_breaker::CircuitBreakerConfig;
use super::forward;
use super::log_codes::srv as log_srv;
use super::provider_router::ProviderRouter;
use super::providers::gemini_shadow::GeminiShadowStore;
use super::{ProxyConfig, ProxyServerInfo, ProxyStatus};

/// Errors raised by the proxy server lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum ProxyServerError {
    #[error("proxy server already running")]
    AlreadyRunning,
    #[error("proxy server not running")]
    NotRunning,
    #[error("bind failed: {0}")]
    BindFailed(String),
    #[error("stop failed: {0}")]
    StopFailed(String),
    #[error("stop timeout")]
    StopTimeout,
}

/// Shared proxy server state.
#[derive(Clone)]
pub struct ProxyState {
    pub db: Arc<Database>,
    pub copilot_auth: Arc<RwLock<CopilotAuthManager>>,
    pub codex_oauth: Arc<RwLock<CodexOAuthManager>>,
    pub config: Arc<RwLock<ProxyConfig>>,
    pub status: Arc<RwLock<ProxyStatus>>,
    pub start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// Shared ProviderRouter (holds cross-request circuit-breaker state).
    pub provider_router: Arc<ProviderRouter>,
    /// Failover-switch dedup manager.
    /// Gemini Native assistant-turn shadow state for tool/thought replay.
    pub gemini_shadow: Arc<GeminiShadowStore>,
    /// Codex Responses -> Chat bridge history for restoring tool-call context.
    pub codex_chat_history: Arc<CodexChatHistoryStore>,
    /// Shared reqwest client for upstream forwarding.
    pub http_client: reqwest::Client,
}

/// The proxy HTTP server.
pub struct ProxyServer {
    config: ProxyConfig,
    state: ProxyState,
    shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
    server_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl ProxyServer {
    pub fn new(
        config: ProxyConfig,
        db: Arc<Database>,
        copilot_auth: Arc<RwLock<CopilotAuthManager>>,
        codex_oauth: Arc<RwLock<CodexOAuthManager>>,
    ) -> Self {
        let provider_router = Arc::new(ProviderRouter::new(db.clone()));
        let gemini_shadow = Arc::new(GeminiShadowStore::default());
        let codex_chat_history = Arc::new(CodexChatHistoryStore::default());

        let http_client = reqwest::Client::builder()
            // Stream large bodies; no global timeout (per-request streaming).
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let state = ProxyState {
            db,
            copilot_auth,
            codex_oauth,
            config: Arc::new(RwLock::new(config.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            provider_router,
            gemini_shadow,
            codex_chat_history,
            http_client,
        };

        Self {
            config,
            state,
            shutdown_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, ProxyServerError> {
        if self.shutdown_tx.read().await.is_some() {
            return Err(ProxyServerError::AlreadyRunning);
        }

        let addr: SocketAddr =
            format!("{}:{}", self.config.listen_address, self.config.listen_port)
                .parse()
                .map_err(|e| ProxyServerError::BindFailed(format!("invalid address: {e}")))?;

        let app = self.build_router();

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ProxyServerError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ProxyServerError::BindFailed(e.to_string()))?;
        let actual_port = local_addr.port();

        log::info!(
            "[{}] proxy server listening on {local_addr}",
            log_srv::STARTED
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        {
            let mut status = self.state.status.write().await;
            status.running = true;
            status.address = self.config.listen_address.clone();
            status.port = actual_port;
        }
        *self.state.start_time.write().await = Some(std::time::Instant::now());

        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = server.await {
                log::error!("[{}] proxy server error: {e}", log_srv::TASK_ERROR);
            }
            state.status.write().await.running = false;
            *state.start_time.write().await = None;
        });

        *self.server_handle.write().await = Some(handle);

        Ok(ProxyServerInfo {
            address: self.config.listen_address.clone(),
            port: actual_port,
            started_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub async fn stop(&self) -> Result<(), ProxyServerError> {
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        } else {
            return Err(ProxyServerError::NotRunning);
        }

        if let Some(handle) = self.server_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    log::info!("[{}] proxy server stopped", log_srv::STOPPED);
                    Ok(())
                }
                Ok(Err(e)) => {
                    log::warn!("[{}] proxy server task aborted: {e}", log_srv::TASK_ERROR);
                    Err(ProxyServerError::StopFailed(e.to_string()))
                }
                Err(_) => {
                    log::warn!(
                        "[{}] proxy server stop timed out (5s)",
                        log_srv::STOP_TIMEOUT
                    );
                    Err(ProxyServerError::StopTimeout)
                }
            }
        } else {
            Ok(())
        }
    }

    pub async fn get_status(&self) -> ProxyStatus {
        let mut status = self.state.status.read().await.clone();
        if let Some(start) = *self.state.start_time.read().await {
            status.uptime_seconds = start.elapsed().as_secs();
        }
        status
    }

    pub async fn apply_runtime_config(&self, config: &ProxyConfig) {
        *self.state.config.write().await = config.clone();
    }

    pub async fn update_circuit_breaker_configs(&self, config: CircuitBreakerConfig) {
        self.state.provider_router.update_all_configs(config).await;
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: CircuitBreakerConfig,
    ) {
        self.state
            .provider_router
            .update_app_configs(app_type, config)
            .await;
    }

    pub async fn reset_provider_circuit_breaker(&self, provider_id: &str, app_type: &str) {
        self.state
            .provider_router
            .reset_provider_breaker(provider_id, app_type)
            .await;
    }

    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<super::circuit_breaker::CircuitBreakerStats> {
        self.state
            .provider_router
            .get_circuit_breaker_stats(provider_id, app_type)
            .await
    }

    fn build_router(&self) -> Router {
        Router::new()
            .route("/health", get(health_check))
            // Claude API (with and without prefix)
            .route("/v1/messages", post(handle_claude))
            .route("/claude/v1/messages", post(handle_claude))
            // OpenAI Chat Completions / Responses (Codex CLI)
            .route("/chat/completions", post(handle_codex))
            .route("/v1/chat/completions", post(handle_codex))
            .route("/v1/v1/chat/completions", post(handle_codex))
            .route("/codex/v1/chat/completions", post(handle_codex))
            .route("/responses", post(handle_codex))
            .route("/v1/responses", post(handle_codex))
            .route("/v1/v1/responses", post(handle_codex))
            .route("/codex/v1/responses", post(handle_codex))
            .route("/responses/compact", post(handle_codex))
            .route("/v1/responses/compact", post(handle_codex))
            .route("/codex/v1/responses/compact", post(handle_codex))
            .route("/models", get(handle_codex))
            .route("/v1/models", get(handle_codex))
            // Gemini API (any method: SDK/CLI sends GET /models too)
            .route("/v1beta/{*path}", any(handle_gemini))
            .route("/gemini/v1beta/{*path}", any(handle_gemini))
            .route("/gemini/v1/{*path}", any(handle_gemini))
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
            .with_state(self.state.clone())
    }
}

async fn health_check() -> Response {
    (axum::http::StatusCode::OK, "OK").into_response()
}

async fn handle_claude(
    State(state): State<ProxyState>,
    method: Method,
    RawQuery(query): RawQuery,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward::forward(
        state,
        AppType::Claude,
        method,
        uri.path(),
        query.as_deref(),
        headers,
        body,
    )
    .await
}

async fn handle_codex(
    State(state): State<ProxyState>,
    method: Method,
    RawQuery(query): RawQuery,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward::forward(
        state,
        AppType::Codex,
        method,
        uri.path(),
        query.as_deref(),
        headers,
        body,
    )
    .await
}

async fn handle_gemini(
    State(state): State<ProxyState>,
    method: Method,
    RawQuery(query): RawQuery,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward::forward(
        state,
        AppType::Gemini,
        method,
        uri.path(),
        query.as_deref(),
        headers,
        body,
    )
    .await
}
