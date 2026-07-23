//! One-click app configuration for user-facing relay stations.
//!
//! For each supported app this creates (or refreshes) a "Local Gateway"
//! provider entry — reusing the normal provider machinery so the change is
//! visible, switchable, and revertible in the regular provider list — and then
//! switches the app to it. The local service and per-app keys are deliberately
//! hidden implementation details.

use serde::Serialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::gateway::types::{
    Dialect, GatewayChannel, GatewayKey, GatewayReasoningConfig, GatewayRoute,
};
use crate::model::Provider;
use crate::services::provider::ProviderService;

/// Fixed provider id for gateway entries (per app list).
pub const GATEWAY_PROVIDER_ID: &str = "local-gateway";
const GATEWAY_PROVIDER_NAME: &str = "OcHub 转发站模式";
pub const STATION_ROUTE_PREFIX: &str = "station:";

/// Result surfaced to the UI after a one-click apply (or for manual clients).
#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub base_url: String,
    pub key_name: String,
    pub key_secret: String,
    pub route_id: String,
    pub route_name: String,
    /// True when an app config was written (false for the generic info case).
    pub applied: bool,
}

/// Which apps support one-click gateway configuration.
pub fn supported_apps() -> &'static [AppType] {
    &[
        AppType::Claude,
        AppType::ClaudeDesktop,
        AppType::Codex,
        AppType::OpenCode,
        AppType::OpenClaw,
        AppType::Hermes,
    ]
}

/// The inlet dialect each client speaks to the local gateway.
pub fn client_dialect(app_type: AppType) -> Dialect {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => Dialect::Messages,
        AppType::Codex => Dialect::Responses,
        AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => Dialect::Chat,
    }
}

/// Can a station with this upstream dialect serve `app_type`? Single source of
/// truth is the pipeline conversion matrix, so lifting a pipeline restriction
/// automatically lifts the UI/apply guards built on this.
pub fn dialect_compatible(dialect: Dialect, app_type: AppType) -> bool {
    crate::gateway::pipeline::conversion_supported(client_dialect(app_type), dialect)
}

/// Ensure a gateway key named after `label` exists, creating it if needed.
pub fn ensure_key(state: &AppState, label: &str) -> Result<GatewayKey, AppError> {
    if let Some(key) = state
        .db
        .get_gateway_keys()?
        .into_iter()
        .find(|key| key.name == label && key.enabled)
    {
        return Ok(key);
    }
    let key = GatewayKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: label.to_string(),
        key: crate::gateway::generate_key_secret(),
        route_id: None,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_key(&key)?;
    Ok(key)
}

/// Ensure a client key exists and points at the selected route profile.
pub fn ensure_key_for_route(
    state: &AppState,
    label: &str,
    route_id: Option<&str>,
) -> Result<GatewayKey, AppError> {
    let existing = state.db.get_gateway_keys()?;
    if let Some(mut k) = existing.into_iter().find(|k| k.name == label && k.enabled) {
        let target_route = route_id.map(str::to_string);
        if k.route_id != target_route {
            k.route_id = target_route;
            state.db.upsert_gateway_key(&k)?;
        }
        return Ok(k);
    }
    let key = GatewayKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: label.to_string(),
        key: crate::gateway::generate_key_secret(),
        route_id: route_id.map(str::to_string),
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_key(&key)?;
    Ok(key)
}

/// Reuse the first enabled route for an app or create its legacy default route.
pub fn ensure_app_route(state: &AppState, app_type: AppType) -> Result<GatewayRoute, AppError> {
    let bound_route_id = state
        .db
        .get_gateway_keys()?
        .into_iter()
        .find(|key| key.name == app_type.as_str() && key.enabled)
        .and_then(|key| key.route_id);
    if let Some(route_id) = bound_route_id {
        if let Some(route) = state
            .db
            .get_gateway_route_by_id(&route_id)?
            .filter(|route| route.enabled)
        {
            return Ok(route);
        }
    }
    if let Some(route) = state.db.get_gateway_route_for_app(app_type.as_str())? {
        return Ok(route);
    }
    let route = GatewayRoute {
        id: format!("route-{}", app_type.as_str()),
        name: format!("{} 默认路由", app_label(app_type)),
        app_type: Some(app_type.as_str().to_string()),
        channel_ids: Vec::new(),
        default_model: None,
        model_rules: Vec::new(),
        reasoning: GatewayReasoningConfig::default(),
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_route(&route)?;
    Ok(route)
}

pub fn station_route_id(channel_id: &str) -> String {
    format!("{STATION_ROUTE_PREFIX}{channel_id}")
}

/// Ensure the hidden local route backing one user-facing relay-station config.
pub fn ensure_station_route(
    state: &AppState,
    channel: &GatewayChannel,
) -> Result<GatewayRoute, AppError> {
    let route_id = station_route_id(&channel.id);
    if let Some(route) = state.db.get_gateway_route_by_id(&route_id)? {
        return Ok(route);
    }
    let route = GatewayRoute {
        id: route_id,
        name: channel.name.clone(),
        app_type: None,
        channel_ids: vec![channel.id.clone()],
        default_model: None,
        model_rules: Vec::new(),
        reasoning: GatewayReasoningConfig::default(),
        enabled: channel.enabled,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_route(&route)?;
    Ok(route)
}

/// Switch one app/client key to another hidden station route without rewriting
/// its config. Legacy app-scoped routes remain accepted for data migration.
pub fn activate_route_for_app(
    state: &AppState,
    app_type: AppType,
    route_id: &str,
) -> Result<GatewayKey, AppError> {
    let route = state
        .db
        .get_gateway_route_by_id(route_id)?
        .ok_or_else(|| AppError::InvalidInput("找不到转发站配置".to_string()))?;
    if !route.enabled {
        return Err(AppError::InvalidInput("该转发站配置已停用".to_string()));
    }
    if route
        .app_type
        .as_deref()
        .is_some_and(|bound| bound != app_type.as_str())
    {
        return Err(AppError::InvalidInput(
            "该转发站配置不适用于当前应用".to_string(),
        ));
    }
    ensure_key_for_route(state, app_type.as_str(), Some(route_id))
}

fn app_label(app_type: AppType) -> &'static str {
    match app_type {
        AppType::Claude => "Claude Code",
        AppType::ClaudeDesktop => "Claude Desktop",
        AppType::Codex => "Codex",
        AppType::OpenCode => "OpenCode",
        AppType::OpenClaw => "OpenClaw",
        AppType::Hermes => "Hermes",
    }
}

/// Client-facing model names declared on a route (mapping aliases + default),
/// used to seed model lists for clients that require one (OpenCode/OpenClaw/
/// Hermes pickers).
fn route_client_models(route: &GatewayRoute) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for rule in &route.model_rules {
        if !rule.model.trim().is_empty() && !models.contains(&rule.model) {
            models.push(rule.model.clone());
        }
    }
    if let Some(default) = &route.default_model {
        if !default.trim().is_empty() && !models.contains(default) {
            models.push(default.clone());
        }
    }
    models
}

fn gateway_settings_for(
    app_type: AppType,
    base_url: &str,
    key: &str,
    models: &[String],
) -> Result<serde_json::Value, AppError> {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => Ok(json!({
            "env": {
                "ANTHROPIC_BASE_URL": base_url,
                "ANTHROPIC_AUTH_TOKEN": key,
            }
        })),
        AppType::Codex => {
            let toml = format!(
                concat!(
                    "model_provider = \"{id}\"\n",
                    "disable_response_storage = true\n",
                    "\n",
                    "[model_providers.{id}]\n",
                    "name = \"{name}\"\n",
                    "base_url = \"{base}/v1\"\n",
                    "wire_api = \"responses\"\n",
                    "env_key = \"OPENAI_API_KEY\"\n",
                ),
                id = GATEWAY_PROVIDER_ID,
                name = GATEWAY_PROVIDER_NAME,
                base = base_url,
            );
            Ok(json!({
                "auth": { "OPENAI_API_KEY": key },
                "config": toml,
            }))
        }
        AppType::OpenCode => {
            let mut config = json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": GATEWAY_PROVIDER_NAME,
                "options": {
                    "baseURL": format!("{base_url}/v1"),
                    "apiKey": key,
                },
            });
            if !models.is_empty() {
                let mut map = serde_json::Map::new();
                for model in models {
                    map.insert(model.clone(), json!({ "name": model }));
                }
                config["models"] = serde_json::Value::Object(map);
            }
            Ok(config)
        }
        AppType::OpenClaw => Ok(json!({
            "baseUrl": format!("{base_url}/v1"),
            "apiKey": key,
            "api": "openai-completions",
            "models": models.iter().map(|m| json!({ "id": m })).collect::<Vec<_>>(),
        })),
        AppType::Hermes => {
            let mut config = json!({
                "name": GATEWAY_PROVIDER_ID,
                "base_url": format!("{base_url}/v1"),
                "api_key": key,
                "api_mode": "chat_completions",
                "models": models.iter().map(|m| json!({ "id": m })).collect::<Vec<_>>(),
            });
            if let Some(first) = models.first() {
                config["model"] = json!(first);
            }
            Ok(config)
        }
    }
}

/// Create/refresh the relay-station provider entry for `app_type` and switch to it.
///
/// `base_url` is the running gateway origin (e.g. `http://127.0.0.1:4180`).
pub fn apply_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
) -> Result<ApplyResult, AppError> {
    if !state
        .db
        .get_gateway_channels()?
        .iter()
        .any(|channel| channel.enabled)
    {
        return Err(AppError::InvalidInput(
            "请先添加并启用一个转发站".to_string(),
        ));
    }
    let mut station_routes: Vec<GatewayRoute> = state
        .db
        .get_gateway_routes()?
        .into_iter()
        .filter(|route| route.enabled && route.id.starts_with(STATION_ROUTE_PREFIX))
        .collect();
    let route = match station_routes.len() {
        0 => {
            return Err(AppError::InvalidInput(
                "请先添加并启用一个转发站".to_string(),
            ))
        }
        1 => station_routes.remove(0),
        // Refuse to pick one implicitly: which station wins would depend on DB
        // ordering, and a silent wrong pick is worse than asking the user.
        _ => {
            return Err(AppError::InvalidInput(
                "存在多个已启用的转发站，请在转发站页面选择要应用的一个".to_string(),
            ))
        }
    };
    apply_route_to_app(state, app_type, base_url, route)
}

/// Apply one user-facing relay-station config to a supported CLI.
pub fn apply_station_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
    station_route_id: &str,
) -> Result<ApplyResult, AppError> {
    let route = state
        .db
        .get_gateway_route_by_id(station_route_id)?
        .ok_or_else(|| AppError::InvalidInput("找不到转发站配置".to_string()))?;
    if !route.enabled {
        return Err(AppError::InvalidInput("该转发站配置已停用".to_string()));
    }
    if !route.id.starts_with(STATION_ROUTE_PREFIX) {
        return Err(AppError::InvalidInput(
            "选择的配置不是转发站配置".to_string(),
        ));
    }
    let channels = state.db.get_gateway_channels()?;
    let enabled_channels: Vec<&GatewayChannel> = channels
        .iter()
        .filter(|channel| channel.enabled && route.channel_ids.contains(&channel.id))
        .collect();
    if enabled_channels.is_empty() {
        return Err(AppError::InvalidInput(
            "转发站没有可用的服务地址".to_string(),
        ));
    }
    if !enabled_channels
        .iter()
        .any(|channel| dialect_compatible(channel.dialect, app_type))
    {
        return Err(AppError::InvalidInput(format!(
            "无法应用到 {}：「{}」是 OpenAI Chat 格式的转发站，暂不支持该应用",
            app_label(app_type),
            route.name
        )));
    }
    apply_route_to_app(state, app_type, base_url, route)
}

fn apply_route_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
    route: GatewayRoute,
) -> Result<ApplyResult, AppError> {
    let key = ensure_key_for_route(state, app_type.as_str(), Some(&route.id))?;
    let settings = gateway_settings_for(app_type, base_url, &key.key, &route_client_models(&route))?;

    let provider = Provider {
        id: GATEWAY_PROVIDER_ID.to_string(),
        name: GATEWAY_PROVIDER_NAME.to_string(),
        settings_config: settings,
        website_url: None,
        category: Some("gateway".to_string()),
        created_at: Some(chrono::Utc::now().timestamp()),
        sort_index: None,
        notes: Some("由 OcHub 转发站模式自动管理".to_string()),
        meta: None,
        icon: None,
        icon_color: None,
    };

    let existing = state
        .db
        .get_provider_by_id(GATEWAY_PROVIDER_ID, app_type.as_str())?;
    if existing.is_some() {
        ProviderService::update(state, app_type, Some(GATEWAY_PROVIDER_ID), provider)?;
    } else {
        ProviderService::add(state, app_type, provider, false)?;
    }
    ProviderService::switch(state, app_type, GATEWAY_PROVIDER_ID)?;

    Ok(ApplyResult {
        base_url: base_url.to_string(),
        key_name: key.name,
        key_secret: key.key,
        route_id: route.id,
        route_name: route.name,
        applied: true,
    })
}

/// Connection info for clients we don't manage (generic chat-dialect tools):
/// creates/reuses a key and returns the endpoint details for copy-paste.
pub fn generic_client_info(state: &AppState, base_url: &str) -> Result<ApplyResult, AppError> {
    let route = match state.db.get_gateway_route_by_id("route-generic-client")? {
        Some(route) if route.enabled => route,
        _ => {
            let route = GatewayRoute {
                id: "route-generic-client".to_string(),
                name: "通用客户端默认路由".to_string(),
                app_type: None,
                channel_ids: Vec::new(),
                default_model: None,
                model_rules: Vec::new(),
                reasoning: GatewayReasoningConfig::default(),
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            };
            state.db.upsert_gateway_route(&route)?;
            route
        }
    };
    let key = ensure_key_for_route(state, "generic-client", Some(&route.id))?;
    Ok(ApplyResult {
        base_url: format!("{base_url}/v1"),
        key_name: key.name,
        key_secret: key.key,
        route_id: route.id,
        route_name: route.name,
        applied: false,
    })
}

/// Import one existing direct connection as an upstream channel.
///
/// Official-login-only providers intentionally fail here because their client
/// session is not an API credential that the local gateway can reuse.
pub fn import_provider_as_channel(
    state: &AppState,
    app_type: AppType,
    provider_id: &str,
) -> Result<GatewayChannel, AppError> {
    let provider = state
        .db
        .get_provider_by_id(provider_id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput("找不到要导入的连接".to_string()))?;
    if provider.id == GATEWAY_PROVIDER_ID || provider.category.as_deref() == Some("gateway") {
        return Err(AppError::InvalidInput(
            "转发站模式不能再次导入为转发站".to_string(),
        ));
    }

    let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "该连接依赖应用官方登录，不能作为网关上游；请选择带 API Key 的连接".to_string(),
        ));
    }
    let dialect = match app_type {
        AppType::Claude | AppType::ClaudeDesktop => Dialect::Messages,
        AppType::Codex => Dialect::Responses,
        AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => Dialect::Chat,
    };
    let channel = GatewayChannel {
        id: format!("imported-{}-{}", app_type.as_str(), provider.id),
        name: provider.name,
        dialect,
        base_url: normalize_upstream_origin(&base_url),
        api_key,
        path_override: None,
        models: Vec::new(),
        model_override: None,
        priority: 0,
        weight: 1,
        enabled: true,
        extra_headers: Vec::new(),
    };
    state.db.upsert_gateway_channel(&channel)?;
    Ok(channel)
}

fn normalize_upstream_origin(base_url: &str) -> String {
    let mut value = base_url.trim().trim_end_matches('/').to_string();
    for suffix in [
        "/v1/messages",
        "/v1/chat/completions",
        "/v1/responses",
        "/v1",
    ] {
        if value.ends_with(suffix) {
            value.truncate(value.len() - suffix.len());
            break;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn settings_shapes_per_app() {
        let base = "http://127.0.0.1:4180";
        let models = vec!["claude-sonnet-4-6".to_string()];

        let claude = gateway_settings_for(AppType::Claude, base, "rd-k", &models).unwrap();
        assert_eq!(claude["env"]["ANTHROPIC_BASE_URL"], base);
        assert_eq!(claude["env"]["ANTHROPIC_AUTH_TOKEN"], "rd-k");

        let codex = gateway_settings_for(AppType::Codex, base, "rd-k", &models).unwrap();
        assert_eq!(codex["auth"]["OPENAI_API_KEY"], "rd-k");
        let toml = codex["config"].as_str().unwrap();
        assert!(toml.contains("model_provider = \"local-gateway\""));
        assert!(toml.contains("base_url = \"http://127.0.0.1:4180/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));

        let opencode = gateway_settings_for(AppType::OpenCode, base, "rd-k", &models).unwrap();
        assert_eq!(opencode["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(opencode["options"]["baseURL"], "http://127.0.0.1:4180/v1");
        assert_eq!(opencode["options"]["apiKey"], "rd-k");
        assert!(opencode["models"]["claude-sonnet-4-6"].is_object());

        let openclaw = gateway_settings_for(AppType::OpenClaw, base, "rd-k", &models).unwrap();
        assert_eq!(openclaw["api"], "openai-completions");
        assert_eq!(openclaw["baseUrl"], "http://127.0.0.1:4180/v1");
        assert_eq!(openclaw["models"][0]["id"], "claude-sonnet-4-6");

        let hermes = gateway_settings_for(AppType::Hermes, base, "rd-k", &models).unwrap();
        assert_eq!(hermes["api_mode"], "chat_completions");
        assert_eq!(hermes["base_url"], "http://127.0.0.1:4180/v1");
        assert_eq!(hermes["model"], "claude-sonnet-4-6");
        assert_eq!(hermes["models"][0]["id"], "claude-sonnet-4-6");

        // Without declared models, model-list fields stay absent/empty and the
        // Hermes default model is omitted.
        let opencode_bare = gateway_settings_for(AppType::OpenCode, base, "rd-k", &[]).unwrap();
        assert!(opencode_bare.get("models").is_none());
        let hermes_bare = gateway_settings_for(AppType::Hermes, base, "rd-k", &[]).unwrap();
        assert!(hermes_bare.get("model").is_none());
    }

    #[test]
    fn route_models_come_from_rules_then_default_without_duplicates() {
        let route = GatewayRoute {
            id: "station:x".into(),
            name: "x".into(),
            app_type: None,
            channel_ids: vec!["x".into()],
            default_model: Some("claude-sonnet-4-6".into()),
            model_rules: vec![
                crate::gateway::types::GatewayModelRule {
                    model: "claude-sonnet-4-6".into(),
                    upstream_model: "up-1".into(),
                    channel_id: None,
                },
                crate::gateway::types::GatewayModelRule {
                    model: "claude-haiku-4-5".into(),
                    upstream_model: "up-2".into(),
                    channel_id: None,
                },
            ],
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 0,
        };
        assert_eq!(
            route_client_models(&route),
            vec!["claude-sonnet-4-6".to_string(), "claude-haiku-4-5".to_string()]
        );
    }

    #[test]
    fn app_route_can_switch_without_rewriting_provider_config() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let default_route = ensure_app_route(&state, AppType::Claude).unwrap();
        assert_eq!(default_route.id, "route-claude");

        let alternate = GatewayRoute {
            id: "route-claude-fast".into(),
            name: "Claude 快速".into(),
            app_type: Some("claude".into()),
            channel_ids: Vec::new(),
            default_model: Some("claude-haiku-4-5".into()),
            model_rules: Vec::new(),
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 2,
        };
        state.db.upsert_gateway_route(&alternate).unwrap();
        activate_route_for_app(&state, AppType::Claude, &alternate.id).unwrap();

        let key = state
            .db
            .get_gateway_keys()
            .unwrap()
            .into_iter()
            .find(|key| key.name == "claude")
            .unwrap();
        assert_eq!(key.route_id.as_deref(), Some("route-claude-fast"));
        assert_eq!(
            ensure_app_route(&state, AppType::Claude).unwrap().id,
            "route-claude-fast"
        );
    }

    #[test]
    fn station_route_is_one_to_one_with_its_hidden_channel() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let channel = GatewayChannel {
            id: "new-api-primary".into(),
            name: "New API 主站".into(),
            dialect: Dialect::Chat,
            base_url: "https://relay.example.com".into(),
            api_key: "sk-test".into(),
            path_override: None,
            models: Vec::new(),
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: Vec::new(),
        };
        state.db.upsert_gateway_channel(&channel).unwrap();

        let route = ensure_station_route(&state, &channel).unwrap();

        assert_eq!(route.id, "station:new-api-primary");
        assert_eq!(route.name, "New API 主站");
        assert_eq!(route.channel_ids, vec!["new-api-primary"]);
        assert_eq!(route.app_type, None);
        assert!(route.enabled);
    }

    #[test]
    fn dialect_compatibility_mirrors_pipeline_matrix() {
        // With chat-upstream reverse conversion in place, every station dialect
        // serves every client; the guard stays wired for future restrictions.
        for app in [
            AppType::Claude,
            AppType::ClaudeDesktop,
            AppType::Codex,
            AppType::OpenCode,
            AppType::OpenClaw,
            AppType::Hermes,
        ] {
            assert!(dialect_compatible(Dialect::Messages, app));
            assert!(dialect_compatible(Dialect::Responses, app));
            assert!(dialect_compatible(Dialect::Chat, app));
        }
    }

    fn station_fixture(state: &AppState, id: &str, dialect: Dialect) -> GatewayRoute {
        let channel = GatewayChannel {
            id: id.into(),
            name: format!("站点 {id}"),
            dialect,
            base_url: "https://relay.example.com".into(),
            api_key: "sk-test".into(),
            path_override: None,
            models: Vec::new(),
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: Vec::new(),
        };
        state.db.upsert_gateway_channel(&channel).unwrap();
        ensure_station_route(state, &channel).unwrap()
    }

    #[test]
    fn implicit_apply_refuses_to_pick_between_multiple_stations() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        station_fixture(&state, "first", Dialect::Messages);
        station_fixture(&state, "second", Dialect::Messages);

        let err = apply_to_app(&state, AppType::Claude, "http://127.0.0.1:4180").unwrap_err();

        assert!(err.to_string().contains("多个已启用的转发站"));
    }

    #[test]
    fn applying_a_station_rejects_legacy_route_profiles() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = ensure_app_route(&state, AppType::Claude).unwrap();

        let err = apply_station_to_app(&state, AppType::Claude, "http://127.0.0.1:4180", &route.id)
            .unwrap_err();

        assert!(err.to_string().contains("不是转发站配置"));
    }

    #[test]
    fn imported_endpoint_is_normalized_to_origin() {
        assert_eq!(
            normalize_upstream_origin("https://example.com/v1/messages/"),
            "https://example.com"
        );
        assert_eq!(
            normalize_upstream_origin("https://example.com/openai/v1"),
            "https://example.com/openai"
        );
    }

    #[test]
    fn generic_client_reuses_key_and_keeps_a_route_binding() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let first = generic_client_info(&state, "http://127.0.0.1:4180").unwrap();
        let second = generic_client_info(&state, "http://127.0.0.1:4180").unwrap();

        assert_eq!(first.key_secret, second.key_secret);
        assert_eq!(first.route_id, "route-generic-client");
        assert_eq!(second.route_id, first.route_id);
        let key = state
            .db
            .get_gateway_keys()
            .unwrap()
            .into_iter()
            .find(|key| key.name == "generic-client")
            .unwrap();
        assert_eq!(key.route_id.as_deref(), Some("route-generic-client"));
    }
}
