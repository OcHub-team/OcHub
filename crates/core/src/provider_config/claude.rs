//! Claude Code provider config codec.
//!
//! Claude Code reads `~/.claude/settings.json`; the provider-relevant slice is
//! the `env` object. OCHUB stores the whole settings document in one
//! `settingsConfig`, shaped `{ "env": { … } }`. The interesting env keys are:
//!
//! - `ANTHROPIC_BASE_URL` — the endpoint.
//! - an *auth* var: either `ANTHROPIC_AUTH_TOKEN` (Bearer, default) **or**
//!   `ANTHROPIC_API_KEY` (`x-api-key`). The two are mutually exclusive and the
//!   user picks which one to write; the choice is recorded in
//!   [`ProviderMeta::api_key_field`].
//! - `ANTHROPIC_MODEL` — the fallback model.
//! - the four per-role overrides `ANTHROPIC_DEFAULT_{SONNET,OPUS,HAIKU,FABLE}_MODEL`
//!   and their display-name twins `*_MODEL_NAME`.
//!
//! A few non-env, transform-only fields live in [`ProviderMeta`] rather than in
//! `settings.json` (they must never be written to the live file): `api_format`,
//! `api_key_field`, `custom_user_agent`, `is_full_url`.
//!
//! This codec edits the `env` object *structurally* — it merges into the prior
//! settings so any native keys (statusline, permissions, hooks, …) and any env
//! keys the form does not model survive a round-trip. It deliberately does **not**
//! re-introduce the legacy `ANTHROPIC_SMALL_FAST_MODEL`, which the switch path
//! (`normalize_claude_models_in_value`) removes.

use serde_json::{json, Map, Value};

use super::{
    bool_val, set_bool, set_str, str_val, AppConfig, ConfigIssue, EncodeResult, FieldKind,
    FormField, FormSection, FormValues, GridColumn, Language, PreviewFile, SelectOption,
};
use crate::model::ProviderMeta;
use crate::AppType;

const AUTH_TOKEN_KEY: &str = "ANTHROPIC_AUTH_TOKEN";
const AUTH_API_KEY: &str = "ANTHROPIC_API_KEY";
const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
const MODEL_KEY: &str = "ANTHROPIC_MODEL";

const API_FORMAT_ANTHROPIC: &str = "anthropic";

/// The `[1M]` context-window marker, in the casing written to the env value
/// (matched case-insensitively elsewhere). Haiku does not support it.
const ONE_M_MARKER: &str = "[1M]";

/// The four routable Claude roles, with their env keys. The display-name twin is
/// always `<model_key>_NAME`; `supports_one_m` is false only for haiku.
struct Role {
    /// Grid `role` cell value / stable identifier.
    id: &'static str,
    /// `ANTHROPIC_DEFAULT_<ROLE>_MODEL`.
    model_key: &'static str,
    /// `ANTHROPIC_DEFAULT_<ROLE>_MODEL_NAME`.
    name_key: &'static str,
    supports_one_m: bool,
}

const ROLES: [Role; 4] = [
    Role {
        id: "sonnet",
        model_key: "ANTHROPIC_DEFAULT_SONNET_MODEL",
        name_key: "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        supports_one_m: true,
    },
    Role {
        id: "opus",
        model_key: "ANTHROPIC_DEFAULT_OPUS_MODEL",
        name_key: "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        supports_one_m: true,
    },
    Role {
        id: "haiku",
        model_key: "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        name_key: "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        supports_one_m: false,
    },
    Role {
        id: "fable",
        model_key: "ANTHROPIC_DEFAULT_FABLE_MODEL",
        name_key: "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
        supports_one_m: true,
    },
];

pub struct ClaudeConfig;

impl AppConfig for ClaudeConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::Claude.app_id()
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "端点与鉴权",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://api.anthropic.com".into(),
                        },
                    )
                    .help("写入 env.ANTHROPIC_BASE_URL；Claude Code 会在其后拼接 /v1/messages。")
                    .required(),
                    FormField::new(
                        "auth_field",
                        "鉴权变量",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(AUTH_TOKEN_KEY, "ANTHROPIC_AUTH_TOKEN（Bearer）")
                                    .with_hint("中转/第三方默认；走 Authorization: Bearer"),
                                SelectOption::new(AUTH_API_KEY, "ANTHROPIC_API_KEY（x-api-key）")
                                    .with_hint("Anthropic 官方密钥；走 x-api-key"),
                            ],
                        },
                    )
                    .help("决定密钥写入哪个 env 变量；二者互斥，所选项同时记入 meta.apiKeyField。"),
                    FormField::new(
                        "api_key",
                        "API Key",
                        FieldKind::Secret {
                            placeholder: "sk-ant-...".into(),
                        },
                    )
                    .help("写入上方所选的鉴权变量。"),
                ],
            ),
            FormSection::new(
                "模型",
                vec![
                    FormField::new(
                        "model",
                        "默认模型 (ANTHROPIC_MODEL)",
                        FieldKind::Text {
                            placeholder: "claude-sonnet-4-6".into(),
                        },
                    )
                    .help("各角色未单独配置时的回退模型。"),
                    FormField::new(
                        "roles",
                        "角色模型映射",
                        FieldKind::ModelGrid {
                            columns: vec![
                                GridColumn::text("role", "角色", "sonnet"),
                                GridColumn::text("model", "模型 ID", "claude-sonnet-4-6"),
                                GridColumn::text("name", "显示名", "Sonnet 4.6"),
                                GridColumn::toggle("one_m", "1M 上下文"),
                            ],
                        },
                    )
                    .help(
                        "sonnet/opus/haiku/fable 对应 ANTHROPIC_DEFAULT_*_MODEL 与 *_MODEL_NAME；\
                         开启 1M 会给模型 ID 追加 [1M] 标记（haiku 不支持）。",
                    ),
                ],
            ),
            FormSection::new(
                "高级",
                vec![
                    FormField::new(
                        "api_format",
                        "API 格式 (meta)",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(API_FORMAT_ANTHROPIC, "anthropic（原生）")
                                    .with_hint("默认；不做请求体转换"),
                                SelectOption::new("openai_chat", "openai_chat")
                                    .with_hint("OpenAI Chat Completions 上游"),
                                SelectOption::new("openai_responses", "openai_responses")
                                    .with_hint("OpenAI Responses 上游"),
                                SelectOption::new("gemini_native", "gemini_native")
                                    .with_hint("Gemini 原生上游"),
                            ],
                        },
                    )
                    .help("仅存于 meta.apiFormat，绝不写入 settings.json；用于网关协议转换。"),
                    FormField::new(
                        "custom_user_agent",
                        "自定义 User-Agent (meta)",
                        FieldKind::Text {
                            placeholder: "claude-cli/1.0".into(),
                        },
                    )
                    .help("存于 meta.customUserAgent；留空则使用默认 UA。"),
                    FormField::new(
                        "is_full_url",
                        "Base URL 为完整地址 (meta)",
                        FieldKind::Toggle,
                    )
                    .help("存于 meta.isFullUrl；上游已是完整端点、网关不再追加路径时开启。"),
                ],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let env = settings_config.get("env");

        set_str(&mut values, "base_url", env_str(env, BASE_URL_KEY));

        // Which auth var holds the key? Prefer the explicit meta choice; else
        // detect from whichever var is present; else default to AUTH_TOKEN_KEY.
        let auth_field = meta
            .and_then(|m| m.api_key_field.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if !env_str(env, AUTH_API_KEY).is_empty() && env_str(env, AUTH_TOKEN_KEY).is_empty()
                {
                    AUTH_API_KEY.to_string()
                } else {
                    AUTH_TOKEN_KEY.to_string()
                }
            });
        let api_key = {
            let primary = env_str(env, &auth_field);
            if primary.is_empty() {
                // Fall back to the other var if the recorded one is empty.
                let other = if auth_field == AUTH_API_KEY {
                    AUTH_TOKEN_KEY
                } else {
                    AUTH_API_KEY
                };
                env_str(env, other)
            } else {
                primary
            }
        };
        set_str(&mut values, "auth_field", auth_field);
        set_str(&mut values, "api_key", api_key);

        set_str(&mut values, "model", env_str(env, MODEL_KEY));

        // Per-role grid rows.
        let rows: Vec<Value> = ROLES
            .iter()
            .map(|role| {
                let raw_model = env_str(env, role.model_key);
                let (model, one_m) = split_one_m(&raw_model);
                json!({
                    "role": role.id,
                    "model": model,
                    "name": env_str(env, role.name_key),
                    "one_m": role.supports_one_m && one_m,
                })
            })
            .collect();
        values.insert("roles".into(), Value::Array(rows));

        // Meta-backed fields.
        set_str(
            &mut values,
            "api_format",
            meta.and_then(|m| m.api_format.as_deref())
                .filter(|s| !s.is_empty())
                .unwrap_or(API_FORMAT_ANTHROPIC),
        );
        set_str(
            &mut values,
            "custom_user_agent",
            meta.and_then(|m| m.custom_user_agent.as_deref())
                .unwrap_or_default(),
        );
        set_bool(
            &mut values,
            "is_full_url",
            meta.and_then(|m| m.is_full_url).unwrap_or(false),
        );

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // Preserve any sibling keys in settingsConfig and inside env.
        let mut settings = prior.as_object().cloned().unwrap_or_default();
        let mut env = settings
            .get("env")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        set_env(
            &mut env,
            BASE_URL_KEY,
            str_val(values, "base_url").trim().trim_end_matches('/'),
        );

        // Auth var: write whichever the user picked, clear the other one so the
        // two never coexist. (Bug fix #1: never hard-lock to ANTHROPIC_AUTH_TOKEN.)
        let auth_field = chosen_auth_field(values);
        let other_field = if auth_field == AUTH_API_KEY {
            AUTH_TOKEN_KEY
        } else {
            AUTH_API_KEY
        };
        let api_key = str_val(values, "api_key");
        set_env(&mut env, auth_field, api_key);
        env.remove(other_field);

        set_env(&mut env, MODEL_KEY, str_val(values, "model").trim());

        // Per-role model + display-name + [1M] marker. (Bug fix #2: every role,
        // including fable, is editable.)
        for role in ROLES.iter() {
            let row = role_row(values, role.id);
            let model = row_str(row.as_ref(), "model");
            let name = row_str(row.as_ref(), "name");
            let one_m = role.supports_one_m && row_bool(row.as_ref(), "one_m");
            let model_value = if one_m && !model.is_empty() {
                format!("{model} {ONE_M_MARKER}")
            } else {
                model.to_string()
            };
            set_env(&mut env, role.model_key, &model_value);
            set_env(&mut env, role.name_key, name);
        }

        settings.insert("env".into(), Value::Object(env));

        // Meta-backed fields (bug fix #3: encode writes meta). Clone prior meta so
        // unrelated fields (auth_binding, pricing, …) survive.
        let mut meta = prior_meta.cloned().unwrap_or_default();
        meta.api_key_field = Some(auth_field.to_string());
        let api_format = str_val(values, "api_format").trim();
        meta.api_format = if api_format.is_empty() || api_format == API_FORMAT_ANTHROPIC {
            None
        } else {
            Some(api_format.to_string())
        };
        let ua = str_val(values, "custom_user_agent").trim();
        meta.custom_user_agent = if ua.is_empty() {
            None
        } else {
            Some(ua.to_string())
        };
        meta.is_full_url = if bool_val(values, "is_full_url") {
            Some(true)
        } else {
            None
        };

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: Some(meta),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let text = contents.first().map(String::as_str).unwrap_or("{}");
        serde_json::from_str::<Value>(text).map_err(|e| format!("settings.json 解析失败: {e}"))
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        let encoded = self.encode(values, prior, None);
        vec![PreviewFile {
            filename: "~/.claude/settings.json".into(),
            language: Language::Json,
            content: serde_json::to_string_pretty(&encoded.settings_config).unwrap_or_default(),
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        if str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        }

        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 API Key。").for_field("api_key"));
        }

        let auth_field = chosen_auth_field(values);
        if auth_field == AUTH_API_KEY {
            issues.push(
                ConfigIssue::info(
                    "已选 ANTHROPIC_API_KEY（x-api-key）；多数中转服务需改用 ANTHROPIC_AUTH_TOKEN（Bearer）。",
                )
                .for_field("auth_field"),
            );
        }

        // 1M on haiku is unsupported and will be ignored.
        for role in ROLES.iter().filter(|r| !r.supports_one_m) {
            let row = role_row(values, role.id);
            if row_bool(row.as_ref(), "one_m") {
                issues.push(ConfigIssue::warning(format!(
                    "{} 不支持 1M 上下文，[1M] 标记将被忽略。",
                    role.id
                )));
            }
        }

        issues
    }
}

// ---- helpers ----------------------------------------------------------------

/// Read an env string from the (optional) env object.
fn env_str(env: Option<&Value>, key: &str) -> String {
    env.and_then(|e| e.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Write a non-empty env value, or remove the key when empty.
fn set_env(env: &mut Map<String, Value>, key: &str, value: &str) {
    if value.trim().is_empty() {
        env.remove(key);
    } else {
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// The auth env var the user picked, defaulting to the Bearer token var.
fn chosen_auth_field(values: &FormValues) -> &'static str {
    match str_val(values, "auth_field") {
        AUTH_API_KEY => AUTH_API_KEY,
        _ => AUTH_TOKEN_KEY,
    }
}

/// Split a stored model id into `(bare_model, has_one_m)`, stripping a trailing
/// `[1M]` marker (matched case-insensitively, with optional separating space).
fn split_one_m(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    let marker = ONE_M_MARKER; // "[1M]"
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with(&marker.to_ascii_lowercase()) {
        let bare = trimmed[..trimmed.len() - marker.len()].trim_end();
        (bare.to_string(), true)
    } else {
        (trimmed.to_string(), false)
    }
}

/// Find the grid row object for a given role id.
fn role_row(values: &FormValues, role_id: &str) -> Option<Value> {
    values
        .get("roles")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("role").and_then(Value::as_str) == Some(role_id))
                .cloned()
        })
}

fn row_str<'a>(row: Option<&'a Value>, key: &str) -> &'a str {
    row.and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn row_bool(row: Option<&Value>, key: &str) -> bool {
    row.and_then(|r| r.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "base_url", "https://api.relay.example/");
        set_str(&mut v, "auth_field", AUTH_TOKEN_KEY);
        set_str(&mut v, "api_key", "sk-relay-123");
        set_str(&mut v, "model", "claude-sonnet-4-6");
        v.insert(
            "roles".into(),
            json!([
                { "role": "sonnet", "model": "claude-sonnet-4-6", "name": "Sonnet 4.6", "one_m": true },
                { "role": "opus", "model": "claude-opus-4-8", "name": "Opus 4.8", "one_m": false },
                { "role": "haiku", "model": "claude-haiku-4-5", "name": "Haiku 4.5", "one_m": false },
                { "role": "fable", "model": "claude-fable-1", "name": "Fable", "one_m": true },
            ]),
        );
        set_str(&mut v, "api_format", API_FORMAT_ANTHROPIC);
        v
    }

    fn env_of(result: &EncodeResult) -> &Map<String, Value> {
        result.settings_config["env"].as_object().unwrap()
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = ClaudeConfig.decode(&Value::Null, None);
        assert_eq!(str_val(&values, "base_url"), "");
        assert_eq!(str_val(&values, "auth_field"), AUTH_TOKEN_KEY);
        assert_eq!(str_val(&values, "api_key"), "");
        assert_eq!(str_val(&values, "api_format"), API_FORMAT_ANTHROPIC);
        assert!(!bool_val(&values, "is_full_url"));
        // Four role rows, all present, fable included.
        let roles = values["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 4);
        let ids: Vec<&str> = roles.iter().map(|r| r["role"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["sonnet", "opus", "haiku", "fable"]);
    }

    #[test]
    fn encode_writes_chosen_auth_var_and_meta() {
        // Bug fix #1: user picks x-api-key -> that var is written, not AUTH_TOKEN.
        let mut v = sample_values();
        set_str(&mut v, "auth_field", AUTH_API_KEY);
        let result = ClaudeConfig.encode(&v, &Value::Null, None);
        let env = env_of(&result);
        assert_eq!(env[AUTH_API_KEY].as_str(), Some("sk-relay-123"));
        assert!(
            !env.contains_key(AUTH_TOKEN_KEY),
            "AUTH_TOKEN must not be written when API_KEY chosen"
        );
        // Bug fix #3: meta records the choice.
        let meta = result.meta.unwrap();
        assert_eq!(meta.api_key_field.as_deref(), Some(AUTH_API_KEY));
    }

    #[test]
    fn encode_default_auth_is_token_not_locked() {
        let result = ClaudeConfig.encode(&sample_values(), &Value::Null, None);
        let env = env_of(&result);
        assert_eq!(env[AUTH_TOKEN_KEY].as_str(), Some("sk-relay-123"));
        assert!(!env.contains_key(AUTH_API_KEY));
        assert_eq!(
            result.meta.unwrap().api_key_field.as_deref(),
            Some(AUTH_TOKEN_KEY)
        );
    }

    #[test]
    fn encode_per_role_models_including_fable_and_one_m() {
        // Bug fix #2: all four DEFAULT_*_MODEL + *_MODEL_NAME, incl. fable.
        let result = ClaudeConfig.encode(&sample_values(), &Value::Null, None);
        let env = env_of(&result);
        assert_eq!(
            env["ANTHROPIC_DEFAULT_SONNET_MODEL"].as_str(),
            Some("claude-sonnet-4-6 [1M]")
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"].as_str(),
            Some("Sonnet 4.6")
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_OPUS_MODEL"].as_str(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_FABLE_MODEL"].as_str(),
            Some("claude-fable-1 [1M]")
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_FABLE_MODEL_NAME"].as_str(),
            Some("Fable")
        );
        // Model fallback present.
        assert_eq!(env["ANTHROPIC_MODEL"].as_str(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn haiku_one_m_is_never_written() {
        let mut v = sample_values();
        // Force the haiku row's one_m on; it must still be ignored.
        v.insert(
            "roles".into(),
            json!([
                { "role": "haiku", "model": "claude-haiku-4-5", "name": "Haiku", "one_m": true },
            ]),
        );
        let result = ClaudeConfig.encode(&v, &Value::Null, None);
        let env = env_of(&result);
        assert_eq!(
            env["ANTHROPIC_DEFAULT_HAIKU_MODEL"].as_str(),
            Some("claude-haiku-4-5"),
            "haiku must not gain a [1M] marker"
        );
    }

    #[test]
    fn encode_preserves_native_env_and_sibling_keys() {
        let prior = json!({
            "statusLine": { "type": "command" },
            "env": {
                "ANTHROPIC_BASE_URL": "https://old/",
                "MY_CUSTOM_VAR": "keep-me",
                "ANTHROPIC_SMALL_FAST_MODEL": "legacy"
            }
        });
        let result = ClaudeConfig.encode(&sample_values(), &prior, None);
        // Sibling top-level key survives.
        assert!(result.settings_config.get("statusLine").is_some());
        let env = env_of(&result);
        // Unknown env key survives.
        assert_eq!(env["MY_CUSTOM_VAR"].as_str(), Some("keep-me"));
        // Base URL is overwritten.
        assert_eq!(
            env[BASE_URL_KEY].as_str(),
            Some("https://api.relay.example")
        );
        // We do not strip SMALL_FAST ourselves (the switch path does that); but we
        // also must not re-introduce it if absent. Here it was present, so it stays.
        assert_eq!(env["ANTHROPIC_SMALL_FAST_MODEL"].as_str(), Some("legacy"));
    }

    #[test]
    fn does_not_introduce_small_fast_model() {
        let result = ClaudeConfig.encode(&sample_values(), &Value::Null, None);
        let env = env_of(&result);
        assert!(!env.contains_key("ANTHROPIC_SMALL_FAST_MODEL"));
    }

    #[test]
    fn round_trip_through_decode() {
        let original = sample_values();
        let encoded = ClaudeConfig.encode(&original, &Value::Null, None);
        let decoded = ClaudeConfig.decode(&encoded.settings_config, encoded.meta.as_ref());

        assert_eq!(str_val(&decoded, "base_url"), "https://api.relay.example");
        assert_eq!(str_val(&decoded, "auth_field"), AUTH_TOKEN_KEY);
        assert_eq!(str_val(&decoded, "api_key"), "sk-relay-123");
        assert_eq!(str_val(&decoded, "model"), "claude-sonnet-4-6");

        // Roles round-trip, marker round-trips, name round-trips.
        let find = |id: &str| -> Value {
            decoded["roles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["role"].as_str() == Some(id))
                .cloned()
                .unwrap()
        };
        let sonnet = find("sonnet");
        assert_eq!(sonnet["model"].as_str(), Some("claude-sonnet-4-6"));
        assert_eq!(sonnet["one_m"].as_bool(), Some(true));
        assert_eq!(sonnet["name"].as_str(), Some("Sonnet 4.6"));
        let fable = find("fable");
        assert_eq!(fable["model"].as_str(), Some("claude-fable-1"));
        assert_eq!(fable["one_m"].as_bool(), Some(true));
        let opus = find("opus");
        assert_eq!(opus["one_m"].as_bool(), Some(false));
    }

    #[test]
    fn round_trip_x_api_key_choice() {
        let mut v = sample_values();
        set_str(&mut v, "auth_field", AUTH_API_KEY);
        let encoded = ClaudeConfig.encode(&v, &Value::Null, None);
        let decoded = ClaudeConfig.decode(&encoded.settings_config, encoded.meta.as_ref());
        assert_eq!(str_val(&decoded, "auth_field"), AUTH_API_KEY);
        assert_eq!(str_val(&decoded, "api_key"), "sk-relay-123");
    }

    #[test]
    fn encode_writes_non_default_api_format_to_meta() {
        let mut v = sample_values();
        set_str(&mut v, "api_format", "openai_responses");
        set_str(&mut v, "custom_user_agent", "ua/9");
        set_bool(&mut v, "is_full_url", true);
        let result = ClaudeConfig.encode(&v, &Value::Null, None);
        let meta = result.meta.clone().unwrap();
        assert_eq!(meta.api_format.as_deref(), Some("openai_responses"));
        assert_eq!(meta.custom_user_agent.as_deref(), Some("ua/9"));
        assert_eq!(meta.is_full_url, Some(true));
        // api_format must NEVER leak into settings.json env.
        let env = env_of(&result);
        assert!(!env.contains_key("api_format"));
        assert!(env.get("ANTHROPIC_API_FORMAT").is_none());
    }

    #[test]
    fn encode_preserves_unrelated_prior_meta() {
        let mut prior_meta = ProviderMeta::default();
        prior_meta.cost_multiplier = Some("1.5".into());
        let result = ClaudeConfig.encode(&sample_values(), &Value::Null, Some(&prior_meta));
        let meta = result.meta.unwrap();
        assert_eq!(meta.cost_multiplier.as_deref(), Some("1.5"));
        assert_eq!(meta.api_key_field.as_deref(), Some(AUTH_TOKEN_KEY));
    }

    #[test]
    fn anthropic_api_format_is_none_in_meta() {
        let result = ClaudeConfig.encode(&sample_values(), &Value::Null, None);
        // Default "anthropic" stays None so it does not override the gateway default.
        assert!(result.meta.unwrap().api_format.is_none());
    }

    #[test]
    fn preview_emits_single_settings_json() {
        let files = ClaudeConfig.preview(&sample_values(), &Value::Null);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "~/.claude/settings.json");
        assert_eq!(files[0].language, Language::Json);
        assert!(files[0].content.contains("\"env\""));
        assert!(files[0].content.contains("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn validate_flags_empty_base_url_and_haiku_one_m() {
        let mut v = sample_values();
        set_str(&mut v, "base_url", "");
        v.insert(
            "roles".into(),
            json!([
                { "role": "haiku", "model": "claude-haiku-4-5", "name": "", "one_m": true },
            ]),
        );
        let issues = ClaudeConfig.validate(&v);
        assert!(issues
            .iter()
            .any(|i| i.severity == super::super::Severity::Error
                && i.field.as_deref() == Some("base_url")));
        assert!(issues.iter().any(|i| i.message.contains("1M")));
    }

    #[test]
    fn split_one_m_handles_casing_and_spacing() {
        assert_eq!(
            split_one_m("claude-sonnet-4-6 [1M]"),
            ("claude-sonnet-4-6".into(), true)
        );
        assert_eq!(
            split_one_m("claude-sonnet-4-6[1m]"),
            ("claude-sonnet-4-6".into(), true)
        );
        assert_eq!(
            split_one_m("claude-sonnet-4-6"),
            ("claude-sonnet-4-6".into(), false)
        );
    }
}
