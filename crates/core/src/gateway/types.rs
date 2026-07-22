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
    pub created_at: i64,
    pub enabled: bool,
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
        };
        assert!(ch.matches_model("claude-x"));
        assert!(!ch.matches_model("gpt-4"));
        assert_eq!(ch.endpoint_url(), "https://api.example.com/v1/messages");
    }
}
