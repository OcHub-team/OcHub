use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::gateway::apply;
use crate::gateway::types::GatewayAppModelPolicy;
use crate::gateway::{
    ChannelHealth, Dialect, GatewayChannel, GatewayConfig, GatewayEndpointTestResult, GatewayKey,
    GatewayModelRule, GatewayReasoningConfig, GatewayRoute, GatewayStatus,
};
use crate::services::provider::{DriftResolution, ProviderService};
use crate::{AppId, AppType};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStation {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub website_url: Option<String>,
    #[serde(default)]
    pub channels: Vec<GatewayChannel>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub model_rules: Vec<GatewayModelRule>,
    #[serde(default)]
    pub reasoning: GatewayReasoningConfig,
    #[serde(default)]
    pub websocket_enabled: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: i64,
}

fn default_true() -> bool {
    true
}

impl Application {
    pub async fn gateway_status(&self) -> ApplicationResult<Value> {
        let status = self.state.gateway.status().await;
        let config = self.state.db.get_gateway_config()?;
        let channels = self.state.db.get_gateway_channels()?;
        let routes = self.state.db.get_gateway_routes()?;
        let health = self.state.gateway.health_snapshot().await;
        let healthy = health
            .values()
            .filter(|item| matches!(item, ChannelHealth::Healthy))
            .count();
        let unhealthy = health
            .values()
            .filter(|item| matches!(item, ChannelHealth::Unhealthy(_)))
            .count();
        Ok(json!({
            "running": status.running,
            "port": status.port,
            "baseUrl": status.base_url,
            "listenHost": "127.0.0.1",
            "requireKey": config.require_key,
            "enabledChannels": channels.iter().filter(|item| item.enabled).count(),
            "channels": channels.len(),
            "enabledRoutes": routes.iter().filter(|item| item.enabled).count(),
            "routes": routes.len(),
            "health": {
                "healthy": healthy,
                "unhealthy": unhealthy,
                "unknown": channels.len().saturating_sub(healthy + unhealthy)
            }
        }))
    }

    pub fn gateway_config(&self) -> ApplicationResult<GatewayConfig> {
        Ok(self.state.db.get_gateway_config()?)
    }

    pub async fn set_gateway_config(
        &self,
        config: GatewayConfig,
    ) -> ApplicationResult<GatewayConfig> {
        if config.port == 0 {
            return Err(ApplicationError::InvalidInput(
                "gateway port must be between 1 and 65535".to_string(),
            ));
        }
        self.state.db.set_gateway_config(&config)?;
        self.state.gateway.reload_config().await?;
        Ok(config)
    }

    pub async fn start_gateway(&self) -> ApplicationResult<GatewayStatus> {
        Ok(self.state.gateway.start().await?)
    }

    pub async fn stop_gateway(&self) -> ApplicationResult<()> {
        self.state.gateway.stop().await?;
        Ok(())
    }

    pub async fn gateway_health(&self) -> ApplicationResult<HashMap<String, ChannelHealth>> {
        Ok(self.state.gateway.health_snapshot().await)
    }

    pub fn gateway_models(&self) -> ApplicationResult<Vec<String>> {
        let mut models = self
            .state
            .db
            .get_gateway_channels()?
            .into_iter()
            .filter(|channel| channel.enabled)
            .flat_map(|channel| channel.models)
            .filter(|model| !model.trim().is_empty() && !model.contains('*'))
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        Ok(models)
    }

    pub fn gateway_supported_apps(&self) -> Vec<String> {
        apply::supported_apps()
            .iter()
            .map(|app| app.as_str().to_string())
            .collect()
    }

    pub async fn probe_gateway_dialects(
        &self,
        base_url: &str,
        api_key: &str,
    ) -> ApplicationResult<Vec<Dialect>> {
        validate_http_url(base_url)?;
        Ok(self
            .state
            .gateway
            .detect_dialects(base_url.to_string(), api_key.to_string())
            .await?)
    }

    pub fn list_gateway_channels(&self) -> ApplicationResult<Vec<GatewayChannel>> {
        Ok(self.state.db.get_gateway_channels()?)
    }

    pub fn get_gateway_channel(&self, id: &str) -> ApplicationResult<GatewayChannel> {
        self.state
            .db
            .get_gateway_channels()?
            .into_iter()
            .find(|channel| channel.id == id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "gateway-channel",
                id: id.to_string(),
            })
    }

    pub fn save_gateway_channel(
        &self,
        mut channel: GatewayChannel,
    ) -> ApplicationResult<GatewayChannel> {
        if channel.id.trim().is_empty() {
            channel.id = uuid::Uuid::new_v4().to_string();
        }
        validate_channel(&channel)?;
        self.state.db.upsert_gateway_channel(&channel)?;
        Ok(channel)
    }

    pub fn set_gateway_channel_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> ApplicationResult<GatewayChannel> {
        let mut channel = self.get_gateway_channel(id)?;
        channel.enabled = enabled;
        self.state.db.upsert_gateway_channel(&channel)?;
        Ok(channel)
    }

    pub fn delete_gateway_channel(&self, id: &str) -> ApplicationResult<()> {
        self.get_gateway_channel(id)?;
        self.state.db.delete_gateway_channel(id)?;
        Ok(())
    }

    pub async fn probe_gateway_channels(
        &self,
    ) -> ApplicationResult<HashMap<String, ChannelHealth>> {
        self.state.gateway.probe_now().await?;
        Ok(self.state.gateway.health_snapshot().await)
    }

    pub async fn probe_gateway_channel(
        &self,
        id: &str,
    ) -> ApplicationResult<GatewayEndpointTestResult> {
        let channel = self.get_gateway_channel(id)?;
        Ok(self
            .state
            .gateway
            .test_endpoint(channel.base_url, channel.api_key)
            .await?)
    }

    pub async fn gateway_channel_models(&self, id: &str) -> ApplicationResult<Vec<String>> {
        let channel = self.get_gateway_channel(id)?;
        Ok(self
            .state
            .gateway
            .fetch_models(channel.base_url, channel.api_key)
            .await?)
    }

    pub fn import_provider_as_gateway_channel(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<GatewayChannel> {
        let app_type = builtin_app(app, "gateway.channel.import-provider")?;
        let channel = apply::import_provider_as_channel(&self.state, app_type, provider_id)?;
        let _ = apply::ensure_station_route(&self.state, &channel)?;
        Ok(channel)
    }

    pub fn list_gateway_routes(&self) -> ApplicationResult<Vec<GatewayRoute>> {
        Ok(self.state.db.get_gateway_routes()?)
    }

    pub fn get_gateway_route(&self, id: &str) -> ApplicationResult<GatewayRoute> {
        self.state
            .db
            .get_gateway_route_by_id(id)?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "gateway-route",
                id: id.to_string(),
            })
    }

    pub fn save_gateway_route(&self, mut route: GatewayRoute) -> ApplicationResult<GatewayRoute> {
        if route.id.trim().is_empty() {
            route.id = uuid::Uuid::new_v4().to_string();
        }
        if route.created_at == 0 {
            route.created_at = chrono::Utc::now().timestamp();
        }
        validate_route_references(&route, &self.state.db.get_gateway_channels()?)?;
        self.state.db.upsert_gateway_route(&route)?;
        Ok(route)
    }

    pub fn set_gateway_route_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> ApplicationResult<GatewayRoute> {
        let mut route = self.get_gateway_route(id)?;
        route.enabled = enabled;
        self.state.db.upsert_gateway_route(&route)?;
        Ok(route)
    }

    pub fn delete_gateway_route(&self, id: &str) -> ApplicationResult<()> {
        self.get_gateway_route(id)?;
        self.state.db.delete_gateway_route(id)?;
        Ok(())
    }

    pub fn list_gateway_route_rules(
        &self,
        route_id: &str,
    ) -> ApplicationResult<Vec<GatewayModelRule>> {
        Ok(self.get_gateway_route(route_id)?.model_rules)
    }

    pub fn add_gateway_route_rule(
        &self,
        route_id: &str,
        rule: GatewayModelRule,
    ) -> ApplicationResult<GatewayRoute> {
        let mut route = self.get_gateway_route(route_id)?;
        if route
            .model_rules
            .iter()
            .any(|item| item.model == rule.model)
        {
            return Err(ApplicationError::InvalidInput(format!(
                "route rule already exists for model {}",
                rule.model
            )));
        }
        route.model_rules.push(rule);
        self.save_gateway_route(route)
    }

    pub fn update_gateway_route_rule(
        &self,
        route_id: &str,
        model: &str,
        rule: GatewayModelRule,
    ) -> ApplicationResult<GatewayRoute> {
        let mut route = self.get_gateway_route(route_id)?;
        let target = route
            .model_rules
            .iter_mut()
            .find(|item| item.model == model)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "gateway-route-rule",
                id: model.to_string(),
            })?;
        *target = rule;
        self.save_gateway_route(route)
    }

    pub fn delete_gateway_route_rule(
        &self,
        route_id: &str,
        model: &str,
    ) -> ApplicationResult<GatewayRoute> {
        let mut route = self.get_gateway_route(route_id)?;
        let before = route.model_rules.len();
        route.model_rules.retain(|item| item.model != model);
        if route.model_rules.len() == before {
            return Err(ApplicationError::NotFound {
                kind: "gateway-route-rule",
                id: model.to_string(),
            });
        }
        self.save_gateway_route(route)
    }

    pub fn sort_gateway_route_rules(
        &self,
        route_id: &str,
        models: &[String],
    ) -> ApplicationResult<GatewayRoute> {
        let mut route = self.get_gateway_route(route_id)?;
        let existing = route
            .model_rules
            .iter()
            .map(|rule| rule.model.clone())
            .collect::<HashSet<_>>();
        let requested = models.iter().cloned().collect::<HashSet<_>>();
        if existing != requested || requested.len() != models.len() {
            return Err(ApplicationError::InvalidInput(
                "rule sort must contain every model exactly once".to_string(),
            ));
        }
        let mut by_model = std::mem::take(&mut route.model_rules)
            .into_iter()
            .map(|rule| (rule.model.clone(), rule))
            .collect::<HashMap<_, _>>();
        route.model_rules = models
            .iter()
            .filter_map(|model| by_model.remove(model))
            .collect();
        self.save_gateway_route(route)
    }

    pub fn list_gateway_keys(&self) -> ApplicationResult<Vec<GatewayKey>> {
        Ok(self.state.db.get_gateway_keys()?)
    }

    pub fn get_gateway_key(&self, id: &str) -> ApplicationResult<GatewayKey> {
        self.state
            .db
            .get_gateway_keys()?
            .into_iter()
            .find(|key| key.id == id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "gateway-key",
                id: id.to_string(),
            })
    }

    pub fn create_gateway_key(
        &self,
        name: &str,
        route_id: Option<&str>,
    ) -> ApplicationResult<GatewayKey> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ApplicationError::InvalidInput(
                "gateway key name cannot be empty".to_string(),
            ));
        }
        if let Some(route_id) = route_id {
            self.get_gateway_route(route_id)?;
        }
        let key = GatewayKey {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            key: crate::gateway::generate_key_secret(),
            route_id: route_id.map(str::to_string),
            model_policy: None,
            enabled: true,
            created_at: chrono::Utc::now().timestamp(),
        };
        self.state.db.upsert_gateway_key(&key)?;
        Ok(key)
    }

    pub fn revoke_gateway_key(&self, id: &str) -> ApplicationResult<GatewayKey> {
        let mut key = self.get_gateway_key(id)?;
        key.enabled = false;
        self.state.db.upsert_gateway_key(&key)?;
        Ok(key)
    }

    pub fn bind_gateway_key(
        &self,
        id: &str,
        route_id: Option<&str>,
    ) -> ApplicationResult<GatewayKey> {
        if let Some(route_id) = route_id {
            self.get_gateway_route(route_id)?;
        }
        let mut key = self.get_gateway_key(id)?;
        key.route_id = route_id.map(str::to_string);
        self.state.db.upsert_gateway_key(&key)?;
        Ok(key)
    }

    pub fn list_gateway_stations(&self) -> ApplicationResult<Vec<GatewayStation>> {
        let channels = self.state.db.get_gateway_channels()?;
        let routes = self.state.db.get_gateway_routes()?;
        let channel_map = channels
            .iter()
            .map(|channel| (channel.id.as_str(), channel))
            .collect::<HashMap<_, _>>();
        let mut referenced = HashSet::new();
        let mut stations = Vec::new();
        for route in routes
            .into_iter()
            .filter(|route| route.id.starts_with(apply::STATION_ROUTE_PREFIX))
        {
            let grouped = route
                .channel_ids
                .iter()
                .filter_map(|id| channel_map.get(id.as_str()).map(|item| (*item).clone()))
                .collect::<Vec<_>>();
            if grouped.is_empty() {
                continue;
            }
            referenced.extend(grouped.iter().map(|channel| channel.id.clone()));
            stations.push(station_from_route(route, grouped));
        }
        for channel in channels
            .into_iter()
            .filter(|channel| !referenced.contains(&channel.id))
        {
            let route = GatewayRoute {
                id: apply::station_route_id(&channel.id),
                name: channel.name.clone(),
                website_url: None,
                app_type: None,
                channel_ids: vec![channel.id.clone()],
                default_model: None,
                model_rules: Vec::new(),
                reasoning: GatewayReasoningConfig::default(),
                websocket_enabled: false,
                enabled: channel.enabled,
                created_at: 0,
            };
            stations.push(station_from_route(route, vec![channel]));
        }
        Ok(stations)
    }

    pub fn get_gateway_station(&self, id: &str) -> ApplicationResult<GatewayStation> {
        let id = station_id(id);
        self.list_gateway_stations()?
            .into_iter()
            .find(|station| station.id == id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "gateway-station",
                id,
            })
    }

    pub fn save_gateway_station(
        &self,
        mut station: GatewayStation,
    ) -> ApplicationResult<GatewayStation> {
        let id = if station.id.trim().is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            station_id(&station.id)
        };
        if station.name.trim().is_empty() {
            return Err(ApplicationError::InvalidInput(
                "station name cannot be empty".to_string(),
            ));
        }
        if station.channels.is_empty() {
            return Err(ApplicationError::InvalidInput(
                "station must contain at least one channel".to_string(),
            ));
        }
        let old = self.get_gateway_station(&id).ok();
        let old_by_id = old
            .as_ref()
            .map(|item| {
                item.channels
                    .iter()
                    .map(|channel| (channel.id.as_str(), channel))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        for (index, channel) in station.channels.iter_mut().enumerate() {
            if channel.id.trim().is_empty() {
                channel.id = format!("station-channel:{id}:{index}:{}", channel.dialect.as_str());
            }
            if channel.name.trim().is_empty() {
                channel.name = station.name.clone();
            }
            if channel.endpoint_id.is_none() {
                channel.endpoint_id = Some(format!("station-endpoint:{id}:{index}"));
            }
            if channel.api_key.is_empty()
                && let Some(existing) = old_by_id.get(channel.id.as_str())
            {
                channel.api_key.clone_from(&existing.api_key);
            }
            validate_channel(channel)?;
        }
        let route = GatewayRoute {
            id: apply::station_route_id(&id),
            name: station.name.clone(),
            website_url: station.website_url.clone(),
            app_type: None,
            channel_ids: station
                .channels
                .iter()
                .map(|channel| channel.id.clone())
                .collect(),
            default_model: station.default_model.clone(),
            model_rules: station.model_rules.clone(),
            reasoning: station.reasoning.clone(),
            websocket_enabled: station.websocket_enabled,
            enabled: station.enabled,
            created_at: if station.created_at == 0 {
                chrono::Utc::now().timestamp()
            } else {
                station.created_at
            },
        };
        validate_route_references(&route, &station.channels)?;
        for channel in &station.channels {
            self.state.db.upsert_gateway_channel(channel)?;
        }
        self.state.db.upsert_gateway_route(&route)?;
        if let Some(old) = old {
            let current = route.channel_ids.iter().collect::<HashSet<_>>();
            for stale in old
                .channels
                .iter()
                .filter(|channel| !current.contains(&channel.id))
            {
                self.state.db.delete_gateway_channel(&stale.id)?;
            }
        }
        station.id = id;
        station.created_at = route.created_at;
        Ok(station)
    }

    pub fn set_gateway_station_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> ApplicationResult<GatewayStation> {
        let mut station = self.get_gateway_station(id)?;
        station.enabled = enabled;
        for channel in &mut station.channels {
            channel.enabled = enabled;
        }
        self.save_gateway_station(station)
    }

    pub fn delete_gateway_station(&self, id: &str) -> ApplicationResult<()> {
        let station = self.get_gateway_station(id)?;
        self.state
            .db
            .delete_gateway_route(&apply::station_route_id(&station.id))?;
        for channel in station.channels {
            self.state.db.delete_gateway_channel(&channel.id)?;
        }
        Ok(())
    }

    pub async fn probe_gateway_station(
        &self,
        id: &str,
    ) -> ApplicationResult<Vec<GatewayEndpointTestResult>> {
        let station = self.get_gateway_station(id)?;
        let mut results = Vec::with_capacity(station.channels.len());
        for channel in station.channels {
            results.push(
                self.state
                    .gateway
                    .test_endpoint(channel.base_url, channel.api_key)
                    .await?,
            );
        }
        Ok(results)
    }

    pub fn gateway_station_models(&self, id: &str) -> ApplicationResult<Vec<String>> {
        let station = self.get_gateway_station(id)?;
        let route = station_route(&station);
        Ok(apply::station_models(&route, &station.channels))
    }

    pub fn select_gateway_station(&self, id: &str, app: &AppId) -> ApplicationResult<GatewayKey> {
        let station = self.get_gateway_station(id)?;
        let app_type = builtin_app(app, "gateway.station.select")?;
        Ok(apply::activate_route_for_app(
            &self.state,
            app_type,
            &apply::station_route_id(&station.id),
        )?)
    }

    pub fn apply_gateway_station(
        &self,
        id: &str,
        app: &AppId,
        policy: Option<GatewayAppModelPolicy>,
    ) -> ApplicationResult<apply::ApplyResult> {
        let station = self.get_gateway_station(id)?;
        let app_type = builtin_app(app, "gateway.station.apply")?;
        let base_url = gateway_base_url(&self.state.db.get_gateway_config()?);
        let route_id = apply::station_route_id(&station.id);
        match policy {
            Some(policy) => Ok(apply::apply_station_to_app_with_policy(
                &self.state,
                app_type,
                &base_url,
                &route_id,
                policy,
            )?),
            None => Ok(apply::apply_station_to_app(
                &self.state,
                app_type,
                &base_url,
                &route_id,
            )?),
        }
    }

    pub fn gateway_connection_info(&self, app: Option<&AppId>) -> ApplicationResult<Value> {
        let base_url = gateway_base_url(&self.state.db.get_gateway_config()?);
        if let Some(app) = app {
            let app_type = builtin_app(app, "gateway.connection-info")?;
            let route = apply::ensure_app_route(&self.state, app_type)?;
            let key = apply::ensure_key_for_route(
                &self.state,
                app_type.as_str(),
                Some(route.id.as_str()),
            )?;
            Ok(connection_value(&base_url, Some(app_type), &route, &key))
        } else {
            let info = apply::generic_client_info(&self.state, &base_url)?;
            serde_json::to_value(info)
                .map_err(|source| crate::AppError::JsonSerialize { source }.into())
        }
    }

    pub fn gateway_station_connection_info(
        &self,
        id: &str,
        app: &AppId,
    ) -> ApplicationResult<Value> {
        let station = self.get_gateway_station(id)?;
        let app_type = builtin_app(app, "gateway.station.connection-info")?;
        let route = station_route(&station);
        if !station
            .channels
            .iter()
            .any(|channel| channel.enabled && apply::dialect_compatible(channel.dialect, app_type))
        {
            return Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "gateway.station.dialect",
            });
        }
        let key = apply::ensure_key_for_route(
            &self.state,
            &apply::gateway_key_label(app_type, &route.id),
            Some(&route.id),
        )?;
        Ok(connection_value(
            &gateway_base_url(&self.state.db.get_gateway_config()?),
            Some(app_type),
            &route,
            &key,
        ))
    }

    pub fn disconnect_gateway_from_app(&self, app: &AppId) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "gateway.station.disconnect")?;
        let providers = self.state.db.get_all_providers(app_type.as_str())?;
        let gateway_ids = providers
            .values()
            .filter(|provider| provider.is_local_gateway())
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        if gateway_ids.is_empty() {
            return Ok(json!({
                "app": app,
                "disconnected": false,
                "reason": "no gateway-managed providers are installed"
            }));
        }

        if app_type.is_additive_mode() {
            for provider_id in &gateway_ids {
                ProviderService::remove_from_live_config(&self.state, app_type, provider_id)?;
            }
            return Ok(json!({
                "app": app,
                "disconnected": true,
                "removedFromLive": gateway_ids
            }));
        }

        let current = ProviderService::current(&self.state, app_type)?;
        if !gateway_ids.iter().any(|id| id == &current) {
            return Ok(json!({
                "app": app,
                "disconnected": false,
                "reason": "the current provider is not a gateway provider",
                "currentProviderId": current
            }));
        }
        let alternatives = providers
            .values()
            .filter(|provider| !provider.is_local_gateway())
            .collect::<Vec<_>>();
        let official = alternatives
            .iter()
            .copied()
            .filter(|provider| {
                provider.category.as_deref() == Some("official") || provider.id == "official"
            })
            .collect::<Vec<_>>();
        let target = if official.len() == 1 {
            official[0]
        } else if alternatives.len() == 1 {
            alternatives[0]
        } else {
            return Err(ApplicationError::InvalidInput(format!(
                "cannot choose a replacement provider for {}; switch explicitly first (candidates: {})",
                app,
                alternatives
                    .iter()
                    .map(|provider| provider.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let result = ProviderService::switch_with(
            &self.state,
            app_type,
            &target.id,
            DriftResolution::Preserve,
        )?;
        Ok(json!({
            "app": app,
            "disconnected": true,
            "providerId": target.id,
            "warnings": result.warnings
        }))
    }
}

fn station_from_route(route: GatewayRoute, channels: Vec<GatewayChannel>) -> GatewayStation {
    GatewayStation {
        id: station_id(&route.id),
        name: route.name,
        website_url: route.website_url,
        channels,
        default_model: route.default_model,
        model_rules: route.model_rules,
        reasoning: route.reasoning,
        websocket_enabled: route.websocket_enabled,
        enabled: route.enabled,
        created_at: route.created_at,
    }
}

fn station_route(station: &GatewayStation) -> GatewayRoute {
    GatewayRoute {
        id: apply::station_route_id(&station.id),
        name: station.name.clone(),
        website_url: station.website_url.clone(),
        app_type: None,
        channel_ids: station
            .channels
            .iter()
            .map(|channel| channel.id.clone())
            .collect(),
        default_model: station.default_model.clone(),
        model_rules: station.model_rules.clone(),
        reasoning: station.reasoning.clone(),
        websocket_enabled: station.websocket_enabled,
        enabled: station.enabled,
        created_at: station.created_at,
    }
}

fn station_id(id: &str) -> String {
    id.trim()
        .strip_prefix(apply::STATION_ROUTE_PREFIX)
        .unwrap_or(id.trim())
        .to_string()
}

fn gateway_base_url(config: &GatewayConfig) -> String {
    format!("http://127.0.0.1:{}", config.port)
}

fn connection_value(
    base_url: &str,
    app: Option<AppType>,
    route: &GatewayRoute,
    key: &GatewayKey,
) -> Value {
    let dialect = app.map(apply::client_dialect);
    json!({
        "baseUrl": base_url,
        "apiBaseUrl": format!("{base_url}/v1"),
        "messagesUrl": format!("{base_url}/v1/messages"),
        "chatCompletionsUrl": format!("{base_url}/v1/chat/completions"),
        "responsesUrl": format!("{base_url}/v1/responses"),
        "modelsUrl": format!("{base_url}/v1/models"),
        "app": app.map(|item| item.as_str()),
        "dialect": dialect,
        "routeId": route.id,
        "routeName": route.name,
        "keyId": key.id,
        "keyName": key.name,
        "apiKey": key.key
    })
}

fn builtin_app(app: &AppId, capability: &'static str) -> ApplicationResult<AppType> {
    AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
        app: app.to_string(),
        capability,
    })
}

fn validate_http_url(value: &str) -> ApplicationResult<()> {
    let url = Url::parse(value)
        .map_err(|error| ApplicationError::InvalidInput(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApplicationError::InvalidInput(
            "gateway upstream URL must use http or https".to_string(),
        ));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(ApplicationError::InvalidInput(
            "gateway upstream URL must contain a host and no embedded credentials".to_string(),
        ));
    }
    Ok(())
}

fn validate_channel(channel: &GatewayChannel) -> ApplicationResult<()> {
    if channel.id.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "gateway channel id cannot be empty".to_string(),
        ));
    }
    if channel.name.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "gateway channel name cannot be empty".to_string(),
        ));
    }
    validate_http_url(&channel.base_url)?;
    if channel.weight == 0 {
        return Err(ApplicationError::InvalidInput(
            "gateway channel weight must be at least 1".to_string(),
        ));
    }
    let mut names = HashSet::new();
    for (name, _) in &channel.extra_headers {
        let normalized = name.trim().to_ascii_lowercase();
        if normalized.is_empty() || !names.insert(normalized) {
            return Err(ApplicationError::InvalidInput(
                "gateway header names must be non-empty and unique".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_route_references(
    route: &GatewayRoute,
    channels: &[GatewayChannel],
) -> ApplicationResult<()> {
    route.validate().map_err(ApplicationError::InvalidInput)?;
    let known = channels
        .iter()
        .map(|channel| channel.id.as_str())
        .collect::<HashSet<_>>();
    let missing = route
        .channel_ids
        .iter()
        .filter(|id| !known.contains(id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ApplicationError::InvalidInput(format!(
            "gateway route references missing channels: {}",
            missing.join(", ")
        )));
    }
    for rule in &route.model_rules {
        if let Some(channel_id) = &rule.channel_id
            && !known.contains(channel_id.as_str())
        {
            return Err(ApplicationError::InvalidInput(format!(
                "gateway route rule references missing channel: {channel_id}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_credentials_in_gateway_urls() {
        let error = validate_http_url("https://user:secret@example.com").unwrap_err();
        assert_eq!(error.code(), "INVALID_ARGUMENT");
    }

    #[test]
    fn normalizes_station_route_prefix() {
        assert_eq!(station_id("station:alpha"), "alpha");
        assert_eq!(station_id("alpha"), "alpha");
    }
}
