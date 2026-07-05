//! Core domain model: providers, provider metadata, usage scripts, and the
//! cross-app "universal provider". Ported from cc-switch `provider.rs`.
//!
//! Field names and serde renames match the reference implementation exactly so
//! the on-disk `config.json` / SQLite payloads and the deeplink/import formats
//! stay byte-compatible with cc-switch.

use std::collections::HashMap;

use http::header::{HeaderValue, InvalidHeaderValue};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::CustomEndpoint;

/// A single provider configuration for one managed app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(rename = "settingsConfig")]
    pub settings_config: Value,
    #[serde(skip_serializing_if = "Option::is_none", rename = "websiteUrl")]
    pub website_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "createdAt")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sortIndex")]
    pub sort_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ProviderMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "iconColor")]
    pub icon_color: Option<String>,
    #[serde(default, rename = "inFailoverQueue")]
    pub in_failover_queue: bool,
}

impl Provider {
    pub fn with_id(
        id: String,
        name: String,
        settings_config: Value,
        website_url: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            settings_config,
            website_url,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    pub fn is_codex_oauth(&self) -> bool {
        self.provider_type() == Some("codex_oauth")
    }

    pub fn is_github_copilot(&self) -> bool {
        self.provider_type() == Some("github_copilot")
            || self.claude_base_url_contains("githubcopilot.com")
    }

    pub fn uses_managed_account_auth(&self) -> bool {
        self.is_github_copilot()
            || self.is_codex_oauth()
            || self.claude_base_url_contains("chatgpt.com/backend-api/codex")
    }

    fn provider_type(&self) -> Option<&str> {
        self.meta.as_ref().and_then(|m| m.provider_type.as_deref())
    }

    fn claude_base_url_contains(&self, needle: &str) -> bool {
        self.settings_config
            .pointer("/env/ANTHROPIC_BASE_URL")
            .and_then(|value| value.as_str())
            .map(|base_url| base_url.contains(needle))
            .unwrap_or(false)
    }

    pub fn codex_fast_mode_enabled(&self) -> bool {
        self.meta
            .as_ref()
            .map(|m| m.codex_fast_mode_enabled())
            .unwrap_or(false)
    }

    pub fn has_usage_script_enabled(&self) -> bool {
        self.meta
            .as_ref()
            .and_then(|m| m.usage_script.as_ref())
            .map(|s| s.enabled)
            .unwrap_or(false)
    }
}

/// Manager of all providers for one app: id -> provider plus the current id.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderManager {
    pub providers: IndexMap<String, Provider>,
    pub current: String,
}

impl ProviderManager {
    pub fn get_all_providers(&self) -> &IndexMap<String, Provider> {
        &self.providers
    }
}

/// Usage-query script config (per-provider, stored in meta).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageScript {
    pub enabled: bool,
    pub language: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "accessToken")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "userId")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "templateType")]
    pub template_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "autoQueryInterval")]
    pub auto_query_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "codingPlanProvider")]
    pub coding_plan_provider: Option<String>,
}

/// One plan's usage data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageData {
    #[serde(skip_serializing_if = "Option::is_none", rename = "planName")]
    pub plan_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "isValid")]
    pub is_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "invalidMessage")]
    pub invalid_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Usage-query result (supports multiple plans).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<UsageData>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Per-provider connectivity-check override config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderTestConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "timeoutSecs", skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(
        rename = "degradedThresholdMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub degraded_threshold_ms: Option<u64>,
    #[serde(rename = "maxRetries", skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
}

/// Auth binding source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthBindingSource {
    #[default]
    ProviderConfig,
    ManagedAccount,
}

/// Generic managed-account auth binding.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthBinding {
    #[serde(default)]
    pub source: AuthBindingSource,
    #[serde(rename = "authProvider", skip_serializing_if = "Option::is_none")]
    pub auth_provider: Option<String>,
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Claude Desktop 3P write mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeDesktopMode {
    Direct,
    Proxy,
}

/// Claude Desktop safe model route exposed in proxy mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeDesktopModelRoute {
    pub model: String,
    #[serde(rename = "labelOverride", skip_serializing_if = "Option::is_none")]
    pub label_override: Option<String>,
    #[serde(rename = "supports1m", skip_serializing_if = "Option::is_none")]
    pub supports_1m: Option<bool>,
}

/// Codex Responses -> Chat Completions reasoning capability description.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CodexChatReasoningConfig {
    #[serde(rename = "supportsThinking", skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(rename = "supportsEffort", skip_serializing_if = "Option::is_none")]
    pub supports_effort: Option<bool>,
    #[serde(rename = "thinkingParam", skip_serializing_if = "Option::is_none")]
    pub thinking_param: Option<String>,
    #[serde(rename = "effortParam", skip_serializing_if = "Option::is_none")]
    pub effort_param: Option<String>,
    #[serde(rename = "effortValueMode", skip_serializing_if = "Option::is_none")]
    pub effort_value_mode: Option<String>,
    #[serde(rename = "outputFormat", skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
}

/// Provider metadata. Stored only in `~/.cc-switch/` data, never written to a
/// live config file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderMeta {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom_endpoints: HashMap<String, CustomEndpoint>,
    #[serde(
        rename = "commonConfigEnabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub common_config_enabled: Option<bool>,
    #[serde(rename = "claudeDesktopMode", skip_serializing_if = "Option::is_none")]
    pub claude_desktop_mode: Option<ClaudeDesktopMode>,
    #[serde(
        default,
        rename = "claudeDesktopModelRoutes",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub claude_desktop_model_routes: HashMap<String, ClaudeDesktopModelRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_script: Option<UsageScript>,
    #[serde(rename = "endpointAutoSelect", skip_serializing_if = "Option::is_none")]
    pub endpoint_auto_select: Option<bool>,
    #[serde(rename = "isPartner", skip_serializing_if = "Option::is_none")]
    pub is_partner: Option<bool>,
    #[serde(
        rename = "partnerPromotionKey",
        skip_serializing_if = "Option::is_none"
    )]
    pub partner_promotion_key: Option<String>,
    #[serde(rename = "costMultiplier", skip_serializing_if = "Option::is_none")]
    pub cost_multiplier: Option<String>,
    #[serde(rename = "pricingModelSource", skip_serializing_if = "Option::is_none")]
    pub pricing_model_source: Option<String>,
    #[serde(rename = "limitDailyUsd", skip_serializing_if = "Option::is_none")]
    pub limit_daily_usd: Option<String>,
    #[serde(rename = "limitMonthlyUsd", skip_serializing_if = "Option::is_none")]
    pub limit_monthly_usd: Option<String>,
    #[serde(rename = "testConfig", skip_serializing_if = "Option::is_none")]
    pub test_config: Option<ProviderTestConfig>,
    #[serde(rename = "apiFormat", skip_serializing_if = "Option::is_none")]
    pub api_format: Option<String>,
    #[serde(rename = "authBinding", skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<AuthBinding>,
    #[serde(rename = "apiKeyField", skip_serializing_if = "Option::is_none")]
    pub api_key_field: Option<String>,
    #[serde(rename = "isFullUrl", skip_serializing_if = "Option::is_none")]
    pub is_full_url: Option<bool>,
    #[serde(rename = "promptCacheKey", skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(rename = "codexFastMode", skip_serializing_if = "Option::is_none")]
    pub codex_fast_mode: Option<bool>,
    #[serde(rename = "codexChatReasoning", skip_serializing_if = "Option::is_none")]
    pub codex_chat_reasoning: Option<CodexChatReasoningConfig>,
    #[serde(rename = "customUserAgent", skip_serializing_if = "Option::is_none")]
    pub custom_user_agent: Option<String>,
    #[serde(rename = "liveConfigManaged", skip_serializing_if = "Option::is_none")]
    pub live_config_managed: Option<bool>,
    #[serde(rename = "providerType", skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(rename = "githubAccountId", skip_serializing_if = "Option::is_none")]
    pub github_account_id: Option<String>,
}

impl ProviderMeta {
    pub fn codex_fast_mode_enabled(&self) -> bool {
        self.codex_fast_mode.unwrap_or(false)
    }

    pub fn custom_user_agent_header(&self) -> Result<Option<HeaderValue>, InvalidHeaderValue> {
        parse_custom_user_agent(self.custom_user_agent.as_deref())
    }

    pub fn managed_account_id_for(&self, auth_provider: &str) -> Option<String> {
        if let Some(binding) = self.auth_binding.as_ref() {
            if binding.source == AuthBindingSource::ManagedAccount
                && binding.auth_provider.as_deref() == Some(auth_provider)
            {
                return binding.account_id.clone();
            }
        }
        if auth_provider == "github_copilot" {
            return self.github_account_id.clone();
        }
        None
    }
}

/// Parse a provider-level custom User-Agent header. Trim whitespace; empty ->
/// `None`; validity decided byte-wise by `HeaderValue::from_str`.
pub fn parse_custom_user_agent(
    raw: Option<&str>,
) -> Result<Option<HeaderValue>, InvalidHeaderValue> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ua) => HeaderValue::from_str(ua).map(Some),
        None => Ok(None),
    }
}

// ============================================================================
// Universal provider (cross-app shared configuration)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UniversalProviderApps {
    #[serde(default)]
    pub claude: bool,
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub gemini: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "haikuModel")]
    pub haiku_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sonnetModel")]
    pub sonnet_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "opusModel")]
    pub opus_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "reasoningEffort")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeminiModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UniversalProviderModels {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeModelConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexModelConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniversalProvider {
    pub id: String,
    pub name: String,
    #[serde(rename = "providerType")]
    pub provider_type: String,
    pub apps: UniversalProviderApps,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(default)]
    pub models: UniversalProviderModels,
    #[serde(skip_serializing_if = "Option::is_none", rename = "websiteUrl")]
    pub website_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "iconColor")]
    pub icon_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<ProviderMeta>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "createdAt")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sortIndex")]
    pub sort_index: Option<usize>,
}

impl Provider {
    /// Resolve `(base_url, api_key)` for the usage-script path, per app type.
    ///
    /// Ported from cc-switch `provider.rs::resolve_usage_credentials`. Mirrors the
    /// frontend's `a || b || c` fallback (JS `||` skips empty strings).
    pub fn resolve_usage_credentials(
        &self,
        app_type: &crate::app_type::AppType,
    ) -> (String, String) {
        use crate::app_type::AppType;

        let settings = &self.settings_config;
        let str_at =
            |value: Option<&Value>| value.and_then(|v| v.as_str()).unwrap_or("").to_string();

        fn first_non_empty(env: Option<&Value>, keys: &[&str]) -> String {
            let Some(env) = env else {
                return String::new();
            };
            for key in keys {
                if let Some(s) = env.get(key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            String::new()
        }

        let (base_url, api_key) = match app_type {
            AppType::Codex => {
                let auth = settings.get("auth");
                let config_text = settings.get("config").and_then(|v| v.as_str());
                let api_key = crate::apps::codex::extract_codex_api_key(auth, config_text)
                    .unwrap_or_default();
                let base_url = config_text
                    .and_then(crate::apps::codex::extract_codex_base_url)
                    .unwrap_or_default();
                (base_url, api_key)
            }
            AppType::Hermes => (
                str_at(settings.get("base_url")),
                str_at(settings.get("api_key")),
            ),
            AppType::OpenClaw => (
                str_at(settings.get("baseUrl")),
                str_at(settings.get("apiKey")),
            ),
            AppType::OpenCode => {
                let options = settings.get("options");
                (
                    str_at(options.and_then(|o| o.get("baseURL"))),
                    str_at(options.and_then(|o| o.get("apiKey"))),
                )
            }
            AppType::Claude | AppType::ClaudeDesktop => {
                let env = settings.get("env");
                let base_url = str_at(env.and_then(|e| e.get("ANTHROPIC_BASE_URL")));
                let api_key = first_non_empty(
                    env,
                    &[
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_API_KEY",
                        "OPENROUTER_API_KEY",
                        "GOOGLE_API_KEY",
                    ],
                );
                (base_url, api_key)
            }
        };

        (base_url.trim_end_matches('/').to_string(), api_key)
    }
}

// ============================================================================
// OpenCode provider config structures
// ============================================================================

/// OpenCode provider `settings_config` structure (AI SDK package + options + models).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeProviderConfig {
    pub npm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub options: OpenCodeProviderOptions,
    #[serde(default)]
    pub models: HashMap<String, OpenCodeModel>,
}

impl Default for OpenCodeProviderConfig {
    fn default() -> Self {
        Self {
            npm: "@ai-sdk/openai-compatible".to_string(),
            name: None,
            options: OpenCodeProviderOptions::default(),
            models: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenCodeProviderOptions {
    #[serde(rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey", skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenCodeModel {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<OpenCodeModelLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<HashMap<String, Value>>,
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenCodeModelLimit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
}

impl UniversalProvider {
    pub fn new(
        id: String,
        name: String,
        provider_type: String,
        base_url: String,
        api_key: String,
    ) -> Self {
        Self {
            id,
            name,
            provider_type,
            apps: UniversalProviderApps::default(),
            base_url,
            api_key,
            models: UniversalProviderModels::default(),
            website_url: None,
            notes: None,
            icon: None,
            icon_color: None,
            meta: None,
            created_at: Some(chrono::Utc::now().timestamp_millis()),
            sort_index: None,
        }
    }

    /// Generate the Claude sub-provider config from this universal provider.
    pub fn to_claude_provider(&self) -> Option<Provider> {
        if !self.apps.claude {
            return None;
        }

        let models = self.models.claude.as_ref();
        let model = models
            .and_then(|m| m.model.clone())
            .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let haiku = models
            .and_then(|m| m.haiku_model.clone())
            .unwrap_or_else(|| model.clone());
        let sonnet = models
            .and_then(|m| m.sonnet_model.clone())
            .unwrap_or_else(|| model.clone());
        let opus = models
            .and_then(|m| m.opus_model.clone())
            .unwrap_or_else(|| model.clone());

        let settings_config = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": self.base_url,
                "ANTHROPIC_AUTH_TOKEN": self.api_key,
                "ANTHROPIC_MODEL": model,
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": haiku,
                "ANTHROPIC_DEFAULT_SONNET_MODEL": sonnet,
                "ANTHROPIC_DEFAULT_OPUS_MODEL": opus,
            }
        });

        Some(Provider {
            id: format!("universal-claude-{}", self.id),
            name: self.name.clone(),
            settings_config,
            website_url: self.website_url.clone(),
            category: Some("aggregator".to_string()),
            created_at: self.created_at,
            sort_index: self.sort_index,
            notes: self.notes.clone(),
            meta: self.meta.clone(),
            icon: self.icon.clone(),
            icon_color: self.icon_color.clone(),
            in_failover_queue: false,
        })
    }

    /// Generate the Codex sub-provider config from this universal provider.
    pub fn to_codex_provider(&self) -> Option<Provider> {
        if !self.apps.codex {
            return None;
        }

        let models = self.models.codex.as_ref();
        let model = models
            .and_then(|m| m.model.clone())
            .unwrap_or_else(|| "gpt-4o".to_string());
        let reasoning_effort = models
            .and_then(|m| m.reasoning_effort.clone())
            .unwrap_or_else(|| "high".to_string());

        let base_trimmed = self.base_url.trim_end_matches('/');
        let origin_only = match base_trimmed.split_once("://") {
            Some((_scheme, rest)) => !rest.contains('/'),
            None => !base_trimmed.contains('/'),
        };
        let codex_base_url = if base_trimmed.ends_with("/v1") {
            base_trimmed.to_string()
        } else if origin_only {
            format!("{base_trimmed}/v1")
        } else {
            base_trimmed.to_string()
        };

        let config_toml = format!(
            r#"model_provider = "custom"
model = "{model}"
model_reasoning_effort = "{reasoning_effort}"
disable_response_storage = true

[model_providers.custom]
name = "NewAPI"
base_url = "{codex_base_url}"
wire_api = "responses"
requires_openai_auth = true"#
        );

        let settings_config = serde_json::json!({
            "auth": {
                "OPENAI_API_KEY": self.api_key
            },
            "config": config_toml
        });

        Some(Provider {
            id: format!("universal-codex-{}", self.id),
            name: self.name.clone(),
            settings_config,
            website_url: self.website_url.clone(),
            category: Some("aggregator".to_string()),
            created_at: self.created_at,
            sort_index: self.sort_index,
            notes: self.notes.clone(),
            meta: self.meta.clone(),
            icon: self.icon.clone(),
            icon_color: self.icon_color.clone(),
            in_failover_queue: false,
        })
    }
}
