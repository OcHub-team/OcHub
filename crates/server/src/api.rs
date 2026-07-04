//! HTTP control API mirroring the cc-switch command surface.
//!
//! Phase 1 wires the provider CRUD/switch endpoints + device settings + config
//! status. Further command groups (MCP, prompts, skills, proxy, usage, sync,
//! auth, sessions) are layered on in their respective phases.

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use routedeck_core::apps::claude_desktop;
use routedeck_core::model::{ClaudeDesktopMode, ClaudeDesktopModelRoute, ProviderMeta, UniversalProvider};
use routedeck_core::services::provider::{self, ProviderService, ProviderSortUpdate};
use routedeck_core::settings::AppSettings;
use routedeck_core::{AppError, AppType, Provider};

use crate::error::{ApiError, ApiResult};
use crate::state::ServerState;

fn parse_app(app: &str) -> Result<AppType, ApiError> {
    app.parse::<AppType>().map_err(ApiError::from)
}

fn to_value<T: serde::Serialize>(v: T) -> ApiResult<Json<Value>> {
    serde_json::to_value(v)
        .map(Json)
        .map_err(|e| ApiError(AppError::JsonSerialize { source: e }))
}

#[derive(Deserialize)]
struct AddProviderRequest {
    provider: Provider,
    #[serde(default, rename = "addToLive")]
    add_to_live: Option<bool>,
}

#[derive(Deserialize)]
struct UpdateProviderRequest {
    provider: Provider,
    #[serde(default, rename = "originalId")]
    original_id: Option<String>,
}

#[derive(Deserialize)]
struct SortRequest {
    updates: Vec<ProviderSortUpdate>,
}

#[derive(Deserialize)]
struct EndpointUrlRequest {
    url: String,
}

async fn list_providers(
    State(state): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let providers = ProviderService::list(&state.app, app_type)?;
    Ok(Json(serde_json::to_value(providers).map_err(|e| {
        ApiError(AppError::JsonSerialize { source: e })
    })?))
}

async fn current_provider(
    State(state): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let current = ProviderService::current(&state.app, app_type)?;
    Ok(Json(json!({ "current": current })))
}

async fn add_provider(
    State(state): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<AddProviderRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let ok = ProviderService::add(
        &state.app,
        app_type,
        req.provider,
        req.add_to_live.unwrap_or(true),
    )?;
    Ok(Json(json!({ "ok": ok })))
}

async fn update_provider(
    State(state): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<UpdateProviderRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let ok = ProviderService::update(
        &state.app,
        app_type,
        req.original_id.as_deref(),
        req.provider,
    )?;
    Ok(Json(json!({ "ok": ok })))
}

async fn delete_provider(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::delete(&state.app, app_type, &id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn switch_provider(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let result = ProviderService::switch(&state.app, app_type, &id)?;
    Ok(Json(json!({ "warnings": result.warnings })))
}

async fn remove_from_live(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::remove_from_live_config(&state.app, app_type, &id)?;
    Ok(Json(json!({ "ok": true })))
}

async fn import_default(
    State(state): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let imported = ProviderService::import_default_config(&state.app, app_type)?;
    Ok(Json(json!({ "imported": imported })))
}

async fn import_live(
    State(state): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    let imported = match app_type {
        AppType::OpenCode => provider::import_opencode_providers_from_live(&state.app)?,
        AppType::OpenClaw => provider::import_openclaw_providers_from_live(&state.app)?,
        AppType::Hermes => provider::import_hermes_providers_from_live(&state.app)?,
        other => {
            return Err(ApiError(AppError::InvalidInput(format!(
                "{} does not support live-provider import",
                other.as_str()
            ))))
        }
    };
    Ok(Json(json!({ "imported": imported })))
}

async fn read_live_settings(Path(app): Path<String>) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    to_value(ProviderService::read_live_settings(app_type)?)
}

async fn sync_current_live(
    State(state): State<ServerState>,
    Path(app): Path<String>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::sync_current_provider_for_app(&state.app, app_type)?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_custom_endpoints(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    to_value(ProviderService::get_custom_endpoints(
        &state.app, app_type, &id,
    )?)
}

async fn add_custom_endpoint(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
    Json(req): Json<EndpointUrlRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::add_custom_endpoint(&state.app, app_type, &id, req.url)?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_custom_endpoint(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
    Json(req): Json<EndpointUrlRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::remove_custom_endpoint(&state.app, app_type, &id, req.url)?;
    Ok(Json(json!({ "ok": true })))
}

async fn endpoint_last_used(
    State(state): State<ServerState>,
    Path((app, id)): Path<(String, String)>,
    Json(req): Json<EndpointUrlRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::update_endpoint_last_used(&state.app, app_type, &id, req.url)?;
    Ok(Json(json!({ "ok": true })))
}

async fn universal_list(State(state): State<ServerState>) -> ApiResult<Json<Value>> {
    to_value(ProviderService::list_universal(&state.app)?)
}

async fn universal_get(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    to_value(ProviderService::get_universal(&state.app, &id)?)
}

async fn universal_upsert(
    State(state): State<ServerState>,
    Json(provider): Json<UniversalProvider>,
) -> ApiResult<Json<Value>> {
    let ok = ProviderService::upsert_universal(&state.app, provider)?;
    Ok(Json(json!({ "ok": ok })))
}

async fn universal_delete(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ok = ProviderService::delete_universal(&state.app, &id)?;
    Ok(Json(json!({ "ok": ok })))
}

async fn universal_sync(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ok = ProviderService::sync_universal_to_apps(&state.app, &id)?;
    Ok(Json(json!({ "ok": ok })))
}

async fn claude_desktop_status(State(state): State<ServerState>) -> ApiResult<Json<Value>> {
    let proxy_running = state.app.proxy_service.is_running().await;
    to_value(claude_desktop::get_status(&state.app.db, proxy_running)?)
}

async fn claude_desktop_default_routes() -> Json<Value> {
    Json(json!(claude_desktop::default_proxy_routes()))
}

async fn claude_desktop_ensure_official(
    State(state): State<ServerState>,
) -> ApiResult<Json<Value>> {
    let ok = state
        .app
        .db
        .ensure_official_seed_by_id("claude-desktop-official", AppType::ClaudeDesktop)?;
    Ok(Json(json!({ "ok": ok })))
}

async fn claude_desktop_import_from_claude(
    State(state): State<ServerState>,
) -> ApiResult<Json<Value>> {
    let claude_providers = state.app.db.get_all_providers(AppType::Claude.as_str())?;
    let desktop_providers = state
        .app
        .db
        .get_all_providers(AppType::ClaudeDesktop.as_str())?;

    let mut imported = 0usize;
    for provider in claude_providers.values() {
        if desktop_providers.contains_key(&provider.id) {
            continue;
        }

        let mut desktop_provider = provider.clone();
        desktop_provider.in_failover_queue = false;
        let meta = desktop_provider
            .meta
            .get_or_insert_with(ProviderMeta::default);

        if claude_desktop::is_compatible_direct_provider(provider)
            && claude_provider_models_are_claude_safe(provider)
        {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Direct);
        } else if let Some(routes) = suggested_claude_desktop_routes(provider) {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Proxy);
            meta.claude_desktop_model_routes = routes;
        } else {
            continue;
        }

        state
            .app
            .db
            .save_provider(AppType::ClaudeDesktop.as_str(), &desktop_provider)?;
        imported += 1;
    }

    if let Err(err) = state
        .app
        .db
        .ensure_official_seed_by_id("claude-desktop-official", AppType::ClaudeDesktop)
    {
        log::warn!("failed to ensure Claude Desktop official seed during import: {err}");
    }

    Ok(Json(json!({ "imported": imported })))
}

async fn update_sort_order(
    State(state): State<ServerState>,
    Path(app): Path<String>,
    Json(req): Json<SortRequest>,
) -> ApiResult<Json<Value>> {
    let app_type = parse_app(&app)?;
    ProviderService::update_sort_order(&state.app, app_type, req.updates)?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_settings() -> ApiResult<Json<AppSettings>> {
    Ok(Json(routedeck_core::settings::get_settings_for_frontend()))
}

fn merge_settings_for_save(mut incoming: AppSettings, existing: &AppSettings) -> AppSettings {
    match (&mut incoming.webdav_sync, &existing.webdav_sync) {
        (None, _) => incoming.webdav_sync = existing.webdav_sync.clone(),
        (Some(incoming_sync), Some(existing_sync))
            if incoming_sync.password.is_empty() && !existing_sync.password.is_empty() =>
        {
            incoming_sync.password = existing_sync.password.clone();
        }
        _ => {}
    }

    match (&mut incoming.s3_sync, &existing.s3_sync) {
        (None, _) => incoming.s3_sync = existing.s3_sync.clone(),
        (Some(incoming_sync), Some(existing_sync))
            if incoming_sync.secret_access_key.is_empty()
                && !existing_sync.secret_access_key.is_empty() =>
        {
            incoming_sync.secret_access_key = existing_sync.secret_access_key.clone();
        }
        _ => {}
    }

    incoming.local_migrations = existing.local_migrations.clone();
    incoming
}

async fn save_settings(
    State(state): State<ServerState>,
    Json(settings): Json<AppSettings>,
) -> ApiResult<Json<Value>> {
    let existing = routedeck_core::settings::get_settings();
    let merged = merge_settings_for_save(settings, &existing);
    let unify_codex_changed =
        merged.unify_codex_session_history != existing.unify_codex_session_history;
    let unify_codex_enabled = merged.unify_codex_session_history;

    routedeck_core::settings::update_settings(merged)?;

    if unify_codex_changed {
        if let Err(err) = provider::reapply_current_codex_official_live(&state.app) {
            log::warn!(
                "failed to reapply Codex official live config after unify-history change; rolling back: {err}"
            );
            if let Err(rollback_err) = routedeck_core::settings::update_settings(existing) {
                log::error!(
                    "failed to roll back settings after Codex live rewrite failure: {rollback_err}"
                );
            }
            return Err(ApiError(AppError::Message(format!(
                "统一 Codex 会话历史开关未生效（live 配置重写失败）: {err}"
            ))));
        }

        if unify_codex_enabled {
            tokio::task::spawn_blocking(|| {
                match routedeck_core::services::codex_history_migration::maybe_migrate_codex_official_history_to_unified_bucket() {
                    Ok(outcome) => {
                        if let Some(reason) = outcome.skipped_reason {
                            log::debug!(
                                "Codex official history unify migration skipped: {reason}"
                            );
                        } else {
                            log::info!(
                                "Codex official history unify migration completed: jsonl_files={}, state_rows={}",
                                outcome.migrated_jsonl_files,
                                outcome.migrated_state_rows
                            );
                        }
                    }
                    Err(err) => {
                        log::warn!("Codex official history unify migration failed: {err}");
                    }
                }
            });
        } else {
            if let Err(err) = routedeck_core::settings::clear_codex_official_history_unify_migration() {
                log::warn!("failed to clear Codex official history unify migration marker: {err}");
            }
            if let Err(err) = routedeck_core::settings::clear_codex_unify_migrate_existing() {
                log::warn!("failed to clear Codex unify migrate-existing flag: {err}");
            }
        }
    }

    Ok(Json(json!({ "ok": true })))
}

async fn claude_config_status() -> Json<Value> {
    let status = routedeck_core::paths::get_claude_config_status();
    Json(json!({ "exists": status.exists, "path": status.path }))
}

fn claude_provider_models_are_claude_safe(provider: &Provider) -> bool {
    let Some(env) = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())
    else {
        return true;
    };

    [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ]
    .into_iter()
    .filter_map(|key| env.get(key).and_then(|value| value.as_str()))
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .all(claude_desktop::is_claude_safe_model_id)
}

fn suggested_claude_desktop_routes(
    provider: &Provider,
) -> Option<HashMap<String, ClaudeDesktopModelRoute>> {
    let env = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())?;
    let mut routes = HashMap::new();
    let supports_1m_default = !matches!(
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.provider_type.as_deref()),
        Some("github_copilot") | Some("codex_oauth")
    );

    fn add_route(
        routes: &mut HashMap<String, ClaudeDesktopModelRoute>,
        env: &serde_json::Map<String, Value>,
        route_key: &str,
        env_key: &str,
        supports_1m_default: bool,
    ) {
        let Some(raw_model) = env
            .get(env_key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        let marker = claude_desktop::ONE_M_CONTEXT_MARKER.as_bytes();
        let raw_bytes = raw_model.as_bytes();
        let has_1m_marker = raw_bytes.len() >= marker.len()
            && raw_bytes[raw_bytes.len() - marker.len()..].eq_ignore_ascii_case(marker);
        let stripped_model = if has_1m_marker {
            raw_model[..raw_model.len() - marker.len()].trim_end()
        } else {
            raw_model
        };
        if stripped_model.is_empty() {
            return;
        }

        let effective_supports_1m = supports_1m_default || has_1m_marker;
        let explicit_label_override = env
            .get(format!("{env_key}_NAME").as_str())
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let label_override = explicit_label_override.clone().or_else(|| {
            (!claude_desktop::is_claude_safe_model_id(stripped_model))
                .then(|| stripped_model.to_string())
        });

        let should_overwrite = |existing: Option<&str>| {
            existing.is_none()
                || explicit_label_override.is_some()
                || existing == Some(stripped_model)
        };

        let merge_into = |existing: &mut ClaudeDesktopModelRoute| {
            let merged = existing.supports_1m.unwrap_or(false) || effective_supports_1m;
            existing.supports_1m = Some(merged);
            if should_overwrite(existing.label_override.as_deref()) {
                existing.label_override = label_override.clone();
            }
        };

        if let Some(existing) = routes
            .values_mut()
            .find(|existing| existing.model == stripped_model)
        {
            merge_into(existing);
            return;
        }

        routes
            .entry(route_key.to_string())
            .and_modify(merge_into)
            .or_insert_with(|| ClaudeDesktopModelRoute {
                model: stripped_model.to_string(),
                label_override,
                supports_1m: Some(effective_supports_1m),
            });
    }

    for spec in claude_desktop::default_proxy_routes() {
        add_route(
            &mut routes,
            env,
            spec.route_id,
            spec.env_key,
            supports_1m_default,
        );
    }

    if routes.is_empty() {
        let primary_route = claude_desktop::default_proxy_routes()
            .first()
            .map(|route| route.route_id)
            .unwrap_or("sonnet");
        add_route(
            &mut routes,
            env,
            primary_route,
            "ANTHROPIC_MODEL",
            supports_1m_default,
        );
    }

    (!routes.is_empty()).then_some(routes)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "RouteDeck" }))
}

/// Build the provider + settings + status routes. State is applied by the caller.
///
/// Per-id routes live under `/by-id/:id` so that, under `/:app/`, the `:id`
/// param never sits as a sibling of static segments (`current`, `sort`, …) —
/// matchit (axum 0.7) rejects static-vs-param siblings at the same position.
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/settings", get(get_settings).put(save_settings))
        .route("/api/config/claude-status", get(claude_config_status))
        .route("/api/claude-desktop/status", get(claude_desktop_status))
        .route(
            "/api/claude-desktop/default-routes",
            get(claude_desktop_default_routes),
        )
        .route(
            "/api/claude-desktop/import-from-claude",
            post(claude_desktop_import_from_claude),
        )
        .route(
            "/api/claude-desktop/ensure-official",
            post(claude_desktop_ensure_official),
        )
        .route(
            "/api/universal-providers",
            get(universal_list)
                .post(universal_upsert)
                .put(universal_upsert),
        )
        .route(
            "/api/universal-providers/:id",
            get(universal_get).delete(universal_delete),
        )
        .route("/api/universal-providers/:id/sync", post(universal_sync))
        .route(
            "/api/providers/:app",
            get(list_providers).post(add_provider).put(update_provider),
        )
        .route("/api/providers/:app/current", get(current_provider))
        .route("/api/providers/:app/import-default", post(import_default))
        .route("/api/providers/:app/import-live", post(import_live))
        .route("/api/providers/:app/live-settings", get(read_live_settings))
        .route(
            "/api/providers/:app/sync-current-live",
            post(sync_current_live),
        )
        .route("/api/providers/:app/sort", put(update_sort_order))
        .route(
            "/api/providers/:app/by-id/:id",
            axum::routing::delete(delete_provider),
        )
        .route(
            "/api/providers/:app/by-id/:id/switch",
            post(switch_provider),
        )
        .route(
            "/api/providers/:app/by-id/:id/remove-from-live",
            post(remove_from_live),
        )
        .route(
            "/api/providers/:app/by-id/:id/custom-endpoints",
            get(get_custom_endpoints)
                .post(add_custom_endpoint)
                .delete(remove_custom_endpoint),
        )
        .route(
            "/api/providers/:app/by-id/:id/custom-endpoints/last-used",
            post(endpoint_last_used),
        )
}
