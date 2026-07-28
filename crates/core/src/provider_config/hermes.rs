//! Hermes provider config codec.
//!
//! Hermes (additive) stores every custom provider as one entry in the
//! `custom_providers:` sequence of `~/.hermes/config.yaml`. OcHub keeps
//! each entry's `settingsConfig` in the *UI-friendly* shape that
//! `apps::hermes::get_providers` denormalizes to:
//!
//! ```json
//! {
//!   "name": "openrouter",
//!   "base_url": "https://openrouter.ai/api/v1",
//!   "api_key": "sk-or-...",
//!   "api_mode": "anthropic_messages",
//!   "models": [{ "id": "anthropic/claude-opus-4-8", "context_length": "200000" }],
//!   "model": "anthropic/claude-opus-4-8",
//!   "rate_limit_delay": 1.5
//! }
//! ```
//!
//! All keys are snake_case (Hermes' `_VALID_CUSTOM_PROVIDER_FIELDS`). `models`
//! is held as a UI **array** of `{id, name?, context_length?}` rows — that is
//! exactly what `apps::hermes::set_provider` expects (it runs
//! `normalize_provider_models_for_write`, array → YAML dict) and what
//! `apply_switch_defaults` reads (`.as_array().first().get("id")`). On disk the
//! `models:` field becomes a dict keyed by model id; the live preview here
//! renders that real on-disk YAML.
//!
//! Two correctness bugs the legacy generic form had, fixed in this codec:
//!   1. it never emitted `api_mode`, so Hermes silently defaulted to
//!      `chat_completions` and broke Anthropic/Codex/Bedrock providers;
//!   2. it could not carry per-model `context_length`, nor keep the singular
//!      `model:` field (= first model id) the runtime + `/model` picker read.

use serde_json::{Map, Value};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues,
    GridColumn, Language, PreviewFile, SelectOption, set_str, str_val,
};
use crate::AppType;
use crate::model::ProviderMeta;

const MODE_CHAT_COMPLETIONS: &str = "chat_completions";
const MODE_ANTHROPIC_MESSAGES: &str = "anthropic_messages";
const MODE_CODEX_RESPONSES: &str = "codex_responses";
const MODE_BEDROCK_CONVERSE: &str = "bedrock_converse";

const API_MODES: &[&str] = &[
    MODE_CHAT_COMPLETIONS,
    MODE_ANTHROPIC_MESSAGES,
    MODE_CODEX_RESPONSES,
    MODE_BEDROCK_CONVERSE,
];

pub struct HermesConfig;

impl AppConfig for HermesConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::Hermes.app_id()
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "供应商",
                vec![
                    FormField::new(
                        "api_mode",
                        "API 模式 (api_mode)",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(MODE_CHAT_COMPLETIONS, "Chat Completions")
                                    .with_hint("OpenAI 兼容 /chat/completions"),
                                SelectOption::new(MODE_ANTHROPIC_MESSAGES, "Anthropic Messages")
                                    .with_hint("Claude /v1/messages"),
                                SelectOption::new(MODE_CODEX_RESPONSES, "Codex Responses")
                                    .with_hint("OpenAI Responses /responses"),
                                SelectOption::new(MODE_BEDROCK_CONVERSE, "Bedrock Converse")
                                    .with_hint("AWS Bedrock Converse"),
                            ],
                        },
                    )
                    .help("必填：决定 Hermes 用哪种协议请求该供应商；缺省会被当作 chat_completions，会让 Anthropic/Codex/Bedrock 供应商失效。")
                    .required(),
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://openrouter.ai/api/v1".into(),
                        },
                    )
                    .required(),
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
                vec![FormField::new(
                    "models",
                    "模型列表",
                    FieldKind::ModelGrid {
                        columns: vec![
                            GridColumn::text("id", "模型 ID", "anthropic/claude-opus-4-8"),
                            GridColumn::text("name", "显示名", "Claude Opus 4.8"),
                            GridColumn::text("context_length", "上下文长度", "200000"),
                        ],
                    },
                )
                .help("首行的模型 ID 会写入单数 model: 字段，供 Hermes 运行时与 /model 选择器作为默认模型。")],
            ),
            FormSection::new(
                "高级",
                vec![FormField::new(
                    "rate_limit_delay",
                    "限流延迟 (rate_limit_delay)",
                    FieldKind::Text {
                        placeholder: "0".into(),
                    },
                )
                .help("每次请求之间的间隔秒数（浮点）；留空表示不限流。")],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let obj = settings_config.as_object();

        let read_str = |key: &str| -> String {
            obj.and_then(|o| o.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };

        // api_mode: ALWAYS surface a concrete value — defaulting to
        // chat_completions for a brand-new provider, but never leaving it blank
        // (a blank/omitted mode is exactly the legacy bug we are fixing).
        let api_mode = {
            let raw = read_str("api_mode");
            if API_MODES.contains(&raw.as_str()) {
                raw
            } else {
                MODE_CHAT_COMPLETIONS.to_string()
            }
        };
        set_str(&mut values, "api_mode", api_mode);
        set_str(&mut values, "base_url", read_str("base_url"));
        set_str(&mut values, "api_key", read_str("api_key"));

        // models: the DB shape is the UI-friendly array of {id, name?,
        // context_length?}. Normalize each row into the grid's string-cell
        // convention so the editor renders cleanly.
        values.insert("models".into(), decode_models(obj));

        // rate_limit_delay is a float on disk; render it as a string cell.
        let delay = obj
            .and_then(|o| o.get("rate_limit_delay"))
            .and_then(json_number_to_string)
            .unwrap_or_default();
        set_str(&mut values, "rate_limit_delay", delay);

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // Merge into prior so native keys the form does not model (e.g.
        // request_timeout_seconds, key_env) survive a round-trip.
        let mut settings = prior.as_object().cloned().unwrap_or_default();

        // (Bug fix 1) ALWAYS write api_mode.
        let api_mode = {
            let m = str_val(values, "api_mode");
            if API_MODES.contains(&m) {
                m
            } else {
                MODE_CHAT_COMPLETIONS
            }
        };
        settings.insert("api_mode".into(), Value::String(api_mode.to_string()));

        settings.insert(
            "base_url".into(),
            Value::String(str_val(values, "base_url").trim().to_string()),
        );
        settings.insert(
            "api_key".into(),
            Value::String(str_val(values, "api_key").to_string()),
        );

        // (Bug fix 2 & 3) models as the UI array that set_provider expects, with
        // per-model context_length, and the singular model: = first model id.
        let models = encode_models(values);
        let first_model_id = models
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|row| row.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());

        settings.insert("models".into(), models);
        match first_model_id {
            Some(id) => {
                settings.insert("model".into(), Value::String(id));
            }
            None => {
                settings.remove("model");
            }
        }

        // rate_limit_delay: float on disk, or dropped when blank.
        match parse_delay(str_val(values, "rate_limit_delay")) {
            Some(n) => {
                settings.insert("rate_limit_delay".into(), n);
            }
            None => {
                settings.remove("rate_limit_delay");
            }
        }

        EncodeResult {
            settings_config: Value::Object(settings),
            // Hermes keeps no fields in ProviderMeta; pass it through unchanged.
            meta: prior_meta.cloned(),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let text = contents.first().map(String::as_str).unwrap_or("");
        let parsed = serde_norway::from_str::<Value>(text)
            .map_err(|e| format!("config.yaml 解析失败: {e}"))?;
        parsed
            .get("custom_providers")
            .and_then(Value::as_array)
            .and_then(|a| a.first().cloned())
            .ok_or_else(|| "缺少 custom_providers 条目".to_string())
    }

    fn preview(&self, values: &FormValues, _prior: &Value) -> Vec<PreviewFile> {
        // Render the actual on-disk YAML: one entry under custom_providers
        // (a sequence), with `name`, snake_case fields, and models as the
        // dict-keyed-by-id shape Hermes reads.
        let name = preview_provider_name(values);
        let entry = build_yaml_entry(values, &name);

        let mut root = serde_norway::Mapping::new();
        root.insert(
            yaml_str("custom_providers"),
            serde_norway::Value::Sequence(vec![entry]),
        );

        let content =
            serde_norway::to_string(&serde_norway::Value::Mapping(root)).unwrap_or_default();

        vec![PreviewFile {
            filename: "~/.hermes/config.yaml".into(),
            language: Language::Yaml,
            content,
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        let api_mode = str_val(values, "api_mode");
        if api_mode.trim().is_empty() {
            issues.push(
                ConfigIssue::error(
                    "api_mode 为必填；缺省会被当作 chat_completions，将使 Anthropic/Codex/Bedrock 供应商失效。",
                )
                .for_field("api_mode"),
            );
        } else if !API_MODES.contains(&api_mode) {
            issues.push(
                ConfigIssue::error(format!(
                    "未知的 api_mode \"{api_mode}\"；必须是 chat_completions / anthropic_messages / codex_responses / bedrock_converse 之一。"
                ))
                .for_field("api_mode"),
            );
        }

        if str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        }

        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 API Key。").for_field("api_key"));
        }

        let rows = encode_models(values);
        let row_count = rows.as_array().map(|a| a.len()).unwrap_or(0);
        if row_count == 0 {
            issues.push(
                ConfigIssue::warning("未配置任何模型；切换到该供应商时 model.default 不会被更新。")
                    .for_field("models"),
            );
        }

        // Per-row context_length must be a positive integer if present. Inspect
        // the RAW grid rows (not the encoded output, which already drops invalid
        // values) so the user is warned about bad input.
        if let Some(arr) = values.get("models").and_then(Value::as_array) {
            for row in arr {
                let ctx = row
                    .get("context_length")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !ctx.is_empty() && ctx.parse::<u64>().is_err() {
                    issues.push(
                        ConfigIssue::warning(format!(
                            "模型 \"{}\" 的上下文长度 \"{ctx}\" 不是正整数，将被忽略。",
                            row.get("id").and_then(Value::as_str).unwrap_or("?")
                        ))
                        .for_field("models"),
                    );
                }
            }
        }

        let delay = str_val(values, "rate_limit_delay").trim();
        if !delay.is_empty() && delay.parse::<f64>().is_err() {
            issues.push(
                ConfigIssue::warning("限流延迟必须是数字（秒），将被忽略。")
                    .for_field("rate_limit_delay"),
            );
        }

        issues
    }
}

/// Read the DB-shape `models` array into the grid's string-cell convention.
/// Each row keeps `id`, optional `name`, and `context_length` rendered as a
/// string so the text grid can edit it.
fn decode_models(obj: Option<&Map<String, Value>>) -> Value {
    let arr = obj.and_then(|o| o.get("models")).and_then(Value::as_array);
    let Some(arr) = arr else {
        return Value::Array(Vec::new());
    };

    let mut rows = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let mut row = Map::new();
        row.insert(
            "id".into(),
            Value::String(
                item_obj
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        );
        row.insert(
            "name".into(),
            Value::String(
                item_obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        );
        row.insert(
            "context_length".into(),
            Value::String(
                item_obj
                    .get("context_length")
                    .and_then(json_number_to_string)
                    .unwrap_or_default(),
            ),
        );
        rows.push(Value::Object(row));
    }
    Value::Array(rows)
}

/// Build the DB-shape `models` array from the grid values. Drops rows with a
/// blank id; coerces `context_length` text into an integer (omitted when blank
/// or unparseable); omits a blank `name`. This is the array shape both
/// `set_provider` (array → YAML dict) and `apply_switch_defaults`
/// (`.as_array().first().get("id")`) consume.
fn encode_models(values: &FormValues) -> Value {
    let rows = values.get("models").and_then(Value::as_array);
    let Some(rows) = rows else {
        return Value::Array(Vec::new());
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(row_obj) = row.as_object() else {
            continue;
        };
        let id = row_obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if id.is_empty() {
            continue;
        }

        let mut entry = Map::new();
        entry.insert("id".into(), Value::String(id.to_string()));

        let name = row_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !name.is_empty() {
            entry.insert("name".into(), Value::String(name.to_string()));
        }

        let ctx = row_obj
            .get("context_length")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if let Ok(n) = ctx.parse::<u64>() {
            entry.insert("context_length".into(), Value::Number(n.into()));
        }

        out.push(Value::Object(entry));
    }
    Value::Array(out)
}

/// Placeholder provider name for the live preview. Hermes upserts each entry by
/// its provider id (which `set_provider` supplies separately, out of band of the
/// codec), so the editor preview has no real id to show.
fn preview_provider_name(_values: &FormValues) -> String {
    "<provider>".to_string()
}

/// Build the on-disk YAML mapping for one custom_providers entry, mirroring
/// what `apps::hermes::set_provider` writes: snake_case fields, `models` as a
/// dict keyed by model id, and the singular `model:` = first model id.
fn build_yaml_entry(values: &FormValues, name: &str) -> serde_norway::Value {
    let mut m = serde_norway::Mapping::new();

    m.insert(yaml_str("name"), yaml_str(name));

    let api_mode = {
        let mode = str_val(values, "api_mode");
        if API_MODES.contains(&mode) {
            mode
        } else {
            MODE_CHAT_COMPLETIONS
        }
    };
    m.insert(
        yaml_str("base_url"),
        yaml_str(str_val(values, "base_url").trim()),
    );
    m.insert(yaml_str("api_key"), yaml_str(str_val(values, "api_key")));
    m.insert(yaml_str("api_mode"), yaml_str(api_mode));

    // models as the dict shape Hermes reads on disk.
    let models = encode_models(values);
    let mut models_map = serde_norway::Mapping::new();
    let mut first_id: Option<String> = None;
    if let Some(arr) = models.as_array() {
        for row in arr {
            let Some(obj) = row.as_object() else { continue };
            let Some(id) = obj.get("id").and_then(Value::as_str) else {
                continue;
            };
            if first_id.is_none() {
                first_id = Some(id.to_string());
            }
            let mut entry = serde_norway::Mapping::new();
            if let Some(n) = obj.get("name").and_then(Value::as_str) {
                entry.insert(yaml_str("name"), yaml_str(n));
            }
            if let Some(ctx) = obj.get("context_length").and_then(Value::as_u64) {
                entry.insert(
                    yaml_str("context_length"),
                    serde_norway::Value::Number(serde_norway::Number::from(ctx)),
                );
            }
            models_map.insert(yaml_str(id), serde_norway::Value::Mapping(entry));
        }
    }

    if let Some(id) = first_id {
        m.insert(yaml_str("model"), yaml_str(&id));
    }
    if !models_map.is_empty() {
        m.insert(yaml_str("models"), serde_norway::Value::Mapping(models_map));
    }

    if let Some(delay) = parse_delay(str_val(values, "rate_limit_delay"))
        && let Some(f) = delay.as_f64()
    {
        m.insert(
            yaml_str("rate_limit_delay"),
            serde_norway::Value::Number(serde_norway::Number::from(f)),
        );
    }

    serde_norway::Value::Mapping(m)
}

fn yaml_str(s: &str) -> serde_norway::Value {
    serde_norway::Value::String(s.to_string())
}

/// Render a JSON number (or numeric string) as a plain string for a text cell.
fn json_number_to_string(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Parse the `rate_limit_delay` text field into a JSON float `Value`, or `None`
/// when blank/unparseable so the key is dropped.
fn parse_delay(raw: &str) -> Option<Value> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let f = t.parse::<f64>().ok()?;
    serde_json::Number::from_f64(f).map(Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn openrouter_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "api_mode", MODE_ANTHROPIC_MESSAGES);
        set_str(&mut v, "base_url", "https://openrouter.ai/api/v1");
        set_str(&mut v, "api_key", "sk-or-123");
        v.insert(
            "models".into(),
            json!([
                { "id": "anthropic/claude-opus-4-8", "name": "Opus", "context_length": "200000" },
                { "id": "anthropic/claude-haiku", "name": "", "context_length": "" }
            ]),
        );
        set_str(&mut v, "rate_limit_delay", "1.5");
        v
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        // A brand-new provider has Null / empty settingsConfig.
        for cfg in [Value::Null, json!({})] {
            let values = HermesConfig.decode(&cfg, None);
            // (Bug fix 1) api_mode is always concrete, never blank.
            assert_eq!(str_val(&values, "api_mode"), MODE_CHAT_COMPLETIONS);
            assert_eq!(str_val(&values, "base_url"), "");
            assert_eq!(str_val(&values, "api_key"), "");
            assert_eq!(str_val(&values, "rate_limit_delay"), "");
            assert_eq!(
                values
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|a| a.len()),
                Some(0)
            );
        }
    }

    #[test]
    fn encode_always_emits_api_mode() {
        // (Bug fix 1) Even if the form somehow lost api_mode, encode writes one.
        let mut v = openrouter_values();
        v.remove("api_mode");
        let result = HermesConfig.encode(&v, &Value::Null, None);
        assert_eq!(
            result.settings_config["api_mode"].as_str(),
            Some(MODE_CHAT_COMPLETIONS)
        );

        // And it honours a real selection.
        let result = HermesConfig.encode(&openrouter_values(), &Value::Null, None);
        assert_eq!(
            result.settings_config["api_mode"].as_str(),
            Some(MODE_ANTHROPIC_MESSAGES)
        );
    }

    #[test]
    fn encode_models_as_array_with_context_and_singular_model() {
        // (Bug fix 2 & 3) models is the array set_provider expects, with
        // per-model context_length as an int, and model: = first id.
        let result = HermesConfig.encode(&openrouter_values(), &Value::Null, None);
        let sc = &result.settings_config;

        let models = sc["models"].as_array().expect("models is an array");
        assert_eq!(models.len(), 2, "blank-id rows kept, both have ids");
        assert_eq!(models[0]["id"].as_str(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(models[0]["name"].as_str(), Some("Opus"));
        assert_eq!(models[0]["context_length"].as_u64(), Some(200000));
        // Second model had blank name + blank context: both omitted.
        assert!(models[1].get("name").is_none());
        assert!(models[1].get("context_length").is_none());

        // Singular model: drives model.default on switch.
        assert_eq!(sc["model"].as_str(), Some("anthropic/claude-opus-4-8"));

        // rate_limit_delay is a float.
        assert_eq!(sc["rate_limit_delay"].as_f64(), Some(1.5));
    }

    #[test]
    fn encode_drops_blank_id_rows() {
        let mut v = openrouter_values();
        v.insert(
            "models".into(),
            json!([
                { "id": "  ", "context_length": "100" },
                { "id": "real-model", "context_length": "" }
            ]),
        );
        let result = HermesConfig.encode(&v, &Value::Null, None);
        let models = result.settings_config["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"].as_str(), Some("real-model"));
        assert_eq!(result.settings_config["model"].as_str(), Some("real-model"));
    }

    #[test]
    fn encode_no_models_drops_singular_model() {
        let mut v = openrouter_values();
        v.insert("models".into(), json!([]));
        let result = HermesConfig.encode(&v, &Value::Null, None);
        assert!(result.settings_config.get("model").is_none());
        assert_eq!(
            result.settings_config["models"].as_array().map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn encode_preserves_unknown_native_keys() {
        // Forward-compat: keys the form doesn't model survive a round-trip.
        let prior = json!({
            "request_timeout_seconds": 300,
            "key_env": "OPENROUTER_API_KEY"
        });
        let result = HermesConfig.encode(&openrouter_values(), &prior, None);
        assert_eq!(
            result.settings_config["request_timeout_seconds"].as_u64(),
            Some(300)
        );
        assert_eq!(
            result.settings_config["key_env"].as_str(),
            Some("OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn decode_encode_round_trip() {
        // Encode -> store as DB shape -> decode must recover every field.
        let original = openrouter_values();
        let encoded = HermesConfig.encode(&original, &Value::Null, None);
        let decoded = HermesConfig.decode(&encoded.settings_config, None);

        assert_eq!(str_val(&decoded, "api_mode"), MODE_ANTHROPIC_MESSAGES);
        assert_eq!(
            str_val(&decoded, "base_url"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(str_val(&decoded, "api_key"), "sk-or-123");
        assert_eq!(str_val(&decoded, "rate_limit_delay"), "1.5");

        let models = decoded.get("models").and_then(Value::as_array).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"].as_str(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(models[0]["name"].as_str(), Some("Opus"));
        // context_length round-trips int -> string for the grid.
        assert_eq!(models[0]["context_length"].as_str(), Some("200000"));
        // second model had no name/context: empty strings in the grid.
        assert_eq!(models[1]["id"].as_str(), Some("anthropic/claude-haiku"));
        assert_eq!(models[1]["name"].as_str(), Some(""));
        assert_eq!(models[1]["context_length"].as_str(), Some(""));
    }

    #[test]
    fn preview_renders_single_yaml_file_with_models_dict() {
        let files = HermesConfig.preview(&openrouter_values(), &Value::Null);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.filename, "~/.hermes/config.yaml");
        assert_eq!(f.language, Language::Yaml);
        // custom_providers sequence, snake_case fields, api_mode present.
        assert!(f.content.contains("custom_providers:"), "{}", f.content);
        assert!(
            f.content.contains("api_mode: anthropic_messages"),
            "{}",
            f.content
        );
        assert!(f.content.contains("base_url:"), "{}", f.content);
        // models on disk is a dict keyed by model id (not an array).
        assert!(
            f.content.contains("anthropic/claude-opus-4-8:"),
            "models should be dict-keyed: {}",
            f.content
        );
        assert!(
            f.content.contains("context_length: 200000"),
            "{}",
            f.content
        );
        // singular model: present.
        assert!(
            f.content.contains("model: anthropic/claude-opus-4-8"),
            "{}",
            f.content
        );
    }

    #[test]
    fn validate_flags_missing_and_bad_api_mode() {
        // Empty mode -> error.
        let mut v = openrouter_values();
        set_str(&mut v, "api_mode", "");
        let issues = HermesConfig.validate(&v);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == super::super::Severity::Error
                    && i.field.as_deref() == Some("api_mode"))
        );

        // Unknown mode -> error.
        let mut v = openrouter_values();
        set_str(&mut v, "api_mode", "weird_mode");
        let issues = HermesConfig.validate(&v);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == super::super::Severity::Error
                    && i.field.as_deref() == Some("api_mode"))
        );

        // A valid config has no api_mode error.
        let issues = HermesConfig.validate(&openrouter_values());
        assert!(
            !issues
                .iter()
                .any(|i| i.field.as_deref() == Some("api_mode"))
        );
    }

    #[test]
    fn validate_warns_on_bad_context_and_delay() {
        let mut v = openrouter_values();
        v.insert(
            "models".into(),
            json!([{ "id": "m", "context_length": "not-a-number" }]),
        );
        set_str(&mut v, "rate_limit_delay", "soon");
        let issues = HermesConfig.validate(&v);
        assert!(issues.iter().any(|i| i.field.as_deref() == Some("models")
            && i.severity == super::super::Severity::Warning));
        assert!(
            issues
                .iter()
                .any(|i| i.field.as_deref() == Some("rate_limit_delay"))
        );
    }
}
