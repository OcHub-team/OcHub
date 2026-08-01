//! Structured editor for Cherry Studio's provider-import payload.

use serde_json::{Value, json};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues, Language,
    PreviewFile, SelectOption, set_str, str_val,
};
use crate::AppType;
use crate::apps::cherry_studio::{DEFAULT_PROVIDER_TYPE, SUPPORTED_PROVIDER_TYPES};
use crate::model::ProviderMeta;

pub struct CherryStudioConfig;

impl AppConfig for CherryStudioConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::CherryStudio.app_id()
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![FormSection::new(
            "Cherry Studio 导入参数",
            vec![
                FormField::new(
                    "type",
                    "协议类型",
                    FieldKind::Select {
                        options: vec![
                            SelectOption::new("openai", "OpenAI Chat Completions"),
                            SelectOption::new("openai-response", "OpenAI Responses"),
                            SelectOption::new("anthropic", "Anthropic Messages"),
                            SelectOption::new("gemini", "Google Gemini"),
                            SelectOption::new("vertexai", "Google Vertex AI"),
                            SelectOption::new("vertex-anthropic", "Vertex Anthropic"),
                            SelectOption::new("ollama", "Ollama"),
                        ],
                    },
                )
                .help("决定 Cherry Studio 为此连接使用的请求协议。")
                .required(),
                FormField::new(
                    "base_url",
                    "Base URL",
                    FieldKind::Text {
                        placeholder: "https://api.example.com/v1".into(),
                    },
                )
                .help("点击导入时作为 baseUrl 传给 Cherry Studio。")
                .required(),
                FormField::new(
                    "api_key",
                    "API Key",
                    FieldKind::Secret {
                        placeholder: "sk-...".into(),
                    },
                )
                .help("API Key 会编码进本机 Deep Link，并由 Cherry Studio 显示确认。")
                .required(),
            ],
        )]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        set_str(
            &mut values,
            "type",
            settings_config
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROVIDER_TYPE),
        );
        set_str(
            &mut values,
            "base_url",
            settings_config
                .get("base_url")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        set_str(
            &mut values,
            "api_key",
            settings_config
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        let mut settings = prior.as_object().cloned().unwrap_or_default();
        settings.insert("type".into(), json!(str_val(values, "type").trim()));
        settings.insert("base_url".into(), json!(str_val(values, "base_url").trim()));
        settings.insert("api_key".into(), json!(str_val(values, "api_key").trim()));
        EncodeResult {
            settings_config: Value::Object(settings),
            meta: prior_meta.cloned(),
        }
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        let settings = self.encode(values, prior, None).settings_config;
        let payload = json!({
            "baseUrl": settings.get("base_url").and_then(Value::as_str).unwrap_or_default(),
            "apiKey": settings.get("api_key").and_then(Value::as_str).unwrap_or_default(),
            "type": settings.get("type").and_then(Value::as_str).unwrap_or(DEFAULT_PROVIDER_TYPE),
        });
        vec![PreviewFile {
            filename: "cherrystudio://providers/api-keys（导入参数）".into(),
            language: Language::Json,
            content: serde_json::to_string_pretty(&payload).unwrap_or_default(),
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        let provider_type = str_val(values, "type").trim();
        if !SUPPORTED_PROVIDER_TYPES.contains(&provider_type) {
            issues.push(
                ConfigIssue::error("请选择 Cherry Studio 支持的协议类型。").for_field("type"),
            );
        }
        let base_url = str_val(values, "base_url").trim();
        if base_url.is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        } else if url::Url::parse(base_url)
            .ok()
            .is_none_or(|url| !matches!(url.scheme(), "http" | "https"))
        {
            issues.push(
                ConfigIssue::error("Base URL 必须是有效的 http:// 或 https:// 地址。")
                    .for_field("base_url"),
            );
        }
        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::error("API Key 不能为空。").for_field("api_key"));
        }
        issues
    }
}
