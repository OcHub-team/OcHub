//! Control-API routes for the local provider proxy (start/stop/status/config +
//! per-app takeover) plus provider reachability checks.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use ochub_core::db::proxy_types::{
    AppProxyConfig, CircuitBreakerConfig, CopilotOptimizerConfig, GlobalProxyConfig, LogConfig,
    OptimizerConfig, ProxyConfig, RectifierConfig,
};
use ochub_core::db::stream_check_types::{HealthStatus, StreamCheckConfig, StreamCheckResult};
use ochub_core::proxy::http_client;
use ochub_core::services::StreamCheckService;
use ochub_core::{AppError, AppType, Provider};

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

async fn proxy_start(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let info = s.app.proxy_service.start().await?;
    to_value(info)
}

async fn proxy_stop(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    s.app.proxy_service.stop().await?;
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_stop_restore(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    s.app.proxy_service.stop_with_restore().await?;
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_status(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.proxy_service.get_status().await?)
}

async fn proxy_running(State(s): State<ServerState>) -> Json<Value> {
    Json(json!({ "running": s.app.proxy_service.is_running().await }))
}

async fn proxy_get_config(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.proxy_service.get_config().await?)
}

async fn proxy_update_config(
    State(s): State<ServerState>,
    Json(config): Json<ProxyConfig>,
) -> ApiResult<Json<Value>> {
    s.app.proxy_service.update_config(&config).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn rectifier_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_rectifier_config()?)
}

async fn rectifier_config_set(
    State(s): State<ServerState>,
    Json(config): Json<RectifierConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_rectifier_config(&config)?;
    Ok(Json(json!({ "ok": true })))
}

async fn optimizer_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_optimizer_config()?)
}

async fn optimizer_config_set(
    State(s): State<ServerState>,
    Json(config): Json<OptimizerConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_optimizer_config(&config)?;
    Ok(Json(json!({ "ok": true })))
}

async fn copilot_optimizer_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_copilot_optimizer_config()?)
}

async fn copilot_optimizer_config_set(
    State(s): State<ServerState>,
    Json(config): Json<CopilotOptimizerConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_copilot_optimizer_config(&config)?;
    Ok(Json(json!({ "ok": true })))
}

async fn log_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_log_config()?)
}

async fn log_config_set(
    State(s): State<ServerState>,
    Json(config): Json<LogConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_log_config(&config)?;
    log::set_max_level(config.to_level_filter());
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_get_global_config(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_global_proxy_config().await?)
}

async fn proxy_update_global_config(
    State(s): State<ServerState>,
    Json(config): Json<GlobalProxyConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.update_global_proxy_config(config).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_get_app_config(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_proxy_config_for_app(&app).await?)
}

async fn proxy_update_app_config(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(mut config): Json<AppProxyConfig>,
) -> ApiResult<Json<Value>> {
    config.app_type = app;
    let circuit_config = CircuitBreakerConfig::from(&config);
    let app_type = config.app_type.clone();
    s.app.db.update_proxy_config_for_app(config).await?;
    s.app
        .proxy_service
        .update_circuit_breaker_config_for_app(&app_type, circuit_config)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_takeover_status(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.proxy_service.get_takeover_status().await?)
}

#[derive(Deserialize)]
struct TakeoverRequest {
    enabled: bool,
}

async fn proxy_set_takeover(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<TakeoverRequest>,
) -> ApiResult<Json<Value>> {
    s.app
        .proxy_service
        .set_takeover_for_app(&app, req.enabled)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn proxy_live_takeover_active(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let active = s.app.proxy_service.is_takeover_active().await?;
    Ok(Json(json!({ "active": active })))
}

#[derive(Deserialize)]
struct SwitchProxyRequest {
    #[serde(rename = "appType")]
    app_type: String,
    #[serde(rename = "providerId")]
    provider_id: String,
}

async fn proxy_switch_provider(
    State(s): State<ServerState>,
    Json(req): Json<SwitchProxyRequest>,
) -> ApiResult<Json<Value>> {
    let provider = s
        .app
        .db
        .get_provider_by_id(&req.provider_id, &req.app_type)?
        .ok_or_else(|| {
            ApiError(AppError::InvalidInput(format!(
                "Provider does not exist: {}",
                req.provider_id
            )))
        })?;
    if provider.category.as_deref() == Some("official") {
        return Err(ApiError(AppError::InvalidInput(
            "Cannot switch to an official provider during proxy takeover".to_string(),
        )));
    }
    s.app
        .proxy_service
        .switch_proxy_target(&req.app_type, &req.provider_id)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn provider_health(
    State(s): State<ServerState>,
    Path((app, provider_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_provider_health(&provider_id, &app).await?)
}

#[derive(Deserialize)]
struct CircuitBreakerResetRequest {
    #[serde(rename = "appType")]
    app_type: String,
    #[serde(rename = "providerId")]
    provider_id: String,
}

async fn reset_circuit_breaker(
    State(s): State<ServerState>,
    Json(req): Json<CircuitBreakerResetRequest>,
) -> ApiResult<Json<Value>> {
    s.app
        .db
        .update_provider_health(&req.provider_id, &req.app_type, true, None)
        .await?;
    s.app
        .proxy_service
        .reset_provider_circuit_breaker(&req.provider_id, &req.app_type)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn circuit_breaker_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_circuit_breaker_config().await?)
}

async fn circuit_breaker_config_update(
    State(s): State<ServerState>,
    Json(config): Json<CircuitBreakerConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.update_circuit_breaker_config(&config).await?;
    s.app
        .proxy_service
        .update_circuit_breaker_configs(config.clone())
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn circuit_breaker_stats(
    State(s): State<ServerState>,
    Path((app, provider_id)): Path<(String, String)>,
) -> Json<Value> {
    Json(json!({
        "stats": s
            .app
            .proxy_service
            .get_circuit_breaker_stats(&provider_id, &app)
            .await
    }))
}

#[derive(Deserialize)]
struct ScalarStringRequest {
    value: String,
}

async fn default_cost_multiplier_get(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "value": s.app.db.get_default_cost_multiplier(&app).await?
    })))
}

async fn default_cost_multiplier_set(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<ScalarStringRequest>,
) -> ApiResult<Json<Value>> {
    s.app
        .db
        .set_default_cost_multiplier(&app, &req.value)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn pricing_model_source_get(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "value": s.app.db.get_pricing_model_source(&app).await?
    })))
}

async fn pricing_model_source_set(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<ScalarStringRequest>,
) -> ApiResult<Json<Value>> {
    s.app.db.set_pricing_model_source(&app, &req.value).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamProxyStatus {
    enabled: bool,
    proxy_url: Option<String>,
}

async fn upstream_proxy_status() -> Json<Value> {
    let url = http_client::get_current_proxy_url();
    Json(json!(UpstreamProxyStatus {
        enabled: url.is_some(),
        proxy_url: url,
    }))
}

async fn upstream_proxy_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "url": s.app.db.get_global_proxy_url()?
    })))
}

#[derive(Deserialize)]
struct UpstreamProxySetRequest {
    #[serde(default)]
    url: String,
}

async fn upstream_proxy_set(
    State(s): State<ServerState>,
    Json(req): Json<UpstreamProxySetRequest>,
) -> ApiResult<Json<Value>> {
    let trimmed = req.url.trim();
    let url_opt = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };
    http_client::validate_proxy(url_opt)?;
    s.app.db.set_global_proxy_url(url_opt)?;
    http_client::apply_proxy(url_opt)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyTestResult {
    success: bool,
    latency_ms: u64,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ProxyUrlRequest {
    url: String,
}

async fn upstream_proxy_test(Json(req): Json<ProxyUrlRequest>) -> ApiResult<Json<Value>> {
    let url = req.url.trim();
    if url.is_empty() {
        return Err(ApiError(AppError::InvalidInput(
            "Proxy URL is empty".to_string(),
        )));
    }

    let start = Instant::now();
    let proxy = reqwest::Proxy::all(url)
        .map_err(|e| ApiError(AppError::InvalidInput(format!("Invalid proxy URL: {e}"))))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| ApiError(AppError::Message(format!("Failed to build client: {e}"))))?;

    let test_urls = [
        "https://httpbin.org/get",
        "https://www.google.com",
        "https://api.anthropic.com",
    ];
    let mut last_error = None;
    for test_url in test_urls {
        match client.head(test_url).send().await {
            Ok(_) => {
                return to_value(ProxyTestResult {
                    success: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: None,
                })
            }
            Err(err) => last_error = Some(err.to_string()),
        }
    }

    to_value(ProxyTestResult {
        success: false,
        latency_ms: start.elapsed().as_millis() as u64,
        error: Some(last_error.unwrap_or_else(|| "All test targets failed".to_string())),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectedProxy {
    url: String,
    proxy_type: String,
    port: u16,
}

const PROXY_PORTS: &[(u16, &str, bool)] = &[
    (7890, "http", true),
    (7891, "socks5", false),
    (1080, "socks5", false),
    (8080, "http", false),
    (8888, "http", false),
    (3128, "http", false),
    (10808, "socks5", false),
    (10809, "http", false),
];

async fn upstream_proxy_scan() -> Json<Value> {
    let found = tokio::task::spawn_blocking(|| {
        let mut found = Vec::new();
        for &(port, primary_type, is_mixed) in PROXY_PORTS {
            let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            if TcpStream::connect_timeout(&addr.into(), Duration::from_millis(100)).is_ok() {
                found.push(DetectedProxy {
                    url: format!("{primary_type}://127.0.0.1:{port}"),
                    proxy_type: primary_type.to_string(),
                    port,
                });
                if is_mixed {
                    let alt_type = if primary_type == "http" {
                        "socks5"
                    } else {
                        "http"
                    };
                    found.push(DetectedProxy {
                        url: format!("{alt_type}://127.0.0.1:{port}"),
                        proxy_type: alt_type.to_string(),
                        port,
                    });
                }
            }
        }
        found
    })
    .await
    .unwrap_or_default();

    Json(json!(found))
}

#[derive(Deserialize)]
struct StreamCheckProviderRequest {
    #[serde(rename = "appType")]
    app_type: AppType,
    #[serde(rename = "providerId")]
    provider_id: String,
}

#[derive(Deserialize)]
struct StreamCheckAllRequest {
    #[serde(rename = "appType")]
    app_type: AppType,
    #[serde(default, rename = "proxyTargetsOnly")]
    proxy_targets_only: bool,
}

async fn resolve_copilot_base_url_override(
    state: &ServerState,
    provider: &Provider,
) -> Result<Option<String>, ApiError> {
    let is_copilot = is_copilot_provider(provider);
    let is_full_url = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.is_full_url)
        .unwrap_or(false);

    if !is_copilot || is_full_url {
        return Ok(None);
    }

    let auth_manager = state.app.copilot_auth.read().await;
    let account_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("github_copilot"));

    let endpoint = match account_id.as_deref() {
        Some(id) => auth_manager.get_api_endpoint(id).await,
        None => auth_manager.get_default_api_endpoint().await,
    };

    Ok(Some(endpoint))
}

fn is_copilot_provider(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.as_deref())
        == Some("github_copilot")
        || provider
            .settings_config
            .pointer("/env/ANTHROPIC_BASE_URL")
            .and_then(|value| value.as_str())
            .map(|url| url.contains("githubcopilot.com"))
            .unwrap_or(false)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn stream_check_provider(
    State(s): State<ServerState>,
    Json(req): Json<StreamCheckProviderRequest>,
) -> ApiResult<Json<Value>> {
    let config = s.app.db.get_stream_check_config()?;
    let providers = s.app.db.get_all_providers(req.app_type.as_str())?;
    let provider = providers.get(&req.provider_id).ok_or_else(|| {
        ApiError(AppError::Message(format!(
            "Provider {} does not exist",
            req.provider_id
        )))
    })?;
    let base_url_override = resolve_copilot_base_url_override(&s, provider).await?;
    let result =
        StreamCheckService::check_with_retry(&req.app_type, provider, &config, base_url_override)
            .await?;
    let _ = s.app.db.save_stream_check_log(
        &req.provider_id,
        &provider.name,
        req.app_type.as_str(),
        &result,
    );
    to_value(result)
}

async fn stream_check_all(
    State(s): State<ServerState>,
    Json(req): Json<StreamCheckAllRequest>,
) -> ApiResult<Json<Value>> {
    let config = s.app.db.get_stream_check_config()?;
    let providers = s.app.db.get_all_providers(req.app_type.as_str())?;

    let allowed_ids: Option<HashSet<String>> = if req.proxy_targets_only {
        let mut ids = HashSet::new();
        if let Ok(Some(current_id)) = s.app.db.get_current_provider(req.app_type.as_str()) {
            ids.insert(current_id);
        }
        Some(ids)
    } else {
        None
    };

    let mut results: Vec<(String, StreamCheckResult)> = Vec::new();
    for (id, provider) in providers {
        if allowed_ids
            .as_ref()
            .map(|ids| !ids.contains(&id))
            .unwrap_or(false)
        {
            continue;
        }

        let base_url_override = resolve_copilot_base_url_override(&s, &provider).await?;
        let result = StreamCheckService::check_with_retry(
            &req.app_type,
            &provider,
            &config,
            base_url_override,
        )
        .await
        .unwrap_or_else(|err| StreamCheckResult {
            status: HealthStatus::Failed,
            success: false,
            message: err.to_string(),
            response_time_ms: None,
            http_status: None,
            model_used: String::new(),
            tested_at: now_unix_seconds(),
            retry_count: 0,
            error_category: None,
        });

        let _ = s
            .app
            .db
            .save_stream_check_log(&id, &provider.name, req.app_type.as_str(), &result);
        results.push((id, result));
    }

    to_value(results)
}

async fn stream_check_config_get(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_stream_check_config()?)
}

async fn stream_check_config_save(
    State(s): State<ServerState>,
    Json(config): Json<StreamCheckConfig>,
) -> ApiResult<Json<Value>> {
    s.app.db.save_stream_check_config(&config)?;
    Ok(Json(json!({ "ok": true })))
}

pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/proxy/start", post(proxy_start))
        .route("/api/proxy/stop", post(proxy_stop))
        .route("/api/proxy/stop-restore", post(proxy_stop_restore))
        .route("/api/proxy/status", get(proxy_status))
        .route("/api/proxy/running", get(proxy_running))
        .route(
            "/api/proxy/config",
            get(proxy_get_config).put(proxy_update_config),
        )
        .route(
            "/api/proxy/rectifier-config",
            get(rectifier_config_get).put(rectifier_config_set),
        )
        .route(
            "/api/proxy/optimizer-config",
            get(optimizer_config_get).put(optimizer_config_set),
        )
        .route(
            "/api/proxy/copilot-optimizer-config",
            get(copilot_optimizer_config_get).put(copilot_optimizer_config_set),
        )
        .route(
            "/api/proxy/log-config",
            get(log_config_get).put(log_config_set),
        )
        .route(
            "/api/proxy/global-config",
            get(proxy_get_global_config).put(proxy_update_global_config),
        )
        .route(
            "/api/proxy/app-config/{app}",
            get(proxy_get_app_config).put(proxy_update_app_config),
        )
        .route(
            "/api/proxy/app-config/{app}/default-cost-multiplier",
            get(default_cost_multiplier_get).put(default_cost_multiplier_set),
        )
        .route(
            "/api/proxy/app-config/{app}/pricing-model-source",
            get(pricing_model_source_get).put(pricing_model_source_set),
        )
        .route("/api/proxy/takeover", get(proxy_takeover_status))
        .route("/api/proxy/takeover/{app}", post(proxy_set_takeover))
        .route(
            "/api/proxy/live-takeover-active",
            get(proxy_live_takeover_active),
        )
        .route("/api/proxy/switch-provider", post(proxy_switch_provider))
        .route(
            "/api/proxy/provider-health/{app}/{provider_id}",
            get(provider_health),
        )
        .route(
            "/api/proxy/circuit-breaker/config",
            get(circuit_breaker_config_get).put(circuit_breaker_config_update),
        )
        .route(
            "/api/proxy/circuit-breaker/reset",
            post(reset_circuit_breaker),
        )
        .route(
            "/api/proxy/circuit-breaker/stats/{app}/{provider_id}",
            get(circuit_breaker_stats),
        )
        .route("/api/upstream-proxy/status", get(upstream_proxy_status))
        .route(
            "/api/upstream-proxy/url",
            get(upstream_proxy_get).put(upstream_proxy_set),
        )
        .route("/api/upstream-proxy/test", post(upstream_proxy_test))
        .route("/api/upstream-proxy/scan-local", get(upstream_proxy_scan))
        .route("/api/proxy/stream-check", post(stream_check_provider))
        .route("/api/proxy/stream-check/all", post(stream_check_all))
        .route(
            "/api/proxy/stream-check/config",
            get(stream_check_config_get).put(stream_check_config_save),
        )
}
