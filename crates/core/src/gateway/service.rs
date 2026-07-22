//! Gateway lifecycle: owns the axum server task, the health prober, and the
//! shared [`GatewayState`]. Mirrors the `ProxyServer` start/stop pattern.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{oneshot, RwLock};
use tokio::task::JoinHandle;

use crate::db::Database;
use crate::error::AppError;
use crate::gateway::pipeline::GatewayState;
use crate::gateway::types::{ChannelHealth, GatewayConfig};

/// Externally visible gateway status.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GatewayStatus {
    pub running: bool,
    pub port: u16,
    pub base_url: String,
}

pub struct GatewayService {
    pub state: GatewayState,
    shutdown_tx: RwLock<Option<oneshot::Sender<()>>>,
    server_handle: RwLock<Option<JoinHandle<()>>>,
    prober_handle: RwLock<Option<JoinHandle<()>>>,
    running_port: RwLock<Option<u16>>,
}

impl GatewayService {
    pub fn new(db: Arc<Database>) -> Self {
        let config = db.get_gateway_config().unwrap_or_default();
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let state = GatewayState {
            db,
            http_client,
            config: Arc::new(RwLock::new(config)),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        Self {
            state,
            shutdown_tx: RwLock::new(None),
            server_handle: RwLock::new(None),
            prober_handle: RwLock::new(None),
            running_port: RwLock::new(None),
        }
    }

    pub async fn status(&self) -> GatewayStatus {
        let port = *self.running_port.read().await;
        match port {
            Some(p) => GatewayStatus {
                running: true,
                port: p,
                base_url: format!("http://127.0.0.1:{p}"),
            },
            None => GatewayStatus::default(),
        }
    }

    /// Reload config from the DB into the live state (applies key requirement /
    /// probe interval immediately; a port change requires restart).
    pub async fn reload_config(&self) -> Result<GatewayConfig, AppError> {
        let config = self.state.db.get_gateway_config()?;
        *self.state.config.write().await = config.clone();
        Ok(config)
    }

    /// Start the gateway server (idempotent error if already running).
    pub async fn start(&self) -> Result<GatewayStatus, AppError> {
        if self.shutdown_tx.read().await.is_some() {
            return Err(AppError::Config("gateway already running".into()));
        }
        let config = self.reload_config().await?;

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", config.port)
            .parse()
            .map_err(|e| AppError::Config(format!("invalid gateway address: {e}")))?;
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| AppError::Config(format!("gateway bind failed on {addr}: {e}")))?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(config.port);

        let router = crate::gateway::server::build_router(self.state.clone());
        let (tx, rx) = oneshot::channel();
        *self.shutdown_tx.write().await = Some(tx);

        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                log::error!("[gateway] server error: {e}");
            }
        });
        *self.server_handle.write().await = Some(handle);
        *self.running_port.write().await = Some(port);

        // Health prober.
        let prober = crate::gateway::health::spawn_prober(
            self.state.db.clone(),
            self.state.http_client.clone(),
            self.state.health.clone(),
            config.health_interval_secs,
        );
        *self.prober_handle.write().await = Some(prober);

        log::info!("[gateway] listening on http://127.0.0.1:{port}");
        Ok(GatewayStatus {
            running: true,
            port,
            base_url: format!("http://127.0.0.1:{port}"),
        })
    }

    pub async fn stop(&self) -> Result<(), AppError> {
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        } else {
            return Err(AppError::Config("gateway not running".into()));
        }
        if let Some(prober) = self.prober_handle.write().await.take() {
            prober.abort();
        }
        if let Some(handle) = self.server_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(_) => log::info!("[gateway] stopped"),
                Err(_) => log::warn!("[gateway] stop timed out"),
            }
        }
        *self.running_port.write().await = None;
        Ok(())
    }

    /// Start the gateway if configured for autostart. Errors are logged only.
    pub async fn maybe_autostart(&self) {
        let enabled = self
            .state
            .db
            .get_gateway_config()
            .map(|c| c.enabled)
            .unwrap_or(false);
        if enabled {
            if let Err(e) = self.start().await {
                log::warn!("[gateway] autostart failed: {e}");
            }
        }
    }

    /// Current health snapshot per channel id.
    pub async fn health_snapshot(&self) -> HashMap<String, ChannelHealth> {
        self.state.health.read().await.clone()
    }

    /// Run one immediate probe round (UI "refresh health" button).
    pub async fn probe_now(&self) {
        crate::gateway::health::probe_all(
            &self.state.db,
            &self.state.http_client,
            &self.state.health,
        )
        .await;
    }
}
