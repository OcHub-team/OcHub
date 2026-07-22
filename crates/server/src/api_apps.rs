//! Control-API routes for the provider-attached subsystems: MCP servers,
//! prompts, common-config snippets, and skills.

use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path as FsPath;

use ochub_core::db::legacy_json::{McpApps, McpServer, Prompt, SkillRepo};
use ochub_core::services::skill::{DiscoverableSkill, ImportSkillSelection};
use ochub_core::services::{ConfigService, McpService, PromptService, SkillService};
use ochub_core::settings::SkillStorageLocation;
use ochub_core::{AppError, AppType};

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn parse_app(app: &str) -> Result<AppType, ApiError> {
    let app_type = parse_app_inner(app)?;
    ochub_core::plugin::ensure_app_type_enabled(&app_type).map_err(ApiError::from)?;
    Ok(app_type)
}

fn parse_app_inner(app: &str) -> Result<AppType, ApiError> {
    app.parse::<AppType>().map_err(ApiError::from)
}

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

// ----- MCP -----

#[derive(Deserialize)]
struct ToggleAppRequest {
    app: String,
    enabled: bool,
}

async fn mcp_list(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(McpService::get_all_servers(&s.app)?)
}

async fn mcp_upsert(
    State(s): State<ServerState>,
    Json(server): Json<McpServer>,
) -> ApiResult<Json<Value>> {
    McpService::upsert_server(&s.app, server)?;
    Ok(Json(json!({ "ok": true })))
}

async fn mcp_delete(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let removed = McpService::delete_server(&s.app, &id)?;
    Ok(Json(json!({ "removed": removed })))
}

async fn mcp_toggle(
    State(s): State<ServerState>,
    Path(id): Path<String>,
    Json(req): Json<ToggleAppRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&req.app)?;
    McpService::toggle_app(&s.app, &id, app, req.enabled)?;
    Ok(Json(json!({ "ok": true })))
}

async fn mcp_sync(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    McpService::sync_all_enabled(&s.app)?;
    Ok(Json(json!({ "ok": true })))
}

async fn mcp_import(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let count = match parse_app(&app)? {
        AppType::Claude | AppType::ClaudeDesktop => McpService::import_from_claude(&s.app)?,
        AppType::Codex => McpService::import_from_codex(&s.app)?,
        AppType::Gemini => McpService::import_from_gemini(&s.app)?,
        AppType::OpenCode => McpService::import_from_opencode(&s.app)?,
        AppType::Hermes => McpService::import_from_hermes(&s.app)?,
        AppType::OpenClaw => 0,
    };
    Ok(Json(json!({ "imported": count })))
}

async fn mcp_config_get(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_ty = parse_app(&app)?;
    #[allow(deprecated)]
    let servers = McpService::get_servers(&s.app, app_ty)?;
    Ok(Json(json!({
        "configPath": ochub_core::paths::get_app_config_path().to_string_lossy().to_string(),
        "servers": servers,
    })))
}

#[derive(Deserialize)]
struct LegacyMcpUpsertRequest {
    spec: Value,
    #[serde(default, rename = "syncOtherSide")]
    sync_other_side: Option<bool>,
}

async fn mcp_config_upsert(
    State(s): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
    Json(req): Json<LegacyMcpUpsertRequest>,
) -> ApiResult<Json<Value>> {
    let app_ty = parse_app(&app)?;
    let existing = s.app.db.get_all_mcp_servers()?.get(&id).cloned();

    let mut server = if let Some(mut existing) = existing {
        existing.server = req.spec.clone();
        existing.apps.set_enabled_for(&app_ty, true);
        existing
    } else {
        let mut apps = McpApps::default();
        apps.set_enabled_for(&app_ty, true);
        let name = req
            .spec
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        McpServer {
            id: id.clone(),
            name,
            server: req.spec,
            apps,
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        }
    };

    if req.sync_other_side.unwrap_or(false) {
        server.apps.claude = true;
        server.apps.codex = true;
        server.apps.gemini = true;
        server.apps.opencode = true;
        server.apps.hermes = true;
    }

    McpService::upsert_server(&s.app, server)?;
    Ok(Json(json!({ "ok": true })))
}

async fn mcp_config_delete(
    State(s): State<ServerState>,
    Path((_app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let removed = McpService::delete_server(&s.app, &id)?;
    Ok(Json(json!({ "removed": removed })))
}

#[derive(Deserialize)]
struct McpEnabledRequest {
    enabled: bool,
}

async fn mcp_config_enabled(
    State(s): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
    Json(req): Json<McpEnabledRequest>,
) -> ApiResult<Json<Value>> {
    let app_ty = parse_app(&app)?;
    McpService::toggle_app(&s.app, &id, app_ty, req.enabled)?;
    Ok(Json(json!({ "ok": true })))
}

// ----- Prompts -----

#[derive(Deserialize)]
struct UpsertPromptRequest {
    #[serde(default)]
    id: Option<String>,
    prompt: Prompt,
}

async fn prompts_list(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    to_value(PromptService::get_prompts(&s.app, app)?)
}

async fn prompts_upsert(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<UpsertPromptRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    let id = req.id.unwrap_or_else(|| req.prompt.id.clone());
    PromptService::upsert_prompt(&s.app, app, &id, req.prompt)?;
    Ok(Json(json!({ "ok": true })))
}

async fn prompts_delete(
    State(s): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    PromptService::delete_prompt(&s.app, app, &id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn prompts_enable(
    State(s): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    PromptService::enable_prompt(&s.app, app, &id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn prompts_current_file(Path(app): Path<String>) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    let content = PromptService::get_current_file_content(app)?;
    Ok(Json(json!({ "content": content })))
}

async fn prompts_import_file(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&app)?;
    let id = PromptService::import_from_file(&s.app, app)?;
    Ok(Json(json!({ "id": id })))
}

// ----- Common config snippet -----

#[derive(Deserialize)]
struct SnippetRequest {
    snippet: String,
}

async fn snippet_get(
    State(s): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let snippet = ConfigService::get_common_config_snippet(&s.app, &app)?;
    Ok(Json(json!({ "snippet": snippet })))
}

async fn snippet_set(
    State(s): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<SnippetRequest>,
) -> ApiResult<Json<Value>> {
    ConfigService::set_common_config_snippet(&s.app, &app, req.snippet)?;
    Ok(Json(json!({ "ok": true })))
}

// ----- Skills -----

async fn skills_list(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(SkillService::get_all_installed(&s.app.db)?)
}

async fn skills_uninstall(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = SkillService::uninstall(&s.app.db, &id)?;
    to_value(result)
}

async fn skills_toggle(
    State(s): State<ServerState>,
    Path(id): Path<String>,
    Json(req): Json<ToggleAppRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(&req.app)?;
    SkillService::toggle_app(&s.app.db, &id, &app, req.enabled)?;
    Ok(Json(json!({ "ok": true })))
}

async fn skills_scan_unmanaged(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(SkillService::scan_unmanaged(&s.app.db)?)
}

#[derive(Deserialize)]
struct SkillInstallRequest {
    skill: DiscoverableSkill,
    #[serde(default, rename = "currentApp")]
    current_app: Option<String>,
}

async fn skills_install(
    State(s): State<ServerState>,
    Json(req): Json<SkillInstallRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(req.current_app.as_deref().unwrap_or("claude"))?;
    let service = SkillService::new();
    to_value(service.install(&s.app.db, &req.skill, &app).await?)
}

#[derive(Deserialize)]
struct SkillDiscoverRequest {
    #[serde(default)]
    repos: Option<Vec<SkillRepo>>,
}

async fn skills_discover(
    State(s): State<ServerState>,
    Json(req): Json<SkillDiscoverRequest>,
) -> ApiResult<Json<Value>> {
    let repos = match req.repos {
        Some(repos) => repos,
        None => s.app.db.get_skill_repos()?,
    };
    let service = SkillService::new();
    to_value(service.discover_available(repos).await?)
}

async fn skills_catalog(
    State(s): State<ServerState>,
    Json(req): Json<SkillDiscoverRequest>,
) -> ApiResult<Json<Value>> {
    let repos = match req.repos {
        Some(repos) => repos,
        None => s.app.db.get_skill_repos()?,
    };
    let service = SkillService::new();
    to_value(service.list_skills(repos, &s.app.db).await?)
}

async fn skills_updates(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    let service = SkillService::new();
    to_value(service.check_updates(&s.app.db).await?)
}

async fn skills_update(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let service = SkillService::new();
    to_value(service.update_skill(&s.app.db, &id).await?)
}

#[derive(Deserialize)]
struct SkillBackupRestoreRequest {
    #[serde(default, rename = "currentApp")]
    current_app: Option<String>,
}

async fn skills_backups() -> ApiResult<Json<Value>> {
    to_value(SkillService::list_backups()?)
}

async fn skills_delete_backup(Path(backup_id): Path<String>) -> ApiResult<Json<Value>> {
    SkillService::delete_backup(&backup_id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn skills_restore_backup(
    State(s): State<ServerState>,
    Path(backup_id): Path<String>,
    Json(req): Json<SkillBackupRestoreRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(req.current_app.as_deref().unwrap_or("claude"))?;
    to_value(SkillService::restore_from_backup(
        &s.app.db, &backup_id, &app,
    )?)
}

#[derive(Deserialize)]
struct SkillImportRequest {
    imports: Vec<ImportSkillSelection>,
}

async fn skills_import_from_apps(
    State(s): State<ServerState>,
    Json(req): Json<SkillImportRequest>,
) -> ApiResult<Json<Value>> {
    to_value(SkillService::import_from_apps(&s.app.db, req.imports)?)
}

#[derive(Deserialize)]
struct SkillZipRequest {
    path: String,
    #[serde(default, rename = "currentApp")]
    current_app: Option<String>,
}

async fn skills_install_zip(
    State(s): State<ServerState>,
    Json(req): Json<SkillZipRequest>,
) -> ApiResult<Json<Value>> {
    let app = parse_app(req.current_app.as_deref().unwrap_or("claude"))?;
    to_value(SkillService::install_from_zip(
        &s.app.db,
        FsPath::new(&req.path),
        &app,
    )?)
}

#[derive(Deserialize)]
struct SkillSearchRequest {
    query: String,
    #[serde(default = "default_skill_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_skill_limit() -> usize {
    20
}

async fn skills_search_sh(Json(req): Json<SkillSearchRequest>) -> ApiResult<Json<Value>> {
    to_value(SkillService::search_skills_sh(&req.query, req.limit, req.offset).await?)
}

async fn skills_migrate_storage(
    State(s): State<ServerState>,
    Json(target): Json<SkillStorageLocation>,
) -> ApiResult<Json<Value>> {
    to_value(SkillService::migrate_storage(&s.app.db, target)?)
}

async fn skill_repos(State(s): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(s.app.db.get_skill_repos()?)
}

async fn skill_repo_upsert(
    State(s): State<ServerState>,
    Json(repo): Json<SkillRepo>,
) -> ApiResult<Json<Value>> {
    s.app.db.save_skill_repo(&repo)?;
    Ok(Json(json!({ "ok": true })))
}

async fn skill_repo_delete(
    State(s): State<ServerState>,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    s.app.db.delete_skill_repo(&owner, &name)?;
    Ok(Json(json!({ "ok": true })))
}

/// MCP + prompts + config-snippet + skills routes. Per-id routes use a `by-id`
/// segment so `:id` never sits as a sibling of a static segment.
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/mcp", get(mcp_list).post(mcp_upsert))
        .route("/api/mcp/sync", post(mcp_sync))
        .route("/api/mcp/import/{app}", post(mcp_import))
        .route("/api/mcp/config/{app}", get(mcp_config_get))
        .route(
            "/api/mcp/config/{app}/by-id/{id}",
            post(mcp_config_upsert).delete(mcp_config_delete),
        )
        .route(
            "/api/mcp/config/{app}/by-id/{id}/enabled",
            put(mcp_config_enabled),
        )
        .route("/api/mcp/by-id/{id}", delete(mcp_delete))
        .route("/api/mcp/by-id/{id}/toggle", post(mcp_toggle))
        .route("/api/prompts/{app}", get(prompts_list).post(prompts_upsert))
        .route("/api/prompts/{app}/current-file", get(prompts_current_file))
        .route("/api/prompts/{app}/import-file", post(prompts_import_file))
        .route("/api/prompts/{app}/by-id/{id}", delete(prompts_delete))
        .route("/api/prompts/{app}/by-id/{id}/enable", post(prompts_enable))
        .route(
            "/api/config/{app}/common-snippet",
            get(snippet_get).put(snippet_set),
        )
        .route("/api/skills", get(skills_list))
        .route("/api/skills/install", post(skills_install))
        .route("/api/skills/discover", post(skills_discover))
        .route("/api/skills/catalog", post(skills_catalog))
        .route("/api/skills/updates", get(skills_updates))
        .route("/api/skills/backups", get(skills_backups))
        .route(
            "/api/skills/backups/{backup_id}",
            delete(skills_delete_backup),
        )
        .route(
            "/api/skills/backups/{backup_id}/restore",
            post(skills_restore_backup),
        )
        .route(
            "/api/skills/import-from-apps",
            post(skills_import_from_apps),
        )
        .route("/api/skills/install-zip", post(skills_install_zip))
        .route("/api/skills/search-sh", post(skills_search_sh))
        .route("/api/skills/migrate-storage", post(skills_migrate_storage))
        .route(
            "/api/skills/repos",
            get(skill_repos).post(skill_repo_upsert),
        )
        .route(
            "/api/skills/repos/{owner}/{name}",
            delete(skill_repo_delete),
        )
        .route("/api/skills/scan-unmanaged", get(skills_scan_unmanaged))
        .route("/api/skills/by-id/{id}", delete(skills_uninstall))
        .route("/api/skills/by-id/{id}/toggle", post(skills_toggle))
        .route("/api/skills/by-id/{id}/update", post(skills_update))
}
