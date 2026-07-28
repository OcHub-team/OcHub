//! OpenCode provider config codec.
//!
//! OpenCode is *additive*: it keeps one shared `~/.config/opencode/opencode.json`
//! and OcHub writes each provider under `provider.<id>`. The
//! `settingsConfig` we store **is** that per-provider object, shaped like an
//! [`OpenCodeProviderConfig`](crate::model::OpenCodeProviderConfig):
//!
//! ```jsonc
//! {
//!   "npm": "@ai-sdk/openai-compatible",   // REQUIRED: the AI-SDK package
//!   "name": "My Provider",                 // optional display name
//!   "options": { "baseURL": …, "apiKey": …, "headers": {…}, …extra },
//!   "models": { "<modelId>": { "name": …, "limit": { "context", "output" }, … } }
//! }
//! ```
//!
//! This codec edits that object *structurally* — it merges into the prior JSON so
//! native keys it does not model (top-level extras, per-model `options`, the
//! `setCacheKey`-style extras under `options`) survive a round-trip, rather than
//! collapsing the provider down to name/baseURL/key/one-model like the legacy
//! generic form did.

use serde_json::{Map, Value, json};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues,
    GridColumn, Language, PreviewFile, SelectOption, set_str, str_val,
};
use crate::AppType;
use crate::model::ProviderMeta;

/// The five AI-SDK packages OpenCode understands as a provider `npm`.
const NPM_OPENAI_COMPATIBLE: &str = "@ai-sdk/openai-compatible";
const NPM_OPENAI: &str = "@ai-sdk/openai";
const NPM_ANTHROPIC: &str = "@ai-sdk/anthropic";
const NPM_BEDROCK: &str = "@ai-sdk/amazon-bedrock";
const NPM_GOOGLE: &str = "@ai-sdk/google";

/// Keys that are modelled by dedicated fields and therefore must not leak into
/// the `options_extra` key/value map on decode.
const KNOWN_OPTION_KEYS: [&str; 3] = ["baseURL", "apiKey", "headers"];

pub struct OpenCodeConfig;

impl AppConfig for OpenCodeConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::OpenCode.app_id()
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
                            placeholder: "myprovider".into(),
                        },
                    )
                    .help("opencode.json 中 provider.<id> 的键，也是 OpenCode 内引用该供应商的标识。")
                    .required(),
                    FormField::new(
                        "npm",
                        "AI SDK 包 (npm)",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(NPM_OPENAI_COMPATIBLE, "@ai-sdk/openai-compatible")
                                    .with_hint("OpenAI 兼容的第三方端点（最常用）"),
                                SelectOption::new(NPM_OPENAI, "@ai-sdk/openai")
                                    .with_hint("官方 OpenAI"),
                                SelectOption::new(NPM_ANTHROPIC, "@ai-sdk/anthropic")
                                    .with_hint("Anthropic / Claude"),
                                SelectOption::new(NPM_BEDROCK, "@ai-sdk/amazon-bedrock")
                                    .with_hint("Amazon Bedrock（鉴权经选项扩展）"),
                                SelectOption::new(NPM_GOOGLE, "@ai-sdk/google")
                                    .with_hint("Google Gemini"),
                            ],
                        },
                    )
                    .help("决定 OpenCode 加载哪个 AI SDK 适配器；必填，请按上游真实协议选择。")
                    .required(),
                    FormField::new(
                        "name",
                        "显示名",
                        FieldKind::Text {
                            placeholder: "My Provider".into(),
                        },
                    )
                    .help("OpenCode UI 中展示的名称；留空则回退到 Provider ID。"),
                ],
            ),
            FormSection::new(
                "端点",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL (options.baseURL)",
                        FieldKind::Text {
                            placeholder: "https://api.example.com/v1".into(),
                        },
                    )
                    .help("OpenAI 兼容端点通常以 /v1 结尾；官方包可留空使用默认地址。"),
                    FormField::new(
                        "api_key",
                        "API Key (options.apiKey)",
                        FieldKind::Secret {
                            placeholder: "sk-...".into(),
                        },
                    )
                    .help("写入 options.apiKey；Bedrock 等需要其它鉴权字段时请改用下方“选项扩展”。"),
                ],
            ),
            FormSection::new(
                "选项",
                vec![
                    FormField::new(
                        "options_extra",
                        "选项扩展 (options.*)",
                        FieldKind::KeyValue {
                            key_placeholder: "setCacheKey".into(),
                            value_placeholder: "true".into(),
                        },
                    )
                    .help("写入 options 下的额外字段（如 setCacheKey、region/accessKeyId/secretAccessKey 等 Bedrock 鉴权）。布尔/数字会按字面量解析。"),
                    FormField::new(
                        "headers",
                        "请求头 (options.headers)",
                        FieldKind::KeyValue {
                            key_placeholder: "X-Header".into(),
                            value_placeholder: "value".into(),
                        },
                    )
                    .help("附加到每次请求的 HTTP 头。"),
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
                            GridColumn::text("output", "最大输出", "16384"),
                        ],
                    },
                )
                .help("可配置多个模型；模型 ID 是 OpenCode 内部键，显示名是 UI 文本，两者相互独立。上下文/最大输出为可选数字（token）。")],
            ),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let cfg = settings_config.as_object();

        // npm: user-selectable; default to openai-compatible for a new provider.
        let npm = cfg
            .and_then(|c| c.get("npm"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(NPM_OPENAI_COMPATIBLE);
        set_str(&mut values, "npm", npm);

        set_str(
            &mut values,
            "name",
            cfg.and_then(|c| c.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );

        // provider_id is not part of the on-disk object (it is the parent key);
        // seed it from the display name so a brand-new provider has something.
        set_str(&mut values, "provider_id", "");

        let options = cfg
            .and_then(|c| c.get("options"))
            .and_then(Value::as_object);
        set_str(
            &mut values,
            "base_url",
            options
                .and_then(|o| o.get("baseURL"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        set_str(
            &mut values,
            "api_key",
            options
                .and_then(|o| o.get("apiKey"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );

        // headers -> string map.
        values.insert(
            "headers".into(),
            string_map_from(options.and_then(|o| o.get("headers"))),
        );

        // options.* extras (anything not modelled by a dedicated field), as a
        // string map so booleans/numbers display editably.
        let mut extra = Map::new();
        if let Some(opts) = options {
            for (k, v) in opts {
                if KNOWN_OPTION_KEYS.contains(&k.as_str()) {
                    continue;
                }
                extra.insert(k.clone(), Value::String(scalar_to_string(v)));
            }
        }
        values.insert("options_extra".into(), Value::Object(extra));

        // models -> grid rows, ALL of them (non-destructive).
        let mut rows: Vec<Value> = Vec::new();
        if let Some(models) = cfg.and_then(|c| c.get("models")).and_then(Value::as_object) {
            // Stable order: sort by model id so the grid is deterministic.
            let mut entries: Vec<(&String, &Value)> = models.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (id, m) in entries {
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let limit = m.get("limit");
                let context = limit
                    .and_then(|l| l.get("context"))
                    .and_then(json_number_to_string);
                let output = limit
                    .and_then(|l| l.get("output"))
                    .and_then(json_number_to_string);
                rows.push(json!({
                    "id": id,
                    "name": name,
                    "context": context.unwrap_or_default(),
                    "output": output.unwrap_or_default(),
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
        // Start from the prior provider object so unknown/native top-level keys
        // (and per-model native fields) survive the round-trip.
        let mut obj = prior.as_object().cloned().unwrap_or_default();

        // npm (required, user-selectable).
        let npm = {
            let n = str_val(values, "npm").trim();
            if n.is_empty() {
                NPM_OPENAI_COMPATIBLE.to_string()
            } else {
                n.to_string()
            }
        };
        obj.insert("npm".into(), Value::String(npm));

        // name (drop when empty so OpenCode falls back to the id).
        let name = str_val(values, "name").trim();
        if name.is_empty() {
            obj.remove("name");
        } else {
            obj.insert("name".into(), Value::String(name.to_string()));
        }

        // options: merge into prior options, preserving native option keys.
        let mut options = obj
            .get("options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        set_or_remove_str(&mut options, "baseURL", str_val(values, "base_url"));
        set_or_remove_str(&mut options, "apiKey", str_val(values, "api_key"));

        // headers
        let headers = string_map_to_object(values.get("headers"));
        if headers.is_empty() {
            options.remove("headers");
        } else {
            options.insert("headers".into(), Value::Object(headers));
        }

        // options extras (parsed literally: true/false/number/string).
        // Remove any previously-written extras that are no longer present, then
        // (re)insert current ones. We only touch keys we are not modelling, so
        // native non-form keys are left alone unless re-supplied by the form.
        let extra = values
            .get("options_extra")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (k, v) in &extra {
            if k.trim().is_empty() || KNOWN_OPTION_KEYS.contains(&k.as_str()) {
                continue;
            }
            options.insert(k.clone(), parse_scalar(v.as_str().unwrap_or_default()));
        }

        if options.is_empty() {
            obj.remove("options");
        } else {
            obj.insert("options".into(), Value::Object(options));
        }

        // models: rebuild from ALL grid rows, merging into prior per-model
        // objects so native per-model fields (options, extra) survive.
        let prior_models = obj
            .get("models")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut models = Map::new();
        if let Some(rows) = values.get("models").and_then(Value::as_array) {
            for row in rows {
                let Some(row) = row.as_object() else { continue };
                let id = row
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if id.is_empty() {
                    continue;
                }
                // Merge into the prior entry for this id, if any.
                let mut entry = prior_models
                    .get(id)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();

                let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
                // OpenCodeModel.name is required (non-optional); fall back to id.
                entry.insert(
                    "name".into(),
                    Value::String(if name.trim().is_empty() {
                        id.to_string()
                    } else {
                        name.to_string()
                    }),
                );

                let context = parse_u64(row.get("context"));
                let output = parse_u64(row.get("output"));
                if context.is_some() || output.is_some() {
                    let mut limit = entry
                        .get("limit")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    match context {
                        Some(n) => {
                            limit.insert("context".into(), json!(n));
                        }
                        None => {
                            limit.remove("context");
                        }
                    }
                    match output {
                        Some(n) => {
                            limit.insert("output".into(), json!(n));
                        }
                        None => {
                            limit.remove("output");
                        }
                    }
                    if limit.is_empty() {
                        entry.remove("limit");
                    } else {
                        entry.insert("limit".into(), Value::Object(limit));
                    }
                } else {
                    entry.remove("limit");
                }

                models.insert(id.to_string(), Value::Object(entry));
            }
        }
        if models.is_empty() {
            obj.remove("models");
        } else {
            obj.insert("models".into(), Value::Object(models));
        }

        EncodeResult {
            settings_config: Value::Object(obj),
            // OpenCode has no ProviderMeta-resident fields; preserve prior meta.
            meta: prior_meta.cloned(),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let text = contents.first().map(String::as_str).unwrap_or("{}");
        let parsed = serde_json::from_str::<Value>(text)
            .map_err(|e| format!("opencode.json 解析失败: {e}"))?;
        parsed
            .get("provider")
            .and_then(Value::as_object)
            .and_then(|m| m.values().next().cloned())
            .ok_or_else(|| "缺少 provider.<id> 配置".to_string())
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        let provider_id = {
            let id = str_val(values, "provider_id").trim();
            if id.is_empty() {
                "myprovider".to_string()
            } else {
                id.to_string()
            }
        };

        // Encode against the working document so unmanaged keys survive.
        let encoded = self.encode(values, prior, None);
        let mut providers = Map::new();
        providers.insert(provider_id, encoded.settings_config);
        let wrapper = json!({
            "$schema": "https://opencode.ai/config.json",
            "provider": Value::Object(providers),
        });

        vec![PreviewFile {
            filename: "~/.config/opencode/opencode.json".into(),
            language: Language::Json,
            content: serde_json::to_string_pretty(&wrapper).unwrap_or_default(),
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        if str_val(values, "provider_id").trim().is_empty() {
            issues.push(ConfigIssue::error("Provider ID 不能为空。").for_field("provider_id"));
        }

        let npm = str_val(values, "npm");
        const KNOWN_NPM: [&str; 5] = [
            NPM_OPENAI_COMPATIBLE,
            NPM_OPENAI,
            NPM_ANTHROPIC,
            NPM_BEDROCK,
            NPM_GOOGLE,
        ];
        if npm.trim().is_empty() {
            issues.push(ConfigIssue::error("必须选择 AI SDK 包 (npm)。").for_field("npm"));
        } else if !KNOWN_NPM.contains(&npm) {
            issues.push(
                ConfigIssue::warning(format!("未知的 npm 包 \"{npm}\"，OpenCode 可能无法加载。"))
                    .for_field("npm"),
            );
        }

        // Bedrock auth lives in options extras for v1; nudge the user.
        if npm == NPM_BEDROCK {
            let extra = values.get("options_extra").and_then(Value::as_object);
            let has_region = extra.map(|m| m.contains_key("region")).unwrap_or(false);
            if !has_region {
                issues.push(
                    ConfigIssue::info(
                        "Bedrock 通常需要 region / accessKeyId / secretAccessKey，请在“选项扩展”中填写。",
                    )
                    .for_field("options_extra"),
                );
            }
        } else if str_val(values, "base_url").trim().is_empty() && npm == NPM_OPENAI_COMPATIBLE {
            issues.push(
                ConfigIssue::warning("openai-compatible 端点通常需要填写 Base URL。")
                    .for_field("base_url"),
            );
        }

        // Model grid: ids must be present and unique.
        if let Some(rows) = values.get("models").and_then(Value::as_array) {
            let mut seen: Vec<&str> = Vec::new();
            for row in rows {
                let id = row
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if id.is_empty() {
                    // Allow blank trailing rows silently; only flag rows that
                    // have a name but no id.
                    let has_name = row
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    if has_name {
                        issues.push(
                            ConfigIssue::error("模型缺少 ID（ID 与显示名不同，必须填写）。")
                                .for_field("models"),
                        );
                    }
                    continue;
                }
                if seen.contains(&id) {
                    issues.push(
                        ConfigIssue::error(format!("模型 ID \"{id}\" 重复。")).for_field("models"),
                    );
                } else {
                    seen.push(id);
                }
            }
        }

        issues
    }
}

// ---- helpers ----------------------------------------------------------------

/// Set a string option key, or remove it when the trimmed value is empty.
fn set_or_remove_str(options: &mut Map<String, Value>, key: &str, value: &str) {
    let v = value.trim();
    if v.is_empty() {
        options.remove(key);
    } else {
        options.insert(key.to_string(), Value::String(v.to_string()));
    }
}

/// Render a `Value` -> string-keyed string map (for `headers` in the form).
fn string_map_from(value: Option<&Value>) -> Value {
    let mut map = Map::new();
    if let Some(obj) = value.and_then(Value::as_object) {
        for (k, v) in obj {
            map.insert(k.clone(), Value::String(scalar_to_string(v)));
        }
    }
    Value::Object(map)
}

/// Read a form string-map field into a JSON object of `string -> string`.
fn string_map_to_object(value: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(obj) = value.and_then(Value::as_object) {
        for (k, v) in obj {
            if k.trim().is_empty() {
                continue;
            }
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                out.insert(k.clone(), Value::String(s.to_string()));
            }
        }
    }
    out
}

/// Best-effort scalar -> display string (for the key/value editor).
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parse a literal from the key/value editor: `true`/`false` -> bool, integer
/// -> number, otherwise a string.
fn parse_scalar(s: &str) -> Value {
    let t = s.trim();
    match t {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>() {
        return json!(n);
    }
    if let Ok(f) = t.parse::<f64>()
        && let Some(num) = serde_json::Number::from_f64(f)
    {
        return Value::Number(num);
    }
    Value::String(s.to_string())
}

/// A grid cell holds a string; parse it into a `u64` token count, or `None`.
fn parse_u64(value: Option<&Value>) -> Option<u64> {
    let s = value.and_then(Value::as_str).unwrap_or("").trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<u64>().ok()
}

/// Render a JSON number (context/output limit) as a grid-cell string.
fn json_number_to_string(v: &Value) -> Option<String> {
    v.as_u64()
        .map(|n| n.to_string())
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_str().map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "provider_id", "myco");
        set_str(&mut v, "npm", NPM_OPENAI_COMPATIBLE);
        set_str(&mut v, "name", "MyCo");
        set_str(&mut v, "base_url", "https://api.myco.com/v1");
        set_str(&mut v, "api_key", "sk-myco");
        v.insert("options_extra".into(), json!({ "setCacheKey": "true" }));
        v.insert("headers".into(), json!({ "X-Org": "acme" }));
        v.insert(
            "models".into(),
            json!([
                { "id": "myco-large", "name": "MyCo Large", "context": "200000", "output": "32000" },
                { "id": "myco-small", "name": "MyCo Small", "context": "", "output": "" },
            ]),
        );
        v
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = OpenCodeConfig.decode(&Value::Null, None);
        assert_eq!(str_val(&values, "npm"), NPM_OPENAI_COMPATIBLE);
        assert_eq!(str_val(&values, "name"), "");
        assert_eq!(str_val(&values, "base_url"), "");
        assert!(
            values
                .get("models")
                .and_then(Value::as_array)
                .map(|a| a.is_empty())
                .unwrap_or(false)
        );
    }

    #[test]
    fn decode_defaults_for_empty_object() {
        let values = OpenCodeConfig.decode(&json!({}), None);
        assert_eq!(str_val(&values, "npm"), NPM_OPENAI_COMPATIBLE);
    }

    #[test]
    fn encode_writes_user_selected_npm_not_hardcoded() {
        // BUG FIX (1): npm is user-selectable, not forced to openai-compatible.
        let mut v = sample_values();
        set_str(&mut v, "npm", NPM_ANTHROPIC);
        let result = OpenCodeConfig.encode(&v, &Value::Null, None);
        assert_eq!(
            result.settings_config["npm"].as_str(),
            Some(NPM_ANTHROPIC),
            "{:?}",
            result.settings_config
        );
    }

    #[test]
    fn encode_produces_expected_options_and_models() {
        let result = OpenCodeConfig.encode(&sample_values(), &Value::Null, None);
        let cfg = &result.settings_config;
        assert_eq!(cfg["npm"].as_str(), Some(NPM_OPENAI_COMPATIBLE));
        assert_eq!(cfg["name"].as_str(), Some("MyCo"));
        assert_eq!(
            cfg["options"]["baseURL"].as_str(),
            Some("https://api.myco.com/v1")
        );
        assert_eq!(cfg["options"]["apiKey"].as_str(), Some("sk-myco"));
        // options_extra parsed literally: "true" -> bool true.
        assert_eq!(cfg["options"]["setCacheKey"].as_bool(), Some(true));
        assert_eq!(cfg["options"]["headers"]["X-Org"].as_str(), Some("acme"));
        // BUG FIX (3): BOTH models are written, never collapsed to one.
        let models = cfg["models"].as_object().unwrap();
        assert_eq!(models.len(), 2, "{models:?}");
        assert!(models.contains_key("myco-large"));
        assert!(models.contains_key("myco-small"));
        // limit numbers are real JSON numbers.
        assert_eq!(
            cfg["models"]["myco-large"]["limit"]["context"].as_u64(),
            Some(200000)
        );
        assert_eq!(
            cfg["models"]["myco-large"]["limit"]["output"].as_u64(),
            Some(32000)
        );
        // small model had blank limits -> no limit object.
        assert!(cfg["models"]["myco-small"].get("limit").is_none());
    }

    #[test]
    fn model_id_distinct_from_display_name() {
        // BUG FIX (2): id and name are separate columns; both preserved.
        let result = OpenCodeConfig.encode(&sample_values(), &Value::Null, None);
        let large = &result.settings_config["models"]["myco-large"];
        assert_eq!(large["name"].as_str(), Some("MyCo Large"));
        // The object key (id) is "myco-large", distinct from the display name.
        assert!(
            result.settings_config["models"]
                .as_object()
                .unwrap()
                .contains_key("myco-large")
        );
    }

    #[test]
    fn round_trip_preserves_all_models_and_fields() {
        let original = sample_values();
        let encoded = OpenCodeConfig.encode(&original, &Value::Null, None);
        let decoded = OpenCodeConfig.decode(&encoded.settings_config, None);

        assert_eq!(str_val(&decoded, "npm"), NPM_OPENAI_COMPATIBLE);
        assert_eq!(str_val(&decoded, "name"), "MyCo");
        assert_eq!(str_val(&decoded, "base_url"), "https://api.myco.com/v1");
        assert_eq!(str_val(&decoded, "api_key"), "sk-myco");

        let rows = decoded["models"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "round-trip must keep BOTH models");
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"myco-large"));
        assert!(ids.contains(&"myco-small"));
        // Distinct name survives.
        let large = rows
            .iter()
            .find(|r| r["id"] == json!("myco-large"))
            .unwrap();
        assert_eq!(large["name"].as_str(), Some("MyCo Large"));
        assert_eq!(large["context"].as_str(), Some("200000"));

        // options_extra round-trips back to a string in the editor.
        assert_eq!(
            decoded["options_extra"]["setCacheKey"].as_str(),
            Some("true")
        );
        assert_eq!(decoded["headers"]["X-Org"].as_str(), Some("acme"));
    }

    #[test]
    fn encode_merges_into_prior_preserving_native_keys() {
        // Native top-level key and a native per-model field must survive.
        let prior = json!({
            "npm": "@ai-sdk/openai-compatible",
            "someNativeKey": "keepme",
            "options": { "baseURL": "old", "nativeOpt": 7 },
            "models": {
                "myco-large": { "name": "old", "options": { "temperature": 0.5 } }
            }
        });
        let result = OpenCodeConfig.encode(&sample_values(), &prior, None);
        let cfg = &result.settings_config;
        // Unknown top-level key preserved.
        assert_eq!(cfg["someNativeKey"].as_str(), Some("keepme"));
        // Unknown option preserved, modelled baseURL updated.
        assert_eq!(cfg["options"]["nativeOpt"].as_u64(), Some(7));
        assert_eq!(
            cfg["options"]["baseURL"].as_str(),
            Some("https://api.myco.com/v1")
        );
        // Per-model native field preserved, name updated.
        assert_eq!(
            cfg["models"]["myco-large"]["options"]["temperature"].as_f64(),
            Some(0.5)
        );
        assert_eq!(
            cfg["models"]["myco-large"]["name"].as_str(),
            Some("MyCo Large")
        );
    }

    #[test]
    fn encode_preserves_prior_meta() {
        let meta = ProviderMeta {
            custom_user_agent: Some("ms/1".into()),
            ..Default::default()
        };
        let result = OpenCodeConfig.encode(&sample_values(), &Value::Null, Some(&meta));
        assert_eq!(
            result.meta.and_then(|m| m.custom_user_agent),
            Some("ms/1".into())
        );
    }

    #[test]
    fn preview_emits_single_wrapped_json_file() {
        let files = OpenCodeConfig.preview(&sample_values(), &Value::Null);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.filename, "~/.config/opencode/opencode.json");
        assert_eq!(f.language, Language::Json);
        // Wrapped under provider.<id>.
        let parsed: Value = serde_json::from_str(&f.content).unwrap();
        assert!(parsed["provider"]["myco"].is_object(), "{}", f.content);
        assert_eq!(
            parsed["provider"]["myco"]["npm"].as_str(),
            Some(NPM_OPENAI_COMPATIBLE)
        );
        // Both models visible in preview.
        assert_eq!(
            parsed["provider"]["myco"]["models"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn validate_flags_duplicate_and_missing_model_ids() {
        let mut v = sample_values();
        v.insert(
            "models".into(),
            json!([
                { "id": "dup", "name": "A" },
                { "id": "dup", "name": "B" },
                { "id": "", "name": "no-id" },
            ]),
        );
        let issues = OpenCodeConfig.validate(&v);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == super::super::Severity::Error && i.message.contains("重复"))
        );
        assert!(
            issues
                .iter()
                .any(|i| i.severity == super::super::Severity::Error
                    && i.message.contains("缺少 ID"))
        );
    }

    #[test]
    fn validate_requires_provider_id() {
        let mut v = sample_values();
        set_str(&mut v, "provider_id", "");
        let issues = OpenCodeConfig.validate(&v);
        assert!(
            issues
                .iter()
                .any(|i| i.field.as_deref() == Some("provider_id")
                    && i.severity == super::super::Severity::Error)
        );
    }
}
