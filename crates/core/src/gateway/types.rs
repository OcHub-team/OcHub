//! Gateway (local relay) configuration types.
//!
//! The gateway is a standing loopback HTTP server exposing the three wire
//! dialects as standard endpoints (`/v1/messages`, `/v1/chat/completions`,
//! `/v1/responses`) and routing each request by model name to a configured
//! *channel* (upstream provider), converting dialects via `ochub-convert`
//! when the inlet and the channel speak different formats.

use serde::{Deserialize, Serialize};

/// Wire dialect an endpoint or channel speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// Block-structured dialect (`/v1/messages`).
    Messages,
    /// Chat-completions dialect (`/v1/chat/completions`).
    Chat,
    /// Item/event dialect (`/v1/responses`), SSE or WebSocket.
    Responses,
}

impl Dialect {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dialect::Messages => "messages",
            Dialect::Chat => "chat",
            Dialect::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "messages" => Some(Dialect::Messages),
            "chat" => Some(Dialect::Chat),
            "responses" => Some(Dialect::Responses),
            _ => None,
        }
    }

    /// Default upstream request path for a channel speaking this dialect.
    pub fn default_path(&self) -> &'static str {
        match self {
            Dialect::Messages => "/v1/messages",
            Dialect::Chat => "/v1/chat/completions",
            Dialect::Responses => "/v1/responses",
        }
    }
}

/// Global gateway settings (persisted as a single settings blob).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Start the gateway automatically with the app.
    pub enabled: bool,
    /// Fixed listen port on loopback. One-click app configs embed this, so it
    /// must stay stable.
    pub port: u16,
    /// Require a local API key (Bearer / x-api-key) on inference endpoints.
    pub require_key: bool,
    /// Interval for channel health probes, seconds. 0 disables probing.
    pub health_interval_secs: u64,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 4180,
            require_key: true,
            health_interval_secs: 300,
        }
    }
}

/// A locally issued API key. Requests carrying it are attributed to `name` in
/// usage records (per-app attribution for one-click configs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayKey {
    pub id: String,
    /// Human label, e.g. "claude-code" or "cherry-studio".
    pub name: String,
    /// The secret, `rd-` prefixed.
    pub key: String,
    /// Optional route profile used to isolate this client's upstreams and
    /// model/reasoning mappings. `None` preserves the legacy all-channels
    /// behavior.
    #[serde(default)]
    pub route_id: Option<String>,
    pub created_at: i64,
    pub enabled: bool,
}

/// Model alias exposed to a client by one route profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayModelRule {
    /// Client-facing model name or wildcard pattern.
    pub model: String,
    /// Model name sent to the selected upstream.
    pub upstream_model: String,
    /// Optional hard binding to one upstream channel.
    #[serde(default)]
    pub channel_id: Option<String>,
}

impl GatewayModelRule {
    pub fn matches_model(&self, model: &str) -> bool {
        pattern_matches(&self.model, model)
    }
}

/// How a route profile handles reasoning/thinking parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GatewayReasoningMode {
    /// Translate effort levels and token budgets between supported dialects.
    #[default]
    Auto,
    /// Keep the source dialect's reasoning fields whenever possible.
    Passthrough,
    /// Remove reasoning/thinking parameters before forwarding.
    Disabled,
}

/// Configurable effort-to-budget mapping used during protocol conversion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayReasoningConfig {
    #[serde(default)]
    pub mode: GatewayReasoningMode,
    #[serde(default = "default_low_budget")]
    pub low_budget: u32,
    #[serde(default = "default_medium_budget")]
    pub medium_budget: u32,
    #[serde(default = "default_high_budget")]
    pub high_budget: u32,
    #[serde(default = "default_max_budget")]
    pub max_budget: u32,
}

impl Default for GatewayReasoningConfig {
    fn default() -> Self {
        Self {
            mode: GatewayReasoningMode::Auto,
            low_budget: default_low_budget(),
            medium_budget: default_medium_budget(),
            high_budget: default_high_budget(),
            max_budget: default_max_budget(),
        }
    }
}

impl GatewayReasoningConfig {
    pub fn budget_for_effort(&self, effort: &str) -> Option<u32> {
        match effort {
            "minimal" | "none" => None,
            "low" => Some(self.low_budget),
            "medium" => Some(self.medium_budget),
            "high" => Some(self.high_budget),
            "max" | "xhigh" => Some(self.max_budget),
            _ => Some(self.medium_budget),
        }
    }

    pub fn effort_for_budget(&self, budget: u32) -> &'static str {
        let low_mid = self.low_budget.saturating_add(self.medium_budget) / 2;
        let medium_high = self.medium_budget.saturating_add(self.high_budget) / 2;
        let high_max = self.high_budget.saturating_add(self.max_budget) / 2;
        if budget <= low_mid {
            "low"
        } else if budget <= medium_high {
            "medium"
        } else if budget <= high_max {
            "high"
        } else {
            "max"
        }
    }
}

fn default_low_budget() -> u32 {
    4_096
}

fn default_medium_budget() -> u32 {
    10_000
}

fn default_high_budget() -> u32 {
    16_000
}

fn default_max_budget() -> u32 {
    32_000
}

/// Per-client routing profile. App-managed profiles use `app_type`; generic
/// clients may leave it empty and bind through a manually issued key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRoute {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub app_type: Option<String>,
    /// Empty means every enabled channel can participate.
    #[serde(default)]
    pub channel_ids: Vec<String>,
    /// Fallback upstream model when no model rule matches. This lets a route
    /// profile switch the active model without rewriting the client config.
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub model_rules: Vec<GatewayModelRule>,
    #[serde(default)]
    pub reasoning: GatewayReasoningConfig,
    pub enabled: bool,
    pub created_at: i64,
}

impl GatewayRoute {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("路由方案 ID 不能为空".to_string());
        }
        if self.name.trim().is_empty() {
            return Err("路由方案名称不能为空".to_string());
        }
        if self
            .app_type
            .as_deref()
            .is_some_and(|app_type| app_type.trim().is_empty())
        {
            return Err("路由方案的应用类型不能为空字符串".to_string());
        }
        if self.reasoning.low_budget == 0
            || self.reasoning.medium_budget == 0
            || self.reasoning.high_budget == 0
            || self.reasoning.max_budget == 0
            || self.reasoning.low_budget > self.reasoning.medium_budget
            || self.reasoning.medium_budget > self.reasoning.high_budget
            || self.reasoning.high_budget > self.reasoning.max_budget
        {
            return Err("思考预算必须大于 0，并按 low、medium、high、max 递增".to_string());
        }
        if self
            .default_model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err("默认模型不能为空字符串".to_string());
        }
        for (index, channel_id) in self.channel_ids.iter().enumerate() {
            if channel_id.trim().is_empty() {
                return Err("允许使用的上游 ID 不能为空".to_string());
            }
            if self.channel_ids[..index].contains(channel_id) {
                return Err(format!("上游 {channel_id} 在路由方案中重复出现"));
            }
        }
        for rule in &self.model_rules {
            if rule.model.trim().is_empty() || rule.upstream_model.trim().is_empty() {
                return Err("模型映射两端都不能为空".to_string());
            }
            if let Some(channel_id) = &rule.channel_id {
                if channel_id.trim().is_empty() {
                    return Err("模型映射指定的上游 ID 不能为空".to_string());
                }
                if !self.channel_ids.is_empty() && !self.channel_ids.contains(channel_id) {
                    return Err(format!(
                        "模型映射指定的上游 {channel_id} 不在路由允许列表中"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn rule_for_model(&self, model: &str) -> Option<&GatewayModelRule> {
        self.model_rules
            .iter()
            .find(|rule| rule.matches_model(model))
    }

    pub fn allows_channel(&self, channel_id: &str) -> bool {
        self.channel_ids.is_empty() || self.channel_ids.iter().any(|id| id == channel_id)
    }
}

/// An upstream channel (provider) the gateway can route to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayChannel {
    pub id: String,
    pub name: String,
    /// Dialect the upstream speaks.
    pub dialect: Dialect,
    /// Upstream origin, e.g. `https://api.example.com` (no trailing path).
    pub base_url: String,
    /// Bearer / x-api-key credential injected upstream.
    pub api_key: String,
    /// Optional explicit request path override (else the dialect default).
    #[serde(default)]
    pub path_override: Option<String>,
    /// Model matchers: exact names or `*` wildcards (e.g. `claude-*`).
    /// Empty = matches every model.
    #[serde(default)]
    pub models: Vec<String>,
    /// Rewrite the model name sent upstream (e.g. route `claude-x` in, send
    /// `some-upstream-model` out). `None` keeps the client's model.
    #[serde(default)]
    pub model_override: Option<String>,
    /// Lower runs first. Channels with equal priority form a weighted pool.
    #[serde(default)]
    pub priority: i32,
    /// Weight within a priority group (>=1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub enabled: bool,
    /// Extra headers to inject upstream (name → value).
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// When the channel was imported from an existing direct connection, the
    /// source as `"{app}:{provider_id}"`. Display-only; never auto-synced.
    #[serde(default)]
    pub imported_from: Option<String>,
}

fn default_weight() -> u32 {
    1
}

impl GatewayChannel {
    /// Does this channel serve `model`? Supports `*` suffix/prefix wildcards.
    pub fn matches_model(&self, model: &str) -> bool {
        if self.models.is_empty() {
            return true;
        }
        self.models.iter().any(|pat| pattern_matches(pat, model))
    }

    /// Full upstream URL for this channel's inference endpoint.
    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self
            .path_override
            .as_deref()
            .unwrap_or_else(|| self.dialect.default_path());
        format!("{base}{path}")
    }
}

/// Glob-lite matcher: `*` matches any run of characters.
pub(crate) fn pattern_matches(pattern: &str, value: &str) -> bool {
    fn inner(p: &[u8], v: &[u8]) -> bool {
        match (p.first(), v.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], v) || (!v.is_empty() && inner(p, &v[1..])),
            (Some(pc), Some(vc)) if pc == vc => inner(&p[1..], &v[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), value.as_bytes())
}

/// Live health snapshot of one channel (kept in memory, not persisted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelHealth {
    /// Not probed yet.
    Unknown,
    Healthy,
    /// Last probe failed; carries a short reason.
    Unhealthy(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(pattern_matches("claude-*", "claude-x-4"));
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("gpt-*-mini", "gpt-5-mini"));
        assert!(!pattern_matches("claude-*", "gpt-4"));
        assert!(pattern_matches("exact", "exact"));
        assert!(!pattern_matches("exact", "exact2"));
    }

    #[test]
    fn channel_model_match_and_url() {
        let ch = GatewayChannel {
            id: "c1".into(),
            name: "n".into(),
            dialect: Dialect::Messages,
            base_url: "https://api.example.com/".into(),
            api_key: "k".into(),
            path_override: None,
            models: vec!["claude-*".into()],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        };
        assert!(ch.matches_model("claude-x"));
        assert!(!ch.matches_model("gpt-4"));
        assert_eq!(ch.endpoint_url(), "https://api.example.com/v1/messages");
    }

    #[test]
    fn route_validation_rejects_inconsistent_upstream_binding() {
        let mut route = GatewayRoute {
            id: "route".into(),
            name: "route".into(),
            app_type: Some("claude".into()),
            channel_ids: vec!["allowed".into()],
            default_model: None,
            model_rules: vec![GatewayModelRule {
                model: "sonnet".into(),
                upstream_model: "claude-sonnet".into(),
                channel_id: Some("other".into()),
            }],
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 1,
        };
        assert!(route.validate().is_err());
        route.model_rules[0].channel_id = Some("allowed".into());
        assert!(route.validate().is_ok());
    }
}
