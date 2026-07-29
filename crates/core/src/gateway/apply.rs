//! One-click app configuration for user-facing relay stations.
//!
//! For each supported app this creates (or refreshes) a "Local Gateway"
//! provider entry — reusing the normal provider machinery so the change is
//! visible, switchable, and revertible in the regular provider list — and then
//! switches the app to it. The local service and per-app keys are deliberately
//! hidden implementation details.

use serde::Serialize;
use serde_json::json;
use std::fmt::Write as _;

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::gateway::types::{
    Dialect, GatewayAppModelPolicy, GatewayChannel, GatewayKey, GatewayReasoningConfig,
    GatewayRoute,
};
use crate::model::{ClaudeDesktopModelRoute, Provider, ProviderMeta};
use crate::provider_config::{self, FormValues, Severity};
use crate::services::provider::ProviderService;

/// Fixed provider id for gateway entries (per app list).
pub const GATEWAY_PROVIDER_ID: &str = "local-gateway";
#[cfg(test)]
const GATEWAY_PROVIDER_NAME: &str = "OcHub 模型供应商模式";
pub const STATION_ROUTE_PREFIX: &str = "station:";

/// Stable provider id for one relay station. Hex encoding makes arbitrary
/// route ids safe in app config formats without introducing collision-prone
/// slug normalization.
pub fn gateway_provider_id(route_id: &str) -> String {
    let route_id = route_id
        .strip_prefix(STATION_ROUTE_PREFIX)
        .unwrap_or(route_id);
    let mut encoded = String::with_capacity(route_id.len() * 2);
    for byte in route_id.bytes() {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    format!("{GATEWAY_PROVIDER_ID}-{encoded}")
}

pub fn gateway_key_label(app_type: AppType, route_id: &str) -> String {
    format!("{}:{route_id}", app_type.as_str())
}

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
        AppType::GrokBuild,
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
        AppType::GrokBuild => Dialect::Responses,
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
        model_policy: None,
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
        model_policy: None,
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
    if let Some(route_id) = bound_route_id
        && let Some(route) = state
            .db
            .get_gateway_route_by_id(&route_id)?
            .filter(|route| route.enabled)
    {
        return Ok(route);
    }
    if let Some(route) = state.db.get_gateway_route_for_app(app_type.as_str())? {
        return Ok(route);
    }
    let route = GatewayRoute {
        id: format!("route-{}", app_type.as_str()),
        name: format!("{} 默认路由", app_label(app_type)),
        website_url: None,
        app_type: Some(app_type.as_str().to_string()),
        channel_ids: Vec::new(),
        default_model: None,
        model_rules: Vec::new(),
        reasoning: GatewayReasoningConfig::default(),
        websocket_enabled: false,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_route(&route)?;
    Ok(route)
}

pub fn station_route_id(channel_id: &str) -> String {
    format!("{STATION_ROUTE_PREFIX}{channel_id}")
}

/// Ensure the hidden local route backing one imported or legacy relay station.
/// The editor may later add more API-interface channels to the same route.
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
        website_url: None,
        app_type: None,
        channel_ids: vec![channel.id.clone()],
        default_model: None,
        model_rules: Vec::new(),
        reasoning: GatewayReasoningConfig::default(),
        websocket_enabled: false,
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
        .ok_or_else(|| AppError::InvalidInput("找不到模型供应商配置".to_string()))?;
    if !route.enabled {
        return Err(AppError::InvalidInput("该模型供应商配置已停用".to_string()));
    }
    if route
        .app_type
        .as_deref()
        .is_some_and(|bound| bound != app_type.as_str())
    {
        return Err(AppError::InvalidInput(
            "该模型供应商配置不适用于当前应用".to_string(),
        ));
    }
    ensure_key_for_route(state, app_type.as_str(), Some(route_id))
}

fn app_label(app_type: AppType) -> &'static str {
    match app_type {
        AppType::Claude => "Claude Code",
        AppType::ClaudeDesktop => "Claude Desktop",
        AppType::Codex => "Codex",
        AppType::GrokBuild => "Grok Build",
        AppType::OpenCode => "OpenCode",
        AppType::OpenClaw => "OpenClaw",
        AppType::Hermes => "Hermes",
    }
}

/// Client-facing model names declared on a route (mapping aliases + default),
/// used to seed model lists for clients that require one (OpenCode/OpenClaw/
/// Hermes pickers).
fn route_client_models(route: &GatewayRoute, channels: &[GatewayChannel]) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    let mapped_targets: std::collections::HashSet<&str> = route
        .model_rules
        .iter()
        .filter(|rule| !rule.model.contains('*'))
        .filter_map(|rule| rule.upstream_model_override())
        .collect();
    for rule in &route.model_rules {
        if !rule.model.trim().is_empty()
            && !rule.model.contains('*')
            && !models.contains(&rule.model)
        {
            models.push(rule.model.clone());
        }
    }
    if let Some(default) = &route.default_model
        && !default.trim().is_empty()
        && !models.contains(default)
    {
        models.push(default.clone());
    }
    for channel in channels
        .iter()
        .filter(|channel| channel.enabled && route.allows_channel(&channel.id))
    {
        for model in &channel.models {
            let model = model.trim();
            if model.is_empty()
                || model.contains('*')
                || mapped_targets.contains(model)
                || models.iter().any(|existing| existing == model)
            {
                continue;
            }
            models.push(model.to_string());
        }
    }
    models
}

/// Exact model ids advertised by one station across all of its enabled
/// channels. Wildcards remain routing constraints and are not useful in an
/// application picker.
pub fn station_models(route: &GatewayRoute, channels: &[GatewayChannel]) -> Vec<String> {
    let mut models = Vec::new();
    for channel in channels
        .iter()
        .filter(|channel| channel.enabled && route.allows_channel(&channel.id))
    {
        for model in &channel.models {
            let model = model.trim();
            if model.is_empty()
                || model.contains('*')
                || models.iter().any(|existing| existing == model)
            {
                continue;
            }
            models.push(model.to_string());
        }
    }
    models.sort();
    models
}

pub fn station_model_policy(
    state: &AppState,
    app_type: AppType,
    route: &GatewayRoute,
) -> Result<GatewayAppModelPolicy, AppError> {
    let label = gateway_key_label(app_type, &route.id);
    if let Some(policy) = state
        .db
        .get_gateway_keys()?
        .into_iter()
        .find(|key| key.name == label && key.enabled)
        .and_then(|key| key.model_policy)
    {
        return Ok(policy);
    }
    let channels = state.db.get_gateway_channels()?;
    Ok(GatewayAppModelPolicy {
        models: station_models(route, &channels),
        preferred_model: None,
        fallback_model: route.default_model.clone(),
        model_rules: route.model_rules.clone(),
    })
}

#[cfg(test)]
fn gateway_settings_for(
    app_type: AppType,
    base_url: &str,
    key: &str,
    models: &[String],
) -> Result<serde_json::Value, AppError> {
    let policy = GatewayAppModelPolicy {
        models: models.to_vec(),
        ..Default::default()
    };
    gateway_settings_for_provider(
        app_type,
        GATEWAY_PROVIDER_ID,
        GATEWAY_PROVIDER_NAME,
        base_url,
        key,
        &policy,
        false,
    )
}

fn gateway_settings_for_provider(
    app_type: AppType,
    provider_id: &str,
    provider_name: &str,
    base_url: &str,
    key: &str,
    policy: &GatewayAppModelPolicy,
    supports_websockets: bool,
) -> Result<serde_json::Value, AppError> {
    let models = policy.client_models();
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => {
            let mut env = serde_json::Map::from_iter([
                ("ANTHROPIC_BASE_URL".to_string(), json!(base_url)),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), json!(key)),
            ]);
            if let Some(model) = policy.preferred_model.as_deref() {
                env.insert("ANTHROPIC_MODEL".to_string(), json!(model));
            }
            for rule in policy
                .model_rules
                .iter()
                .filter(|rule| !rule.model.contains('*'))
            {
                let lowercase = rule.model.to_ascii_lowercase();
                let role = ["SONNET", "OPUS", "HAIKU", "FABLE"]
                    .into_iter()
                    .find(|role| lowercase.contains(&role.to_ascii_lowercase()));
                if let Some(role) = role {
                    env.insert(format!("ANTHROPIC_DEFAULT_{role}_MODEL"), json!(rule.model));
                    env.insert(
                        format!("ANTHROPIC_DEFAULT_{role}_MODEL_NAME"),
                        json!(rule.model),
                    );
                }
            }
            Ok(json!({ "env": env }))
        }
        AppType::Codex => {
            let mut document = toml_edit::DocumentMut::new();
            if let Some(model) = policy.preferred_model.as_deref() {
                document["model"] = toml_edit::value(model);
            }
            document["model_provider"] = toml_edit::value(provider_id);
            document["disable_response_storage"] = toml_edit::value(true);
            document["model_providers"] = toml_edit::table();
            document["model_providers"][provider_id] = toml_edit::table();
            document["model_providers"][provider_id]["name"] = toml_edit::value(provider_name);
            document["model_providers"][provider_id]["base_url"] =
                toml_edit::value(format!("{base_url}/v1"));
            document["model_providers"][provider_id]["wire_api"] = toml_edit::value("responses");
            if supports_websockets {
                document["model_providers"][provider_id]["supports_websockets"] =
                    toml_edit::value(true);
            }
            document["model_providers"][provider_id]["experimental_bearer_token"] =
                toml_edit::value(key);
            let toml = document.to_string();
            let mut config = json!({
                "auth": {},
                "config": toml,
            });
            if !models.is_empty() {
                config["modelCatalog"] = json!({
                    "models": models.iter().map(|model| json!({
                        "model": model,
                        "displayName": model,
                    })).collect::<Vec<_>>()
                });
            }
            Ok(config)
        }
        AppType::GrokBuild => {
            let profiles: Vec<&str> = if models.is_empty() {
                vec![crate::apps::grokbuild::DEFAULT_MODEL]
            } else {
                models.iter().map(String::as_str).collect()
            };
            let profile = profiles[0];
            let mut document = toml_edit::DocumentMut::new();
            document["models"] = toml_edit::table();
            document["model"] = toml_edit::table();
            document["models"]["default"] = toml_edit::value(profile);
            for profile in profiles {
                document["model"]
                    .as_table_mut()
                    .expect("Grok model registry is a TOML table")
                    .insert(profile, toml_edit::Item::Table(toml_edit::Table::new()));
                document["model"][profile]["model"] = toml_edit::value(profile);
                document["model"][profile]["base_url"] = toml_edit::value(format!("{base_url}/v1"));
                document["model"][profile]["name"] = toml_edit::value(provider_name);
                document["model"][profile]["api_key"] = toml_edit::value(key);
                document["model"][profile]["api_backend"] = toml_edit::value("responses");
                document["model"][profile]["context_window"] =
                    toml_edit::value(crate::apps::grokbuild::DEFAULT_CONTEXT_WINDOW);
            }
            Ok(json!({ "config": document.to_string() }))
        }
        AppType::OpenCode => {
            let mut config = json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": provider_name,
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
                "name": provider_id,
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
            "请先添加并启用一个模型供应商".to_string(),
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
                "请先添加并启用一个模型供应商".to_string(),
            ));
        }
        1 => station_routes.remove(0),
        // Refuse to pick one implicitly: which station wins would depend on DB
        // ordering, and a silent wrong pick is worse than asking the user.
        _ => {
            return Err(AppError::InvalidInput(
                "存在多个已启用的模型供应商，请在模型供应商页面选择要应用的一个".to_string(),
            ));
        }
    };
    let policy = station_model_policy(state, app_type, &route)?;
    apply_route_to_app(state, app_type, base_url, route, Some(policy))
}

fn station_route_for_apply(
    state: &AppState,
    app_type: AppType,
    station_route_id: &str,
) -> Result<GatewayRoute, AppError> {
    let route = state
        .db
        .get_gateway_route_by_id(station_route_id)?
        .ok_or_else(|| AppError::InvalidInput("找不到模型供应商配置".to_string()))?;
    if !route.enabled {
        return Err(AppError::InvalidInput("该模型供应商配置已停用".to_string()));
    }
    if !route.id.starts_with(STATION_ROUTE_PREFIX) {
        return Err(AppError::InvalidInput(
            "选择的配置不是模型供应商配置".to_string(),
        ));
    }
    let channels = state.db.get_gateway_channels()?;
    let enabled_channels: Vec<&GatewayChannel> = channels
        .iter()
        .filter(|channel| channel.enabled && route.channel_ids.contains(&channel.id))
        .collect();
    if enabled_channels.is_empty() {
        return Err(AppError::InvalidInput(
            "模型供应商没有可用的服务地址".to_string(),
        ));
    }
    if !enabled_channels
        .iter()
        .any(|channel| dialect_compatible(channel.dialect, app_type))
    {
        return Err(AppError::InvalidInput(format!(
            "无法应用到 {}：「{}」是 OpenAI Chat 格式的模型供应商，暂不支持该应用",
            app_label(app_type),
            route.name
        )));
    }
    Ok(route)
}

/// Apply one station using its saved per-application policy, or initialize a
/// policy from the station's legacy route-level settings.
pub fn apply_station_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
    station_route_id: &str,
) -> Result<ApplyResult, AppError> {
    let route = station_route_for_apply(state, app_type, station_route_id)?;
    let policy = station_model_policy(state, app_type, &route)?;
    apply_route_to_app(state, app_type, base_url, route, Some(policy))
}

/// Apply one station and persist a model policy isolated to this application.
pub fn apply_station_to_app_with_policy(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
    station_route_id: &str,
    policy: GatewayAppModelPolicy,
) -> Result<ApplyResult, AppError> {
    policy.validate().map_err(AppError::InvalidInput)?;
    let route = station_route_for_apply(state, app_type, station_route_id)?;
    apply_route_to_app(state, app_type, base_url, route, Some(policy))
}

// ---- Station-sourced channels (provider editor "relay station" source) -------

/// One relay station the provider editor can offer as a channel source.
#[derive(Debug, Clone, Serialize)]
pub struct StationChannelOption {
    pub route_id: String,
    pub name: String,
    /// Exact upstream model ids the station declares, for model pickers.
    pub models: Vec<String>,
}

/// Relay stations able to serve `app_type`, for the channel editor's station
/// picker. Disabled, empty, or dialect-incompatible stations are skipped —
/// they would fail at save time anyway, so offering them is just noise.
pub fn station_channel_options(
    state: &AppState,
    app_type: AppType,
) -> Result<Vec<StationChannelOption>, AppError> {
    let channels = state.db.get_gateway_channels()?;
    let mut options = Vec::new();
    for route in state.db.get_gateway_routes()? {
        if !route.enabled || !route.id.starts_with(STATION_ROUTE_PREFIX) {
            continue;
        }
        let enabled_channels: Vec<&GatewayChannel> = channels
            .iter()
            .filter(|channel| channel.enabled && route.channel_ids.contains(&channel.id))
            .collect();
        if enabled_channels.is_empty()
            || !enabled_channels
                .iter()
                .any(|channel| dialect_compatible(channel.dialect, app_type))
        {
            continue;
        }
        options.push(StationChannelOption {
            route_id: route.id.clone(),
            name: route.name.clone(),
            models: station_models(&route, &channels),
        });
    }
    options.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.route_id.cmp(&b.route_id))
    });
    Ok(options)
}

/// Identity fields for a station-sourced channel, straight from the editor.
#[derive(Debug, Clone)]
pub struct StationChannelIdentity {
    pub id: String,
    pub name: String,
    pub website_url: Option<String>,
    pub category: Option<String>,
    pub notes: Option<String>,
}

/// Build (without persisting) a provider that talks to the local gateway and
/// is routed into `station_route_id`. `values` carries the user's model
/// fields; every station-managed field is overwritten with the gateway
/// endpoint + shared station key, so a station channel never stores upstream
/// credentials of its own. `prior`/`prior_meta` keep unknown keys alive across
/// edits, exactly like a manual channel save.
#[allow(clippy::too_many_arguments)]
pub fn build_station_channel(
    state: &AppState,
    app_type: AppType,
    station_route_id: &str,
    values: &FormValues,
    identity: StationChannelIdentity,
    base_url: &str,
    prior: &serde_json::Value,
    prior_meta: Option<&ProviderMeta>,
) -> Result<Provider, AppError> {
    let route = station_route_for_apply(state, app_type, station_route_id)?;
    let key = ensure_key_for_route(
        state,
        &gateway_key_label(app_type, &route.id),
        Some(&route.id),
    )?;
    let codec = provider_config::config_for(app_type).ok_or_else(|| {
        AppError::InvalidInput(format!("{} 暂不支持模型供应商渠道", app_label(app_type)))
    })?;
    let mut merged = values.clone();
    provider_config::inject_station_endpoint(
        &mut merged,
        app_type,
        base_url,
        &key.key,
        &identity.id,
        &identity.name,
        route.websocket_enabled,
    );
    validate_station_channel_models(app_type, &merged)?;
    if let Some(issue) = codec
        .validate_for_category(&merged, identity.category.as_deref())
        .into_iter()
        .find(|issue| issue.severity == Severity::Error)
    {
        return Err(AppError::InvalidInput(issue.message));
    }
    let encoded = codec.encode(&merged, prior, prior_meta);
    let mut settings = encoded.settings_config;
    if app_type == AppType::Codex {
        inject_codex_model_catalog(
            state,
            &route,
            &mut settings,
            provider_config::str_val(&merged, "model"),
        )?;
    }
    let mut meta = encoded.meta.unwrap_or_default();
    meta.gateway_route_id = Some(route.id.clone());
    Ok(Provider {
        id: identity.id,
        name: identity.name,
        settings_config: settings,
        website_url: identity.website_url,
        category: identity.category,
        created_at: Some(chrono::Utc::now().timestamp()),
        sort_index: None,
        notes: identity.notes,
        meta: Some(meta),
        icon: None,
        icon_color: None,
    })
}

/// Re-embed the current gateway origin + shared key into a station channel's
/// stored settings (e.g. after the listen port changed), returning the
/// updated `(settings_config, meta)` for the caller to persist.
pub fn refresh_station_channel_settings(
    state: &AppState,
    app_type: AppType,
    provider: &Provider,
    base_url: &str,
) -> Result<(serde_json::Value, Option<ProviderMeta>), AppError> {
    let route_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.gateway_route_id.as_deref())
        .ok_or_else(|| AppError::InvalidInput("该渠道不关联模型供应商".to_string()))?;
    let route = station_route_for_apply(state, app_type, route_id)?;
    let key = ensure_key_for_route(
        state,
        &gateway_key_label(app_type, &route.id),
        Some(&route.id),
    )?;
    let codec = provider_config::config_for(app_type).ok_or_else(|| {
        AppError::InvalidInput(format!("{} 暂不支持模型供应商渠道", app_label(app_type)))
    })?;
    let mut values = codec.decode(&provider.settings_config, provider.meta.as_ref());
    provider_config::inject_station_endpoint(
        &mut values,
        app_type,
        base_url,
        &key.key,
        &provider.id,
        &provider.name,
        route.websocket_enabled,
    );
    let encoded = codec.encode(&values, &provider.settings_config, provider.meta.as_ref());
    let mut settings = encoded.settings_config;
    if app_type == AppType::Codex {
        inject_codex_model_catalog(
            state,
            &route,
            &mut settings,
            provider_config::str_val(&values, "model"),
        )?;
    }
    let mut meta = encoded.meta.unwrap_or_default();
    meta.gateway_route_id = Some(route.id.clone());
    Ok((settings, Some(meta)))
}

/// A station channel must pin at least one model: Codex needs its single
/// `model`, Claude needs the default model or one role filled in.
fn validate_station_channel_models(app_type: AppType, values: &FormValues) -> Result<(), AppError> {
    match app_type {
        AppType::Codex => {
            if provider_config::str_val(values, "model").trim().is_empty() {
                return Err(AppError::InvalidInput("请选择要使用的模型。".to_string()));
            }
        }
        AppType::Claude => {
            let default_model = provider_config::str_val(values, "model").trim();
            let any_role = values
                .get("roles")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row.get("model")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|model| !model.trim().is_empty())
                    })
                });
            if default_model.is_empty() && !any_role {
                return Err(AppError::InvalidInput(
                    "请至少为默认模型或一个角色选择模型。".to_string(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Codex learns its model picker from `modelCatalog`; the codec only preserves
/// that key, so station channels populate it with the station's declared
/// models plus the one the user actually picked.
fn inject_codex_model_catalog(
    state: &AppState,
    route: &GatewayRoute,
    settings: &mut serde_json::Value,
    picked_model: &str,
) -> Result<(), AppError> {
    let channels = state.db.get_gateway_channels()?;
    let mut catalog = station_models(route, &channels);
    let picked = picked_model.trim();
    if !picked.is_empty() && !catalog.iter().any(|model| model == picked) {
        catalog.push(picked.to_string());
    }
    if catalog.is_empty() {
        return Ok(());
    }
    catalog.sort();
    catalog.dedup();
    settings["modelCatalog"] = json!({
        "models": catalog.iter().map(|model| json!({
            "model": model,
            "displayName": model,
        })).collect::<Vec<_>>()
    });
    Ok(())
}

fn apply_route_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
    route: GatewayRoute,
    model_policy: Option<GatewayAppModelPolicy>,
) -> Result<ApplyResult, AppError> {
    let provider_id = gateway_provider_id(&route.id);
    let provider_name = format!("OcHub · {}", route.name);
    let key_label = gateway_key_label(app_type, &route.id);
    let mut key = ensure_key_for_route(state, &key_label, Some(&route.id))?;
    if key.model_policy != model_policy {
        key.model_policy = model_policy.clone();
        state.db.upsert_gateway_key(&key)?;
    }
    let channels = state.db.get_gateway_channels()?;
    let config_policy = model_policy.unwrap_or_else(|| GatewayAppModelPolicy {
        models: route_client_models(&route, &channels),
        preferred_model: None,
        fallback_model: route.default_model.clone(),
        model_rules: route.model_rules.clone(),
    });
    let client_models = config_policy.client_models();
    let settings = gateway_settings_for_provider(
        app_type,
        &provider_id,
        &provider_name,
        base_url,
        &key.key,
        &config_policy,
        route.websocket_enabled,
    )?;
    let mut meta = ProviderMeta {
        gateway_route_id: Some(route.id.clone()),
        ..Default::default()
    };
    if app_type == AppType::ClaudeDesktop {
        meta.claude_desktop_model_routes = client_models
            .iter()
            .filter(|model| crate::apps::claude_desktop::is_claude_safe_model_id(model))
            .map(|model| {
                (
                    model.clone(),
                    ClaudeDesktopModelRoute {
                        model: model.clone(),
                        label_override: Some(model.clone()),
                        supports_1m: None,
                    },
                )
            })
            .collect();
    }

    let provider = Provider {
        id: provider_id.clone(),
        name: provider_name,
        settings_config: settings,
        website_url: route.website_url.clone(),
        category: Some("gateway".to_string()),
        created_at: Some(chrono::Utc::now().timestamp()),
        sort_index: None,
        notes: Some("由 OcHub 模型供应商模式自动管理".to_string()),
        meta: Some(meta),
        icon: None,
        icon_color: None,
    };

    let existing = state
        .db
        .get_provider_by_id(&provider_id, app_type.as_str())?;
    if existing.is_some() {
        ProviderService::update(state, app_type, Some(&provider_id), provider)?;
    } else {
        ProviderService::add(state, app_type, provider, false)?;
    }
    ProviderService::switch(state, app_type, &provider_id)?;

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
                website_url: None,
                app_type: None,
                channel_ids: Vec::new(),
                default_model: None,
                model_rules: Vec::new(),
                reasoning: GatewayReasoningConfig::default(),
                websocket_enabled: false,
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
            "模型供应商模式不能再次导入为模型供应商".to_string(),
        ));
    }
    if provider
        .meta
        .as_ref()
        .is_some_and(|meta| meta.gateway_route_id.is_some())
    {
        return Err(AppError::InvalidInput(
            "模型供应商渠道指向本地网关，不能导入为上游".to_string(),
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
        AppType::GrokBuild => provider
            .settings_config
            .get("config")
            .and_then(serde_json::Value::as_str)
            .and_then(crate::apps::grokbuild::extract_model_config)
            .map(|config| match config.api_backend.as_str() {
                "messages" => Dialect::Messages,
                "responses" => Dialect::Responses,
                _ => Dialect::Chat,
            })
            .unwrap_or(Dialect::Responses),
        AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => Dialect::Chat,
    };
    let channel = GatewayChannel {
        id: format!("imported-{}-{}", app_type.as_str(), provider.id),
        endpoint_id: Some(format!("imported-{}-{}", app_type.as_str(), provider.id)),
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
        imported_from: Some(format!("{}:{}", app_type.as_str(), provider.id)),
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
    use crate::gateway::types::GatewayModelRule;
    use std::sync::Arc;

    #[test]
    fn settings_shapes_per_app() {
        let base = "http://127.0.0.1:4180";
        let models = vec!["claude-sonnet-4-6".to_string()];

        let claude = gateway_settings_for(AppType::Claude, base, "rd-k", &models).unwrap();
        assert_eq!(claude["env"]["ANTHROPIC_BASE_URL"], base);
        assert_eq!(claude["env"]["ANTHROPIC_AUTH_TOKEN"], "rd-k");

        let codex = gateway_settings_for(AppType::Codex, base, "rd-k", &models).unwrap();
        assert_eq!(codex["auth"], json!({}));
        let toml = codex["config"].as_str().unwrap();
        assert!(toml.contains("model_provider = \"local-gateway\""));
        assert!(toml.contains("base_url = \"http://127.0.0.1:4180/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(!toml.contains("supports_websockets"));
        assert!(toml.contains("experimental_bearer_token = \"rd-k\""));
        assert!(!toml.contains("env_key"));
        assert_eq!(
            codex["modelCatalog"]["models"][0]["model"],
            "claude-sonnet-4-6"
        );

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
    fn app_policy_sets_active_models_and_claude_aliases() {
        let policy = GatewayAppModelPolicy {
            models: vec!["grok-4.5".into(), "gpt-5.6".into()],
            preferred_model: Some("gpt-5.6".into()),
            fallback_model: None,
            model_rules: vec![GatewayModelRule {
                model: "claude-opus-5".into(),
                upstream_model: "grok-4.5".into(),
                channel_id: None,
                dialect: None,
            }],
        };

        let codex = gateway_settings_for_provider(
            AppType::Codex,
            "relay",
            "Relay",
            "http://127.0.0.1:4180",
            "rd-k",
            &policy,
            true,
        )
        .unwrap();
        let codex_toml = codex["config"].as_str().unwrap();
        assert!(codex_toml.contains("model = \"gpt-5.6\""));
        assert!(codex_toml.contains("supports_websockets = true"));
        assert_eq!(codex["modelCatalog"]["models"][0]["model"], "gpt-5.6");
        assert!(
            codex["modelCatalog"]["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["model"] == "claude-opus-5")
        );

        let claude = gateway_settings_for_provider(
            AppType::Claude,
            "relay",
            "Relay",
            "http://127.0.0.1:4180",
            "rd-k",
            &policy,
            false,
        )
        .unwrap();
        assert_eq!(claude["env"]["ANTHROPIC_MODEL"], "gpt-5.6");
        assert_eq!(
            claude["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "claude-opus-5"
        );
        assert_eq!(
            claude["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"],
            "claude-opus-5"
        );
    }

    #[test]
    fn station_policy_is_isolated_by_application_key() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = station_fixture(&state, "shared", Dialect::Responses);
        let claude_policy = GatewayAppModelPolicy {
            models: vec!["grok-4.5".into()],
            model_rules: vec![GatewayModelRule {
                model: "claude-opus-5".into(),
                upstream_model: "grok-4.5".into(),
                channel_id: None,
                dialect: None,
            }],
            ..Default::default()
        };
        let mut claude_key = ensure_key_for_route(
            &state,
            &gateway_key_label(AppType::Claude, &route.id),
            Some(&route.id),
        )
        .unwrap();
        claude_key.model_policy = Some(claude_policy.clone());
        state.db.upsert_gateway_key(&claude_key).unwrap();

        assert_eq!(
            station_model_policy(&state, AppType::Claude, &route).unwrap(),
            claude_policy
        );
        let codex_policy = station_model_policy(&state, AppType::Codex, &route).unwrap();
        assert!(codex_policy.model_rules.is_empty());
        assert_ne!(codex_policy, claude_policy);
    }

    #[test]
    fn route_models_skip_wildcards_and_deduplicate_the_default() {
        let route = GatewayRoute {
            id: "station:x".into(),
            name: "x".into(),
            website_url: None,
            app_type: None,
            channel_ids: vec!["x".into()],
            default_model: Some("claude-sonnet-4-6".into()),
            model_rules: vec![
                crate::gateway::types::GatewayModelRule {
                    model: "claude-sonnet-4-6".into(),
                    upstream_model: "up-1".into(),
                    channel_id: None,
                    dialect: None,
                },
                crate::gateway::types::GatewayModelRule {
                    model: "claude-haiku-4-5".into(),
                    upstream_model: "up-2".into(),
                    channel_id: None,
                    dialect: None,
                },
                crate::gateway::types::GatewayModelRule {
                    model: "claude-*".into(),
                    upstream_model: String::new(),
                    channel_id: Some("messages".into()),
                    dialect: Some(Dialect::Messages),
                },
            ],
            reasoning: GatewayReasoningConfig::default(),
            websocket_enabled: false,
            enabled: true,
            created_at: 0,
        };
        let channel = GatewayChannel {
            id: "x".into(),
            endpoint_id: Some("endpoint-x".into()),
            name: "x".into(),
            dialect: Dialect::Messages,
            base_url: "https://example.com".into(),
            api_key: String::new(),
            path_override: None,
            models: vec!["up-1".into(), "direct-model".into()],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: Vec::new(),
            imported_from: None,
        };
        assert_eq!(
            route_client_models(&route, &[channel]),
            vec![
                "claude-sonnet-4-6".to_string(),
                "claude-haiku-4-5".to_string(),
                "direct-model".to_string()
            ]
        );
    }

    #[test]
    fn each_station_gets_a_distinct_provider_and_key_identity() {
        let first = gateway_provider_id("station:first");
        let second = gateway_provider_id("station:second");
        assert_ne!(first, second);
        assert!(first.starts_with("local-gateway-"));
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        );
        assert_ne!(
            gateway_key_label(AppType::Claude, "station:first"),
            gateway_key_label(AppType::Claude, "station:second")
        );
        assert_ne!(
            gateway_key_label(AppType::Claude, "station:first"),
            gateway_key_label(AppType::Codex, "station:first")
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
            website_url: None,
            app_type: Some("claude".into()),
            channel_ids: Vec::new(),
            default_model: Some("claude-haiku-4-5".into()),
            model_rules: Vec::new(),
            reasoning: GatewayReasoningConfig::default(),
            websocket_enabled: false,
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
    fn imported_station_route_starts_with_its_single_channel() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let channel = GatewayChannel {
            id: "new-api-primary".into(),
            endpoint_id: Some("primary".into()),
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
            imported_from: None,
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
            endpoint_id: Some(format!("endpoint-{id}")),
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
            imported_from: None,
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

        assert!(err.to_string().contains("多个已启用的模型供应商"));
    }

    #[test]
    fn applying_a_station_rejects_legacy_route_profiles() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = ensure_app_route(&state, AppType::Claude).unwrap();

        let err = apply_station_to_app(&state, AppType::Claude, "http://127.0.0.1:4180", &route.id)
            .unwrap_err();

        assert!(err.to_string().contains("不是模型供应商配置"));
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

    fn modeled_station_fixture(state: &AppState, id: &str, dialect: Dialect) -> GatewayRoute {
        let route = station_fixture(state, id, dialect);
        let mut channel = state
            .db
            .get_gateway_channels()
            .unwrap()
            .into_iter()
            .find(|channel| channel.id == id)
            .unwrap();
        channel.models = vec!["claude-sonnet-4-6".into(), "gpt-5.5".into()];
        state.db.upsert_gateway_channel(&channel).unwrap();
        route
    }

    fn identity(id: &str) -> StationChannelIdentity {
        StationChannelIdentity {
            id: id.into(),
            name: format!("渠道 {id}"),
            website_url: None,
            category: None,
            notes: None,
        }
    }

    #[test]
    fn station_channel_options_expose_station_models() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = modeled_station_fixture(&state, "alpha", Dialect::Messages);

        let options = station_channel_options(&state, AppType::Claude).unwrap();

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].route_id, route.id);
        assert_eq!(
            options[0].models,
            vec!["claude-sonnet-4-6".to_string(), "gpt-5.5".to_string()]
        );
    }

    #[test]
    fn station_channel_options_skip_disabled_stations() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let mut route = modeled_station_fixture(&state, "alpha", Dialect::Messages);
        route.enabled = false;
        state.db.upsert_gateway_route(&route).unwrap();

        assert!(
            station_channel_options(&state, AppType::Claude)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn build_station_channel_claude_points_at_gateway_with_role_models() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = modeled_station_fixture(&state, "alpha", Dialect::Messages);
        let mut values = FormValues::new();
        provider_config::set_str(&mut values, "model", "claude-sonnet-4-6");
        values.insert(
            "roles".into(),
            json!([
                {"role": "sonnet", "model": "claude-sonnet-4-6", "name": "Sonnet", "one_m": false},
                {"role": "opus", "model": "gpt-5.5", "name": "", "one_m": false},
                {"role": "haiku", "model": "", "name": "", "one_m": false},
                {"role": "fable", "model": "", "name": "", "one_m": false},
            ]),
        );

        let provider = build_station_channel(
            &state,
            AppType::Claude,
            &route.id,
            &values,
            identity("claude-main"),
            "http://127.0.0.1:4180",
            &serde_json::Value::Null,
            None,
        )
        .unwrap();

        let env = &provider.settings_config["env"];
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4180");
        let key = env["ANTHROPIC_AUTH_TOKEN"].as_str().unwrap();
        assert!(key.starts_with("rd-"), "expected a gateway key, got {key}");
        assert!(env.get("ANTHROPIC_API_KEY").is_none());
        assert_eq!(env["ANTHROPIC_MODEL"], "claude-sonnet-4-6");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-4-6");
        assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "gpt-5.5");
        assert!(env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").is_none());
        assert_eq!(
            provider.meta.as_ref().unwrap().gateway_route_id.as_deref(),
            Some(route.id.as_str())
        );
        // The shared per-(app, station) key exists and is bound to the route.
        let stored = state
            .db
            .get_gateway_keys()
            .unwrap()
            .into_iter()
            .find(|k| k.name == gateway_key_label(AppType::Claude, &route.id))
            .unwrap();
        assert_eq!(stored.route_id.as_deref(), Some(route.id.as_str()));
        assert_eq!(stored.key, key);
        // A normal channel: not flagged as the auto-managed gateway entry.
        assert!(!provider.is_local_gateway());
    }

    #[test]
    fn build_station_channel_codex_writes_toml_and_model_catalog() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let mut route = modeled_station_fixture(&state, "alpha", Dialect::Responses);
        route.websocket_enabled = true;
        state.db.upsert_gateway_route(&route).unwrap();
        let mut values = FormValues::new();
        provider_config::set_str(&mut values, "model", "gpt-5.5");
        provider_config::set_str(&mut values, "reasoning_effort", "high");

        let provider = build_station_channel(
            &state,
            AppType::Codex,
            &route.id,
            &values,
            identity("Codex 主力"),
            "http://127.0.0.1:4180",
            &serde_json::Value::Null,
            None,
        )
        .unwrap();

        let toml = provider.settings_config["config"].as_str().unwrap();
        assert!(toml.contains("model = \"gpt-5.5\""));
        assert!(toml.contains("model_reasoning_effort = \"high\""));
        assert!(toml.contains("model_provider = \"codex\""));
        assert!(toml.contains("base_url = \"http://127.0.0.1:4180/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("supports_websockets = true"));
        assert!(toml.contains("experimental_bearer_token = \"rd-"));
        assert!(toml.contains("disable_response_storage = true"));
        let catalog = provider.settings_config["modelCatalog"]["models"]
            .as_array()
            .unwrap();
        assert!(catalog.iter().any(|m| m["model"] == "gpt-5.5"));
        assert!(catalog.iter().any(|m| m["model"] == "claude-sonnet-4-6"));
        assert_eq!(
            provider.meta.as_ref().unwrap().gateway_route_id.as_deref(),
            Some(route.id.as_str())
        );
    }

    #[test]
    fn build_station_channel_requires_a_model() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = modeled_station_fixture(&state, "alpha", Dialect::Responses);

        let err = build_station_channel(
            &state,
            AppType::Codex,
            &route.id,
            &FormValues::new(),
            identity("codex-x"),
            "http://127.0.0.1:4180",
            &serde_json::Value::Null,
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("模型"));
    }

    #[test]
    fn build_station_channel_rejects_unknown_station() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));

        let err = build_station_channel(
            &state,
            AppType::Claude,
            "station:missing",
            &FormValues::new(),
            identity("claude-x"),
            "http://127.0.0.1:4180",
            &serde_json::Value::Null,
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("找不到模型供应商配置"));
    }

    #[test]
    fn refresh_station_channel_settings_rewrites_origin_and_keeps_models() {
        let state = AppState::new(Arc::new(crate::db::Database::memory().unwrap()));
        let route = modeled_station_fixture(&state, "alpha", Dialect::Messages);
        let mut values = FormValues::new();
        provider_config::set_str(&mut values, "model", "claude-sonnet-4-6");
        let provider = build_station_channel(
            &state,
            AppType::Claude,
            &route.id,
            &values,
            identity("claude-main"),
            "http://127.0.0.1:4180",
            &serde_json::Value::Null,
            None,
        )
        .unwrap();

        let (settings, meta) = refresh_station_channel_settings(
            &state,
            AppType::Claude,
            &provider,
            "http://127.0.0.1:5000",
        )
        .unwrap();

        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:5000"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "claude-sonnet-4-6");
        assert_eq!(
            meta.unwrap().gateway_route_id.as_deref(),
            Some(route.id.as_str())
        );
    }
}
