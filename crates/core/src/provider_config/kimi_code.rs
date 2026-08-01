//! Structured editor codec for Kimi Code's providers/models configuration.

use serde_json::{Map, Value, json};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues,
    GridColumn, Language, PreviewFile, SelectOption, set_str, str_val,
};
use crate::AppType;
use crate::model::ProviderMeta;

const DEFAULT_PROVIDER_TYPE: &str = "openai";

pub struct KimiCodeConfig;

impl AppConfig for KimiCodeConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::KimiCode.app_id()
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "供应商",
                vec![
                    FormField::new(
                        "provider_id",
                        "Provider ID",
                        FieldKind::Text {
                            placeholder: "my-provider".into(),
                        },
                    )
                    .help("写入 config.toml 的 [providers.<id>]，模型通过该 ID 引用供应商。")
                    .required(),
                    FormField::new(
                        "provider_type",
                        "协议类型",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new("openai", "OpenAI Chat Completions"),
                                SelectOption::new("openai_responses", "OpenAI Responses"),
                                SelectOption::new("anthropic", "Anthropic Messages"),
                                SelectOption::new("kimi", "Kimi"),
                                SelectOption::new("google-genai", "Google GenAI"),
                                SelectOption::new("vertexai", "Vertex AI"),
                            ],
                        },
                    )
                    .help("必须与上游实际协议一致。")
                    .required(),
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://api.example.com/v1".into(),
                        },
                    ),
                    FormField::new(
                        "api_key",
                        "API Key",
                        FieldKind::Secret {
                            placeholder: "sk-...".into(),
                        },
                    ),
                ],
            ),
            FormSection::new(
                "模型",
                vec![
                    FormField::new(
                        "default_model",
                        "默认模型别名",
                        FieldKind::Text {
                            placeholder: "gpt-4.1".into(),
                        },
                    )
                    .help("必须与下方某一行的模型别名一致；切换供应商时会写入 default_model。")
                    .required(),
                    FormField::new(
                        "models",
                        "模型列表",
                        FieldKind::ModelGrid {
                            columns: vec![
                                GridColumn::text("alias", "模型别名", "gpt-4.1"),
                                GridColumn::text("model", "上游模型 ID", "gpt-4.1"),
                                GridColumn::text("context", "上下文", "128000"),
                                GridColumn::text("output", "最大输出", "16384"),
                                GridColumn::text("display_name", "显示名", "GPT-4.1"),
                                GridColumn::text(
                                    "capabilities",
                                    "能力",
                                    "thinking,tool_use,image_in",
                                ),
                            ],
                        },
                    )
                    .help("上下文为必填正整数；能力使用英文逗号分隔。"),
                ],
            ),
            FormSection::new(
                "高级",
                vec![
                    FormField::new(
                        "custom_headers",
                        "自定义请求头",
                        FieldKind::KeyValue {
                            key_placeholder: "X-Header".into(),
                            value_placeholder: "value".into(),
                        },
                    ),
                    FormField::new(
                        "env",
                        "凭据环境映射",
                        FieldKind::KeyValue {
                            key_placeholder: "OPENAI_API_KEY".into(),
                            value_placeholder: "sk-...".into(),
                        },
                    )
                    .help("这些键写入 [providers.<id>.env]，并不会修改 shell 环境。"),
                ],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let providers = settings_config.get("providers").and_then(Value::as_object);
        let (provider_id, provider) = providers
            .and_then(|items| items.iter().next())
            .map(|(id, value)| (id.as_str(), value))
            .unwrap_or(("", &Value::Null));
        set_str(&mut values, "provider_id", provider_id);
        set_str(
            &mut values,
            "provider_type",
            provider
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROVIDER_TYPE),
        );
        set_str(
            &mut values,
            "base_url",
            provider
                .get("base_url")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        set_str(
            &mut values,
            "api_key",
            provider
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        values.insert(
            "custom_headers".into(),
            string_map(provider.get("custom_headers")),
        );
        values.insert("env".into(), string_map(provider.get("env")));

        let default_model = settings_config
            .get("default_model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        set_str(&mut values, "default_model", default_model);

        let mut rows = Vec::new();
        if let Some(models) = settings_config.get("models").and_then(Value::as_object) {
            let mut entries = models.iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (alias, model) in entries {
                rows.push(json!({
                    "alias": alias,
                    "model": model.get("model").and_then(Value::as_str).unwrap_or(alias),
                    "context": scalar_string(model.get("max_context_size")),
                    "output": scalar_string(model.get("max_output_size")),
                    "display_name": model.get("display_name").and_then(Value::as_str).unwrap_or_default(),
                    "capabilities": string_list(model.get("capabilities")),
                }));
            }
        }
        values.insert("models".into(), Value::Array(rows));
        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        let provider_id = str_val(values, "provider_id").trim();
        let prior_provider = prior
            .get("providers")
            .and_then(Value::as_object)
            .and_then(|providers| providers.values().next())
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut provider = prior_provider;
        set_or_remove(&mut provider, "type", str_val(values, "provider_type"));
        set_or_remove(&mut provider, "base_url", str_val(values, "base_url"));
        set_or_remove(&mut provider, "api_key", str_val(values, "api_key"));
        set_map_or_remove(
            &mut provider,
            "custom_headers",
            values.get("custom_headers"),
        );
        set_map_or_remove(&mut provider, "env", values.get("env"));

        let prior_models = prior.get("models").and_then(Value::as_object);
        let models = encode_models(values, provider_id, prior_models);
        let default_model = {
            let requested = str_val(values, "default_model").trim();
            if requested.is_empty() {
                models.keys().next().cloned().unwrap_or_default()
            } else {
                requested.to_string()
            }
        };
        let settings_config = json!({
            "default_provider": provider_id,
            "default_model": default_model,
            "providers": { provider_id: Value::Object(provider) },
            "models": models,
        });
        EncodeResult {
            settings_config,
            meta: prior_meta.cloned(),
        }
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        let encoded = self.encode(values, prior, None).settings_config;
        let content = toml::to_string_pretty(&encoded).unwrap_or_default();
        vec![PreviewFile {
            filename: "~/.kimi-code/config.toml".into(),
            language: Language::Toml,
            content,
        }]
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let text = contents.first().map(String::as_str).unwrap_or("");
        let parsed = toml::from_str::<toml::Value>(text)
            .map_err(|error| format!("config.toml 解析失败: {error}"))?;
        serde_json::to_value(parsed).map_err(|error| format!("config.toml 转换失败: {error}"))
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        if str_val(values, "provider_id").trim().is_empty() {
            issues.push(ConfigIssue::error("Provider ID 不能为空。").for_field("provider_id"));
        }
        if str_val(values, "provider_type").trim().is_empty() {
            issues.push(ConfigIssue::error("协议类型不能为空。").for_field("provider_type"));
        }
        let Some(rows) = values.get("models").and_then(Value::as_array) else {
            issues.push(ConfigIssue::error("至少需要配置一个模型。").for_field("models"));
            return issues;
        };
        let aliases = rows
            .iter()
            .filter_map(|row| row.get("alias").and_then(Value::as_str))
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .collect::<Vec<_>>();
        if aliases.is_empty() {
            issues.push(ConfigIssue::error("至少需要配置一个模型。").for_field("models"));
        }
        for row in rows {
            let alias = row
                .get("alias")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if alias.is_empty() {
                continue;
            }
            if row
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                issues.push(
                    ConfigIssue::error(format!("模型 {alias} 缺少上游模型 ID。"))
                        .for_field("models"),
                );
            }
            let context = row
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if context
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .is_none()
            {
                issues.push(
                    ConfigIssue::error(format!("模型 {alias} 的上下文必须是正整数。"))
                        .for_field("models"),
                );
            }
        }
        let default_model = str_val(values, "default_model").trim();
        if !default_model.is_empty() && !aliases.contains(&default_model) {
            issues.push(
                ConfigIssue::error("默认模型别名必须存在于模型列表中。").for_field("default_model"),
            );
        }
        issues
    }
}

fn string_map(value: Option<&Value>) -> Value {
    Value::Object(
        value
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    )
}

fn scalar_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn string_list(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn set_or_remove(target: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        target.remove(key);
    } else {
        target.insert(key.into(), Value::String(value.into()));
    }
}

fn set_map_or_remove(target: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    match value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
    {
        Some(map) => {
            target.insert(key.into(), Value::Object(map.clone()));
        }
        None => {
            target.remove(key);
        }
    }
}

fn encode_models(
    values: &FormValues,
    provider_id: &str,
    prior: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut models = Map::new();
    let Some(rows) = values.get("models").and_then(Value::as_array) else {
        return models;
    };
    for row in rows {
        let Some(row) = row.as_object() else { continue };
        let alias = row
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if alias.is_empty() {
            continue;
        }
        let mut model = prior
            .and_then(|items| items.get(alias))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        model.insert("provider".into(), Value::String(provider_id.to_string()));
        set_or_remove(
            &mut model,
            "model",
            row.get("model").and_then(Value::as_str).unwrap_or(""),
        );
        set_or_remove(
            &mut model,
            "display_name",
            row.get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        set_u64_or_remove(
            &mut model,
            "max_context_size",
            row.get("context").and_then(Value::as_str).unwrap_or(""),
        );
        set_u64_or_remove(
            &mut model,
            "max_output_size",
            row.get("output").and_then(Value::as_str).unwrap_or(""),
        );
        let capabilities = row
            .get("capabilities")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| Value::String(item.to_string()))
            .collect::<Vec<_>>();
        if capabilities.is_empty() {
            model.remove("capabilities");
        } else {
            model.insert("capabilities".into(), Value::Array(capabilities));
        }
        models.insert(alias.to_string(), Value::Object(model));
    }
    models
}

fn set_u64_or_remove(target: &mut Map<String, Value>, key: &str, value: &str) {
    match value.trim().parse::<u64>().ok().filter(|value| *value > 0) {
        Some(value) => {
            target.insert(key.into(), Value::Number(value.into()));
        }
        None => {
            target.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_builds_native_kimi_sections() {
        let codec = KimiCodeConfig;
        let mut values = FormValues::new();
        set_str(&mut values, "provider_id", "openrouter");
        set_str(&mut values, "provider_type", "openai");
        set_str(&mut values, "base_url", "https://openrouter.ai/api/v1");
        set_str(&mut values, "api_key", "sk-test");
        set_str(&mut values, "default_model", "claude");
        values.insert(
            "models".into(),
            json!([{
                "alias": "claude", "model": "anthropic/claude-sonnet-4", "context": "200000",
                "output": "", "display_name": "Claude", "capabilities": "thinking,tool_use"
            }]),
        );
        let encoded = codec.encode(&values, &Value::Null, None).settings_config;
        assert_eq!(encoded["default_model"], "claude");
        assert_eq!(encoded["models"]["claude"]["provider"], "openrouter");
        assert_eq!(encoded["models"]["claude"]["max_context_size"], 200000);
        assert!(codec.validate(&values).is_empty());
    }
}
