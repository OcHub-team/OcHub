//! Claude Desktop direct-provider config codec.
//!
//! Cross-dialect routing belongs to the local gateway. A Claude Desktop
//! provider therefore only describes a native Anthropic-compatible inference
//! endpoint and optional Claude-safe model entries.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::{
    set_str, str_val, AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection,
    FormValues, GridColumn, Language, PreviewFile, SelectOption,
};
use crate::model::{ClaudeDesktopModelRoute, ProviderMeta};
use crate::AppType;

const AUTH_TOKEN: &str = "token";
const AUTH_API_KEY: &str = "api_key";
const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const ENV_API_KEY: &str = "ANTHROPIC_API_KEY";
const ENV_INERT_MODEL: &str = "ANTHROPIC_MODEL";
const PROFILE_ID: &str = "00000000-0000-4000-8000-000000157210";

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
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::ClaudeDesktop.app_id()
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
                            placeholder: "https://gateway.example.com".into(),
                        },
                    )
                    .help("原生 Anthropic Messages 端点；跨格式服务请在 OcHub 中转页面配置。")
                    .required(),
                    FormField::new(
                        "auth_field",
                        "鉴权字段",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(AUTH_TOKEN, "ANTHROPIC_AUTH_TOKEN（Bearer）"),
                                SelectOption::new(AUTH_API_KEY, "ANTHROPIC_API_KEY"),
                            ],
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
                vec![FormField::new(
                    "routes",
                    "Claude 模型（Sonnet / Opus / Fable / Haiku）",
                    FieldKind::ModelGrid {
                        columns: vec![
                            GridColumn::text("role", "角色", ""),
                            GridColumn::text("model", "模型", "claude-sonnet-4-6"),
                            GridColumn::text("label", "显示名", "Sonnet"),
                            GridColumn::toggle("one_m", "1M 上下文"),
                        ],
                    },
                )
                .help(
                    "这里只接受对应角色的 Claude-safe 模型名；任意模型映射请在 OcHub 中转页面配置。",
                )],
            ),
        ]
    }

    fn decode(&self, settings_config: &Value, meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let env = settings_config.get("env").and_then(Value::as_object);
        let read_env = |key: &str| {
            env.and_then(|env| env.get(key))
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
        } else {
            set_str(&mut values, "auth_field", AUTH_API_KEY);
            set_str(&mut values, "api_key", api_key);
        }

        let routes = meta.map(|meta| &meta.claude_desktop_model_routes);
        values.insert(
            "routes".into(),
            Value::Array(
                ROLE_SLOTS
                    .iter()
                    .map(|slot| {
                        let route = routes.and_then(|routes| routes.get(slot.route_id));
                        json!({
                            "role": slot.role,
                            "model": route.map(|route| route.model.clone()).unwrap_or_default(),
                            "label": route.and_then(|route| route.label_override.clone()).unwrap_or_default(),
                            "one_m": route.and_then(|route| route.supports_1m).unwrap_or(false),
                        })
                    })
                    .collect(),
            ),
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
        let mut env = settings
            .get("env")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        set_or_remove(&mut env, ENV_BASE_URL, str_val(values, "base_url"));
        env.remove(ENV_AUTH_TOKEN);
        env.remove(ENV_API_KEY);
        env.remove(ENV_INERT_MODEL);
        let api_key = str_val(values, "api_key");
        if !api_key.trim().is_empty() {
            let key = if str_val(values, "auth_field") == AUTH_API_KEY {
                ENV_API_KEY
            } else {
                ENV_AUTH_TOKEN
            };
            env.insert(key.into(), Value::String(api_key.trim().to_string()));
        }
        settings.insert("env".into(), Value::Object(env));

        let mut meta = prior_meta.cloned().unwrap_or_default();
        meta.api_format = Some("anthropic".to_string());
        meta.claude_desktop_model_routes = routes_from_values(values);

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: Some(meta),
        }
    }

    fn preview(&self, values: &FormValues, _prior: &Value) -> Vec<PreviewFile> {
        let mut profile = json!({
            "coworkEgressAllowedHosts": ["*"],
            "disableDeploymentModeChooser": true,
            "inferenceGatewayApiKey": str_val(values, "api_key").trim(),
            "inferenceGatewayAuthScheme": "bearer",
            "inferenceGatewayBaseUrl": str_val(values, "base_url").trim(),
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
        if str_val(values, "base_url").trim().is_empty() {
            issues.push(ConfigIssue::error("Base URL 不能为空。").for_field("base_url"));
        }
        if str_val(values, "api_key").trim().is_empty() {
            issues.push(ConfigIssue::warning("尚未填写 API Key。").for_field("api_key"));
        }
        if str_val(values, "auth_field") == AUTH_API_KEY {
            issues.push(
                ConfigIssue::warning(
                    "Claude Desktop profile 使用 Bearer；建议选择 ANTHROPIC_AUTH_TOKEN。",
                )
                .for_field("auth_field"),
            );
        }
        for (index, row) in grid_rows(values).iter().enumerate() {
            let route_id = ROLE_SLOTS
                .get(index)
                .map(|slot| slot.route_id)
                .unwrap_or("");
            let model = row
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !model.is_empty() && model != route_id {
                issues.push(ConfigIssue::error(format!(
                    "{route_id} 不能映射到 {model}；请在 OcHub 中转页面配置模型映射。"
                )));
            }
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
                    Some("base_url" | "api_key" | "auth_field")
                )
            });
        }
        issues
    }
}

fn set_or_remove(map: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.into(), Value::String(value.to_string()));
    }
}

fn grid_rows(values: &FormValues) -> Vec<Map<String, Value>> {
    values
        .get("routes")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn routes_from_values(values: &FormValues) -> HashMap<String, ClaudeDesktopModelRoute> {
    let mut routes = HashMap::new();
    for (index, row) in grid_rows(values).iter().enumerate() {
        let Some(slot) = ROLE_SLOTS.get(index) else {
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
        let supports_1m = row.get("one_m").and_then(Value::as_bool).unwrap_or(false);
        if model.is_empty() && label.is_empty() && !supports_1m {
            continue;
        }
        routes.insert(
            slot.route_id.to_string(),
            ClaudeDesktopModelRoute {
                model: model.to_string(),
                label_override: (!label.is_empty()).then(|| label.to_string()),
                supports_1m: supports_1m.then_some(true),
            },
        );
    }
    routes
}

fn inference_models_json(values: &FormValues) -> Vec<Value> {
    grid_rows(values)
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let slot = ROLE_SLOTS.get(index)?;
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
            let supports_1m = row.get("one_m").and_then(Value::as_bool).unwrap_or(false);
            if model.is_empty() && label.is_empty() && !supports_1m {
                return None;
            }
            if supports_1m || !label.is_empty() {
                let mut item = json!({ "name": slot.route_id });
                if !label.is_empty() {
                    item["labelOverride"] = json!(label);
                }
                if supports_1m {
                    item["supports1m"] = json!(true);
                }
                Some(item)
            } else {
                Some(Value::String(slot.route_id.to_string()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_config_round_trips_credentials() {
        let mut values = FormValues::new();
        set_str(&mut values, "base_url", "https://api.example.com");
        set_str(&mut values, "auth_field", AUTH_TOKEN);
        set_str(&mut values, "api_key", "secret");
        values.insert("routes".into(), Value::Array(Vec::new()));
        let encoded = ClaudeDesktopConfig.encode(&values, &Value::Null, None);
        assert_eq!(
            encoded.settings_config["env"][ENV_BASE_URL],
            "https://api.example.com"
        );
        assert_eq!(encoded.settings_config["env"][ENV_AUTH_TOKEN], "secret");
        assert_eq!(
            encoded.meta.unwrap().api_format.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn official_provider_uses_desktop_login_without_gateway_credentials() {
        let mut values = FormValues::new();
        set_str(&mut values, "base_url", "");
        set_str(&mut values, "api_key", "");
        values.insert("routes".into(), Value::Array(Vec::new()));

        let issues = ClaudeDesktopConfig.validate_for_category(&values, Some("official"));

        assert!(!issues.iter().any(|issue| {
            matches!(
                issue.field.as_deref(),
                Some("base_url" | "api_key" | "auth_field")
            )
        }));
    }
}
