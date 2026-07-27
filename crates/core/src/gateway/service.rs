//! Gateway lifecycle hosted by an in-process background runtime.
//!
//! GPUI futures are not guaranteed to run inside a Tokio runtime. Keeping the
//! listener on a dedicated worker lets UI and control-API callers use the same
//! lifecycle API without restarting the app or launching a terminal process.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::JoinHandle;

use crate::db::Database;
use crate::error::AppError;
use crate::gateway::pipeline::GatewayState;
use crate::gateway::types::{ChannelHealth, Dialect, GatewayConfig, GatewayEndpointTestResult};

/// Externally visible gateway status.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatus {
    pub running: bool,
    pub port: u16,
    pub base_url: String,
}

enum GatewayCommand {
    Start(oneshot::Sender<Result<GatewayStatus, AppError>>),
    Stop(oneshot::Sender<Result<(), AppError>>),
    Reload(oneshot::Sender<Result<GatewayConfig, AppError>>),
    Probe(oneshot::Sender<Result<(), AppError>>),
    DetectDialects {
        base_url: String,
        api_key: String,
        reply: oneshot::Sender<Vec<Dialect>>,
    },
    FetchModels {
        base_url: String,
        api_key: String,
        reply: oneshot::Sender<Result<Vec<String>, String>>,
    },
    TestEndpoint {
        base_url: String,
        api_key: String,
        reply: oneshot::Sender<Result<GatewayEndpointTestResult, String>>,
    },
    Shutdown,
}

/// Public gateway handle. The actual listener and its Tokio tasks always live
/// on `worker_thread`, never on the UI/main thread or in a child process.
pub struct GatewayService {
    pub state: GatewayState,
    running_port: Arc<RwLock<Option<u16>>>,
    command_tx: Option<mpsc::UnboundedSender<GatewayCommand>>,
    worker_error: Option<String>,
    worker_thread: Option<std::thread::JoinHandle<()>>,
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
        let running_port = Arc::new(RwLock::new(None));
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ochub-gateway-io")
            .enable_all()
            .build();

        let (command_tx, worker_error, worker_thread) = match runtime {
            Ok(runtime) => {
                let worker = GatewayWorker::new(state.clone(), running_port.clone());
                match std::thread::Builder::new()
                    .name("ochub-gateway".into())
                    .spawn(move || runtime.block_on(worker.run(command_rx)))
                {
                    Ok(thread) => (Some(command_tx), None, Some(thread)),
                    Err(err) => (
                        None,
                        Some(format!("failed to start gateway worker thread: {err}")),
                        None,
                    ),
                }
            }
            Err(err) => (
                None,
                Some(format!("failed to build gateway runtime: {err}")),
                None,
            ),
        };

        Self {
            state,
            running_port,
            command_tx,
            worker_error,
            worker_thread,
        }
    }

    fn send(&self, command: GatewayCommand) -> Result<(), AppError> {
        let Some(command_tx) = &self.command_tx else {
            return Err(AppError::Config(self.worker_error.clone().unwrap_or_else(
                || "gateway background service is unavailable".into(),
            )));
        };
        command_tx
            .send(command)
            .map_err(|_| AppError::Config("gateway background service stopped unexpectedly".into()))
    }

    pub async fn status(&self) -> GatewayStatus {
        status_for_port(*self.running_port.read().await)
    }

    /// Reload live settings on the gateway worker. If the probe interval
    /// changed while the gateway is running, its periodic task is replaced.
    pub async fn reload_config(&self) -> Result<GatewayConfig, AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::Reload(reply_tx))?;
        reply_rx.await.map_err(|_| {
            AppError::Config("gateway background service dropped the reload request".into())
        })?
    }

    /// Start the gateway in the app's background worker. Repeated or racing
    /// start requests are idempotent and return the existing listener.
    pub async fn start(&self) -> Result<GatewayStatus, AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::Start(reply_tx))?;
        reply_rx.await.map_err(|_| {
            AppError::Config("gateway background service dropped the start request".into())
        })?
    }

    /// Stop the in-process listener. This never terminates or restarts OcHub.
    pub async fn stop(&self) -> Result<(), AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::Stop(reply_tx))?;
        reply_rx.await.map_err(|_| {
            AppError::Config("gateway background service dropped the stop request".into())
        })?
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
                log::warn!("[gateway] background autostart failed: {e}");
            }
        }
    }

    /// Current health snapshot per channel id.
    pub async fn health_snapshot(&self) -> HashMap<String, ChannelHealth> {
        self.state.health.read().await.clone()
    }

    /// Run one immediate probe round on the background runtime.
    pub async fn probe_now(&self) -> Result<(), AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::Probe(reply_tx))?;
        reply_rx.await.map_err(|_| {
            AppError::Config("gateway background service dropped the probe request".into())
        })?
    }

    /// Detect every API dialect exposed by an upstream on the background
    /// runtime (see [`crate::gateway::health::detect_dialects`]).
    pub async fn detect_dialects(
        &self,
        base_url: String,
        api_key: String,
    ) -> Result<Vec<Dialect>, AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::DetectDialects {
            base_url,
            api_key,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            AppError::Config("gateway background service dropped the detect request".into())
        })
    }

    /// Backwards-compatible single-dialect view of [`Self::detect_dialects`].
    pub async fn detect_dialect(
        &self,
        base_url: String,
        api_key: String,
    ) -> Result<Option<Dialect>, AppError> {
        Ok(self
            .detect_dialects(base_url, api_key)
            .await?
            .into_iter()
            .next())
    }

    /// Fetch the upstream's OpenAI-compatible model list on the background
    /// runtime. The UI may still add or edit models manually afterwards.
    pub async fn fetch_models(
        &self,
        base_url: String,
        api_key: String,
    ) -> Result<Vec<String>, AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::FetchModels {
            base_url,
            api_key,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| {
                AppError::Config("gateway background service dropped the model request".into())
            })?
            .map_err(AppError::Config)
    }

    /// Run a user-triggered HTTP latency test against one upstream URL.
    pub async fn test_endpoint(
        &self,
        base_url: String,
        api_key: String,
    ) -> Result<GatewayEndpointTestResult, AppError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(GatewayCommand::TestEndpoint {
            base_url,
            api_key,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| {
                AppError::Config("gateway background service dropped the endpoint test".into())
            })?
            .map_err(AppError::Config)
    }
}

impl Drop for GatewayService {
    fn drop(&mut self) {
        if let Some(command_tx) = &self.command_tx {
            let _ = command_tx.send(GatewayCommand::Shutdown);
        }
        if let Some(worker_thread) = self.worker_thread.take() {
            if worker_thread.thread().id() != std::thread::current().id() {
                let _ = worker_thread.join();
            }
        }
    }
}

struct GatewayWorker {
    state: GatewayState,
    running_port: Arc<RwLock<Option<u16>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_handle: Option<JoinHandle<()>>,
    prober_handle: Option<JoinHandle<()>>,
}

impl GatewayWorker {
    fn new(state: GatewayState, running_port: Arc<RwLock<Option<u16>>>) -> Self {
        Self {
            state,
            running_port,
            shutdown_tx: None,
            server_handle: None,
            prober_handle: None,
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<GatewayCommand>) {
        while let Some(command) = commands.recv().await {
            match command {
                GatewayCommand::Start(reply) => {
                    let _ = reply.send(self.start().await);
                }
                GatewayCommand::Stop(reply) => {
                    let _ = reply.send(self.stop().await);
                }
                GatewayCommand::Reload(reply) => {
                    let _ = reply.send(self.reload_config().await);
                }
                GatewayCommand::Probe(reply) => {
                    let state = self.state.clone();
                    tokio::spawn(async move {
                        crate::gateway::health::probe_all(
                            &state.db,
                            &state.http_client,
                            &state.health,
                        )
                        .await;
                        let _ = reply.send(Ok(()));
                    });
                }
                GatewayCommand::DetectDialects {
                    base_url,
                    api_key,
                    reply,
                } => {
                    let client = self.state.http_client.clone();
                    tokio::spawn(async move {
                        let dialects =
                            crate::gateway::health::detect_dialects(&client, &base_url, &api_key)
                                .await;
                        let _ = reply.send(dialects);
                    });
                }
                GatewayCommand::FetchModels {
                    base_url,
                    api_key,
                    reply,
                } => {
                    let client = self.state.http_client.clone();
                    tokio::spawn(async move {
                        let result = crate::gateway::health::fetch_endpoint_models(
                            &client, &base_url, &api_key,
                        )
                        .await;
                        let _ = reply.send(result);
                    });
                }
                GatewayCommand::TestEndpoint {
                    base_url,
                    api_key,
                    reply,
                } => {
                    let client = self.state.http_client.clone();
                    tokio::spawn(async move {
                        let result =
                            crate::gateway::health::test_endpoint(&client, &base_url, &api_key)
                                .await;
                        let _ = reply.send(result);
                    });
                }
                GatewayCommand::Shutdown => break,
            }
        }
        self.shutdown_now().await;
    }

    async fn reload_config(&mut self) -> Result<GatewayConfig, AppError> {
        let config = self.state.db.get_gateway_config()?;
        *self.state.config.write().await = config.clone();
        if self.shutdown_tx.is_some() {
            self.restart_prober(&config);
        }
        Ok(config)
    }

    async fn reap_finished_server(&mut self) {
        let finished = self
            .server_handle
            .as_ref()
            .is_some_and(JoinHandle::is_finished);
        if !finished {
            return;
        }
        if let Some(handle) = self.server_handle.take() {
            let _ = handle.await;
        }
        self.shutdown_tx.take();
        if let Some(prober) = self.prober_handle.take() {
            prober.abort();
        }
        *self.running_port.write().await = None;
    }

    async fn start(&mut self) -> Result<GatewayStatus, AppError> {
        self.reap_finished_server().await;
        if self.shutdown_tx.is_some() {
            return Ok(status_for_port(*self.running_port.read().await));
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
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let running_port = self.running_port.clone();
        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = server.await {
                log::error!("[gateway] server error: {e}");
            }
            let mut current_port = running_port.write().await;
            if *current_port == Some(port) {
                *current_port = None;
            }
        });

        self.shutdown_tx = Some(shutdown_tx);
        self.server_handle = Some(handle);
        *self.running_port.write().await = Some(port);
        self.restart_prober(&config);

        log::info!("[gateway] listening in background on http://127.0.0.1:{port}");
        Ok(status_for_port(Some(port)))
    }

    async fn stop(&mut self) -> Result<(), AppError> {
        self.reap_finished_server().await;
        let Some(shutdown_tx) = self.shutdown_tx.take() else {
            return Ok(());
        };
        let _ = shutdown_tx.send(());
        if let Some(prober) = self.prober_handle.take() {
            prober.abort();
        }
        if let Some(mut handle) = self.server_handle.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle).await {
                Ok(Ok(())) => log::info!("[gateway] stopped"),
                Ok(Err(err)) => log::warn!("[gateway] server task stopped with error: {err}"),
                Err(_) => {
                    log::warn!("[gateway] graceful stop timed out; aborting listener task");
                    handle.abort();
                    let _ = handle.await;
                }
            }
        }
        *self.running_port.write().await = None;
        Ok(())
    }

    fn restart_prober(&mut self, config: &GatewayConfig) {
        if let Some(prober) = self.prober_handle.take() {
            prober.abort();
        }
        if config.health_interval_secs == 0 || self.shutdown_tx.is_none() {
            return;
        }
        self.prober_handle = Some(crate::gateway::health::spawn_prober(
            self.state.db.clone(),
            self.state.http_client.clone(),
            self.state.health.clone(),
            config.health_interval_secs,
        ));
    }

    async fn shutdown_now(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(prober) = self.prober_handle.take() {
            prober.abort();
        }
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        *self.running_port.write().await = None;
    }
}

fn status_for_port(port: Option<u16>) -> GatewayStatus {
    match port {
        Some(port) => GatewayStatus {
            running: true,
            port,
            base_url: format!("http://127.0.0.1:{port}"),
        },
        None => GatewayStatus::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use super::*;

    #[test]
    fn autostart_runs_without_a_caller_tokio_runtime() {
        let db = Arc::new(Database::memory().unwrap());
        db.set_gateway_config(&GatewayConfig {
            enabled: true,
            port: 0,
            health_interval_secs: 0,
            ..GatewayConfig::default()
        })
        .unwrap();
        let gateway = GatewayService::new(db);

        futures::executor::block_on(gateway.maybe_autostart());
        let first = futures::executor::block_on(gateway.status());
        assert!(first.running);
        assert_ne!(first.port, 0);

        let second = futures::executor::block_on(gateway.start()).unwrap();
        assert_eq!(second.port, first.port);

        let mut stream = TcpStream::connect(("127.0.0.1", first.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        futures::executor::block_on(gateway.stop()).unwrap();
        assert!(!futures::executor::block_on(gateway.status()).running);
        futures::executor::block_on(gateway.stop()).unwrap();
    }
}
