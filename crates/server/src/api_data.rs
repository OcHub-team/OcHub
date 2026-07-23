//! Control-API routes for usage statistics, session manager, environment
//! conflicts, and deeplink import.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

use ochub_core::services::usage_stats::LogFilters;
use ochub_core::AppError;

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

// ----- Usage statistics -----

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(alias = "startDate")]
    start: Option<i64>,
    #[serde(alias = "endDate")]
    end: Option<i64>,
    #[serde(alias = "appType")]
    app: Option<String>,
    #[serde(alias = "providerName")]
    provider: Option<String>,
    model: Option<String>,
}

async fn usage_summary(
    State(s): State<ServerState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    let summary = s.app.db.get_usage_summary(
        q.start,
        q.end,
        q.app.as_deref(),
        q.provider.as_deref(),
        q.model.as_deref(),
    )?;
    to_value(summary)
}

async fn usage_by_app(
    State(s): State<ServerState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_usage_summary_by_app(
        q.start,
        q.end,
        q.provider.as_deref(),
        q.model.as_deref(),
    )?)
}

#[derive(Deserialize)]
struct LogsRequest {
    #[serde(default)]
    filters: LogFilters,
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    page_size: u32,
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

async fn usage_logs(
    State(s): State<ServerState>,
    Json(req): Json<LogsRequest>,
) -> ApiResult<Json<Value>> {
    to_value(
        s.app
            .db
            .get_request_logs(&req.filters, req.page, req.page_size)?,
    )
}

async fn usage_trends(
    State(s): State<ServerState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_daily_trends(
        q.start,
        q.end,
        q.app.as_deref(),
        q.provider.as_deref(),
        q.model.as_deref(),
    )?)
}

async fn usage_provider_stats(
    State(s): State<ServerState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_provider_stats(
        q.start,
        q.end,
        q.app.as_deref(),
        q.provider.as_deref(),
        q.model.as_deref(),
    )?)
}

async fn usage_model_stats(
    State(s): State<ServerState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_model_stats(
        q.start,
        q.end,
        q.app.as_deref(),
        q.provider.as_deref(),
        q.model.as_deref(),
    )?)
}

async fn usage_request_detail(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_request_detail(&id)?)
}

async fn usage_model_pricing(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_model_pricing()?)
}

#[derive(Deserialize)]
struct PricingUpdateRequest {
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "inputCost")]
    input_cost: String,
    #[serde(rename = "outputCost")]
    output_cost: String,
    #[serde(rename = "cacheReadCost")]
    cache_read_cost: String,
    #[serde(rename = "cacheCreationCost")]
    cache_creation_cost: String,
}

async fn usage_model_pricing_update(
    State(s): State<ServerState>,
    Path(model_id): Path<String>,
    Json(req): Json<PricingUpdateRequest>,
) -> ApiResult<Json<Value>> {
    s.app.db.update_model_pricing(
        &model_id,
        &req.display_name,
        &req.input_cost,
        &req.output_cost,
        &req.cache_read_cost,
        &req.cache_creation_cost,
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn usage_model_pricing_delete(
    State(s): State<ServerState>,
    Path(model_id): Path<String>,
) -> ApiResult<Json<Value>> {
    s.app.db.delete_model_pricing(&model_id)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(rename = "providerId")]
    provider_id: String,
    #[serde(rename = "appType")]
    app_type: String,
}

async fn usage_provider_limits(
    State(s): State<ServerState>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    to_value(
        s.app
            .db
            .check_provider_limits(&q.provider_id, &q.app_type)?,
    )
}

async fn usage_session_sync(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let mut result = ochub_core::services::session_usage::sync_claude_session_logs(&s.app.db)?;
    for (label, sync_result) in [
        (
            "Codex",
            ochub_core::services::session_usage_codex::sync_codex_usage(&s.app.db),
        ),
        (
            "Gemini",
            ochub_core::services::session_usage_gemini::sync_gemini_usage(&s.app.db),
        ),
        (
            "OpenCode",
            ochub_core::services::session_usage_opencode::sync_opencode_usage(&s.app.db),
        ),
    ] {
        match sync_result {
            Ok(r) => {
                result.imported += r.imported;
                result.skipped += r.skipped;
                result.files_scanned += r.files_scanned;
                result.errors.extend(r.errors);
            }
            Err(err) => result.errors.push(format!("{label} sync failed: {err}")),
        }
    }
    to_value(result)
}

async fn usage_data_sources(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::session_usage::get_data_source_breakdown(&s.app.db)?)
}

// ----- Sessions -----

async fn sessions_list() -> ApiResult<Json<Value>> {
    to_value(ochub_core::session_manager::scan_sessions())
}

#[derive(Deserialize)]
struct SessionMessagesRequest {
    provider_id: String,
    source_path: String,
}

async fn session_messages(Json(req): Json<SessionMessagesRequest>) -> ApiResult<Json<Value>> {
    let messages = ochub_core::session_manager::load_messages(&req.provider_id, &req.source_path)?;
    to_value(messages)
}

#[derive(Deserialize)]
struct DeleteSessionBody {
    provider_id: String,
    session_id: String,
    source_path: String,
}

async fn session_delete(Json(req): Json<DeleteSessionBody>) -> ApiResult<Json<Value>> {
    let deleted = ochub_core::session_manager::delete_session(
        &req.provider_id,
        &req.session_id,
        &req.source_path,
    )?;
    Ok(Json(json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
struct DeleteSessionsBody {
    items: Vec<ochub_core::session_manager::DeleteSessionRequest>,
}

async fn sessions_delete_batch(Json(req): Json<DeleteSessionsBody>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::session_manager::delete_sessions(&req.items))
}

#[derive(Deserialize)]
struct LaunchTerminalBody {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "customConfig")]
    custom_config: Option<String>,
    #[serde(default)]
    target: Option<String>,
}

async fn session_launch_terminal(Json(req): Json<LaunchTerminalBody>) -> ApiResult<Json<Value>> {
    let preferred = ochub_core::settings::get_settings().preferred_terminal;
    let target = req
        .target
        .or(preferred)
        .map(|value| {
            if value == "iterm2" {
                "iterm".to_string()
            } else {
                value
            }
        })
        .unwrap_or_else(|| "terminal".to_string());
    tokio::task::spawn_blocking(move || {
        ochub_core::session_manager::terminal::launch_terminal(
            &target,
            &req.command,
            req.cwd.as_deref(),
            req.custom_config.as_deref(),
        )
    })
    .await
    .map_err(|e| ApiError(AppError::Message(format!("Failed to launch terminal: {e}"))))??;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ProviderTerminalBody {
    app: String,
    #[serde(rename = "providerId")]
    provider_id: String,
    #[serde(default)]
    cwd: Option<String>,
}

async fn provider_terminal(
    State(s): State<ServerState>,
    Json(req): Json<ProviderTerminalBody>,
) -> ApiResult<Json<Value>> {
    let app = s.app.clone();
    let opened = tokio::task::spawn_blocking(move || {
        ochub_core::session_manager::open_provider_terminal(
            &app,
            &req.app,
            &req.provider_id,
            req.cwd,
        )
    })
    .await
    .map_err(|e| {
        ApiError(AppError::Message(format!(
            "Failed to open provider terminal: {e}"
        )))
    })??;
    Ok(Json(json!({ "opened": opened })))
}

// ----- CLI tools -----

#[derive(Deserialize)]
struct ToolVersionsRequest {
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default, rename = "wslShellByTool")]
    wsl_shell_by_tool:
        Option<HashMap<String, ochub_core::session_manager::tools::WslShellPreferenceInput>>,
}

async fn tool_versions(Json(req): Json<ToolVersionsRequest>) -> ApiResult<Json<Value>> {
    to_value(
        ochub_core::session_manager::get_tool_versions(req.tools, req.wsl_shell_by_tool).await?,
    )
}

#[derive(Deserialize)]
struct ToolLifecycleRequest {
    tools: Vec<String>,
    action: String,
    #[serde(default, rename = "wslShellByTool")]
    wsl_shell_by_tool:
        Option<HashMap<String, ochub_core::session_manager::tools::WslShellPreferenceInput>>,
}

async fn tool_lifecycle(Json(req): Json<ToolLifecycleRequest>) -> ApiResult<Json<Value>> {
    tokio::task::spawn_blocking(move || {
        ochub_core::session_manager::run_tool_lifecycle_action(
            req.tools,
            req.action,
            req.wsl_shell_by_tool,
        )
    })
    .await
    .map_err(|e| {
        ApiError(AppError::Message(format!(
            "Tool lifecycle task failed: {e}"
        )))
    })??;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ToolProbeRequest {
    tools: Vec<String>,
}

async fn tool_probe(Json(req): Json<ToolProbeRequest>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::session_manager::probe_tool_installations(
        req.tools,
    )?)
}

// ----- Environment conflicts -----

async fn env_conflicts(Path(app): Path<String>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::env::check_env_conflicts(&app)?)
}

#[derive(Deserialize)]
struct DeleteEnvRequest {
    conflicts: Vec<ochub_core::services::env::EnvConflict>,
}

async fn env_delete(Json(req): Json<DeleteEnvRequest>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::services::env::delete_env_vars(req.conflicts)?)
}

#[derive(Deserialize)]
struct RestoreEnvRequest {
    #[serde(rename = "backupPath")]
    backup_path: String,
}

async fn env_restore(Json(req): Json<RestoreEnvRequest>) -> ApiResult<Json<Value>> {
    ochub_core::services::env::restore_env_backup(req.backup_path)?;
    Ok(Json(json!({ "ok": true })))
}

// ----- Import/export and database backups -----

#[derive(Deserialize)]
struct FilePathRequest {
    #[serde(rename = "filePath")]
    file_path: String,
}

async fn export_config(
    State(s): State<ServerState>,
    Json(req): Json<FilePathRequest>,
) -> ApiResult<Json<Value>> {
    let db = s.app.db.clone();
    let file_path = req.file_path.clone();
    tokio::task::spawn_blocking(move || db.export_sql(&PathBuf::from(&file_path)))
        .await
        .map_err(|e| ApiError(AppError::Message(format!("Export task failed: {e}"))))??;
    Ok(Json(json!({
        "success": true,
        "message": "SQL exported successfully",
        "filePath": req.file_path
    })))
}

async fn import_config(
    State(s): State<ServerState>,
    Json(req): Json<FilePathRequest>,
) -> ApiResult<Json<Value>> {
    let db = s.app.db.clone();
    let file_path = req.file_path.clone();
    let backup_id = tokio::task::spawn_blocking(move || db.import_sql(&PathBuf::from(file_path)))
        .await
        .map_err(|e| ApiError(AppError::Message(format!("Import task failed: {e}"))))??;
    let warning = match ochub_core::services::ProviderService::sync_current_to_live(&s.app) {
        Ok(()) => None,
        Err(err) => {
            log::warn!("[Import] post-import live sync warning: {err}");
            Some(err.to_string())
        }
    };
    Ok(Json(json!({
        "success": true,
        "backupId": backup_id,
        "warning": warning
    })))
}

async fn sync_current_providers_live(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    ochub_core::services::ProviderService::sync_current_to_live(&s.app)?;
    Ok(Json(json!({
        "success": true,
        "message": "Live configuration synchronized"
    })))
}

async fn db_backups_list() -> ApiResult<Json<Value>> {
    to_value(ochub_core::db::Database::list_backups()?)
}

async fn db_backup_create(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let db = s.app.db.clone();
    let filename = tokio::task::spawn_blocking(move || db.create_backup_file())
        .await
        .map_err(|e| ApiError(AppError::Message(format!("Backup task failed: {e}"))))??;
    Ok(Json(json!({ "filename": filename })))
}

async fn db_backup_restore(
    State(s): State<ServerState>,
    Path(filename): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = s.app.db.clone();
    let backup_id = tokio::task::spawn_blocking(move || db.restore_from_backup(&filename))
        .await
        .map_err(|e| ApiError(AppError::Message(format!("Restore task failed: {e}"))))??;
    Ok(Json(json!({ "backupId": backup_id })))
}

#[derive(Deserialize)]
struct RenameBackupRequest {
    #[serde(rename = "newName")]
    new_name: String,
}

async fn db_backup_rename(
    Path(filename): Path<String>,
    Json(req): Json<RenameBackupRequest>,
) -> ApiResult<Json<Value>> {
    let renamed = ochub_core::db::Database::rename_backup(&filename, &req.new_name)?;
    Ok(Json(json!({ "filename": renamed })))
}

async fn db_backup_delete(Path(filename): Path<String>) -> ApiResult<Json<Value>> {
    ochub_core::db::Database::delete_backup(&filename)?;
    Ok(Json(json!({ "ok": true })))
}

// ----- Deeplink -----

#[derive(Deserialize)]
struct DeeplinkBody {
    url: String,
}

async fn deeplink_parse(Json(body): Json<DeeplinkBody>) -> ApiResult<Json<Value>> {
    to_value(ochub_core::deeplink::parse_deeplink_url(&body.url)?)
}

async fn deeplink_import(
    State(s): State<ServerState>,
    Json(body): Json<DeeplinkBody>,
) -> ApiResult<Json<Value>> {
    let request = ochub_core::deeplink::parse_deeplink_url(&body.url)?;
    match request.resource.as_str() {
        "provider" => {
            let id = ochub_core::deeplink::import_provider_from_deeplink(&s.app, request)?;
            Ok(Json(json!({ "resource": "provider", "id": id })))
        }
        "mcp" => {
            let result = ochub_core::deeplink::import_mcp_from_deeplink(&s.app, request)?;
            to_value(json!({ "resource": "mcp", "result": serde_json::to_value(result).ok() }))
        }
        "skill" => {
            let id = ochub_core::deeplink::import_skill_from_deeplink(&s.app, request)?;
            to_value(json!({ "resource": "skill", "result": serde_json::to_value(id).ok() }))
        }
        other => Err(ApiError(AppError::InvalidInput(format!(
            "unknown deeplink resource: {other}"
        )))),
    }
}

pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/usage/summary", get(usage_summary))
        .route("/api/usage/by-app", get(usage_by_app))
        .route("/api/usage/trends", get(usage_trends))
        .route("/api/usage/providers", get(usage_provider_stats))
        .route("/api/usage/models", get(usage_model_stats))
        .route("/api/usage/logs", post(usage_logs))
        .route("/api/usage/request/{id}", get(usage_request_detail))
        .route("/api/usage/pricing", get(usage_model_pricing))
        .route(
            "/api/usage/pricing/{model_id}",
            put(usage_model_pricing_update).delete(usage_model_pricing_delete),
        )
        .route("/api/usage/provider-limits", get(usage_provider_limits))
        .route("/api/usage/session-sync", post(usage_session_sync))
        .route("/api/usage/data-sources", get(usage_data_sources))
        .route("/api/sessions", get(sessions_list))
        .route("/api/sessions/messages", post(session_messages))
        .route("/api/sessions/delete", post(session_delete))
        .route("/api/sessions/delete-batch", post(sessions_delete_batch))
        .route(
            "/api/sessions/launch-terminal",
            post(session_launch_terminal),
        )
        .route("/api/provider-terminal", post(provider_terminal))
        .route("/api/tools/versions", post(tool_versions))
        .route("/api/tools/lifecycle", post(tool_lifecycle))
        .route("/api/tools/probe", post(tool_probe))
        .route("/api/env/conflicts/{app}", get(env_conflicts))
        .route("/api/env/delete", post(env_delete))
        .route("/api/env/restore", post(env_restore))
        .route("/api/config/export", post(export_config))
        .route("/api/config/import", post(import_config))
        .route(
            "/api/config/sync-current-providers-live",
            post(sync_current_providers_live),
        )
        .route(
            "/api/backups/db",
            get(db_backups_list).post(db_backup_create),
        )
        .route(
            "/api/backups/db/{filename}/restore",
            post(db_backup_restore),
        )
        .route("/api/backups/db/{filename}/rename", put(db_backup_rename))
        .route(
            "/api/backups/db/{filename}",
            axum::routing::delete(db_backup_delete),
        )
        .route("/api/deeplink/parse", post(deeplink_parse))
        .route("/api/deeplink/import", post(deeplink_import))
}
