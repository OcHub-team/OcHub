//! OpenClaw provider config codec.
//!
//! OpenClaw is *additive*: every provider lives together inside one on-disk file
//! `~/.openclaw/openclaw.json` (JSON5) under `models.providers.<id>`. RouteDeck
//! stores a single provider's slice as `settingsConfig`, shaped exactly like an
//! [`OpenClawProviderConfig`]:
//!
//! ```json
//! { "baseUrl": …, "apiKey": …, "api": "openai-completions",
//!   "models": [ { "id": …, "name"?: …, "contextWindow"?: …, … } ],
//!   "headers"?: { … } }
//! ```
//!
//! `api` is a **required enum** — one of the five wire protocols OpenClaw
//! understands. This codec edits the provider slice *structurally* (it parses the
//! prior `settingsConfig` into the typed structs and re-serializes), so unknown
//! native keys on both the provider and each model entry survive a round-trip,
//! and *all* models are decoded/encoded rather than just the first.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::{
    str_val, AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues,
    GridColumn, Language, PreviewFile, SelectOption,
};
use crate::apps::openclaw::{OpenClawModelEntry, OpenClawProviderConfig};
use crate::model::ProviderMeta;
use crate::AppType;

/// The five valid `api` wire protocols OpenClaw accepts.
const API_PROTOCOLS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
    "google-generative-ai",
    "bedrock-converse-stream",
];
const DEFAULT_API: &str = "openai-completions";

pub struct OpenClawConfig;

impl AppConfig for OpenClawConfig {
    fn app(&self) -> AppType {
        AppType::OpenClaw
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "供应商",
                vec![
                    FormField::new(
                        "api",
                        "API 协议",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new("openai-completions", "OpenAI Completions")
                                    .with_hint("/chat/completions 风格"),
                                SelectOption::new("openai-responses", "OpenAI Responses")
                                    .with_hint("/responses 风格"),
                                SelectOption::new("anthropic-messages", "Anthropic Messages")
                                    .with_hint("/v1/messages 风格"),
                                SelectOption::new("google-generative-ai", "Google Generative AI")
                                    .with_hint("Gemini generateContent"),
                                SelectOption::new(
                                    "bedrock-converse-stream",
                                    "Bedrock Converse Stream",
                                )
                                .with_hint("AWS Bedrock converse-stream"),
                            ],
                        },
                    )
                    .help("OpenClaw 用此枚举决定请求协议；必填，默认 openai-completions。")
                    .required(),
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://api.example.com/v1".into(),
                        },
                    )
                    .help("写入 baseUrl；OpenClaw 按所选协议在其后拼接路径。"),
                    FormField::new(
                        "api_key",
                        "API Key",
                        FieldKind::Secret {
                            placeholder: "sk-...".into(),
                        },
                    )
                    .help("写入 apiKey。"),
                ],
            ),
            FormSection::new(
                "模型",
                vec![FormField::new(
                    "models",
                    "模型列表",
                    FieldKind::ModelGrid {
                        columns: vec![
                            GridColumn::text("id", "模型 ID", "gpt-4o"),
                            GridColumn::text("name", "显示名", "GPT-4o"),
                            GridColumn::text("context", "上下文窗口", "128000"),
                            GridColumn::toggle("reasoning", "推理"),
                        ],
                    },
                )
                .help("provider.models[]，每行一个模型；全部保留，非破坏式回写。")],
            ),
            FormSection::new(
                "高级",
                vec![FormField::new(
                    "headers",
                    "自定义请求头",
                    FieldKind::KeyValue {
                        key_placeholder: "X-Header".into(),
                        value_placeholder: "value".into(),
                    },
                )
                .help("写入 headers{}，附加到每次请求。")],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();

        // Parse the prior provider slice into the typed struct; defaults for a
        // brand-new provider (Null / {}).
        let provider: OpenClawProviderConfig = serde_json::from_value(settings_config.clone())
            .unwrap_or_else(|_| OpenClawProviderConfig {
                base_url: None,
                api_key: None,
                api: None,
                models: Vec::new(),
                headers: HashMap::new(),
                extra: HashMap::new(),
            });

        let api = provider
            .api
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API.to_string());
        super::set_str(&mut values, "api", api);
        super::set_str(
            &mut values,
            "base_url",
            provider.base_url.unwrap_or_default(),
        );
        super::set_str(&mut values, "api_key", provider.api_key.unwrap_or_default());

        // Decode ALL models into grid rows.
        let rows: Vec<Value> = provider.models.iter().map(model_entry_to_row).collect();
        values.insert("models".into(), Value::Array(rows));

        // headers{} -> KeyValue map.
        let mut headers = Map::new();
        for (k, v) in &provider.headers {
            headers.insert(k.clone(), Value::String(v.clone()));
        }
        values.insert("headers".into(), Value::Object(headers));

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // Start from the prior typed provider so unknown native keys survive.
        let mut provider: OpenClawProviderConfig = serde_json::from_value(prior.clone())
            .unwrap_or_else(|_| OpenClawProviderConfig {
                base_url: None,
                api_key: None,
                api: None,
                models: Vec::new(),
                headers: HashMap::new(),
                extra: HashMap::new(),
            });

        // Required enum — never write an invalid value; fall back to default.
        let api = {
            let raw = str_val(values, "api").trim();
            if API_PROTOCOLS.contains(&raw) {
                raw.to_string()
            } else {
                DEFAULT_API.to_string()
            }
        };
        provider.api = Some(api);

        provider.base_url = non_empty(str_val(values, "base_url"));
        provider.api_key = non_empty(str_val(values, "api_key"));

        // Index prior models by id so per-model native extras (cost, alias,
        // input[], maxTokens, …) survive when the row is re-encoded.
        let prior_by_id: HashMap<String, OpenClawModelEntry> = provider
            .models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| (m.id.clone(), m.clone()))
            .collect();

        let rows = values.get("models").and_then(Value::as_array);
        let mut models: Vec<OpenClawModelEntry> = Vec::new();
        if let Some(rows) = rows {
            for row in rows {
                let id = row_str(row, "id");
                if id.trim().is_empty() {
                    continue; // skip blank rows
                }
                let mut entry =
                    prior_by_id
                        .get(id.trim())
                        .cloned()
                        .unwrap_or_else(|| OpenClawModelEntry {
                            id: id.trim().to_string(),
                            name: None,
                            alias: None,
                            cost: None,
                            context_window: None,
                            extra: HashMap::new(),
                        });
                entry.id = id.trim().to_string();

                entry.name = non_empty(&row_str(row, "name"));

                let ctx = row_str(row, "context");
                entry.context_window = ctx.trim().parse::<u32>().ok();

                let reasoning = row_bool(row, "reasoning");
                if reasoning {
                    entry
                        .extra
                        .insert("reasoning".to_string(), Value::Bool(true));
                } else {
                    entry.extra.remove("reasoning");
                }

                models.push(entry);
            }
        }
        provider.models = models;

        // headers{} from the KeyValue map.
        let mut headers = HashMap::new();
        if let Some(obj) = values.get("headers").and_then(Value::as_object) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    if !k.trim().is_empty() {
                        headers.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        provider.headers = headers;

        let settings_config = serde_json::to_value(&provider).unwrap_or(Value::Null);

        EncodeResult {
            settings_config,
            meta: prior_meta.cloned(),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let text = contents.first().map(String::as_str).unwrap_or("{}");
        let parsed = serde_json::from_str::<Value>(text)
            .map_err(|e| format!("openclaw.json 解析失败: {e}"))?;
        parsed
            .pointer("/models/providers")
            .and_then(Value::as_object)
            .and_then(|m| m.values().next().cloned())
            .ok_or_else(|| "缺少 models.providers.<id> 配置".to_string())
    }

    fn preview(&self, values: &FormValues) -> Vec<PreviewFile> {
        let provider = self.encode(values, &Value::Null, None).settings_config;
        let id = provider_preview_id(values);

        let on_disk = json!({
            "models": {
                "providers": {
                    id: provider,
                }
            }
        });

        vec![PreviewFile {
            filename: "~/.openclaw/openclaw.json".into(),
            language: Language::Json,
            content: serde_json::to_string_pretty(&on_disk).unwrap_or_default(),
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        let api = str_val(values, "api").trim();
        if api.is_empty() {
            issues.push(ConfigIssue::error("API 协议为必填项。").for_field("api"));
        } else if !API_PROTOCOLS.contains(&api) {
            issues.push(
                ConfigIssue::error(format!(
                    "api = \"{api}\" 不是合法协议；必须是 openai-completions / openai-responses / anthropic-messages / google-generative-ai / bedrock-converse-stream 之一。"
                ))
                .for_field("api"),
            );
        }

        if str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 Base URL。").for_field("base_url"));
        }
        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 API Key。").for_field("api_key"));
        }

        // Model-grid sanity: warn on blank/duplicate ids.
        let rows = values.get("models").and_then(Value::as_array);
        let mut seen: HashMap<String, ()> = HashMap::new();
        let mut non_empty_rows = 0usize;
        if let Some(rows) = rows {
            for row in rows {
                let id = row_str(row, "id");
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                non_empty_rows += 1;
                if seen.insert(id.to_string(), ()).is_some() {
                    issues.push(
                        ConfigIssue::warning(format!("模型 ID \"{id}\" 重复。"))
                            .for_field("models"),
                    );
                }
                if !row_str(row, "context").trim().is_empty()
                    && row_str(row, "context").trim().parse::<u32>().is_err()
                {
                    issues.push(
                        ConfigIssue::warning(format!(
                            "模型 \"{id}\" 的上下文窗口不是有效整数，将被忽略。"
                        ))
                        .for_field("models"),
                    );
                }
            }
        }
        if non_empty_rows == 0 {
            issues.push(ConfigIssue::info("尚未配置任何模型。").for_field("models"));
        }

        issues
    }
}

// ---- helpers ----------------------------------------------------------------

/// Turn a typed model entry into a grid row object keyed by the column keys.
fn model_entry_to_row(entry: &OpenClawModelEntry) -> Value {
    let mut row = Map::new();
    row.insert("id".into(), Value::String(entry.id.clone()));
    row.insert(
        "name".into(),
        Value::String(entry.name.clone().unwrap_or_default()),
    );
    row.insert(
        "context".into(),
        Value::String(
            entry
                .context_window
                .map(|c| c.to_string())
                .unwrap_or_default(),
        ),
    );
    let reasoning = entry
        .extra
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    row.insert("reasoning".into(), Value::Bool(reasoning));
    Value::Object(row)
}

/// Read a string cell from a grid row.
fn row_str(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read a boolean cell from a grid row.
fn row_bool(row: &Value, key: &str) -> bool {
    row.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `Some(trimmed)` when non-empty, else `None` (for skip-if-none serde fields).
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Best-effort id for the preview wrapper key; the real id is the provider's map
/// key (chosen elsewhere), so we use the first model id or a placeholder.
fn provider_preview_id(values: &FormValues) -> String {
    values
        .get("models")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find_map(|r| non_empty(&row_str(r, "id"))))
        .unwrap_or_else(|| "<provider-id>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_values() -> FormValues {
        let mut v = FormValues::new();
        super::super::set_str(&mut v, "api", "openai-responses");
        super::super::set_str(&mut v, "base_url", "https://api.example.com/v1");
        super::super::set_str(&mut v, "api_key", "sk-test");
        v.insert(
            "models".into(),
            json!([
                { "id": "gpt-4o", "name": "GPT-4o", "context": "128000", "reasoning": false },
                { "id": "o3", "name": "o3", "context": "200000", "reasoning": true }
            ]),
        );
        v.insert("headers".into(), json!({ "X-Org": "acme" }));
        v
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = OpenClawConfig.decode(&Value::Null, None);
        // BUG FIX (1): default api is a real enum value, not the invalid "openai".
        assert_eq!(str_val(&values, "api"), DEFAULT_API);
        assert!(API_PROTOCOLS.contains(&str_val(&values, "api")));
        assert_eq!(str_val(&values, "base_url"), "");
        assert_eq!(str_val(&values, "api_key"), "");
        assert!(values["models"].as_array().unwrap().is_empty());
    }

    #[test]
    fn decode_empty_object_is_defaults() {
        let values = OpenClawConfig.decode(&json!({}), None);
        assert_eq!(str_val(&values, "api"), DEFAULT_API);
    }

    #[test]
    fn encode_produces_typed_provider_keys() {
        let result = OpenClawConfig.encode(&sample_values(), &Value::Null, None);
        let sc = &result.settings_config;
        assert_eq!(sc["api"].as_str(), Some("openai-responses"));
        assert_eq!(sc["baseUrl"].as_str(), Some("https://api.example.com/v1"));
        assert_eq!(sc["apiKey"].as_str(), Some("sk-test"));
        assert_eq!(sc["headers"]["X-Org"].as_str(), Some("acme"));
        // BUG FIX (2): MULTI-model — both models survive encode.
        let models = sc["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"].as_str(), Some("gpt-4o"));
        assert_eq!(models[0]["contextWindow"].as_u64(), Some(128000));
        assert_eq!(models[1]["id"].as_str(), Some("o3"));
        // reasoning toggle lands in the flattened extra as `reasoning: true`.
        assert_eq!(models[1]["reasoning"].as_bool(), Some(true));
        assert!(models[0].get("reasoning").is_none());
    }

    #[test]
    fn encode_rejects_invalid_api_uses_default() {
        let mut v = sample_values();
        super::super::set_str(&mut v, "api", "openai"); // the old invalid value
        let result = OpenClawConfig.encode(&v, &Value::Null, None);
        assert_eq!(result.settings_config["api"].as_str(), Some(DEFAULT_API));
    }

    #[test]
    fn round_trip_preserves_all_models_and_fields() {
        let original = sample_values();
        let encoded = OpenClawConfig.encode(&original, &Value::Null, None);
        let decoded = OpenClawConfig.decode(&encoded.settings_config, None);
        assert_eq!(str_val(&decoded, "api"), "openai-responses");
        assert_eq!(str_val(&decoded, "base_url"), "https://api.example.com/v1");
        assert_eq!(str_val(&decoded, "api_key"), "sk-test");

        let rows = decoded["models"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"].as_str(), Some("gpt-4o"));
        assert_eq!(rows[0]["context"].as_str(), Some("128000"));
        assert_eq!(rows[0]["reasoning"].as_bool(), Some(false));
        assert_eq!(rows[1]["id"].as_str(), Some("o3"));
        assert_eq!(rows[1]["reasoning"].as_bool(), Some(true));
        assert_eq!(decoded["headers"]["X-Org"].as_str(), Some("acme"));
    }

    #[test]
    fn encode_preserves_native_provider_and_model_extras() {
        // Prior provider with native keys the form never models.
        let prior = json!({
            "api": "openai-completions",
            "baseUrl": "https://old.example.com",
            "apiKey": "sk-old",
            "weirdProviderKey": 42,
            "models": [
                {
                    "id": "gpt-4o",
                    "name": "old name",
                    "contextWindow": 1000,
                    "cost": { "input": 1.5, "output": 2.0 },
                    "maxTokens": 8192
                }
            ]
        });
        let mut v = sample_values();
        // keep gpt-4o, drop the rest, to exercise per-model merge.
        v.insert(
            "models".into(),
            json!([{ "id": "gpt-4o", "name": "GPT-4o", "context": "128000", "reasoning": false }]),
        );
        let result = OpenClawConfig.encode(&v, &prior, None);
        let sc = &result.settings_config;
        // Provider-level native key survives.
        assert_eq!(sc["weirdProviderKey"].as_u64(), Some(42));
        let m = &sc["models"][0];
        // Form fields updated…
        assert_eq!(m["name"].as_str(), Some("GPT-4o"));
        assert_eq!(m["contextWindow"].as_u64(), Some(128000));
        // …but native per-model extras (cost, maxTokens) preserved.
        assert_eq!(m["cost"]["input"].as_f64(), Some(1.5));
        assert_eq!(m["maxTokens"].as_u64(), Some(8192));
    }

    #[test]
    fn validate_errors_on_invalid_api() {
        let mut v = sample_values();
        super::super::set_str(&mut v, "api", "openai");
        let issues = OpenClawConfig.validate(&v);
        assert!(issues
            .iter()
            .any(|i| i.severity == super::super::Severity::Error
                && i.field.as_deref() == Some("api")));
    }

    #[test]
    fn validate_accepts_each_valid_protocol() {
        for proto in API_PROTOCOLS {
            let mut v = sample_values();
            super::super::set_str(&mut v, "api", *proto);
            let issues = OpenClawConfig.validate(&v);
            assert!(
                !issues
                    .iter()
                    .any(|i| i.severity == super::super::Severity::Error
                        && i.field.as_deref() == Some("api")),
                "protocol {proto} should be accepted"
            );
        }
    }

    #[test]
    fn preview_emits_single_nested_openclaw_file() {
        let files = OpenClawConfig.preview(&sample_values());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "~/.openclaw/openclaw.json");
        assert_eq!(files[0].language, Language::Json);
        // Shows the on-disk { models: { providers: { <id>: {...} } } } shape.
        let parsed: Value = serde_json::from_str(&files[0].content).unwrap();
        let providers = parsed["models"]["providers"].as_object().unwrap();
        assert_eq!(providers.len(), 1);
        let (_id, provider) = providers.iter().next().unwrap();
        assert_eq!(provider["api"].as_str(), Some("openai-responses"));
        assert_eq!(provider["models"].as_array().unwrap().len(), 2);
    }
}
