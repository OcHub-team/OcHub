//! Control-API routes for model discovery, managed-account auth, and sync
//! connection tests.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path as FsPath;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use ochub_core::apps::{claude_desktop, claude_plugin, codex, hermes, openclaw, opencode};
use ochub_core::settings::{self, S3SyncSettings, WebDavSyncSettings};
use ochub_core::{AppError, AppType};

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

static LIGHTWEIGHT_MODE: AtomicBool = AtomicBool::new(false);

// ----- Model discovery -----

#[derive(Deserialize)]
struct FetchModelsRequest {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(default, rename = "isFullUrl")]
    is_full_url: bool,
    #[serde(default, rename = "modelsUrl")]
    models_url: Option<String>,
    #[serde(default, rename = "customUserAgent")]
    custom_user_agent: Option<String>,
}

async fn fetch_models(Json(req): Json<FetchModelsRequest>) -> ApiResult<Json<Value>> {
    let user_agent = ochub_core::model::parse_custom_user_agent(req.custom_user_agent.as_deref())
        .ok()
        .flatten();
    let models = ochub_core::services::model_fetch::fetch_models(
        &req.base_url,
        &req.api_key,
        req.is_full_url,
        req.models_url.as_deref(),
        user_agent,
    )
    .await?;
    to_value(models)
}

#[derive(Deserialize)]
struct EndpointSpeedtestRequest {
    urls: Vec<String>,
    #[serde(default, rename = "timeoutSecs")]
    timeout_secs: Option<u64>,
}

async fn endpoint_speedtest(Json(req): Json<EndpointSpeedtestRequest>) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::services::SpeedtestService::test_endpoints(req.urls, req.timeout_secs).await?,
    )
}

#[derive(Deserialize)]
struct BalanceRequest {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
}

async fn balance(Json(req): Json<BalanceRequest>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::balance::get_balance(&req.base_url, &req.api_key).await?)
}

#[derive(Deserialize)]
struct ProviderUsageRequest {
    #[serde(rename = "providerId")]
    provider_id: String,
    app: String,
}

async fn provider_usage(
    State(s): State<ServerState>,
    Json(req): Json<ProviderUsageRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = req.app.parse::<AppType>()?;
    ochub_core::plugin::ensure_app_type_enabled(&app_type).map_err(ApiError::from)?;
    let result =
        ochub_core::services::ProviderService::query_usage(&s.app, app_type, &req.provider_id)
            .await;
    let snapshot = match &result {
        Ok(value) => value.clone(),
        Err(err) => ochub_core::UsageResult {
            success: false,
            data: None,
            error: Some(err.to_string()),
        },
    };
    s.app
        .usage_cache
        .put_script(app_type, req.provider_id.clone(), snapshot);
    to_value(result?)
}

#[derive(Deserialize)]
struct TestUsageScriptRequest {
    #[serde(rename = "providerId")]
    provider_id: String,
    app: String,
    #[serde(rename = "scriptCode")]
    script_code: String,
    #[serde(default = "default_usage_timeout")]
    timeout: u64,
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "userId")]
    user_id: Option<String>,
    #[serde(default, rename = "templateType")]
    template_type: Option<String>,
}

fn default_usage_timeout() -> u64 {
    10
}

async fn test_usage_script(
    State(s): State<ServerState>,
    Json(req): Json<TestUsageScriptRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = req.app.parse::<AppType>()?;
    ochub_core::plugin::ensure_app_type_enabled(&app_type).map_err(ApiError::from)?;
    to_value(
        ochub_core::services::ProviderService::test_usage_script(
            &s.app,
            app_type,
            &req.provider_id,
            &req.script_code,
            req.timeout,
            req.api_key.as_deref(),
            req.base_url.as_deref(),
            req.access_token.as_deref(),
            req.user_id.as_deref(),
            req.template_type.as_deref(),
        )
        .await?,
    )
}

// ----- Managed-account auth -----

async fn auth_accounts(
    State(s): State<ServerState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::auth_list_accounts(&s.app, &provider).await?)
}

async fn auth_status(
    State(s): State<ServerState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::auth_get_status(&s.app, &provider).await?)
}

#[derive(Deserialize)]
struct LoginRequest {
    #[serde(default, rename = "githubDomain")]
    github_domain: Option<String>,
}

async fn auth_login(
    State(s): State<ServerState>,
    Path(provider): Path<String>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<Value>> {
    let code = ochub_core::services::auth::auth_start_login(
        &s.app,
        &provider,
        req.github_domain.as_deref(),
    )
    .await?;
    to_value(code)
}

#[derive(Deserialize)]
struct AuthPollRequest {
    #[serde(rename = "deviceCode")]
    device_code: String,
    #[serde(default, rename = "githubDomain")]
    github_domain: Option<String>,
}

async fn auth_poll(
    State(s): State<ServerState>,
    Path(provider): Path<String>,
    Json(req): Json<AuthPollRequest>,
) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::services::auth::auth_poll_for_account(
            &s.app,
            &provider,
            &req.device_code,
            req.github_domain.as_deref(),
        )
        .await?,
    )
}

async fn auth_remove_account(
    State(s): State<ServerState>,
    Path((provider, account_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::auth_remove_account(&s.app, &provider, &account_id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn auth_set_default_account(
    State(s): State<ServerState>,
    Path((provider, account_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::auth_set_default_account(&s.app, &provider, &account_id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn auth_logout(
    State(s): State<ServerState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::auth_logout(&s.app, &provider).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn copilot_device_flow(
    State(s): State<ServerState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::services::auth::copilot_start_device_flow(&s.app, req.github_domain.as_deref())
            .await?,
    )
}

async fn copilot_poll_auth(
    State(s): State<ServerState>,
    Json(req): Json<AuthPollRequest>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "authenticated": ochub_core::services::auth::copilot_poll_for_auth(
            &s.app,
            &req.device_code,
            req.github_domain.as_deref(),
        )
        .await?
    })))
}

async fn copilot_poll_account(
    State(s): State<ServerState>,
    Json(req): Json<AuthPollRequest>,
) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::services::auth::copilot_poll_for_account(
            &s.app,
            &req.device_code,
            req.github_domain.as_deref(),
        )
        .await?,
    )
}

async fn copilot_accounts(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_list_accounts(&s.app).await?)
}

async fn copilot_remove_account(
    State(s): State<ServerState>,
    Path(account_id): Path<String>,
) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::copilot_remove_account(&s.app, &account_id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn copilot_default_account(
    State(s): State<ServerState>,
    Path(account_id): Path<String>,
) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::copilot_set_default_account(&s.app, &account_id).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn copilot_status(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_get_auth_status(&s.app).await?)
}

async fn copilot_authenticated(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "authenticated": ochub_core::services::auth::copilot_is_authenticated(&s.app).await?
    })))
}

async fn copilot_logout(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    ochub_core::services::auth::copilot_logout(&s.app).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn copilot_token(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "token": ochub_core::services::auth::copilot_get_token(&s.app).await?
    })))
}

async fn copilot_token_for_account(
    State(s): State<ServerState>,
    Path(account_id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "token": ochub_core::services::auth::copilot_get_token_for_account(&s.app, &account_id).await?
    })))
}

async fn copilot_models(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_get_models(&s.app).await?)
}

async fn copilot_models_for_account(
    State(s): State<ServerState>,
    Path(account_id): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_get_models_for_account(&s.app, &account_id).await?)
}

async fn copilot_usage(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_get_usage(&s.app).await?)
}

async fn copilot_usage_for_account(
    State(s): State<ServerState>,
    Path(account_id): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::copilot_get_usage_for_account(&s.app, &account_id).await?)
}

#[derive(Deserialize)]
struct AccountQuery {
    #[serde(default, alias = "accountId")]
    account_id: Option<String>,
}

async fn codex_oauth_quota(
    State(s): State<ServerState>,
    Query(q): Query<AccountQuery>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::get_codex_oauth_quota(&s.app, q.account_id).await?)
}

async fn codex_oauth_models(
    State(s): State<ServerState>,
    Query(q): Query<AccountQuery>,
) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::auth::get_codex_oauth_models(&s.app, q.account_id).await?)
}

async fn subscription_quota(
    State(s): State<ServerState>,
    Path(tool): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = ochub_core::services::subscription::get_subscription_quota(&tool).await;
    let snapshot = match &result {
        Ok(value) => value.clone(),
        Err(err) => ochub_core::services::subscription::SubscriptionQuota {
            tool: tool.clone(),
            credential_status: ochub_core::services::subscription::CredentialStatus::Valid,
            credential_message: Some(err.clone()),
            success: false,
            tiers: vec![],
            extra_usage: None,
            error: Some(err.clone()),
            queried_at: Some(now_millis()),
        },
    };
    if let Ok(app_type) = tool.parse::<AppType>() {
        s.app.usage_cache.put_subscription(app_type, snapshot);
    }
    to_value(result?)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct CodingPlanQuotaRequest {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
}

async fn coding_plan_quota(Json(req): Json<CodingPlanQuotaRequest>) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::services::coding_plan::get_coding_plan_quota(&req.base_url, &req.api_key)
            .await?,
    )
}

// ----- OpenCode / OpenClaw / Hermes live app details -----

async fn opencode_live_provider_ids() -> ApiResult<Json<Value>> {
    to_value(
        opencode::get_providers()?
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn openclaw_live_provider_ids() -> ApiResult<Json<Value>> {
    to_value(
        openclaw::get_providers()?
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn openclaw_live_provider(Path(provider_id): Path<String>) -> ApiResult<Json<Value>> {
    to_value(openclaw::get_provider(&provider_id)?)
}

async fn openclaw_health() -> ApiResult<Json<Value>> {
    to_value(openclaw::scan_openclaw_config_health()?)
}

async fn openclaw_default_model() -> ApiResult<Json<Value>> {
    to_value(openclaw::get_default_model()?)
}

async fn openclaw_set_default_model(
    Json(model): Json<openclaw::OpenClawDefaultModel>,
) -> ApiResult<Json<Value>> {
    to_value(openclaw::set_default_model(&model)?)
}

async fn openclaw_model_catalog() -> ApiResult<Json<Value>> {
    to_value(openclaw::get_model_catalog()?)
}

async fn openclaw_set_model_catalog(
    Json(catalog): Json<HashMap<String, openclaw::OpenClawModelCatalogEntry>>,
) -> ApiResult<Json<Value>> {
    to_value(openclaw::set_model_catalog(&catalog)?)
}

async fn openclaw_agents_defaults() -> ApiResult<Json<Value>> {
    to_value(openclaw::get_agents_defaults()?)
}

async fn openclaw_set_agents_defaults(
    Json(defaults): Json<openclaw::OpenClawAgentsDefaults>,
) -> ApiResult<Json<Value>> {
    to_value(openclaw::set_agents_defaults(&defaults)?)
}

async fn openclaw_env() -> ApiResult<Json<Value>> {
    to_value(openclaw::get_env_config()?)
}

async fn openclaw_set_env(Json(env): Json<openclaw::OpenClawEnvConfig>) -> ApiResult<Json<Value>> {
    to_value(openclaw::set_env_config(&env)?)
}

async fn openclaw_tools() -> ApiResult<Json<Value>> {
    to_value(openclaw::get_tools_config()?)
}

async fn openclaw_set_tools(
    Json(tools): Json<openclaw::OpenClawToolsConfig>,
) -> ApiResult<Json<Value>> {
    to_value(openclaw::set_tools_config(&tools)?)
}

async fn hermes_live_provider_ids() -> ApiResult<Json<Value>> {
    to_value(hermes::get_providers()?.keys().cloned().collect::<Vec<_>>())
}

async fn hermes_live_provider(Path(provider_id): Path<String>) -> ApiResult<Json<Value>> {
    to_value(hermes::get_provider(&provider_id)?)
}

async fn hermes_model_config() -> ApiResult<Json<Value>> {
    to_value(hermes::get_model_config()?)
}

#[derive(Deserialize)]
struct HermesMemoryQuery {
    kind: hermes::MemoryKind,
}

async fn hermes_memory(Query(q): Query<HermesMemoryQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "content": hermes::read_memory(q.kind)? })))
}

#[derive(Deserialize)]
struct HermesMemoryWriteRequest {
    kind: hermes::MemoryKind,
    content: String,
}

async fn hermes_set_memory(Json(req): Json<HermesMemoryWriteRequest>) -> ApiResult<Json<Value>> {
    hermes::write_memory(req.kind, &req.content)?;
    Ok(Json(json!({ "ok": true })))
}

async fn hermes_memory_limits() -> ApiResult<Json<Value>> {
    to_value(hermes::read_memory_limits()?)
}

#[derive(Deserialize)]
struct HermesMemoryEnabledRequest {
    kind: hermes::MemoryKind,
    enabled: bool,
}

async fn hermes_set_memory_enabled(
    Json(req): Json<HermesMemoryEnabledRequest>,
) -> ApiResult<Json<Value>> {
    to_value(hermes::set_memory_enabled(req.kind, req.enabled)?)
}

#[derive(Deserialize)]
struct HermesOpenRequest {
    #[serde(default)]
    path: Option<String>,
}

async fn hermes_open_web_ui(Json(req): Json<HermesOpenRequest>) -> ApiResult<Json<Value>> {
    let port = std::env::var("HERMES_WEB_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .unwrap_or(9119);
    let base = format!("http://127.0.0.1:{port}");
    let probe_url = format!("{base}/api/status");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1200))
        .no_proxy()
        .build()
        .map_err(|e| {
            ApiError(AppError::Message(format!(
                "failed to build probe client: {e}"
            )))
        })?;
    client
        .get(&probe_url)
        .send()
        .await
        .map_err(|_| ApiError(AppError::Message("hermes_web_offline".to_string())))?;

    let target = match req.path.as_deref() {
        Some(p) if p.starts_with('/') => format!("{base}{p}"),
        Some(p) if !p.is_empty() => format!("{base}/{p}"),
        _ => format!("{base}/"),
    };
    open_url(&target)?;
    Ok(Json(json!({ "ok": true, "url": target })))
}

async fn hermes_launch_dashboard() -> ApiResult<Json<Value>> {
    let preferred = ochub_core::settings::get_settings().preferred_terminal;
    let target = match preferred.as_deref() {
        Some("iterm2") => "iterm",
        Some(t) => t,
        None => "terminal",
    };
    ochub_core::session_manager::terminal::launch_terminal(target, "hermes dashboard", None, None)?;
    Ok(Json(json!({ "ok": true })))
}

fn open_url(url: &str) -> Result<(), ApiError> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    cmd.status()
        .map_err(|e| ApiError(AppError::Message(format!("failed to open URL: {e}"))))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(ApiError(AppError::Message(format!(
                    "failed to open URL, status: {status}"
                ))))
            }
        })
}

fn open_path(path: &FsPath) -> Result<(), ApiError> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("explorer");
        cmd.arg(path);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    cmd.status()
        .map_err(|e| ApiError(AppError::Message(format!("failed to open path: {e}"))))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(ApiError(AppError::Message(format!(
                    "failed to open path, status: {status}"
                ))))
            }
        })
}

fn app_config_dir(app: AppType) -> Result<std::path::PathBuf, AppError> {
    ochub_core::plugin::get_plugin(&app.app_id())
        .ok_or_else(|| AppError::InvalidInput(format!("未知的应用类型: {app}")))?
        .config_dir()
}

fn app_config_status_sync(
    app: AppType,
    state: Option<&ServerState>,
) -> Result<ochub_core::paths::ConfigStatus, AppError> {
    let status = match app {
        AppType::Claude => ochub_core::paths::get_claude_config_status(),
        AppType::ClaudeDesktop => {
            let Some(state) = state else {
                return Err(AppError::Message(
                    "Claude Desktop status requires server state".to_string(),
                ));
            };
            let status = claude_desktop::get_status(&state.app.db)?;
            ochub_core::paths::ConfigStatus {
                exists: status.configured,
                path: status.config_library_path.unwrap_or_default(),
            }
        }
        AppType::Codex => {
            let auth_path = codex::get_codex_auth_path();
            let config_text = codex::read_codex_config_text().unwrap_or_default();
            ochub_core::paths::ConfigStatus {
                exists: auth_path.exists() || !config_text.trim().is_empty(),
                path: codex::get_codex_config_dir().to_string_lossy().to_string(),
            }
        }
        AppType::OpenCode => {
            let config_path = opencode::get_opencode_config_path();
            ochub_core::paths::ConfigStatus {
                exists: config_path.exists(),
                path: opencode::get_opencode_dir().to_string_lossy().to_string(),
            }
        }
        AppType::OpenClaw => {
            let config_path = openclaw::get_openclaw_config_path();
            ochub_core::paths::ConfigStatus {
                exists: config_path.exists(),
                path: openclaw::get_openclaw_dir().to_string_lossy().to_string(),
            }
        }
        AppType::Hermes => {
            let config_path = hermes::get_hermes_config_path();
            ochub_core::paths::ConfigStatus {
                exists: config_path.exists(),
                path: hermes::get_hermes_dir().to_string_lossy().to_string(),
            }
        }
    };
    Ok(status)
}

async fn config_status(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app = app.parse::<AppType>()?;
    to_value(app_config_status_sync(app, Some(&s))?)
}

async fn claude_code_config_path() -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "path": ochub_core::paths::get_claude_settings_path().to_string_lossy().to_string()
    })))
}

async fn config_dir(Path(app): Path<String>) -> ApiResult<Json<Value>> {
    let app = app.parse::<AppType>()?;
    Ok(Json(json!({
        "path": app_config_dir(app)?.to_string_lossy().to_string()
    })))
}

async fn open_config_folder(Path(app): Path<String>) -> ApiResult<Json<Value>> {
    let app = app.parse::<AppType>()?;
    ochub_core::plugin::ensure_app_type_enabled(&app).map_err(ApiError::from)?;
    let dir = app_config_dir(app)?;
    std::fs::create_dir_all(&dir).map_err(|e| ApiError(AppError::io(&dir, e)))?;
    open_path(&dir)?;
    Ok(Json(json!({ "ok": true, "path": dir.to_string_lossy() })))
}

async fn app_config_path() -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "path": ochub_core::paths::get_app_config_path().to_string_lossy().to_string()
    })))
}

async fn open_app_config_folder() -> ApiResult<Json<Value>> {
    let dir = ochub_core::paths::get_app_config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ApiError(AppError::io(&dir, e)))?;
    open_path(&dir)?;
    Ok(Json(json!({ "ok": true, "path": dir.to_string_lossy() })))
}

async fn app_config_dir_override_get() -> ApiResult<Json<Value>> {
    let value = ochub_core::app_store::refresh_app_config_dir_override()
        .map(|p| p.to_string_lossy().to_string());
    Ok(Json(json!({ "path": value })))
}

#[derive(Deserialize)]
struct AppConfigDirOverrideRequest {
    #[serde(default)]
    path: Option<String>,
}

async fn app_config_dir_override_set(
    Json(req): Json<AppConfigDirOverrideRequest>,
) -> ApiResult<Json<Value>> {
    ochub_core::app_store::set_app_config_dir_to_store(req.path.as_deref())?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct OpenExternalRequest {
    url: String,
}

async fn open_external(Json(req): Json<OpenExternalRequest>) -> ApiResult<Json<Value>> {
    let url = if req.url.starts_with("http://") || req.url.starts_with("https://") {
        req.url
    } else {
        format!("https://{}", req.url)
    };
    open_url(&url)?;
    Ok(Json(json!({ "ok": true, "url": url })))
}

#[derive(Deserialize)]
struct ClipboardRequest {
    text: String,
}

async fn copy_text_to_clipboard(Json(req): Json<ClipboardRequest>) -> ApiResult<Json<Value>> {
    tokio::task::spawn_blocking(move || {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|e| AppError::Message(format!("访问系统剪贴板失败: {e}")))?;
        clipboard
            .set_text(req.text)
            .map_err(|e| AppError::Message(format!("写入系统剪贴板失败: {e}")))?;
        Ok::<_, AppError>(true)
    })
    .await
    .map_err(|e| ApiError(AppError::Message(format!("剪贴板任务执行失败: {e}"))))??;
    Ok(Json(json!({ "ok": true })))
}

async fn portable_mode() -> ApiResult<Json<Value>> {
    let exe_path = std::env::current_exe()
        .map_err(|e| ApiError(AppError::Message(format!("获取可执行路径失败: {e}"))))?;
    let enabled = exe_path
        .parent()
        .map(|dir| dir.join("portable.ini").is_file())
        .unwrap_or(false);
    Ok(Json(json!({ "portable": enabled })))
}

async fn check_for_updates() -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::update::check_for_updates(None).await?)
}

async fn restart_app() -> ApiResult<Json<Value>> {
    let exe = std::env::current_exe()
        .map_err(|e| ApiError(AppError::Message(format!("获取当前可执行文件失败: {e}"))))?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();

    std::thread::Builder::new()
        .name("OCHub-restart".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            if let Err(err) = Command::new(&exe).args(args).spawn() {
                log::error!("重启 OCHub 失败: {err}");
                return;
            }
            std::process::exit(0);
        })
        .map_err(|e| ApiError(AppError::Message(format!("创建重启任务失败: {e}"))))?;

    Ok(Json(json!({ "ok": true })))
}

async fn install_update_and_restart() -> ApiResult<Json<Value>> {
    let url = ochub_core::services::latest_release_url(None);
    open_url(&url)?;
    Ok(Json(json!({
        "ok": false,
        "url": url,
        "reason": "当前 GPUI/Axum 版本没有内置安装器，已打开发布页，请手动安装更新后重启"
    })))
}

#[cfg(target_os = "macos")]
fn macos_app_bundle_path(exe_path: &FsPath) -> Option<std::path::PathBuf> {
    let path_str = exe_path.to_string_lossy();
    path_str.find(".app/Contents/MacOS/").map(|app_pos| {
        let app_bundle_end = app_pos + 4;
        std::path::PathBuf::from(&path_str[..app_bundle_end])
    })
}

fn auto_launch_handle() -> Result<AutoLaunch, AppError> {
    let exe_path =
        std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))?;

    #[cfg(target_os = "macos")]
    let app_path = macos_app_bundle_path(&exe_path).unwrap_or(exe_path);

    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    AutoLaunchBuilder::new()
        .set_app_name("OCHub")
        .set_app_path(&app_path.to_string_lossy())
        .build()
        .map_err(|e| AppError::Message(format!("创建 AutoLaunch 失败: {e}")))
}

#[derive(Deserialize)]
struct AutoLaunchRequest {
    enabled: bool,
}

async fn auto_launch_status() -> ApiResult<Json<Value>> {
    let enabled = auto_launch_handle()?
        .is_enabled()
        .map_err(|e| ApiError(AppError::Message(format!("获取开机自启状态失败: {e}"))))?;
    Ok(Json(json!({ "enabled": enabled })))
}

async fn auto_launch_set(Json(req): Json<AutoLaunchRequest>) -> ApiResult<Json<Value>> {
    let handle = auto_launch_handle()?;
    if req.enabled {
        handle
            .enable()
            .map_err(|e| ApiError(AppError::Message(format!("启用开机自启失败: {e}"))))?;
    } else {
        handle
            .disable()
            .map_err(|e| ApiError(AppError::Message(format!("禁用开机自启失败: {e}"))))?;
    }
    Ok(Json(json!({ "ok": true, "enabled": req.enabled })))
}

async fn lightweight_status() -> Json<Value> {
    Json(json!({
        "enabled": LIGHTWEIGHT_MODE.load(Ordering::Acquire)
    }))
}

async fn lightweight_enter() -> Json<Value> {
    LIGHTWEIGHT_MODE.store(true, Ordering::Release);
    Json(json!({ "ok": true, "enabled": true }))
}

async fn lightweight_exit() -> Json<Value> {
    LIGHTWEIGHT_MODE.store(false, Ordering::Release);
    Json(json!({ "ok": true, "enabled": false }))
}

async fn codex_unify_history_backup() -> Json<Value> {
    Json(json!({
        "exists": ochub_core::services::codex_history_migration::has_codex_official_history_unify_backup()
    }))
}

async fn codex_restore_unified_history() -> ApiResult<Json<Value>> {
    let outcome = tokio::task::spawn_blocking(|| {
        ochub_core::services::codex_history_migration::restore_codex_official_history_from_backups()
    })
    .await
    .map_err(|e| {
        ApiError(AppError::Message(format!(
            "Codex 历史还原任务执行失败: {e}"
        )))
    })??;

    if let Some(reason) = &outcome.skipped_reason {
        log::debug!("Codex official history restore skipped: {reason}");
    } else {
        log::info!(
            "Codex official history restored from backups: jsonl_files={}, state_rows={}",
            outcome.restored_jsonl_files,
            outcome.restored_state_rows
        );
    }

    Ok(Json(json!({
        "restoredJsonlFiles": outcome.restored_jsonl_files,
        "restoredStateRows": outcome.restored_state_rows,
        "skippedReason": outcome.skipped_reason,
    })))
}

// ----- OMO / Claude MCP -----

async fn omo_local_file() -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::OmoService::read_local_file(
        &ochub_core::services::omo::STANDARD,
    )?)
}

async fn omo_slim_local_file() -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::OmoService::read_local_file(
        &ochub_core::services::omo::SLIM,
    )?)
}

async fn omo_current_provider(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let provider = s
        .app
        .db
        .get_current_omo_provider("opencode", "omo")?
        .map(|p| p.id)
        .unwrap_or_default();
    Ok(Json(json!({ "providerId": provider })))
}

async fn omo_slim_current_provider(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let provider = s
        .app
        .db
        .get_current_omo_provider("opencode", "omo-slim")?
        .map(|p| p.id)
        .unwrap_or_default();
    Ok(Json(json!({ "providerId": provider })))
}

fn disable_omo_category(state: &ServerState, category: &str) -> Result<(), AppError> {
    let providers = state.app.db.get_all_providers("opencode")?;
    for (id, provider) in &providers {
        if provider.category.as_deref() == Some(category) {
            state
                .app
                .db
                .clear_omo_provider_current("opencode", id, category)?;
        }
    }
    Ok(())
}

async fn omo_disable(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    disable_omo_category(&s, "omo")?;
    ochub_core::services::OmoService::delete_config_file(&ochub_core::services::omo::STANDARD)?;
    Ok(Json(json!({ "ok": true })))
}

async fn omo_slim_disable(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    disable_omo_category(&s, "omo-slim")?;
    ochub_core::services::OmoService::delete_config_file(&ochub_core::services::omo::SLIM)?;
    Ok(Json(json!({ "ok": true })))
}

async fn claude_mcp_status() -> ApiResult<Json<Value>> {
    to_value(ochub_core::mcp::get_mcp_status()?)
}

async fn claude_mcp_config() -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "content": ochub_core::mcp::read_mcp_json()? }),
    ))
}

#[derive(Deserialize)]
struct ClaudeMcpServerRequest {
    spec: Value,
}

async fn claude_mcp_upsert(
    Path(id): Path<String>,
    Json(req): Json<ClaudeMcpServerRequest>,
) -> ApiResult<Json<Value>> {
    let changed = ochub_core::mcp::upsert_mcp_server(&id, req.spec)?;
    Ok(Json(json!({ "changed": changed })))
}

async fn claude_mcp_delete(Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let changed = ochub_core::mcp::delete_mcp_server(&id)?;
    Ok(Json(json!({ "changed": changed })))
}

#[derive(Deserialize)]
struct ClaudeMcpValidateRequest {
    command: String,
}

async fn claude_mcp_validate(Json(req): Json<ClaudeMcpValidateRequest>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "valid": ochub_core::mcp::validate_command_in_path(&req.command)?
    })))
}

// ----- Sync connection tests (settings come from device settings) -----

async fn sync_webdav_test() -> ApiResult<Json<Value>> {
    let settings = ochub_core::settings::get_settings()
        .webdav_sync
        .ok_or_else(|| ApiError(AppError::Config("WebDAV sync is not configured".into())))?;
    ochub_core::services::webdav_sync::check_connection(&settings).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn sync_s3_test() -> ApiResult<Json<Value>> {
    let settings = ochub_core::settings::get_settings()
        .s3_sync
        .ok_or_else(|| ApiError(AppError::Config("S3 sync is not configured".into())))?;
    ochub_core::services::s3_sync::check_connection(&settings).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn sync_webdav_status() -> Json<Value> {
    match settings::get_webdav_sync_settings() {
        Some(settings) => Json(json!({
            "configured": true,
            "enabled": settings.enabled,
            "autoSync": settings.auto_sync,
            "status": settings.status,
        })),
        None => Json(json!({
            "configured": false,
            "enabled": false,
            "autoSync": false,
            "status": null,
        })),
    }
}

async fn sync_s3_status() -> Json<Value> {
    match settings::get_s3_sync_settings() {
        Some(settings) => Json(json!({
            "configured": true,
            "enabled": settings.enabled,
            "autoSync": settings.auto_sync,
            "status": settings.status,
        })),
        None => Json(json!({
            "configured": false,
            "enabled": false,
            "autoSync": false,
            "status": null,
        })),
    }
}

#[derive(Deserialize)]
struct WebDavSettingsRequest {
    settings: WebDavSyncSettings,
    #[serde(default, rename = "preserveEmptyPassword")]
    preserve_empty_password: Option<bool>,
}

fn resolve_webdav_password(
    mut incoming: WebDavSyncSettings,
    existing: Option<WebDavSyncSettings>,
    preserve_empty_password: bool,
) -> WebDavSyncSettings {
    if let Some(existing) = existing {
        if preserve_empty_password && incoming.password.is_empty() {
            incoming.password = existing.password;
        }
    }
    incoming
}

async fn sync_webdav_test_with_settings(
    Json(req): Json<WebDavSettingsRequest>,
) -> ApiResult<Json<Value>> {
    let resolved = resolve_webdav_password(
        req.settings,
        settings::get_webdav_sync_settings(),
        req.preserve_empty_password.unwrap_or(true),
    );
    ochub_core::services::webdav_sync::check_connection(&resolved).await?;
    Ok(Json(
        json!({ "success": true, "message": "WebDAV connection ok" }),
    ))
}

async fn sync_webdav_save_settings(
    Json(req): Json<WebDavSettingsRequest>,
) -> ApiResult<Json<Value>> {
    let existing = settings::get_webdav_sync_settings();
    let mut sync_settings = resolve_webdav_password(
        req.settings,
        existing.clone(),
        req.preserve_empty_password.unwrap_or(true),
    );
    if let Some(existing) = existing {
        sync_settings.status = existing.status;
    }
    sync_settings.normalize();
    sync_settings.validate()?;
    settings::set_webdav_sync_settings(Some(sync_settings))?;
    Ok(Json(json!({ "success": true })))
}

fn require_enabled_webdav_settings() -> Result<WebDavSyncSettings, ApiError> {
    let settings = settings::get_webdav_sync_settings().ok_or_else(|| {
        ApiError(AppError::localized(
            "webdav.sync.not_configured",
            "未配置 WebDAV 同步",
            "WebDAV sync is not configured.",
        ))
    })?;
    if !settings.enabled {
        return Err(ApiError(AppError::localized(
            "webdav.sync.disabled",
            "WebDAV 同步未启用",
            "WebDAV sync is disabled.",
        )));
    }
    Ok(settings)
}

async fn sync_webdav_upload(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let mut settings = require_enabled_webdav_settings()?;
    match ochub_core::services::webdav_sync::run_with_sync_lock(
        ochub_core::services::webdav_sync::upload(&s.app.db, &mut settings),
    )
    .await
    {
        Ok(value) => Ok(Json(value)),
        Err(err) => {
            settings.status.last_error = Some(err.to_string());
            settings.status.last_error_source = Some("manual".to_string());
            let _ = settings::update_webdav_sync_status(settings.status.clone());
            Err(ApiError(err))
        }
    }
}

async fn sync_webdav_download(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let mut settings = require_enabled_webdav_settings()?;
    match ochub_core::services::webdav_sync::run_with_sync_lock(
        ochub_core::services::webdav_sync::download(&s.app.db, &mut settings),
    )
    .await
    {
        Ok(mut value) => {
            if let Err(err) = ochub_core::services::ProviderService::sync_current_to_live(&s.app) {
                log::warn!("[WebDAV] post-download live sync warning: {err}");
                attach_warning(&mut value, err.to_string());
            }
            Ok(Json(value))
        }
        Err(err) => {
            settings.status.last_error = Some(err.to_string());
            settings.status.last_error_source = Some("manual".to_string());
            let _ = settings::update_webdav_sync_status(settings.status.clone());
            Err(ApiError(err))
        }
    }
}

async fn sync_webdav_remote_info() -> ApiResult<Json<Value>> {
    let settings = require_enabled_webdav_settings()?;
    let info = ochub_core::services::webdav_sync::fetch_remote_info(&settings).await?;
    Ok(Json(info.unwrap_or(json!({ "empty": true }))))
}

#[derive(Deserialize)]
struct S3SettingsRequest {
    settings: S3SyncSettings,
    #[serde(default, rename = "preserveEmptyPassword")]
    preserve_empty_password: Option<bool>,
}

fn resolve_s3_secret(
    mut incoming: S3SyncSettings,
    existing: Option<S3SyncSettings>,
    preserve_empty_secret: bool,
) -> S3SyncSettings {
    if let Some(existing) = existing {
        if preserve_empty_secret && incoming.secret_access_key.is_empty() {
            incoming.secret_access_key = existing.secret_access_key;
        }
    }
    incoming
}

async fn sync_s3_test_with_settings(Json(req): Json<S3SettingsRequest>) -> ApiResult<Json<Value>> {
    let resolved = resolve_s3_secret(
        req.settings,
        settings::get_s3_sync_settings(),
        req.preserve_empty_password.unwrap_or(true),
    );
    ochub_core::services::s3_sync::check_connection(&resolved).await?;
    Ok(Json(
        json!({ "success": true, "message": "S3 connection ok" }),
    ))
}

async fn sync_s3_save_settings(Json(req): Json<S3SettingsRequest>) -> ApiResult<Json<Value>> {
    let existing = settings::get_s3_sync_settings();
    let mut sync_settings = resolve_s3_secret(
        req.settings,
        existing.clone(),
        req.preserve_empty_password.unwrap_or(true),
    );
    if let Some(existing) = existing {
        sync_settings.status = existing.status;
    }
    sync_settings.normalize();
    sync_settings.validate()?;
    settings::set_s3_sync_settings(Some(sync_settings))?;
    Ok(Json(json!({ "success": true })))
}

fn require_enabled_s3_settings() -> Result<S3SyncSettings, ApiError> {
    let settings = settings::get_s3_sync_settings().ok_or_else(|| {
        ApiError(AppError::localized(
            "s3.sync.not_configured",
            "未配置 S3 同步",
            "S3 sync is not configured.",
        ))
    })?;
    if !settings.enabled {
        return Err(ApiError(AppError::localized(
            "s3.sync.disabled",
            "S3 同步未启用",
            "S3 sync is disabled.",
        )));
    }
    Ok(settings)
}

async fn sync_s3_upload(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let mut settings = require_enabled_s3_settings()?;
    match ochub_core::services::s3_sync::run_with_sync_lock(ochub_core::services::s3_sync::upload(
        &s.app.db,
        &mut settings,
    ))
    .await
    {
        Ok(value) => Ok(Json(value)),
        Err(err) => {
            settings.status.last_error = Some(err.to_string());
            settings.status.last_error_source = Some("manual".to_string());
            let _ = settings::update_s3_sync_status(settings.status.clone());
            Err(ApiError(err))
        }
    }
}

async fn sync_s3_download(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let mut settings = require_enabled_s3_settings()?;
    match ochub_core::services::s3_sync::run_with_sync_lock(
        ochub_core::services::s3_sync::download(&s.app.db, &mut settings),
    )
    .await
    {
        Ok(mut value) => {
            if let Err(err) = ochub_core::services::ProviderService::sync_current_to_live(&s.app) {
                log::warn!("[S3] post-download live sync warning: {err}");
                attach_warning(&mut value, err.to_string());
            }
            Ok(Json(value))
        }
        Err(err) => {
            settings.status.last_error = Some(err.to_string());
            settings.status.last_error_source = Some("manual".to_string());
            let _ = settings::update_s3_sync_status(settings.status.clone());
            Err(ApiError(err))
        }
    }
}

async fn sync_s3_remote_info() -> ApiResult<Json<Value>> {
    let settings = require_enabled_s3_settings()?;
    let info = ochub_core::services::s3_sync::fetch_remote_info(&settings).await?;
    Ok(Json(info.unwrap_or(json!({ "empty": true }))))
}

fn attach_warning(value: &mut Value, warning: String) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert("warning".to_string(), json!(warning));
    }
}

// ----- Claude plugin / onboarding -----

async fn claude_plugin_status() -> ApiResult<Json<Value>> {
    let (exists, path) = claude_plugin::claude_config_status()?;
    Ok(Json(json!({ "exists": exists, "path": path })))
}

async fn claude_plugin_read() -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "content": claude_plugin::read_claude_config()? }),
    ))
}

#[derive(Deserialize)]
struct ClaudePluginApplyRequest {
    #[serde(default)]
    official: bool,
}

async fn claude_plugin_apply(Json(req): Json<ClaudePluginApplyRequest>) -> ApiResult<Json<Value>> {
    let changed = if req.official {
        claude_plugin::clear_claude_config()?
    } else {
        claude_plugin::write_claude_config()?
    };
    Ok(Json(json!({ "changed": changed })))
}

async fn claude_plugin_applied() -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "applied": claude_plugin::is_claude_config_applied()?
    })))
}

async fn claude_onboarding_skip() -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "changed": ochub_core::mcp::set_has_completed_onboarding()?
    })))
}

async fn claude_onboarding_clear() -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "changed": ochub_core::mcp::clear_has_completed_onboarding()?
    })))
}

pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/models/fetch", post(fetch_models))
        .route("/api/models/speedtest", post(endpoint_speedtest))
        .route("/api/config/status/{app}", get(config_status))
        .route("/api/config/claude-code-path", get(claude_code_config_path))
        .route("/api/config/dir/{app}", get(config_dir))
        .route("/api/config/open-folder/{app}", post(open_config_folder))
        .route("/api/config/app-path", get(app_config_path))
        .route("/api/config/app-folder/open", post(open_app_config_folder))
        .route(
            "/api/config/app-dir-override",
            get(app_config_dir_override_get).put(app_config_dir_override_set),
        )
        .route("/api/open-external", post(open_external))
        .route("/api/clipboard", post(copy_text_to_clipboard))
        .route("/api/portable", get(portable_mode))
        .route("/api/updates/check", post(check_for_updates))
        .route(
            "/api/updates/install-restart",
            post(install_update_and_restart),
        )
        .route("/api/restart", post(restart_app))
        .route(
            "/api/auto-launch",
            get(auto_launch_status).put(auto_launch_set),
        )
        .route("/api/lightweight", get(lightweight_status))
        .route("/api/lightweight/enter", post(lightweight_enter))
        .route("/api/lightweight/exit", post(lightweight_exit))
        .route(
            "/api/codex/history/unify-backup",
            get(codex_unify_history_backup),
        )
        .route(
            "/api/codex/history/restore-unified",
            post(codex_restore_unified_history),
        )
        .route("/api/omo/local-file", get(omo_local_file))
        .route("/api/omo/current-provider", get(omo_current_provider))
        .route("/api/omo/disable", post(omo_disable))
        .route("/api/omo-slim/local-file", get(omo_slim_local_file))
        .route(
            "/api/omo-slim/current-provider",
            get(omo_slim_current_provider),
        )
        .route("/api/omo-slim/disable", post(omo_slim_disable))
        .route("/api/claude-mcp/status", get(claude_mcp_status))
        .route("/api/claude-mcp/config", get(claude_mcp_config))
        .route(
            "/api/claude-mcp/server/{id}",
            post(claude_mcp_upsert).delete(claude_mcp_delete),
        )
        .route("/api/claude-mcp/validate", post(claude_mcp_validate))
        .route("/api/balance", post(balance))
        .route("/api/provider-usage", post(provider_usage))
        .route("/api/provider-usage/test-script", post(test_usage_script))
        .route("/api/subscription/{tool}/quota", get(subscription_quota))
        .route("/api/coding-plan/quota", post(coding_plan_quota))
        .route("/api/auth/{provider}/accounts", get(auth_accounts))
        .route("/api/auth/{provider}/status", get(auth_status))
        .route("/api/auth/{provider}/login", post(auth_login))
        .route("/api/auth/{provider}/poll", post(auth_poll))
        .route("/api/auth/{provider}/logout", post(auth_logout))
        .route(
            "/api/auth/{provider}/accounts/{account_id}",
            axum::routing::delete(auth_remove_account),
        )
        .route(
            "/api/auth/{provider}/accounts/{account_id}/default",
            post(auth_set_default_account),
        )
        .route("/api/copilot/device-flow", post(copilot_device_flow))
        .route("/api/copilot/poll-auth", post(copilot_poll_auth))
        .route("/api/copilot/poll-account", post(copilot_poll_account))
        .route("/api/copilot/accounts", get(copilot_accounts))
        .route("/api/copilot/status", get(copilot_status))
        .route("/api/copilot/authenticated", get(copilot_authenticated))
        .route("/api/copilot/logout", post(copilot_logout))
        .route("/api/copilot/token", get(copilot_token))
        .route("/api/copilot/models", get(copilot_models))
        .route("/api/copilot/usage", get(copilot_usage))
        .route(
            "/api/copilot/accounts/{account_id}",
            axum::routing::delete(copilot_remove_account),
        )
        .route(
            "/api/copilot/accounts/{account_id}/default",
            post(copilot_default_account),
        )
        .route(
            "/api/copilot/accounts/{account_id}/token",
            get(copilot_token_for_account),
        )
        .route(
            "/api/copilot/accounts/{account_id}/models",
            get(copilot_models_for_account),
        )
        .route(
            "/api/copilot/accounts/{account_id}/usage",
            get(copilot_usage_for_account),
        )
        .route("/api/codex-oauth/quota", get(codex_oauth_quota))
        .route("/api/codex-oauth/models", get(codex_oauth_models))
        .route(
            "/api/opencode/live-provider-ids",
            get(opencode_live_provider_ids),
        )
        .route(
            "/api/openclaw/live-provider-ids",
            get(openclaw_live_provider_ids),
        )
        .route(
            "/api/openclaw/live-provider/{provider_id}",
            get(openclaw_live_provider),
        )
        .route("/api/openclaw/health", get(openclaw_health))
        .route(
            "/api/openclaw/default-model",
            get(openclaw_default_model).put(openclaw_set_default_model),
        )
        .route(
            "/api/openclaw/model-catalog",
            get(openclaw_model_catalog).put(openclaw_set_model_catalog),
        )
        .route(
            "/api/openclaw/agents-defaults",
            get(openclaw_agents_defaults).put(openclaw_set_agents_defaults),
        )
        .route("/api/openclaw/env", get(openclaw_env).put(openclaw_set_env))
        .route(
            "/api/openclaw/tools",
            get(openclaw_tools).put(openclaw_set_tools),
        )
        .route(
            "/api/hermes/live-provider-ids",
            get(hermes_live_provider_ids),
        )
        .route(
            "/api/hermes/live-provider/{provider_id}",
            get(hermes_live_provider),
        )
        .route("/api/hermes/model-config", get(hermes_model_config))
        .route(
            "/api/hermes/memory",
            get(hermes_memory).put(hermes_set_memory),
        )
        .route("/api/hermes/memory-limits", get(hermes_memory_limits))
        .route("/api/hermes/memory-enabled", put(hermes_set_memory_enabled))
        .route("/api/hermes/open-web-ui", post(hermes_open_web_ui))
        .route(
            "/api/hermes/launch-dashboard",
            post(hermes_launch_dashboard),
        )
        .route("/api/claude-plugin/status", get(claude_plugin_status))
        .route("/api/claude-plugin/config", get(claude_plugin_read))
        .route("/api/claude-plugin/apply", post(claude_plugin_apply))
        .route("/api/claude-plugin/applied", get(claude_plugin_applied))
        .route("/api/claude/onboarding-skip", post(claude_onboarding_skip))
        .route(
            "/api/claude/onboarding-clear",
            post(claude_onboarding_clear),
        )
        .route("/api/sync/webdav/test", post(sync_webdav_test))
        .route("/api/sync/webdav/status", get(sync_webdav_status))
        .route(
            "/api/sync/webdav/test-settings",
            post(sync_webdav_test_with_settings),
        )
        .route("/api/sync/webdav/settings", put(sync_webdav_save_settings))
        .route("/api/sync/webdav/upload", post(sync_webdav_upload))
        .route("/api/sync/webdav/download", post(sync_webdav_download))
        .route("/api/sync/webdav/remote-info", get(sync_webdav_remote_info))
        .route("/api/sync/s3/test", post(sync_s3_test))
        .route("/api/sync/s3/status", get(sync_s3_status))
        .route(
            "/api/sync/s3/test-settings",
            post(sync_s3_test_with_settings),
        )
        .route("/api/sync/s3/settings", put(sync_s3_save_settings))
        .route("/api/sync/s3/upload", post(sync_s3_upload))
        .route("/api/sync/s3/download", post(sync_s3_download))
        .route("/api/sync/s3/remote-info", get(sync_s3_remote_info))
}
