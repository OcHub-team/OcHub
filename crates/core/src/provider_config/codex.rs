//! Codex provider config codec.
//!
//! Codex reads two files: `~/.codex/auth.json` (OpenAI-login material) and
//! `~/.codex/config.toml` (model + provider table, including relay bearer
//! tokens). OcHub
//! stores both inside one `settingsConfig` object shaped
//! `{ "auth": { … }, "config": "<config.toml text>" }`.
//!
//! This codec edits that shape *structurally* — the `config.toml` is parsed and
//! emitted with `toml_edit`, preserving any keys the form does not model, rather
//! than asking the user to hand-write escaped TOML inside a JSON blob.

use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value as TomlValue};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues, Language,
    PreviewFile, SelectOption, bool_val, set_bool, set_str, str_val,
};
use crate::AppType;
use crate::model::ProviderMeta;

const AUTH_API_KEY: &str = "api_key";
const AUTH_OPENAI_LOGIN: &str = "openai_login";
const AUTH_OPENAI_LOGIN_WITH_API_KEY: &str = "openai_login_with_api_key";
const OPENAI_PROVIDER_NAME: &str = "OpenAI";
const HAS_OPENAI_LOGIN: &str = "_has_openai_login";
const LEGACY_ENV_KEY: &str = "_legacy_env_key";

pub struct CodexConfig;

impl AppConfig for CodexConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::Codex.app_id()
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
                            placeholder: "custom".into(),
                        },
                    )
                    .help("config.toml 中 [model_providers.<id>] 的键，也是 model_provider 的值。")
                    .required(),
                    FormField::new(
                        "name",
                        "普通 Provider 名称",
                        FieldKind::Text {
                            placeholder: "Custom".into(),
                        },
                    )
                    .help("这是 config.toml 中的 provider name，不是 OcHub 列表名称；开启远程压缩时会强制写为 OpenAI。"),
                    FormField::new("remote_compaction", "远程压缩", FieldKind::Toggle)
                        .help("Codex 仅在 provider name 精确为 OpenAI 时开启远程压缩。请确认上游完整支持 Responses 远程压缩。"),
                ],
            ),
            FormSection::new(
                "端点与鉴权",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://api.example.com/v1".into(),
                        },
                    )
                    .help("第三方 API 模式必须填写，通常以 /v1 结尾；仅账号登录时可留空使用 Codex 官方端点。"),
                    FormField::new(
                        "auth_mode",
                        "鉴权方式",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(AUTH_API_KEY, "仅第三方 API")
                                    .with_hint("experimental_bearer_token"),
                                SelectOption::new(AUTH_OPENAI_LOGIN, "仅 ChatGPT 账号登录")
                                    .with_hint("requires_openai_auth=true"),
                                SelectOption::new(
                                    AUTH_OPENAI_LOGIN_WITH_API_KEY,
                                    "ChatGPT 登录 + 第三方 API",
                                )
                                .with_hint(
                                    "auth.json 保留登录态，请求使用 experimental_bearer_token",
                                ),
                            ],
                        },
                    )
                    .help("组合模式不会把第三方密钥伪装成 HTTP 头：账号仍由 auth.json 管理，模型请求的 Authorization 由 experimental_bearer_token 提供。"),
                    FormField::new(
                        "api_key",
                        "第三方 API Key",
                        FieldKind::Secret {
                            placeholder: "sk-...".into(),
                        },
                    )
                    .help("仅两种第三方 API 模式使用；保存后写入当前 provider 的 experimental_bearer_token。"),
                ],
            ),
            FormSection::new(
                "模型",
                vec![
                    FormField::new(
                        "model",
                        "模型",
                        FieldKind::Text {
                            placeholder: "gpt-5.5".into(),
                        },
                    ),
                    FormField::new(
                        "reasoning_effort",
                        "推理强度 (model_reasoning_effort)",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new("", "（不设置）"),
                                SelectOption::new("minimal", "minimal"),
                                SelectOption::new("low", "low"),
                                SelectOption::new("medium", "medium"),
                                SelectOption::new("high", "high"),
                                SelectOption::new("xhigh", "xhigh"),
                            ],
                        },
                    )
                    .help("仅 Responses API 且依模型而定。"),
                ],
            ),
            FormSection::new(
                "高级",
                vec![
                    FormField::new(
                        "wire_api",
                        "wire_api",
                        FieldKind::Select {
                            options: vec![SelectOption::new("responses", "responses")
                                .with_hint("当前 Codex 仅支持 responses")],
                        },
                    )
                    .help("chat 已被 Codex 移除；chat-only 上游需经网关翻译。"),
                    FormField::new("disable_response_storage", "禁用响应存储", FieldKind::Toggle)
                        .help("disable_response_storage = true（ZDR / 不支持存储的上游）。"),
                    FormField::new(
                        "supports_websockets",
                        "支持 Responses WebSocket",
                        FieldKind::Toggle,
                    )
                    .help("直连时可手动声明；模型供应商模式由所选供应商的 WebSocket 能力决定，不可单独覆盖。"),
                    FormField::new(
                        "query_params",
                        "Query 参数",
                        FieldKind::KeyValue {
                            key_placeholder: "api-version".into(),
                            value_placeholder: "2025-04-01-preview".into(),
                        },
                    )
                    .help("追加到每次请求 URL（如 Azure 的 api-version）。"),
                    FormField::new(
                        "http_headers",
                        "HTTP 头",
                        FieldKind::KeyValue {
                            key_placeholder: "X-Header".into(),
                            value_placeholder: "value".into(),
                        },
                    ),
                ],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();

        let config_text = settings_config
            .get("config")
            .and_then(Value::as_str)
            .unwrap_or("");
        let doc = config_text.parse::<DocumentMut>().ok();

        let provider_id = doc
            .as_ref()
            .and_then(|d| d.get("model_provider"))
            .and_then(Item::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("custom")
            .to_string();
        set_str(&mut values, "provider_id", provider_id.clone());

        set_str(
            &mut values,
            "model",
            doc.as_ref()
                .and_then(|d| d.get("model"))
                .and_then(Item::as_str)
                .unwrap_or_default(),
        );
        set_str(
            &mut values,
            "reasoning_effort",
            doc.as_ref()
                .and_then(|d| d.get("model_reasoning_effort"))
                .and_then(Item::as_str)
                .unwrap_or_default(),
        );
        set_bool(
            &mut values,
            "disable_response_storage",
            doc.as_ref()
                .and_then(|d| d.get("disable_response_storage"))
                .and_then(Item::as_bool)
                .unwrap_or(false),
        );

        // Provider table fields.
        let ptbl = doc
            .as_ref()
            .and_then(|d| d.get("model_providers"))
            .and_then(Item::as_table)
            .and_then(|t| t.get(&provider_id))
            .and_then(Item::as_table);
        set_bool(
            &mut values,
            "supports_websockets",
            ptbl.and_then(|table| table.get("supports_websockets"))
                .and_then(Item::as_bool)
                .unwrap_or(false),
        );

        let read = |key: &str| -> String {
            ptbl.and_then(|t| t.get(key))
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string()
        };

        let name = read("name");
        let remote_compaction = name == OPENAI_PROVIDER_NAME;
        set_bool(&mut values, "remote_compaction", remote_compaction);
        set_str(
            &mut values,
            "name",
            if name.is_empty() || remote_compaction {
                provider_id.clone()
            } else {
                name
            },
        );
        set_str(&mut values, "base_url", read("base_url"));

        let wire_api = read("wire_api");
        set_str(
            &mut values,
            "wire_api",
            if wire_api.is_empty() {
                "responses".into()
            } else {
                wire_api
            },
        );

        let env_key = read("env_key");
        let provider_token =
            crate::apps::codex::extract_codex_experimental_bearer_token(config_text);
        let auth_token = settings_config
            .get("auth")
            .and_then(crate::apps::codex::extract_codex_auth_api_key);
        let literal_env_key =
            (!env_key.is_empty() && !is_valid_env_key_name(&env_key)).then(|| env_key.clone());
        let api_key = provider_token.or(auth_token).or(literal_env_key);
        set_str(&mut values, "api_key", api_key.clone().unwrap_or_default());
        set_str(
            &mut values,
            LEGACY_ENV_KEY,
            if is_valid_env_key_name(&env_key) {
                env_key.clone()
            } else {
                String::new()
            },
        );

        let requires_openai_auth = ptbl
            .and_then(|t| t.get("requires_openai_auth"))
            .and_then(Item::as_bool)
            .unwrap_or(false);
        let has_relay_credentials = api_key.is_some() || !env_key.is_empty();
        let auth_mode = if requires_openai_auth && has_relay_credentials {
            AUTH_OPENAI_LOGIN_WITH_API_KEY
        } else if requires_openai_auth {
            AUTH_OPENAI_LOGIN
        } else {
            AUTH_API_KEY
        };
        set_str(&mut values, "auth_mode", auth_mode);
        set_bool(
            &mut values,
            HAS_OPENAI_LOGIN,
            settings_config
                .get("auth")
                .is_some_and(crate::apps::codex::codex_auth_has_oauth_login_material),
        );

        values.insert(
            "query_params".into(),
            inline_table_to_json(ptbl, "query_params"),
        );
        values.insert(
            "http_headers".into(),
            inline_table_to_json(ptbl, "http_headers"),
        );

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        let prior_config_text = prior.get("config").and_then(Value::as_str).unwrap_or("");
        let config_text = build_config_text(values, prior_config_text);

        // Preserve any sibling keys in settingsConfig (e.g. modelCatalog).
        let mut settings = prior.as_object().cloned().unwrap_or_default();
        settings.insert("auth".into(), build_auth(values, prior));
        settings.insert("config".into(), Value::String(config_text));

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: prior_meta.cloned(),
        }
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let config = contents.first().cloned().unwrap_or_default();
        config
            .parse::<DocumentMut>()
            .map_err(|e| format!("config.toml 解析失败: {e}"))?;
        let auth = match contents.get(1) {
            Some(s) if !s.trim().is_empty() => {
                serde_json::from_str::<Value>(s).map_err(|e| format!("auth.json 解析失败: {e}"))?
            }
            _ => json!({}),
        };
        Ok(json!({ "auth": auth, "config": config }))
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        // The stored `config` TOML text is the merge base, so hand-edited /
        // native keys show up in the preview exactly as they will be written.
        let prior_config = prior
            .get("config")
            .and_then(Value::as_str)
            .unwrap_or_default();
        vec![
            PreviewFile {
                filename: "~/.codex/config.toml".into(),
                language: Language::Toml,
                content: build_config_text(values, prior_config),
            },
            PreviewFile {
                filename: "~/.codex/auth.json".into(),
                language: Language::Json,
                content: serde_json::to_string_pretty(&build_auth(values, prior))
                    .unwrap_or_default(),
            },
        ]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        let wire_api = str_val(values, "wire_api");
        if wire_api == "chat" {
            issues.push(
                ConfigIssue::error(
                    "wire_api = \"chat\" 已被 Codex 移除（约 2026-02）。请改用 responses；纯 chat-completions 上游需经网关翻译。",
                )
                .for_field("wire_api"),
            );
        }

        let auth_mode = str_val(values, "auth_mode");
        let uses_relay = auth_mode_uses_relay(auth_mode);
        let uses_login = auth_mode_uses_login(auth_mode);

        if uses_relay && str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        } else if !str_val(values, "base_url").trim().is_empty() {
            let base = str_val(values, "base_url").trim_end_matches('/');
            if !base.ends_with("/v1") && !base.contains("127.0.0.1") && !base.contains("localhost")
            {
                issues.push(
                    ConfigIssue::info(
                        "Base URL 通常应以 /v1 结尾（Codex 会在其后拼接 /responses）。",
                    )
                    .for_field("base_url"),
                );
            }
        }

        if uses_relay
            && str_val(values, "api_key").trim().is_empty()
            && str_val(values, LEGACY_ENV_KEY).trim().is_empty()
        {
            issues.push(
                ConfigIssue::warning("第三方 API 模式尚未填写 API Key。").for_field("api_key"),
            );
        }
        if !uses_relay && !str_val(values, "api_key").trim().is_empty() {
            issues.push(
                ConfigIssue::info("仅账号登录模式不会使用该第三方 API Key，保存时将移除。")
                    .for_field("api_key"),
            );
        }
        if uses_login && !bool_val(values, HAS_OPENAI_LOGIN) {
            issues.push(ConfigIssue::warning(
                "此供应商尚未保存 ChatGPT 登录态；应用时会沿用当前 auth.json，请先在 Codex 完成登录。",
            ));
        }

        issues
    }

    fn validate_for_category(
        &self,
        values: &FormValues,
        category: Option<&str>,
    ) -> Vec<ConfigIssue> {
        let mut issues = self.validate(values);
        if category == Some("official") {
            issues.retain(|issue| {
                !matches!(
                    issue.field.as_deref(),
                    Some("base_url" | "api_key" | "auth_mode")
                )
            });
        }
        issues
    }

    fn presets(&self) -> Vec<super::Preset> {
        vec![
            codex_preset(
                "OpenAI 官方",
                "openai-account",
                "OpenAI",
                "",
                AUTH_OPENAI_LOGIN,
                "gpt-5.5",
                "high",
            ),
            codex_preset(
                "DeepSeek",
                "deepseek",
                "DeepSeek",
                "https://api.deepseek.com/v1",
                AUTH_API_KEY,
                "deepseek-chat",
                "high",
            ),
            codex_preset(
                "OpenRouter",
                "openrouter",
                "OpenRouter",
                "https://openrouter.ai/api/v1",
                AUTH_API_KEY,
                "openai/gpt-5.5",
                "high",
            ),
            codex_preset(
                "Azure OpenAI",
                "azure",
                "Azure OpenAI",
                "https://YOUR.openai.azure.com/openai",
                AUTH_API_KEY,
                "gpt-5.5",
                "high",
            ),
        ]
    }
}

/// Build a Codex preset's pre-filled form values.
#[allow(clippy::too_many_arguments)]
fn codex_preset(
    label: &str,
    id: &str,
    name: &str,
    base_url: &str,
    auth_mode: &str,
    model: &str,
    effort: &str,
) -> super::Preset {
    let mut v = FormValues::new();
    set_str(&mut v, "provider_id", id);
    set_str(&mut v, "name", name);
    set_str(&mut v, "base_url", base_url);
    set_str(&mut v, "auth_mode", auth_mode);
    set_bool(&mut v, "remote_compaction", name == OPENAI_PROVIDER_NAME);
    set_str(&mut v, "model", model);
    set_str(&mut v, "reasoning_effort", effort);
    set_str(&mut v, "wire_api", "responses");
    super::Preset {
        name: label.to_string(),
        values: v,
    }
}

/// Smart `/v1` handling: trim trailing slashes; append `/v1` only when the URL
/// is an origin with no path, mirroring the reference behaviour.
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    let origin_only = match trimmed.split_once("://") {
        Some((_scheme, rest)) => !rest.contains('/'),
        None => !trimmed.contains('/'),
    };
    if trimmed.ends_with("/v1") || !origin_only {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn auth_mode_uses_relay(auth_mode: &str) -> bool {
    matches!(auth_mode, AUTH_API_KEY | AUTH_OPENAI_LOGIN_WITH_API_KEY)
}

fn auth_mode_uses_login(auth_mode: &str) -> bool {
    matches!(
        auth_mode,
        AUTH_OPENAI_LOGIN | AUTH_OPENAI_LOGIN_WITH_API_KEY
    )
}

fn is_valid_env_key_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `auth.json` remains the account-login document. Legacy providers may have
/// kept a relay key in `auth.OPENAI_API_KEY`; saving migrates that key into the
/// active provider's `experimental_bearer_token` without discarding OAuth data.
fn build_auth(_values: &FormValues, prior: &Value) -> Value {
    let mut auth = prior
        .get("auth")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    auth.remove("OPENAI_API_KEY");
    Value::Object(auth)
}

/// Build the `config.toml` text from structured values, merging into `prior`
/// (so unknown keys survive).
fn build_config_text(values: &FormValues, prior: &str) -> String {
    let mut doc = prior.parse::<DocumentMut>().unwrap_or_default();

    let provider_id = {
        let id = str_val(values, "provider_id").trim();
        if id.is_empty() {
            "custom".to_string()
        } else {
            id.to_string()
        }
    };

    let root = doc.as_table_mut();
    root.insert("model_provider", toml_edit::value(provider_id.as_str()));
    // Provider-scoped bearer tokens are canonical for form-managed providers.
    // Remove a legacy top-level projection so it cannot shadow the active table.
    root.remove("experimental_bearer_token");

    let model = str_val(values, "model").trim();
    if model.is_empty() {
        root.remove("model");
    } else {
        root.insert("model", toml_edit::value(model));
    }

    let effort = str_val(values, "reasoning_effort").trim();
    if effort.is_empty() {
        root.remove("model_reasoning_effort");
    } else {
        root.insert("model_reasoning_effort", toml_edit::value(effort));
    }

    if bool_val(values, "disable_response_storage") {
        root.insert("disable_response_storage", toml_edit::value(true));
    } else {
        root.remove("disable_response_storage");
    }

    // [model_providers.<id>]
    let mps = root.entry("model_providers").or_insert(Item::Table({
        let mut t = Table::new();
        t.set_implicit(true);
        t
    }));
    if let Some(mps) = mps.as_table_mut() {
        mps.set_implicit(true);
        let ptbl = mps.entry(&provider_id).or_insert(Item::Table(Table::new()));
        if let Some(ptbl) = ptbl.as_table_mut() {
            let name = str_val(values, "name").trim();
            let remote_compaction = bool_val(values, "remote_compaction");
            let normal_name = if name.is_empty() || name == OPENAI_PROVIDER_NAME {
                provider_id.as_str()
            } else {
                name
            };
            ptbl.insert(
                "name",
                toml_edit::value(if remote_compaction {
                    OPENAI_PROVIDER_NAME
                } else {
                    normal_name
                }),
            );
            let base_url = normalize_base_url(str_val(values, "base_url"));
            if base_url.is_empty() {
                ptbl.remove("base_url");
            } else {
                ptbl.insert("base_url", toml_edit::value(base_url));
            }
            ptbl.insert("wire_api", toml_edit::value("responses"));
            if bool_val(values, "supports_websockets") {
                ptbl.insert("supports_websockets", toml_edit::value(true));
            } else {
                ptbl.remove("supports_websockets");
            }

            match str_val(values, "auth_mode") {
                AUTH_OPENAI_LOGIN => {
                    ptbl.remove("env_key");
                    ptbl.remove("experimental_bearer_token");
                    ptbl.insert("requires_openai_auth", toml_edit::value(true));
                }
                AUTH_OPENAI_LOGIN_WITH_API_KEY => {
                    ptbl.insert("requires_openai_auth", toml_edit::value(true));
                    write_relay_credentials(ptbl, values);
                }
                _ => {
                    ptbl.remove("requires_openai_auth");
                    write_relay_credentials(ptbl, values);
                }
            }

            set_or_remove_inline(ptbl, "query_params", values.get("query_params"));
            set_or_remove_inline(ptbl, "http_headers", values.get("http_headers"));
        }
    }

    doc.to_string()
}

fn write_relay_credentials(table: &mut Table, values: &FormValues) {
    let api_key = str_val(values, "api_key").trim();
    if !api_key.is_empty() {
        table.remove("env_key");
        table.insert("experimental_bearer_token", toml_edit::value(api_key));
        return;
    }

    table.remove("experimental_bearer_token");
    let legacy_env_key = str_val(values, LEGACY_ENV_KEY).trim();
    if legacy_env_key.is_empty() {
        table.remove("env_key");
    } else {
        table.insert("env_key", toml_edit::value(legacy_env_key));
    }
}

/// Write a JSON string-map field as a TOML inline table, or remove it when empty.
fn set_or_remove_inline(table: &mut Table, key: &str, value: Option<&Value>) {
    let entries: Vec<(String, String)> = value
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .filter(|(k, _)| !k.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        table.remove(key);
        return;
    }
    let mut inline = InlineTable::new();
    for (k, v) in entries {
        inline.insert(&k, TomlValue::from(v));
    }
    table.insert(key, Item::Value(TomlValue::InlineTable(inline)));
}

/// Read a TOML inline-table field into a JSON string-map for the form.
fn inline_table_to_json(table: Option<&Table>, key: &str) -> Value {
    let mut map = Map::new();
    if let Some(item) = table.and_then(|t| t.get(key)) {
        if let Some(inline) = item.as_inline_table() {
            for (k, v) in inline.iter() {
                if let Some(s) = v.as_str() {
                    map.insert(k.to_string(), Value::String(s.to_string()));
                }
            }
        } else if let Some(tbl) = item.as_table() {
            for (k, v) in tbl.iter() {
                if let Some(s) = v.as_str() {
                    map.insert(k.to_string(), Value::String(s.to_string()));
                }
            }
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deepseek_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "provider_id", "deepseek");
        set_str(&mut v, "name", "DeepSeek");
        set_str(&mut v, "base_url", "https://api.deepseek.com/v1");
        set_str(&mut v, "auth_mode", AUTH_API_KEY);
        set_str(&mut v, "api_key", "sk-deepseek");
        set_str(&mut v, "model", "deepseek-chat");
        set_str(&mut v, "reasoning_effort", "high");
        v
    }

    #[test]
    fn schema_exposes_responses_websocket_capability_toggle() {
        let field = CodexConfig
            .schema()
            .into_iter()
            .flat_map(|section| section.fields)
            .find(|field| field.id == "supports_websockets")
            .expect("Codex schema should expose WebSocket capability");
        assert!(matches!(field.kind, FieldKind::Toggle));
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = CodexConfig.decode(&Value::Null, None);
        assert_eq!(str_val(&values, "provider_id"), "custom");
        assert_eq!(str_val(&values, "wire_api"), "responses");
        assert_eq!(str_val(&values, "auth_mode"), AUTH_API_KEY);
        assert!(!bool_val(&values, "remote_compaction"));
    }

    #[test]
    fn api_key_mode_writes_provider_bearer_not_auth_json_or_env_key() {
        let result = CodexConfig.encode(&deepseek_values(), &Value::Null, None);
        let cfg = result.settings_config["config"].as_str().unwrap();
        assert!(
            cfg.contains("experimental_bearer_token = \"sk-deepseek\""),
            "{cfg}"
        );
        assert!(!cfg.contains("env_key"), "{cfg}");
        assert!(!cfg.contains("requires_openai_auth"), "{cfg}");
        assert!(cfg.contains("wire_api = \"responses\""));
        assert!(cfg.contains("[model_providers.deepseek]"), "{cfg}");
        assert_eq!(result.settings_config["auth"], json!({}));
    }

    #[test]
    fn openai_login_mode_sets_requires_auth_and_drops_relay_credentials() {
        let mut v = deepseek_values();
        set_str(&mut v, "auth_mode", AUTH_OPENAI_LOGIN);
        set_str(&mut v, "base_url", "");
        let prior = json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "oauth-access" }
            },
            "config": ""
        });
        let result = CodexConfig.encode(&v, &prior, None);
        let cfg = result.settings_config["config"].as_str().unwrap();
        assert!(cfg.contains("requires_openai_auth = true"), "{cfg}");
        assert!(!cfg.contains("env_key"), "{cfg}");
        assert!(!cfg.contains("experimental_bearer_token"), "{cfg}");
        assert!(!cfg.contains("base_url"), "{cfg}");
        assert_eq!(
            result.settings_config["auth"]["tokens"]["access_token"],
            "oauth-access"
        );
    }

    #[test]
    fn combined_mode_preserves_login_and_uses_provider_bearer() {
        let mut v = deepseek_values();
        set_str(&mut v, "auth_mode", AUTH_OPENAI_LOGIN_WITH_API_KEY);
        let prior = json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "oauth-access",
                    "refresh_token": "oauth-refresh"
                }
            },
            "config": ""
        });

        let result = CodexConfig.encode(&v, &prior, None);
        let cfg = result.settings_config["config"].as_str().unwrap();

        assert!(cfg.contains("requires_openai_auth = true"), "{cfg}");
        assert!(
            cfg.contains("experimental_bearer_token = \"sk-deepseek\""),
            "{cfg}"
        );
        assert_eq!(
            result.settings_config["auth"]["tokens"]["refresh_token"],
            "oauth-refresh"
        );
        assert!(
            result.settings_config["auth"]
                .get("OPENAI_API_KEY")
                .is_none()
        );
    }

    #[test]
    fn round_trip_preserves_fields() {
        let original = deepseek_values();
        let encoded = CodexConfig.encode(&original, &Value::Null, None);
        let decoded = CodexConfig.decode(&encoded.settings_config, None);
        for key in [
            "provider_id",
            "name",
            "base_url",
            "model",
            "reasoning_effort",
            "api_key",
        ] {
            assert_eq!(
                str_val(&decoded, key),
                str_val(&original, key),
                "field {key}"
            );
        }
        assert_eq!(str_val(&decoded, "auth_mode"), AUTH_API_KEY);
    }

    #[test]
    fn current_login_plus_relay_shape_decodes_and_round_trips() {
        let settings = json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "oauth-access",
                    "refresh_token": "oauth-refresh"
                }
            },
            "config": r#"model = "gpt-5.6-sol"
model_provider = "zhizheng"

[model_providers.zhizheng]
base_url = "https://relay.example.com/openai/v1"
experimental_bearer_token = "sk-relay"
name = "OpenAI"
requires_openai_auth = true
wire_api = "responses"
supports_websockets = true
"#
        });

        let values = CodexConfig.decode(&settings, None);
        assert_eq!(
            str_val(&values, "auth_mode"),
            AUTH_OPENAI_LOGIN_WITH_API_KEY
        );
        assert_eq!(str_val(&values, "api_key"), "sk-relay");
        assert!(bool_val(&values, "remote_compaction"));
        assert!(bool_val(&values, HAS_OPENAI_LOGIN));

        let result = CodexConfig.encode(&values, &settings, None);
        let cfg = result.settings_config["config"].as_str().unwrap();
        assert!(cfg.contains("name = \"OpenAI\""), "{cfg}");
        assert!(cfg.contains("requires_openai_auth = true"), "{cfg}");
        assert!(
            cfg.contains("experimental_bearer_token = \"sk-relay\""),
            "{cfg}"
        );
        assert!(cfg.contains("supports_websockets = true"), "{cfg}");
        assert_eq!(
            result.settings_config["auth"]["tokens"]["refresh_token"],
            "oauth-refresh"
        );
    }

    #[test]
    fn remote_compaction_toggle_exclusively_controls_openai_name() {
        let mut values = deepseek_values();
        set_str(&mut values, "name", OPENAI_PROVIDER_NAME);
        set_bool(&mut values, "remote_compaction", false);

        let disabled = CodexConfig.encode(&values, &Value::Null, None);
        let disabled_cfg = disabled.settings_config["config"].as_str().unwrap();
        assert!(
            disabled_cfg.contains("name = \"deepseek\""),
            "{disabled_cfg}"
        );
        assert!(
            !disabled_cfg.contains("name = \"OpenAI\""),
            "{disabled_cfg}"
        );

        set_bool(&mut values, "remote_compaction", true);
        let enabled = CodexConfig.encode(&values, &Value::Null, None);
        let enabled_cfg = enabled.settings_config["config"].as_str().unwrap();
        assert!(enabled_cfg.contains("name = \"OpenAI\""), "{enabled_cfg}");
    }

    #[test]
    fn legacy_inline_env_key_secret_migrates_to_provider_bearer() {
        let settings = json!({
            "auth": {
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "oauth-access" }
            },
            "config": r#"model_provider = "legacy"

[model_providers.legacy]
name = "OpenAI"
base_url = "https://relay.example.com/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "sk-legacy-inline-secret"
"#
        });

        let values = CodexConfig.decode(&settings, None);
        assert_eq!(
            str_val(&values, "auth_mode"),
            AUTH_OPENAI_LOGIN_WITH_API_KEY
        );
        assert_eq!(str_val(&values, "api_key"), "sk-legacy-inline-secret");

        let result = CodexConfig.encode(&values, &settings, None);
        let cfg = result.settings_config["config"].as_str().unwrap();
        assert!(!cfg.contains("env_key"), "{cfg}");
        assert!(
            cfg.contains("experimental_bearer_token = \"sk-legacy-inline-secret\""),
            "{cfg}"
        );
    }

    #[test]
    fn legacy_real_env_key_is_preserved_until_a_secret_is_supplied() {
        let settings = json!({
            "auth": {},
            "config": r#"model_provider = "legacy"

[model_providers.legacy]
name = "Legacy"
base_url = "https://relay.example.com/v1"
wire_api = "responses"
env_key = "LEGACY_API_KEY"
"#
        });

        let values = CodexConfig.decode(&settings, None);
        assert_eq!(str_val(&values, "api_key"), "");
        assert_eq!(str_val(&values, LEGACY_ENV_KEY), "LEGACY_API_KEY");

        let result = CodexConfig.encode(&values, &settings, None);
        let cfg = result.settings_config["config"].as_str().unwrap();
        assert!(cfg.contains("env_key = \"LEGACY_API_KEY\""), "{cfg}");
        assert!(!cfg.contains("experimental_bearer_token"), "{cfg}");
    }

    #[test]
    fn encode_preserves_unknown_settings_keys() {
        let prior = json!({ "modelCatalog": { "models": [] }, "config": "" });
        let result = CodexConfig.encode(&deepseek_values(), &prior, None);
        assert!(result.settings_config.get("modelCatalog").is_some());
    }

    #[test]
    fn query_params_round_trip_as_inline_table() {
        let mut v = deepseek_values();
        v.insert(
            "query_params".into(),
            json!({ "api-version": "2025-04-01-preview" }),
        );
        let encoded = CodexConfig.encode(&v, &Value::Null, None);
        let cfg = encoded.settings_config["config"].as_str().unwrap();
        assert!(cfg.contains("api-version"), "{cfg}");
        let decoded = CodexConfig.decode(&encoded.settings_config, None);
        assert_eq!(
            decoded["query_params"]["api-version"].as_str(),
            Some("2025-04-01-preview")
        );
    }

    #[test]
    fn validate_rejects_chat_wire_api() {
        let mut v = deepseek_values();
        set_str(&mut v, "wire_api", "chat");
        let issues = CodexConfig.validate(&v);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == super::super::Severity::Error
                    && i.field.as_deref() == Some("wire_api"))
        );
    }

    #[test]
    fn official_provider_uses_openai_login_without_api_key_warning() {
        let mut values = deepseek_values();
        set_str(&mut values, "auth_mode", AUTH_OPENAI_LOGIN);
        set_str(&mut values, "api_key", "");

        let issues = CodexConfig.validate_for_category(&values, Some("official"));

        assert!(!issues.iter().any(|issue| {
            matches!(
                issue.field.as_deref(),
                Some("base_url" | "api_key" | "auth_mode")
            )
        }));
    }

    #[test]
    fn preview_emits_config_and_auth_files() {
        let files = CodexConfig.preview(&deepseek_values(), &Value::Null);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.filename.ends_with("config.toml")));
        assert!(files.iter().any(|f| f.filename.ends_with("auth.json")));
    }

    #[test]
    fn normalize_base_url_appends_v1_only_for_origin() {
        assert_eq!(
            normalize_base_url("https://api.deepseek.com"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://host.com/openai"),
            "https://host.com/openai"
        );
    }

    #[test]
    fn parse_files_round_trips_preview() {
        let original = deepseek_values();
        // preview -> edit (verbatim) -> parse_files -> decode should preserve fields.
        let files: Vec<String> = CodexConfig
            .preview(&original, &Value::Null)
            .into_iter()
            .map(|f| f.content)
            .collect();
        let settings = CodexConfig.parse_files(&files).unwrap();
        let decoded = CodexConfig.decode(&settings, None);
        for key in ["provider_id", "name", "base_url", "model", "api_key"] {
            assert_eq!(
                str_val(&decoded, key),
                str_val(&original, key),
                "field {key}"
            );
        }
    }
}
