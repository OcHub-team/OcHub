//! Claude Desktop provider config codec.
//!
//! Unlike most apps, Claude Desktop's provider config lives *almost entirely in
//! [`ProviderMeta`]*, not in `settingsConfig`. The live writer
//! (`crate::apps::claude_desktop`) reads:
//!
//! - `meta.claude_desktop_mode` (`Direct` | `Proxy`),
//! - `meta.api_format` (`anthropic` | `openai_chat` | `openai_responses` |
//!   `gemini_native`, only meaningful in proxy mode), and
//! - `meta.claude_desktop_model_routes` (`{ routeId -> { model, labelOverride?,
//!   supports1m? } }`) for the four role slots Sonnet / Opus / Fable / Haiku.
//!
//! `settingsConfig.env` only supplies the *direct-mode* upstream credentials
//! (`ANTHROPIC_BASE_URL` + a bearer token in `ANTHROPIC_AUTH_TOKEN` or
//! `ANTHROPIC_API_KEY`). The generated on-disk artifact is a "3P profile" JSON
//! at `…/configLibrary/<profile>.json` whose keys mirror
//! `build_gateway_profile` (`inferenceGatewayBaseUrl`, `inferenceGatewayApiKey`,
//! `inferenceGatewayAuthScheme`, `inferenceProvider`, `inferenceModels[]`).
//!
//! The legacy generic editor never touched `meta`, so this app was effectively
//! unconfigurable: mode, api_format, and the route map all silently stayed at
//! their defaults. This codec writes all three into `meta`, and deliberately
//! does **not** write the inert `env.ANTHROPIC_MODEL` (Claude Desktop never
//! reads it).

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::{
    set_str, str_val, AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection,
    FormValues, GridColumn, Language, PreviewFile, SelectOption,
};
use crate::model::{ClaudeDesktopMode, ClaudeDesktopModelRoute, ProviderMeta};
use crate::AppType;

const MODE_DIRECT: &str = "direct";
const MODE_PROXY: &str = "proxy";

const AUTH_TOKEN: &str = "token";
const AUTH_API_KEY: &str = "api_key";

const FORMAT_ANTHROPIC: &str = "anthropic";
const FORMAT_OPENAI_CHAT: &str = "openai_chat";
const FORMAT_OPENAI_RESPONSES: &str = "openai_responses";
const FORMAT_GEMINI_NATIVE: &str = "gemini_native";

const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const ENV_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Inert: Claude Desktop never reads `ANTHROPIC_MODEL`; we strip it so a
/// round-trip cannot resurrect it.
const ENV_INERT_MODEL: &str = "ANTHROPIC_MODEL";

const PROFILE_ID: &str = "00000000-0000-4000-8000-000000157210";

/// The four role slots, in UI order (Sonnet / Opus / Fable / Haiku). The
/// `route_id` is what gets written as the key in `claude_desktop_model_routes`
/// and as `inferenceModels[].name` in the generated profile.
struct RoleSlot {
    role: &'static str,
    route_id: &'static str,
}

const ROLE_SLOTS: &[RoleSlot] = &[
    RoleSlot {
        role: "Sonnet",
        route_id: "claude-sonnet-4-6",
    },
    RoleSlot {
        role: "Opus",
        route_id: "claude-opus-4-8",
    },
    RoleSlot {
        role: "Fable",
        route_id: "claude-fable-5",
    },
    RoleSlot {
        role: "Haiku",
        route_id: "claude-haiku-4-5",
    },
];

pub struct ClaudeDesktopConfig;

impl AppConfig for ClaudeDesktopConfig {
    fn app(&self) -> AppType {
        AppType::ClaudeDesktop
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "模式",
                vec![FormField::new(
                    "mode",
                    "写入模式",
                    FieldKind::Select {
                        options: vec![
                            SelectOption::new(MODE_DIRECT, "直连 (Direct)")
                                .with_hint("env 中的 Base URL/Token 直接写入 3P profile；仅支持原生 Anthropic 上游"),
                            SelectOption::new(MODE_PROXY, "本地路由 (Proxy)")
                                .with_hint("经本地代理转换；可路由到任意上游模型"),
                        ],
                    },
                )
                .help("Claude Desktop 的核心模式，存储于 meta.claude_desktop_mode（旧编辑器从不写入它）。")],
            ),
            FormSection::new(
                "端点与鉴权",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://gateway.example.com".into(),
                        },
                    )
                    .help("写入 settingsConfig.env.ANTHROPIC_BASE_URL；直连模式直接成为 inferenceGatewayBaseUrl。")
                    .required(),
                    FormField::new(
                        "auth_field",
                        "鉴权字段",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(AUTH_TOKEN, "ANTHROPIC_AUTH_TOKEN（Bearer）")
                                    .with_hint("直连模式必须用此字段；它即是 inferenceGatewayApiKey"),
                                SelectOption::new(AUTH_API_KEY, "ANTHROPIC_API_KEY"),
                            ],
                        },
                    )
                    .help("决定密钥写入 env 的哪个键。直连模式只认 ANTHROPIC_AUTH_TOKEN。"),
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
                "格式",
                vec![FormField::new(
                    "api_format",
                    "API 格式 (api_format)",
                    FieldKind::Select {
                        options: vec![
                            SelectOption::new(FORMAT_ANTHROPIC, "anthropic（原生 Messages）"),
                            SelectOption::new(FORMAT_OPENAI_CHAT, "openai_chat"),
                            SelectOption::new(FORMAT_OPENAI_RESPONSES, "openai_responses"),
                            SelectOption::new(FORMAT_GEMINI_NATIVE, "gemini_native"),
                        ],
                    },
                )
                .visible_when("mode", MODE_PROXY)
                .help("仅本地路由模式有意义，存储于 meta.api_format；直连模式恒为 anthropic。")],
            ),
            FormSection::new(
                "模型路由",
                vec![FormField::new(
                    "routes",
                    "角色路由（Sonnet / Opus / Fable / Haiku）",
                    FieldKind::ModelGrid {
                        columns: vec![
                            GridColumn::text("role", "角色", ""),
                            GridColumn::text("model", "上游模型", "kimi-k2"),
                            GridColumn::text("label", "显示名 (labelOverride)", "Kimi K2"),
                            GridColumn::toggle("one_m", "1M 上下文"),
                        ],
                    },
                )
                .help(
                    "每行映射一个 Claude Desktop 角色档到上游模型，存储于 meta.claude_desktop_model_routes。\
                     直连模式留空“上游模型”即写入官方同名 Claude 模型；本地路由模式填上游模型名。",
                )],
            ),
        ]
    }

    fn decode(&self, settings_config: &Value, meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();

        // Mode lives in meta; default to direct for a brand-new provider.
        let mode = match meta.and_then(|m| m.claude_desktop_mode.as_ref()) {
            Some(ClaudeDesktopMode::Proxy) => MODE_PROXY,
            _ => MODE_DIRECT,
        };
        set_str(&mut values, "mode", mode);

        // api_format lives in meta; default anthropic.
        let api_format = meta
            .and_then(|m| m.api_format.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(FORMAT_ANTHROPIC);
        set_str(&mut values, "api_format", api_format);

        // Credentials live in settingsConfig.env.
        let env = settings_config.get("env").and_then(Value::as_object);
        let read_env = |key: &str| -> String {
            env.and_then(|e| e.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        set_str(&mut values, "base_url", read_env(ENV_BASE_URL));

        let token = read_env(ENV_AUTH_TOKEN);
        let api_key = read_env(ENV_API_KEY);
        if !token.is_empty() {
            set_str(&mut values, "auth_field", AUTH_TOKEN);
            set_str(&mut values, "api_key", token);
        } else if !api_key.is_empty() {
            set_str(&mut values, "auth_field", AUTH_API_KEY);
            set_str(&mut values, "api_key", api_key);
        } else {
            set_str(&mut values, "auth_field", AUTH_TOKEN);
            set_str(&mut values, "api_key", "");
        }

        // Routes live in meta; seed all four slots so the grid is always full.
        let routes = meta.map(|m| &m.claude_desktop_model_routes);
        let rows: Vec<Value> = ROLE_SLOTS
            .iter()
            .map(|slot| {
                let route = routes.and_then(|r| r.get(slot.route_id));
                json!({
                    "role": slot.role,
                    "model": route.map(|r| r.model.clone()).unwrap_or_default(),
                    "label": route
                        .and_then(|r| r.label_override.clone())
                        .unwrap_or_default(),
                    "one_m": route.and_then(|r| r.supports_1m).unwrap_or(false),
                })
            })
            .collect();
        values.insert("routes".into(), Value::Array(rows));

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // ---- settingsConfig: merge into prior, only touching env creds ----
        let mut settings = prior.as_object().cloned().unwrap_or_default();
        let mut env = settings
            .get("env")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let base_url = str_val(values, "base_url").trim();
        if base_url.is_empty() {
            env.remove(ENV_BASE_URL);
        } else {
            env.insert(ENV_BASE_URL.into(), Value::String(base_url.to_string()));
        }

        let api_key = str_val(values, "api_key").trim();
        // Both possible key fields are managed by this codec; clear whichever is
        // not selected so toggling the field doesn't leave a stale key behind.
        env.remove(ENV_AUTH_TOKEN);
        env.remove(ENV_API_KEY);
        if !api_key.is_empty() {
            let key = match str_val(values, "auth_field") {
                AUTH_API_KEY => ENV_API_KEY,
                _ => ENV_AUTH_TOKEN,
            };
            env.insert(key.into(), Value::String(api_key.to_string()));
        }

        // BUG FIX (2): never persist the inert ANTHROPIC_MODEL — Claude Desktop
        // ignores it, and the old editor leaked it in.
        env.remove(ENV_INERT_MODEL);

        settings.insert("env".into(), Value::Object(env));

        // ---- meta: clone prior so unrelated meta fields survive ----
        // BUG FIX (1): the old editor never wrote any of these, leaving the app
        // unconfigurable.
        let mut meta = prior_meta.cloned().unwrap_or_default();

        let mode = match str_val(values, "mode") {
            MODE_PROXY => ClaudeDesktopMode::Proxy,
            _ => ClaudeDesktopMode::Direct,
        };
        let is_proxy = matches!(mode, ClaudeDesktopMode::Proxy);
        meta.claude_desktop_mode = Some(mode);

        // api_format only meaningful in proxy mode; direct mode is always native
        // anthropic.
        meta.api_format = Some(if is_proxy {
            let fmt = str_val(values, "api_format").trim();
            if fmt.is_empty() {
                FORMAT_ANTHROPIC.to_string()
            } else {
                fmt.to_string()
            }
        } else {
            FORMAT_ANTHROPIC.to_string()
        });

        meta.claude_desktop_model_routes = routes_from_values(values);

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: Some(meta),
        }
    }

    fn preview(&self, values: &FormValues) -> Vec<PreviewFile> {
        let base_url = str_val(values, "base_url").trim().to_string();
        let api_key = str_val(values, "api_key").trim().to_string();

        // Mirror build_gateway_profile key order/shape.
        let mut profile = json!({
            "coworkEgressAllowedHosts": ["*"],
            "disableDeploymentModeChooser": true,
            "inferenceGatewayApiKey": api_key,
            "inferenceGatewayAuthScheme": "bearer",
            "inferenceGatewayBaseUrl": base_url,
            "inferenceProvider": "gateway"
        });

        let models = inference_models_json(values);
        if !models.is_empty() {
            profile["inferenceModels"] = Value::Array(models);
        }

        vec![PreviewFile {
            filename: format!(
                "~/Library/Application Support/Claude-3p/configLibrary/{PROFILE_ID}.json"
            ),
            language: Language::Json,
            content: serde_json::to_string_pretty(&profile).unwrap_or_default(),
        }]
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        let mode = str_val(values, "mode");
        let is_proxy = mode == MODE_PROXY;

        if str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        }
        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 API Key。").for_field("api_key"));
        }

        // Direct mode only accepts the native bearer token field.
        if !is_proxy && str_val(values, "auth_field") == AUTH_API_KEY {
            issues.push(
                ConfigIssue::warning(
                    "直连模式只读取 ANTHROPIC_AUTH_TOKEN（Bearer Token），ANTHROPIC_API_KEY 将被忽略。",
                )
                .for_field("auth_field"),
            );
        }

        // Direct mode requires Claude-safe route ids and cannot remap to a
        // non-Claude upstream (mirrors validate_direct_provider semantics).
        let rows = grid_rows(values);
        let mut has_route = false;
        for (idx, row) in rows.iter().enumerate() {
            let route_id = ROLE_SLOTS.get(idx).map(|s| s.route_id).unwrap_or("");
            let model = row
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if model.is_empty() {
                continue;
            }
            has_route = true;
            if !is_proxy && model != route_id {
                issues.push(ConfigIssue::error(format!(
                    "直连模式不能把 {route_id} 映射到非官方模型 {model}；请改用本地路由模式。"
                )));
            }
        }

        if is_proxy && !has_route {
            issues.push(ConfigIssue::warning(
                "本地路由模式至少需要为一个角色填写上游模型。",
            ));
        }

        // api_format must be one of the supported proxy formats.
        if is_proxy {
            let fmt = str_val(values, "api_format");
            if !matches!(
                fmt,
                "" | FORMAT_ANTHROPIC
                    | FORMAT_OPENAI_CHAT
                    | FORMAT_OPENAI_RESPONSES
                    | FORMAT_GEMINI_NATIVE
            ) {
                issues.push(
                    ConfigIssue::error(format!("不支持的 API 格式: {fmt}。"))
                        .for_field("api_format"),
                );
            }
        }

        issues
    }
}

// ---- helpers ----------------------------------------------------------------

/// Pull the ModelGrid rows out of the form values.
fn grid_rows(values: &FormValues) -> Vec<Map<String, Value>> {
    values
        .get("routes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the `meta.claude_desktop_model_routes` map from the grid, keyed by the
/// fixed role-slot route ids. Rows whose `model` is blank still produce a route
/// entry only when they carry a label/1m flag; an entirely empty row is dropped.
fn routes_from_values(values: &FormValues) -> HashMap<String, ClaudeDesktopModelRoute> {
    let rows = grid_rows(values);
    let mut map = HashMap::new();
    for (idx, row) in rows.iter().enumerate() {
        let Some(slot) = ROLE_SLOTS.get(idx) else {
            continue;
        };
        let model = row
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let label = row
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let one_m = row.get("one_m").and_then(Value::as_bool).unwrap_or(false);

        // An untouched row contributes nothing.
        if model.is_empty() && label.is_empty() && !one_m {
            continue;
        }

        map.insert(
            slot.route_id.to_string(),
            ClaudeDesktopModelRoute {
                model: model.to_string(),
                label_override: (!label.is_empty()).then(|| label.to_string()),
                supports_1m: one_m.then_some(true),
            },
        );
    }
    map
}

/// Build `inferenceModels[]` for the preview, mirroring `inference_model_json`
/// in the live writer (string when plain, object when it carries label/1m).
fn inference_models_json(values: &FormValues) -> Vec<Value> {
    let mode = str_val(values, "mode");
    let is_proxy = mode == MODE_PROXY;
    let rows = grid_rows(values);
    let mut out = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        let Some(slot) = ROLE_SLOTS.get(idx) else {
            continue;
        };
        let model = row
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let label = row
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let one_m = row.get("one_m").and_then(Value::as_bool).unwrap_or(false);

        // Direct mode with no explicit upstream still publishes the native
        // route id; proxy mode requires an upstream model to expose the slot.
        if model.is_empty() && label.is_empty() && !one_m {
            continue;
        }
        if is_proxy && model.is_empty() {
            continue;
        }

        // The published profile always advertises the *route id* as `name`
        // (Claude-safe), never the raw upstream model.
        if one_m || !label.is_empty() {
            let mut item = json!({ "name": slot.route_id });
            if !label.is_empty() {
                item["labelOverride"] = json!(label);
            }
            if one_m {
                item["supports1m"] = json!(true);
            }
            out.push(item);
        } else {
            out.push(Value::String(slot.route_id.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proxy_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "mode", MODE_PROXY);
        set_str(&mut v, "base_url", "https://gateway.example.com");
        set_str(&mut v, "auth_field", AUTH_TOKEN);
        set_str(&mut v, "api_key", "sk-proxy");
        set_str(&mut v, "api_format", FORMAT_OPENAI_CHAT);
        v.insert(
            "routes".into(),
            json!([
                { "role": "Sonnet", "model": "kimi-k2", "label": "Kimi K2", "one_m": true },
                { "role": "Opus", "model": "", "label": "", "one_m": false },
                { "role": "Fable", "model": "", "label": "", "one_m": false },
                { "role": "Haiku", "model": "", "label": "", "one_m": false },
            ]),
        );
        v
    }

    #[test]
    fn decode_defaults_for_new_provider() {
        let values = ClaudeDesktopConfig.decode(&Value::Null, None);
        assert_eq!(str_val(&values, "mode"), MODE_DIRECT);
        assert_eq!(str_val(&values, "api_format"), FORMAT_ANTHROPIC);
        assert_eq!(str_val(&values, "auth_field"), AUTH_TOKEN);
        assert_eq!(str_val(&values, "api_key"), "");
        // Grid is always seeded with all four role slots.
        let rows = values["routes"].as_array().expect("routes array");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["role"], json!("Sonnet"));
        assert_eq!(rows[1]["role"], json!("Opus"));
        assert_eq!(rows[2]["role"], json!("Fable"));
        assert_eq!(rows[3]["role"], json!("Haiku"));
    }

    #[test]
    fn encode_writes_meta_mode_format_and_routes() {
        // BUG FIX (1): meta must carry mode + api_format + routes.
        let result = ClaudeDesktopConfig.encode(&proxy_values(), &Value::Null, None);
        let meta = result.meta.expect("meta must be written");
        assert_eq!(meta.claude_desktop_mode, Some(ClaudeDesktopMode::Proxy));
        assert_eq!(meta.api_format.as_deref(), Some(FORMAT_OPENAI_CHAT));
        let route = meta
            .claude_desktop_model_routes
            .get("claude-sonnet-4-6")
            .expect("sonnet route written to meta");
        assert_eq!(route.model, "kimi-k2");
        assert_eq!(route.label_override.as_deref(), Some("Kimi K2"));
        assert_eq!(route.supports_1m, Some(true));
        // Empty slots are not persisted.
        assert!(!meta
            .claude_desktop_model_routes
            .contains_key("claude-opus-4-8"));
    }

    #[test]
    fn encode_writes_token_into_env_and_strips_inert_model() {
        let result = ClaudeDesktopConfig.encode(&proxy_values(), &Value::Null, None);
        let env = &result.settings_config["env"];
        assert_eq!(env[ENV_BASE_URL], json!("https://gateway.example.com"));
        assert_eq!(env[ENV_AUTH_TOKEN], json!("sk-proxy"));
        assert!(env.get(ENV_API_KEY).is_none());
        // BUG FIX (2): the inert ANTHROPIC_MODEL must never be written.
        assert!(env.get(ENV_INERT_MODEL).is_none());
    }

    #[test]
    fn encode_drops_inert_model_from_prior_env() {
        // A prior env carrying ANTHROPIC_MODEL must be cleaned out on re-encode.
        let prior = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://old.example.com",
                "ANTHROPIC_MODEL": "claude-sonnet-4-6"
            }
        });
        let result = ClaudeDesktopConfig.encode(&proxy_values(), &prior, None);
        assert!(result.settings_config["env"].get(ENV_INERT_MODEL).is_none());
    }

    #[test]
    fn api_key_field_writes_anthropic_api_key() {
        let mut v = proxy_values();
        set_str(&mut v, "auth_field", AUTH_API_KEY);
        let result = ClaudeDesktopConfig.encode(&v, &Value::Null, None);
        let env = &result.settings_config["env"];
        assert_eq!(env[ENV_API_KEY], json!("sk-proxy"));
        assert!(env.get(ENV_AUTH_TOKEN).is_none());
    }

    #[test]
    fn encode_preserves_unrelated_meta_fields() {
        let prior_meta = ProviderMeta {
            custom_user_agent: Some("ms/1.0".to_string()),
            ..Default::default()
        };
        let result = ClaudeDesktopConfig.encode(&proxy_values(), &Value::Null, Some(&prior_meta));
        let meta = result.meta.expect("meta");
        assert_eq!(meta.custom_user_agent.as_deref(), Some("ms/1.0"));
        assert_eq!(meta.claude_desktop_mode, Some(ClaudeDesktopMode::Proxy));
    }

    #[test]
    fn round_trip_preserves_fields() {
        let original = proxy_values();
        let encoded = ClaudeDesktopConfig.encode(&original, &Value::Null, None);
        let decoded = ClaudeDesktopConfig.decode(&encoded.settings_config, encoded.meta.as_ref());
        assert_eq!(str_val(&decoded, "mode"), MODE_PROXY);
        assert_eq!(str_val(&decoded, "api_format"), FORMAT_OPENAI_CHAT);
        assert_eq!(str_val(&decoded, "base_url"), "https://gateway.example.com");
        assert_eq!(str_val(&decoded, "auth_field"), AUTH_TOKEN);
        assert_eq!(str_val(&decoded, "api_key"), "sk-proxy");
        let rows = decoded["routes"].as_array().expect("routes");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["model"], json!("kimi-k2"));
        assert_eq!(rows[0]["label"], json!("Kimi K2"));
        assert_eq!(rows[0]["one_m"], json!(true));
    }

    #[test]
    fn direct_mode_forces_anthropic_format() {
        let mut v = proxy_values();
        set_str(&mut v, "mode", MODE_DIRECT);
        set_str(&mut v, "api_format", FORMAT_OPENAI_CHAT);
        let result = ClaudeDesktopConfig.encode(&v, &Value::Null, None);
        let meta = result.meta.expect("meta");
        assert_eq!(meta.claude_desktop_mode, Some(ClaudeDesktopMode::Direct));
        assert_eq!(meta.api_format.as_deref(), Some(FORMAT_ANTHROPIC));
    }

    #[test]
    fn validate_direct_rejects_non_claude_upstream() {
        let mut v = proxy_values();
        set_str(&mut v, "mode", MODE_DIRECT);
        // Sonnet slot maps to a non-Claude upstream -> error in direct mode.
        let issues = ClaudeDesktopConfig.validate(&v);
        assert!(issues
            .iter()
            .any(|i| i.severity == super::super::Severity::Error && i.message.contains("kimi-k2")));
    }

    #[test]
    fn preview_emits_single_profile_with_gateway_keys() {
        let files = ClaudeDesktopConfig.preview(&proxy_values());
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert!(file.filename.ends_with(&format!("{PROFILE_ID}.json")));
        assert!(file.filename.contains("configLibrary"));
        assert_eq!(file.language, Language::Json);
        let profile: Value = serde_json::from_str(&file.content).expect("valid json");
        assert_eq!(profile["inferenceProvider"], json!("gateway"));
        assert_eq!(profile["inferenceGatewayAuthScheme"], json!("bearer"));
        assert_eq!(
            profile["inferenceGatewayBaseUrl"],
            json!("https://gateway.example.com")
        );
        assert_eq!(profile["inferenceGatewayApiKey"], json!("sk-proxy"));
        // The published model advertises the route id, never the raw upstream.
        assert_eq!(
            profile["inferenceModels"],
            json!([{ "name": "claude-sonnet-4-6", "labelOverride": "Kimi K2", "supports1m": true }])
        );
        assert!(!file.content.contains("kimi-k2"));
    }
}
