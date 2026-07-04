//! Gemini CLI provider config codec.
//!
//! The Gemini CLI reads two files: `~/.gemini/.env` (plain `KEY=VALUE` lines —
//! the API key, base URL, model and any arbitrary env the user wants exported)
//! and `~/.gemini/settings.json` (the CLI's own settings object, e.g.
//! `mcpServers`, `security.auth.selectedType`, …). RouteDeck stores both
//! inside one `settingsConfig` object shaped
//! `{ "env": { GOOGLE_GEMINI_BASE_URL?, GEMINI_API_KEY?, GEMINI_MODEL?, … },
//!    "config": { …settings.json object… } }`.
//!
//! This codec edits the `env` map *structurally* — base URL, key and model get
//! their own fields, everything else round-trips through an `extra_env`
//! key/value map — while the `config` object (settings.json) is preserved
//! verbatim so unknown native keys (`mcpServers`, `theme`, …) survive a
//! round-trip.
//!
//! Two correctness rules are baked in:
//!   1. Never emit an empty-string `GOOGLE_GEMINI_BASE_URL` / `GEMINI_API_KEY` —
//!      an empty base URL flips the CLI into GATEWAY auth and fails validation,
//!      so empty keys are *omitted* from `.env` entirely.
//!   2. OAuth mode means *no* env auth: when `auth_mode = oauth` the `env` map is
//!      emitted empty (`{}`), which the strict validator treats as "use OAuth".

use serde_json::{Map, Value};

use super::{
    set_str, str_val, AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection,
    FormValues, Language, PreviewFile, SelectOption,
};
use crate::model::ProviderMeta;
use crate::AppType;

const AUTH_API_KEY: &str = "api_key";
const AUTH_OAUTH: &str = "oauth";

const ENV_BASE_URL: &str = "GOOGLE_GEMINI_BASE_URL";
const ENV_API_KEY: &str = "GEMINI_API_KEY";
const ENV_MODEL: &str = "GEMINI_MODEL";

/// Env keys that get their own dedicated form field (everything else lands in
/// the `extra_env` key/value map).
const RESERVED_ENV_KEYS: [&str; 3] = [ENV_BASE_URL, ENV_API_KEY, ENV_MODEL];

pub struct GeminiConfig;

impl AppConfig for GeminiConfig {
    fn app(&self) -> AppType {
        AppType::Gemini
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "鉴权",
                vec![FormField::new(
                    "auth_mode",
                    "鉴权方式",
                    FieldKind::Select {
                        options: vec![
                            SelectOption::new(AUTH_API_KEY, "API Key（第三方 / PackyCode）")
                                .with_hint("写入 .env 的 GEMINI_API_KEY + GOOGLE_GEMINI_BASE_URL"),
                            SelectOption::new(AUTH_OAUTH, "Google 官方 OAuth 登录")
                                .with_hint(".env 留空，使用 oauth-personal 登录"),
                        ],
                    },
                )
                .help("第三方 / PackyCode 选 API Key；Google 官方账号登录选 OAuth。OAuth 模式下 .env 必须为空。")],
            ),
            FormSection::new(
                "端点",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://generativelanguage.googleapis.com".into(),
                        },
                    )
                    .visible_when("auth_mode", AUTH_API_KEY)
                    .help("写入 .env 的 GOOGLE_GEMINI_BASE_URL；留空将被忽略（切勿写空串，会触发 GATEWAY 鉴权失败）。"),
                    FormField::new(
                        "api_key",
                        "API Key",
                        FieldKind::Secret {
                            placeholder: "AIza...".into(),
                        },
                    )
                    .visible_when("auth_mode", AUTH_API_KEY)
                    .help("写入 .env 的 GEMINI_API_KEY。"),
                ],
            ),
            FormSection::new(
                "模型",
                vec![FormField::new(
                    "model",
                    "模型",
                    FieldKind::Text {
                        placeholder: "gemini-2.5-pro".into(),
                    },
                )
                .help("写入 .env 的 GEMINI_MODEL；留空则不写该变量，由 CLI 使用默认模型。")],
            ),
            FormSection::new(
                "高级",
                vec![FormField::new(
                    "extra_env",
                    "额外环境变量",
                    FieldKind::KeyValue {
                        key_placeholder: "GEMINI_API_VERSION".into(),
                        value_placeholder: "v1beta".into(),
                    },
                )
                .help("追加写入 .env 的其它变量（不含上面已有的 base URL / key / model）。settings.json 中的其它字段（mcpServers、theme 等）会原样保留。")],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();

        // Read the `env` map (KEY -> string).
        let env = settings_config.get("env").and_then(Value::as_object);

        let read_env = |key: &str| -> String {
            env.and_then(|e| e.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };

        // Empty / missing env => OAuth (Google official / personal login).
        // Any env entries => API-key mode.
        let env_is_empty = env.map(|e| e.is_empty()).unwrap_or(true);
        let auth_mode = if env_is_empty {
            AUTH_OAUTH
        } else {
            AUTH_API_KEY
        };
        set_str(&mut values, "auth_mode", auth_mode);

        set_str(&mut values, "base_url", read_env(ENV_BASE_URL));
        set_str(&mut values, "api_key", read_env(ENV_API_KEY));
        set_str(&mut values, "model", read_env(ENV_MODEL));

        // Everything else in env => extra_env map.
        let mut extra = Map::new();
        if let Some(env) = env {
            for (k, v) in env {
                if RESERVED_ENV_KEYS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    extra.insert(k.clone(), Value::String(s.to_string()));
                }
            }
        }
        values.insert("extra_env".into(), Value::Object(extra));

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // Preserve any sibling keys in settingsConfig.
        let mut settings = prior.as_object().cloned().unwrap_or_default();

        let env = build_env(values);
        settings.insert("env".into(), Value::Object(env));

        // Preserve the `config` (settings.json) object verbatim. If the prior
        // had a non-object `config` we still want a valid object/null shape.
        match prior.get("config") {
            Some(cfg) if cfg.is_object() || cfg.is_null() => {
                settings.insert("config".into(), cfg.clone());
            }
            Some(_) => {
                // Coerce an unexpected non-object/non-null config to an empty
                // object rather than propagating something the writer rejects.
                settings.insert("config".into(), Value::Object(Map::new()));
            }
            None => {
                // Leave `config` absent so the live writer preserves the
                // existing settings.json on disk (its null/absent behaviour).
            }
        }

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: prior_meta.cloned(),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let mut env = serde_json::Map::new();
        for line in contents.first().map(String::as_str).unwrap_or("").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                if !k.is_empty() {
                    env.insert(k.to_string(), Value::String(v.trim().to_string()));
                }
            }
        }
        let config_text = contents.get(1).map(String::as_str).unwrap_or("").trim();
        let mut settings = serde_json::Map::new();
        settings.insert("env".to_string(), Value::Object(env));
        if !config_text.is_empty() {
            let cfg = serde_json::from_str::<Value>(config_text)
                .map_err(|e| format!("settings.json 解析失败: {e}"))?;
            settings.insert("config".to_string(), cfg);
        }
        Ok(Value::Object(settings))
    }

    fn preview(&self, values: &FormValues) -> Vec<PreviewFile> {
        let env = build_env(values);

        // settings.json preview: the config object as the CLI will see it.
        // The codec preserves it verbatim, so preview an empty object placeholder
        // (the on-disk file merges with whatever already exists).
        let config_obj = Value::Object(Map::new());

        vec![
            PreviewFile {
                filename: "~/.gemini/.env".into(),
                language: Language::Env,
                content: serialize_env(&env),
            },
            PreviewFile {
                filename: "~/.gemini/settings.json".into(),
                language: Language::Json,
                content: serde_json::to_string_pretty(&config_obj).unwrap_or_default(),
            },
        ]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        let auth_mode = str_val(values, "auth_mode");

        if auth_mode == AUTH_API_KEY {
            if str_val(values, "api_key").trim().is_empty() {
                issues.push(
                    ConfigIssue::warning("API Key 模式尚未填写 GEMINI_API_KEY。")
                        .for_field("api_key"),
                );
            }
            // Base URL is optional, but if present must not be a blank string —
            // build_env() drops blanks, so warn the user rather than silently.
            let base = str_val(values, "base_url");
            if !base.is_empty() && base.trim().is_empty() {
                issues.push(
                    ConfigIssue::warning("Base URL 仅含空白字符，将被忽略。").for_field("base_url"),
                );
            }
        } else if auth_mode == AUTH_OAUTH {
            // OAuth mode => env must be empty. Warn if the user filled fields
            // that would be dropped.
            if !str_val(values, "api_key").trim().is_empty()
                || !str_val(values, "base_url").trim().is_empty()
            {
                issues.push(ConfigIssue::info(
                    "OAuth 模式下 .env 将被清空（base URL / API Key 不会写入）。",
                ));
            }
        }

        issues
    }
}

/// Build the `.env` map (as a JSON object of strings) from form values.
///
/// Correctness rules:
///   - OAuth mode => empty map (no env auth at all).
///   - API-key mode => only write `GOOGLE_GEMINI_BASE_URL` / `GEMINI_API_KEY`
///     when non-empty (empty base URL flips the CLI into GATEWAY auth).
///   - `GEMINI_MODEL` is shared by both modes but only written when non-empty.
///   - `extra_env` entries are appended (reserved keys and blank keys skipped).
fn build_env(values: &FormValues) -> Map<String, Value> {
    let mut env = Map::new();

    let oauth = str_val(values, "auth_mode") == AUTH_OAUTH;

    if !oauth {
        let base_url = str_val(values, "base_url").trim();
        if !base_url.is_empty() {
            env.insert(ENV_BASE_URL.into(), Value::String(base_url.to_string()));
        }

        let api_key = str_val(values, "api_key").trim();
        if !api_key.is_empty() {
            env.insert(ENV_API_KEY.into(), Value::String(api_key.to_string()));
        }

        let model = str_val(values, "model").trim();
        if !model.is_empty() {
            env.insert(ENV_MODEL.into(), Value::String(model.to_string()));
        }

        // Arbitrary extra env vars.
        if let Some(extra) = values.get("extra_env").and_then(Value::as_object) {
            for (k, v) in extra {
                let key = k.trim();
                if key.is_empty() || RESERVED_ENV_KEYS.contains(&key) {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    env.insert(key.to_string(), Value::String(s.to_string()));
                }
            }
        }
    }
    // OAuth: leave env empty.

    env
}

/// Serialize a `.env` JSON map to `KEY=VALUE` lines (sorted for stable output,
/// matching the live writer's `serialize_env_file`).
fn serialize_env(env: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = env.keys().collect();
    keys.sort();

    let mut lines = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(value) = env.get(key).and_then(Value::as_str) {
            lines.push(format!("{key}={value}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn api_key_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "auth_mode", AUTH_API_KEY);
        set_str(&mut v, "base_url", "https://gemini.example.com");
        set_str(&mut v, "api_key", "AIza-test");
        set_str(&mut v, "model", "gemini-2.5-pro");
        v
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = GeminiConfig.decode(&Value::Null, None);
        // Empty/absent env => OAuth by default.
        assert_eq!(str_val(&values, "auth_mode"), AUTH_OAUTH);
        assert_eq!(str_val(&values, "base_url"), "");
        assert_eq!(str_val(&values, "api_key"), "");
        assert_eq!(str_val(&values, "model"), "");
        assert!(values
            .get("extra_env")
            .and_then(Value::as_object)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn decode_empty_object_is_oauth() {
        let values = GeminiConfig.decode(&json!({ "env": {}, "config": {} }), None);
        assert_eq!(str_val(&values, "auth_mode"), AUTH_OAUTH);
    }

    #[test]
    fn encode_api_key_writes_env_keys() {
        let result = GeminiConfig.encode(&api_key_values(), &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert_eq!(env[ENV_BASE_URL], json!("https://gemini.example.com"));
        assert_eq!(env[ENV_API_KEY], json!("AIza-test"));
        assert_eq!(env[ENV_MODEL], json!("gemini-2.5-pro"));
    }

    #[test]
    fn encode_omits_empty_base_url_and_key() {
        // BUG FIX (1): empty base_url / api_key must NOT be emitted as empty
        // strings (empty base URL flips the CLI into GATEWAY auth).
        let mut v = FormValues::new();
        set_str(&mut v, "auth_mode", AUTH_API_KEY);
        set_str(&mut v, "base_url", "");
        set_str(&mut v, "api_key", "");
        set_str(&mut v, "model", "gemini-2.5-flash");
        let result = GeminiConfig.encode(&v, &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert!(
            !env.contains_key(ENV_BASE_URL),
            "empty base_url leaked: {env:?}"
        );
        assert!(
            !env.contains_key(ENV_API_KEY),
            "empty api_key leaked: {env:?}"
        );
        assert_eq!(env[ENV_MODEL], json!("gemini-2.5-flash"));
        // And not as empty strings in the .env preview.
        let files = GeminiConfig.preview(&v);
        let env_file = files.iter().find(|f| f.filename.ends_with(".env")).unwrap();
        assert!(
            !env_file.content.contains(ENV_BASE_URL),
            "{}",
            env_file.content
        );
        assert!(
            !env_file.content.contains(ENV_API_KEY),
            "{}",
            env_file.content
        );
    }

    #[test]
    fn encode_oauth_emits_empty_env() {
        // BUG FIX (2): OAuth mode => env must be EMPTY, even if base_url/key set.
        let mut v = api_key_values();
        set_str(&mut v, "auth_mode", AUTH_OAUTH);
        let result = GeminiConfig.encode(&v, &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert!(env.is_empty(), "oauth env not empty: {env:?}");
    }

    #[test]
    fn encode_preserves_config_object_verbatim() {
        let prior = json!({
            "env": { "GEMINI_API_KEY": "old" },
            "config": { "mcpServers": { "fs": { "command": "x" } }, "theme": "dark" }
        });
        let result = GeminiConfig.encode(&api_key_values(), &prior, None);
        // The settings.json (config) object survives untouched.
        assert_eq!(
            result.settings_config["config"]["mcpServers"]["fs"]["command"],
            json!("x")
        );
        assert_eq!(result.settings_config["config"]["theme"], json!("dark"));
    }

    #[test]
    fn encode_preserves_unknown_sibling_keys() {
        let prior = json!({ "env": {}, "config": {}, "modelCatalog": { "models": [] } });
        let result = GeminiConfig.encode(&api_key_values(), &prior, None);
        assert!(result.settings_config.get("modelCatalog").is_some());
    }

    #[test]
    fn extra_env_round_trips() {
        let mut v = api_key_values();
        v.insert(
            "extra_env".into(),
            json!({ "GEMINI_API_VERSION": "v1beta" }),
        );
        let encoded = GeminiConfig.encode(&v, &Value::Null, None);
        let env = encoded.settings_config["env"].as_object().unwrap();
        assert_eq!(env["GEMINI_API_VERSION"], json!("v1beta"));

        let decoded = GeminiConfig.decode(&encoded.settings_config, None);
        assert_eq!(
            decoded["extra_env"]["GEMINI_API_VERSION"].as_str(),
            Some("v1beta")
        );
        // Reserved keys never bleed into extra_env.
        let extra = decoded["extra_env"].as_object().unwrap();
        assert!(!extra.contains_key(ENV_API_KEY));
    }

    #[test]
    fn round_trip_preserves_api_key_fields() {
        let original = api_key_values();
        let encoded = GeminiConfig.encode(&original, &Value::Null, None);
        let decoded = GeminiConfig.decode(&encoded.settings_config, None);
        for key in ["auth_mode", "base_url", "api_key", "model"] {
            assert_eq!(
                str_val(&decoded, key),
                str_val(&original, key),
                "field {key}"
            );
        }
    }

    #[test]
    fn preview_emits_env_and_settings_files() {
        let files = GeminiConfig.preview(&api_key_values());
        assert_eq!(files.len(), 2);
        let env_file = files.iter().find(|f| f.filename.ends_with(".env")).unwrap();
        assert_eq!(env_file.language, Language::Env);
        // KEY=VALUE lines.
        assert!(
            env_file.content.contains("GEMINI_API_KEY=AIza-test"),
            "{}",
            env_file.content
        );
        assert!(env_file
            .content
            .contains("GOOGLE_GEMINI_BASE_URL=https://gemini.example.com"));
        assert!(env_file.content.contains("GEMINI_MODEL=gemini-2.5-pro"));

        let settings_file = files
            .iter()
            .find(|f| f.filename.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings_file.language, Language::Json);
    }

    #[test]
    fn validate_warns_missing_api_key() {
        let mut v = api_key_values();
        set_str(&mut v, "api_key", "");
        let issues = GeminiConfig.validate(&v);
        assert!(issues.iter().any(|i| i.field.as_deref() == Some("api_key")));
    }
}
