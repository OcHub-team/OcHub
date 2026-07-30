use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::GatewayStatus;
use crate::services::provider::LiveDrift;
use crate::services::usage_stats::ProviderLimitStatus;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppModeDto {
    Switch,
    Additive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSummary {
    pub id: String,
    pub display_name: String,
    pub enabled: bool,
    pub mode: AppModeDto,
    pub config_dir: Option<String>,
    pub config_error: Option<String>,
    pub supports_provider: bool,
    pub supports_mcp: bool,
    pub supports_skills: bool,
    pub user_manifest: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSummary {
    pub version: String,
    pub data_dir: String,
    pub database_path: String,
    pub enabled_apps: usize,
    pub registered_apps: usize,
    pub gateway: GatewayStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingDefault {
    pub app: String,
    pub multiplier: String,
    pub model_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderListItem {
    pub id: String,
    pub name: String,
    pub app: String,
    pub current: bool,
    pub category: Option<String>,
    pub website_url: Option<String>,
    pub sort_index: Option<usize>,
    pub live_config_managed: Option<bool>,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDetails {
    pub app: String,
    pub provider: crate::Provider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSwitchPlan {
    pub app: String,
    pub provider_id: String,
    pub current_provider_id: Option<String>,
    pub config_path: String,
    pub drift: LiveDrift,
    pub would_change: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSchemaDto {
    pub app: String,
    pub sections: Vec<ConfigSectionDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSectionDto {
    pub title: String,
    pub advanced: bool,
    pub fields: Vec<ConfigFieldDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFieldDto {
    pub id: String,
    pub label: String,
    pub help: Option<String>,
    pub required: bool,
    pub visible_when: Option<(String, String)>,
    pub kind: ConfigFieldKindDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ConfigFieldKindDto {
    Text {
        placeholder: String,
    },
    Secret {
        placeholder: String,
    },
    Select {
        options: Vec<Value>,
    },
    Toggle,
    KeyValue {
        key_placeholder: String,
        value_placeholder: String,
    },
    ModelGrid {
        columns: Vec<Value>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub app: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitItem {
    pub app: String,
    pub provider_id: String,
    pub provider_name: String,
    pub status: ProviderLimitStatus,
}

#[derive(Debug, Clone)]
pub struct OperationOutcome<T> {
    pub data: T,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub healthy: bool,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub enabled: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetails {
    pub plugin: PluginSummary,
    pub manifest: String,
}
