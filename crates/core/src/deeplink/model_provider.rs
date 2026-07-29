//! Model-provider (Gateway Station) import from `ochub://` deep links.

use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use super::DeepLinkImportRequest;
use crate::error::AppError;
use crate::gateway::apply;
use crate::gateway::types::{
    Dialect, GatewayChannel, GatewayModelRule, GatewayReasoningConfig, GatewayReasoningMode,
    GatewayRoute, pattern_matches,
};
use crate::{AppState, AppType};

pub const MODEL_PROVIDER_SCHEMA: &str = "io.ochub.model-provider/v1";
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_ENDPOINTS: usize = 8;
const MAX_MODELS: usize = 500;
const MAX_MODEL_RULES: usize = 100;
const MAX_STRING_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportSource {
    pub id: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderImportEndpoint {
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProviderImportTarget {
    pub app: AppType,
    #[serde(default)]
    pub preferred_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportModelRule {
    pub model: String,
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub dialect: Option<Dialect>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportReasoning {
    #[serde(default = "default_reasoning_mode")]
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

fn default_reasoning_mode() -> GatewayReasoningMode {
    GatewayReasoningMode::Passthrough
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

impl Default for ModelProviderImportReasoning {
    fn default() -> Self {
        Self {
            mode: default_reasoning_mode(),
            low_budget: default_low_budget(),
            medium_budget: default_medium_budget(),
            high_budget: default_high_budget(),
            max_budget: default_max_budget(),
        }
    }
}

impl From<ModelProviderImportReasoning> for GatewayReasoningConfig {
    fn from(value: ModelProviderImportReasoning) -> Self {
        Self {
            mode: value.mode,
            low_budget: value.low_budget,
            medium_budget: value.medium_budget,
            high_budget: value.high_budget,
            max_budget: value.max_budget,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportManifest {
    pub schema: String,
    #[serde(default)]
    pub source: Option<ModelProviderImportSource>,
    pub name: String,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    pub dialects: Vec<Dialect>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub websocket_enabled: bool,
    pub endpoints: Vec<ModelProviderImportEndpoint>,
    #[serde(default)]
    pub apply_to: Vec<ModelProviderImportTarget>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub model_rules: Vec<ModelProviderImportModelRule>,
    #[serde(default)]
    pub reasoning: ModelProviderImportReasoning,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub requires: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

impl ModelProviderImportManifest {
    pub fn website_url(&self) -> Option<&str> {
        self.website.as_deref().or_else(|| {
            self.source
                .as_ref()
                .and_then(|source| source.website.as_deref())
        })
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema != MODEL_PROVIDER_SCHEMA {
            return Err(AppError::InvalidInput(format!(
                "Unsupported model-provider schema: {}",
                self.schema
            )));
        }
        validate_required_string("name", &self.name)?;
        if !self.requires.is_empty() {
            return Err(AppError::InvalidInput(format!(
                "Unsupported required capabilities: {}",
                self.requires.join(", ")
            )));
        }
        if self.endpoints.is_empty() || self.endpoints.len() > MAX_ENDPOINTS {
            return Err(AppError::InvalidInput(format!(
                "Model provider must contain between 1 and {MAX_ENDPOINTS} endpoints"
            )));
        }
        if self.model_rules.len() > MAX_MODEL_RULES {
            return Err(AppError::InvalidInput(format!(
                "Model provider contains more than {MAX_MODEL_RULES} model rules"
            )));
        }
        if let Some(api_key) = &self.api_key {
            validate_string_size("apiKey", api_key)?;
        }
        if let Some(website) = self.website_url() {
            validate_http_url("website", website)?;
        }
        if let Some(source) = &self.source {
            validate_required_string("source.id", &source.id)?;
            if let Some(revision) = &source.revision {
                validate_string_size("source.revision", revision)?;
            }
        }
        if let Some(model) = &self.default_model {
            validate_required_string("defaultModel", model)?;
        }

        if self.dialects.is_empty() {
            return Err(AppError::InvalidInput(
                "dialects must not be empty".to_string(),
            ));
        }
        let mut dialects = HashSet::new();
        for dialect in &self.dialects {
            if !dialects.insert(*dialect) {
                return Err(AppError::InvalidInput(format!(
                    "Model provider contains duplicate dialect {}",
                    dialect.as_str()
                )));
            }
        }
        if self.websocket_enabled && !dialects.contains(&Dialect::Responses) {
            return Err(AppError::InvalidInput(
                "websocketEnabled requires the responses dialect".to_string(),
            ));
        }
        if self.models.len() > MAX_MODELS {
            return Err(AppError::InvalidInput(format!(
                "Model provider contains more than {MAX_MODELS} models"
            )));
        }
        let mut models = HashSet::new();
        for model in &self.models {
            validate_required_string("models", model)?;
            if !models.insert(model) {
                return Err(AppError::InvalidInput(format!(
                    "Model provider contains duplicate model {model}"
                )));
            }
        }

        let mut endpoints = HashSet::new();
        for (endpoint_index, endpoint) in self.endpoints.iter().enumerate() {
            validate_http_url(
                &format!("endpoints[{endpoint_index}].baseUrl"),
                &endpoint.base_url,
            )?;
            let normalized = endpoint.base_url.trim_end_matches('/');
            if !endpoints.insert(normalized) {
                return Err(AppError::InvalidInput(format!(
                    "Model provider contains duplicate endpoint {normalized}"
                )));
            }
        }

        let mut target_apps = HashSet::new();
        if !self.apply_to.is_empty() && !self.enabled {
            return Err(AppError::InvalidInput(
                "enabled must be true when applyTo is not empty".to_string(),
            ));
        }
        for (target_index, target) in self.apply_to.iter().enumerate() {
            if !target_apps.insert(target.app) {
                return Err(AppError::InvalidInput(format!(
                    "applyTo contains duplicate app {}",
                    target.app.as_str()
                )));
            }
            if !apply::supported_apps().contains(&target.app) {
                return Err(AppError::InvalidInput(format!(
                    "applyTo[{target_index}] uses unsupported app {}",
                    target.app.as_str()
                )));
            }
            if !self
                .dialects
                .iter()
                .any(|dialect| apply::dialect_compatible(*dialect, target.app))
            {
                return Err(AppError::InvalidInput(format!(
                    "Provider interfaces are incompatible with applyTo app {}",
                    target.app.as_str()
                )));
            }
            if let Some(model) = &target.preferred_model {
                validate_required_string(
                    &format!("applyTo[{target_index}].preferredModel"),
                    model,
                )?;
                if !self.models.is_empty()
                    && !self
                        .models
                        .iter()
                        .any(|pattern| pattern_matches(pattern, model))
                {
                    return Err(AppError::InvalidInput(format!(
                        "applyTo[{target_index}].preferredModel is not allowed by provider models"
                    )));
                }
            }
        }

        for rule in &self.model_rules {
            validate_required_string("modelRules.model", &rule.model)?;
            if !rule.upstream_model.is_empty() {
                validate_required_string("modelRules.upstreamModel", &rule.upstream_model)?;
            }
            if rule
                .dialect
                .is_some_and(|dialect| !dialects.contains(&dialect))
            {
                return Err(AppError::InvalidInput(format!(
                    "Model rule dialect {} is not enabled by this provider",
                    rule.dialect.expect("checked above").as_str()
                )));
            }
        }

        let reasoning = &self.reasoning;
        if reasoning.low_budget == 0
            || reasoning.low_budget > reasoning.medium_budget
            || reasoning.medium_budget > reasoning.high_budget
            || reasoning.high_budget > reasoning.max_budget
        {
            return Err(AppError::InvalidInput(
                "Reasoning budgets must be greater than zero and ordered low ≤ medium ≤ high ≤ max"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PreparedModelProviderImport {
    pub manifest: ModelProviderImportManifest,
    pub route: GatewayRoute,
    pub channels: Vec<GatewayChannel>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportApplyFailure {
    pub app: AppType,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderImportResult {
    pub route_id: String,
    pub channel_ids: Vec<String>,
    pub applied_to: Vec<AppType>,
    pub apply_failures: Vec<ModelProviderImportApplyFailure>,
    pub imported: bool,
}

pub fn decode_model_provider_request(
    request: &DeepLinkImportRequest,
) -> Result<ModelProviderImportManifest, AppError> {
    if request.resource != "model-provider" {
        return Err(AppError::InvalidInput(format!(
            "Expected model-provider resource, got '{}'",
            request.resource
        )));
    }
    let payload = request
        .payload
        .as_deref()
        .ok_or_else(|| AppError::InvalidInput("Missing 'payload' parameter".to_string()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| AppError::InvalidInput(format!("Invalid Base64URL payload: {error}")))?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(AppError::InvalidInput(format!(
            "Decoded payload exceeds {MAX_PAYLOAD_BYTES} bytes"
        )));
    }
    let manifest: ModelProviderImportManifest = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::InvalidInput(format!("Invalid model-provider JSON: {error}")))?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn prepare_model_provider_import(
    manifest: ModelProviderImportManifest,
) -> Result<PreparedModelProviderImport, AppError> {
    manifest.validate()?;
    let station_id = Uuid::new_v4().to_string();
    let route_id = format!("{}{station_id}", apply::STATION_ROUTE_PREFIX);
    let api_key = manifest.api_key.clone().unwrap_or_default();
    let mut channels = Vec::new();
    for (endpoint_index, endpoint) in manifest.endpoints.iter().enumerate() {
        let endpoint_id = Uuid::new_v4().to_string();
        for dialect in &manifest.dialects {
            channels.push(GatewayChannel {
                id: format!(
                    "station-channel:{station_id}:{endpoint_id}:{}",
                    dialect.as_str()
                ),
                endpoint_id: Some(endpoint_id.clone()),
                name: manifest.name.trim().to_string(),
                dialect: *dialect,
                base_url: endpoint.base_url.trim_end_matches('/').to_string(),
                api_key: api_key.clone(),
                path_override: None,
                models: manifest.models.clone(),
                model_override: None,
                priority: endpoint_index as i32 * 10,
                weight: 1,
                enabled: manifest.enabled,
                extra_headers: Vec::new(),
                imported_from: manifest
                    .source
                    .as_ref()
                    .map(|source| format!("deeplink:{}", source.id)),
            });
        }
    }
    let route = GatewayRoute {
        id: route_id,
        name: manifest.name.trim().to_string(),
        website_url: manifest.website_url().map(ToOwned::to_owned),
        app_type: None,
        channel_ids: channels.iter().map(|channel| channel.id.clone()).collect(),
        default_model: manifest.default_model.clone(),
        model_rules: manifest
            .model_rules
            .iter()
            .map(|rule| GatewayModelRule {
                model: rule.model.clone(),
                upstream_model: rule.upstream_model.clone(),
                channel_id: None,
                dialect: rule.dialect,
            })
            .collect(),
        reasoning: manifest.reasoning.clone().into(),
        websocket_enabled: manifest.websocket_enabled,
        enabled: manifest.enabled,
        created_at: chrono::Utc::now().timestamp(),
    };
    route.validate().map_err(AppError::InvalidInput)?;
    Ok(PreparedModelProviderImport {
        manifest,
        route,
        channels,
    })
}

pub fn import_model_provider_from_deeplink(
    state: &AppState,
    request: DeepLinkImportRequest,
) -> Result<ModelProviderImportResult, AppError> {
    let manifest = decode_model_provider_request(&request)?;
    let prepared = prepare_model_provider_import(manifest)?;
    if prepared
        .channels
        .first()
        .is_none_or(|channel| channel.api_key.trim().is_empty())
    {
        return Err(AppError::InvalidInput(
            "API key is required for CLI import".to_string(),
        ));
    }
    state
        .db
        .save_gateway_station(&prepared.channels, &prepared.route, &[])?;
    let mut applied_to = Vec::new();
    let mut apply_failures = Vec::new();
    if !prepared.manifest.apply_to.is_empty() {
        let mut config = state.db.get_gateway_config()?;
        if !config.enabled {
            config.enabled = true;
            state.db.set_gateway_config(&config)?;
        }
        let base_url = format!("http://127.0.0.1:{}", config.port);
        for target in &prepared.manifest.apply_to {
            let result = apply::station_model_policy(state, target.app, &prepared.route).and_then(
                |mut policy| {
                    policy.preferred_model = target.preferred_model.clone();
                    apply::apply_station_to_app_with_policy(
                        state,
                        target.app,
                        &base_url,
                        &prepared.route.id,
                        policy,
                    )
                },
            );
            match result {
                Ok(_) => applied_to.push(target.app),
                Err(error) => apply_failures.push(ModelProviderImportApplyFailure {
                    app: target.app,
                    error: error.to_string(),
                }),
            }
        }
    }
    Ok(ModelProviderImportResult {
        route_id: prepared.route.id,
        channel_ids: prepared
            .channels
            .into_iter()
            .map(|channel| channel.id)
            .collect(),
        applied_to,
        apply_failures,
        imported: true,
    })
}

pub fn encode_model_provider_payload(
    manifest: &ModelProviderImportManifest,
) -> Result<String, AppError> {
    manifest.validate()?;
    let json = serde_json::to_vec(manifest)
        .map_err(|error| AppError::Config(format!("JSON serialization failed: {error}")))?;
    if json.len() > MAX_PAYLOAD_BYTES {
        return Err(AppError::InvalidInput(format!(
            "Encoded payload source exceeds {MAX_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(URL_SAFE_NO_PAD.encode(json))
}

fn validate_required_string(field: &str, value: &str) -> Result<(), AppError> {
    validate_string_size(field, value)?;
    if value.trim().is_empty() {
        return Err(AppError::InvalidInput(format!("{field} must not be empty")));
    }
    Ok(())
}

fn validate_string_size(field: &str, value: &str) -> Result<(), AppError> {
    if value.len() > MAX_STRING_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{field} exceeds {MAX_STRING_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_http_url(field: &str, value: &str) -> Result<(), AppError> {
    validate_required_string(field, value)?;
    let url = Url::parse(value)
        .map_err(|error| AppError::InvalidInput(format!("Invalid {field}: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::InvalidInput(format!(
            "{field} must use http or https"
        )));
    }
    if url.host_str().is_none() {
        return Err(AppError::InvalidInput(format!(
            "{field} must contain a host"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::InvalidInput(format!(
            "{field} must not contain credentials"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ModelProviderImportManifest {
        ModelProviderImportManifest {
            schema: MODEL_PROVIDER_SCHEMA.to_string(),
            source: None,
            name: "Aster API".to_string(),
            website: Some("https://aster.example".to_string()),
            api_key: Some("sk-secret".to_string()),
            dialects: vec![Dialect::Messages, Dialect::Responses],
            models: vec!["claude-*".to_string()],
            websocket_enabled: false,
            endpoints: vec![ModelProviderImportEndpoint {
                base_url: "https://api.aster.example/".to_string(),
            }],
            apply_to: Vec::new(),
            default_model: Some("claude-sonnet-4-5".to_string()),
            model_rules: Vec::new(),
            reasoning: ModelProviderImportReasoning::default(),
            enabled: true,
            requires: Vec::new(),
        }
    }

    #[test]
    fn payload_round_trip_defaults_to_passthrough() {
        let manifest = manifest();
        let payload = encode_model_provider_payload(&manifest).unwrap();
        let request = DeepLinkImportRequest {
            version: "v1".to_string(),
            resource: "model-provider".to_string(),
            payload: Some(payload),
            ..Default::default()
        };
        let decoded = decode_model_provider_request(&request).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.reasoning.mode, GatewayReasoningMode::Passthrough);
    }

    #[test]
    fn parser_accepts_a_model_provider_payload_url() {
        let payload = encode_model_provider_payload(&manifest()).unwrap();
        let uri = format!("ochub://v1/import?resource=model-provider&payload={payload}");
        let request = crate::deeplink::parse_deeplink_url(&uri).unwrap();
        let decoded = decode_model_provider_request(&request).unwrap();
        assert_eq!(decoded.name, "Aster API");
        assert_eq!(decoded.api_key.as_deref(), Some("sk-secret"));
    }

    #[test]
    fn prepared_station_creates_one_channel_per_dialect() {
        let prepared = prepare_model_provider_import(manifest()).unwrap();
        assert_eq!(prepared.channels.len(), 2);
        assert_eq!(
            prepared.route.reasoning.mode,
            GatewayReasoningMode::Passthrough
        );
        assert!(
            prepared
                .channels
                .iter()
                .all(|channel| channel.api_key == "sk-secret")
        );
        assert!(
            prepared
                .channels
                .iter()
                .all(|channel| channel.base_url == "https://api.aster.example")
        );
        assert!(!prepared.route.websocket_enabled);
    }

    #[test]
    fn websocket_capability_is_provider_scoped_and_requires_responses() {
        let mut enabled_manifest = manifest();
        enabled_manifest.websocket_enabled = true;
        let prepared = prepare_model_provider_import(enabled_manifest).unwrap();
        assert!(prepared.route.websocket_enabled);

        let mut invalid = manifest();
        invalid.websocket_enabled = true;
        invalid.dialects = vec![Dialect::Messages];
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_dialects() {
        let mut manifest = manifest();
        manifest.dialects = vec![Dialect::Messages, Dialect::Messages];
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn endpoints_are_failover_addresses_for_the_same_capabilities() {
        let mut manifest = manifest();
        manifest.endpoints.push(ModelProviderImportEndpoint {
            base_url: "https://backup.aster.example/".to_string(),
        });

        let prepared = prepare_model_provider_import(manifest).unwrap();
        assert_eq!(prepared.channels.len(), 4);
        assert_eq!(
            prepared
                .channels
                .iter()
                .map(|channel| channel.priority)
                .collect::<Vec<_>>(),
            vec![0, 0, 10, 10]
        );
        assert!(prepared.channels.iter().all(|channel| {
            channel.models == ["claude-*"]
                && matches!(channel.dialect, Dialect::Messages | Dialect::Responses)
        }));
    }

    #[test]
    fn rejects_the_old_endpoint_scoped_capability_shape() {
        let json = r#"{
            "schema":"io.ochub.model-provider/v1",
            "name":"Old shape",
            "dialects":["messages"],
            "endpoints":[{
                "baseUrl":"https://api.example.com",
                "dialects":["messages"],
                "models":["model"]
            }]
        }"#;
        let error = serde_json::from_str::<ModelProviderImportManifest>(json).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn accepts_multiple_compatible_application_targets() {
        let mut manifest = manifest();
        manifest.models.push("gpt-5.4".to_string());
        manifest.apply_to = vec![
            ModelProviderImportTarget {
                app: AppType::Claude,
                preferred_model: Some("claude-sonnet-4-5".to_string()),
            },
            ModelProviderImportTarget {
                app: AppType::Codex,
                preferred_model: Some("gpt-5.4".to_string()),
            },
            ModelProviderImportTarget {
                app: AppType::OpenCode,
                preferred_model: None,
            },
        ];
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_targets_or_applying_a_disabled_provider() {
        let mut duplicate = manifest();
        duplicate.apply_to = vec![
            ModelProviderImportTarget {
                app: AppType::Codex,
                preferred_model: None,
            },
            ModelProviderImportTarget {
                app: AppType::Codex,
                preferred_model: None,
            },
        ];
        assert!(duplicate.validate().is_err());

        let mut disabled = manifest();
        disabled.enabled = false;
        disabled.apply_to = vec![ModelProviderImportTarget {
            app: AppType::Claude,
            preferred_model: None,
        }];
        assert!(disabled.validate().is_err());

        let mut unknown_model = manifest();
        unknown_model.apply_to = vec![ModelProviderImportTarget {
            app: AppType::Codex,
            preferred_model: Some("gpt-5.4".to_string()),
        }];
        assert!(unknown_model.validate().is_err());
    }
}
