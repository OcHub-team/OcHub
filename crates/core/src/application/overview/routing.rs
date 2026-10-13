//! Secret-free configuration graph and deterministic model-policy explanation.
use crate::gateway::routing_policy::{
    channel_supports_request, dialect_conversion_rank, model_override,
};
use crate::gateway::types::{GatewayAppModelPolicy, pattern_matches};
use crate::gateway::{ChannelHealth, Dialect, GatewayChannel, GatewayModelRule, GatewayRoute};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A credential's public routing policy. No credential value is read/exported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRouteBinding {
    pub id: String,
    pub name: String,
    pub route_id: String,
    pub model_policy: Option<GatewayAppModelPolicy>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewCandidateState {
    Eligible,
    RouteDisabled,
    BindingDisabled,
    ChannelDisabled,
    OutsideRoute,
    RuleChannelMismatch,
    RuleDialectMismatch,
    UnsupportedDialect,
    ModelMismatch,
}

#[derive(Debug, Clone)]
pub struct OverviewRouteCandidate {
    pub channel_id: String,
    pub state: OverviewCandidateState,
    pub upstream_model: String,
    /// Native dialect preference is subordinate to explicit channel priority.
    pub dialect_preference: u8,
}

#[derive(Debug, Clone)]
pub struct OverviewRouteExplanation {
    pub requested_model: String,
    pub routing_model: String,
    pub matched_rule: Option<String>,
    pub uses_binding_policy: bool,
    pub uses_fallback: bool,
    /// Configured candidates only. No random draw, health probe, or request.
    pub candidates: Vec<OverviewRouteCandidate>,
}

impl OverviewRouting {
    /// Explain a normal inference request against this snapshot. Runtime health,
    /// quota, transport extensions, and actual failover are not simulated.
    /// An unavailable/older snapshot or stale selection returns no explanation.
    pub fn explain(
        &self,
        route_id: &str,
        binding_id: Option<&str>,
        requested_model: &str,
        inlet: Dialect,
    ) -> Option<OverviewRouteExplanation> {
        if !self.policy_details_available || requested_model.trim().is_empty() {
            return None;
        }
        let route = self.routes.iter().find(|route| route.id == route_id)?;
        let binding = match binding_id {
            Some(id) => Some(
                self.bindings
                    .iter()
                    .find(|binding| binding.id == id && binding.route_id == route.id)?,
            ),
            None => None,
        };
        let policy = binding.and_then(|binding| binding.model_policy.as_ref());
        let (rules, fallback) = match policy {
            Some(policy) => (&policy.model_rules, policy.fallback_model.as_deref()),
            None => (&route.model_rules, route.default_model.as_deref()),
        };
        let rule = rules
            .iter()
            .find(|rule| rule.matches_model(requested_model));
        let mapped_model = model_override(rule, fallback);
        let routing_model = mapped_model.unwrap_or(requested_model);
        let mut channels: Vec<_> = self.channels.iter().collect();
        channels.sort_by_key(|channel| {
            (
                channel.priority,
                dialect_conversion_rank(inlet, channel.dialect),
                &channel.id,
            )
        });
        let candidates = channels
            .into_iter()
            .map(|channel| {
                use OverviewCandidateState::*;
                let state = if !route.enabled {
                    RouteDisabled
                } else if binding.is_some_and(|binding| !binding.enabled) {
                    BindingDisabled
                } else if !channel.enabled {
                    ChannelDisabled
                } else if !route.uses_all_channels && !route.channel_ids.contains(&channel.id) {
                    OutsideRoute
                } else if rule
                    .and_then(|rule| rule.channel_id.as_ref())
                    .is_some_and(|id| id != &channel.id)
                {
                    RuleChannelMismatch
                } else if rule
                    .and_then(|rule| rule.dialect)
                    .is_some_and(|dialect| dialect != channel.dialect)
                {
                    RuleDialectMismatch
                } else if !channel_supports_request(inlet, channel.dialect, false) {
                    UnsupportedDialect
                } else if !channel.models.is_empty()
                    && !channel
                        .models
                        .iter()
                        .any(|pattern| pattern_matches(pattern, routing_model))
                {
                    ModelMismatch
                } else {
                    Eligible
                };
                OverviewRouteCandidate {
                    channel_id: channel.id.clone(),
                    state,
                    upstream_model: mapped_model
                        .or(channel.model_override.as_deref())
                        .unwrap_or(requested_model)
                        .to_owned(),
                    dialect_preference: dialect_conversion_rank(inlet, channel.dialect),
                }
            })
            .collect();
        Some(OverviewRouteExplanation {
            requested_model: requested_model.to_owned(),
            routing_model: routing_model.to_owned(),
            matched_rule: rule.map(|rule| rule.model.clone()),
            uses_binding_policy: policy.is_some(),
            uses_fallback: rule.is_none() && mapped_model.is_some(),
            candidates,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRouting {
    pub routes: Vec<OverviewRoute>,
    pub channels: Vec<OverviewChannel>,
    #[serde(default)]
    pub bindings: Vec<OverviewRouteBinding>,
    /// False for older nodes whose snapshots do not carry model policies.
    #[serde(default)]
    pub policy_details_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRoute {
    pub id: String,
    pub name: String,
    pub app: Option<String>,
    pub enabled: bool,
    pub is_station: bool,
    /// An empty configured allowlist means every enabled channel, not zero.
    pub uses_all_channels: bool,
    pub channel_ids: Vec<String>,
    /// Configuration only; model/dialect rules still constrain actual routing.
    pub enabled_channel_count: usize,
    pub missing_channel_ids: Vec<String>,
    pub default_model: Option<String>,
    pub model_rule_count: usize,
    #[serde(default)]
    pub model_rules: Vec<GatewayModelRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewChannel {
    pub id: String,
    pub endpoint_id: Option<String>,
    pub name: String,
    pub dialect: Dialect,
    /// Scheme, host and port only. Credentials, path, query and fragment are
    /// deliberately omitted, since any of them can carry an upstream secret.
    pub origin: Option<String>,
    pub enabled: bool,
    pub priority: i32,
    pub weight: u32,
    pub model_count: usize,
    pub accepts_all_models: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_override: Option<String>,
    pub reachability: OverviewReachability,
    /// The current health cache has no observation timestamps. Never replace
    /// this with generated_at or imply a continuously healthy upstream.
    pub reachability_observed_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverviewReachability {
    Unknown,
    /// A successful cache result may only mean HTTP < 500, including 401/403.
    /// It does not establish valid authentication or working model inference.
    Reachable,
    Unreachable,
}

pub(super) fn project_routing(
    routes: Vec<GatewayRoute>,
    channels: Vec<GatewayChannel>,
    health: &HashMap<String, ChannelHealth>,
) -> OverviewRouting {
    let known: HashSet<&str> = channels.iter().map(|channel| channel.id.as_str()).collect();
    let routes = routes
        .into_iter()
        .map(|route| {
            let enabled_channel_count = channels
                .iter()
                .filter(|channel| channel.enabled && route.allows_channel(&channel.id))
                .count();
            let missing_channel_ids = route
                .channel_ids
                .iter()
                .filter(|id| !known.contains(id.as_str()))
                .cloned()
                .collect();
            OverviewRoute {
                is_station: route
                    .id
                    .starts_with(crate::gateway::apply::STATION_ROUTE_PREFIX),
                id: route.id,
                name: route.name,
                app: route.app_type,
                enabled: route.enabled,
                uses_all_channels: route.channel_ids.is_empty(),
                channel_ids: route.channel_ids,
                enabled_channel_count,
                missing_channel_ids,
                default_model: route.default_model,
                model_rule_count: route.model_rules.len(),
                model_rules: route.model_rules,
            }
        })
        .collect();
    let channels = channels
        .into_iter()
        .map(|channel| OverviewChannel {
            reachability: match health.get(&channel.id) {
                Some(ChannelHealth::Healthy) => OverviewReachability::Reachable,
                Some(ChannelHealth::Unhealthy(_)) => OverviewReachability::Unreachable,
                _ => OverviewReachability::Unknown,
            },
            origin: url::Url::parse(&channel.base_url)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
                .map(|url| url.origin().ascii_serialization()),
            id: channel.id,
            endpoint_id: channel.endpoint_id,
            name: channel.name,
            dialect: channel.dialect,
            enabled: channel.enabled,
            priority: channel.priority,
            weight: channel.weight,
            model_count: channel.models.len(),
            accepts_all_models: channel.models.is_empty(),
            models: channel.models,
            model_override: channel.model_override,
            reachability_observed_at: None,
        })
        .collect();
    OverviewRouting {
        routes,
        channels,
        bindings: Vec::new(),
        policy_details_available: true,
    }
}
