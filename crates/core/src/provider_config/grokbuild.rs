//! Grok Build provider config codec.

use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, value};

use super::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues, Language,
    Preset, PreviewFile, SelectOption, set_str, str_val,
};
use crate::AppType;
use crate::apps::grokbuild::{DEFAULT_API_BACKEND, DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL};
use crate::model::ProviderMeta;

pub(crate) const CREDENTIAL_INLINE: &str = "inline";
const CREDENTIAL_ENV: &str = "env";

pub struct GrokBuildConfig;

impl AppConfig for GrokBuildConfig {
    fn app_id(&self) -> crate::app_id::AppId {
        AppType::GrokBuild.app_id()
    }

    fn schema(&self) -> Vec<FormSection> {
        vec![
            FormSection::new(
                "模型",
                vec![
                    FormField::new(
                        "profile",
                        "配置名称",
                        FieldKind::Text {
                            placeholder: DEFAULT_MODEL.into(),
                        },
                    )
                    .help("[models].default 与 [model.<name>] 使用的客户端配置名。")
                    .required(),
                    FormField::new(
                        "upstream_model",
                        "上游模型",
                        FieldKind::Text {
                            placeholder: DEFAULT_MODEL.into(),
                        },
                    )
                    .required(),
                    FormField::new(
                        "name",
                        "显示名",
                        FieldKind::Text {
                            placeholder: "xAI Grok".into(),
                        },
                    )
                    .required(),
                ],
            ),
            FormSection::new(
                "端点与鉴权",
                vec![
                    FormField::new(
                        "base_url",
                        "Base URL",
                        FieldKind::Text {
                            placeholder: "https://api.x.ai/v1".into(),
                        },
                    )
                    .required(),
                    FormField::new(
                        "credential_mode",
                        "鉴权方式",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new(CREDENTIAL_INLINE, "API Key"),
                                SelectOption::new(CREDENTIAL_ENV, "环境变量"),
                            ],
                        },
                    ),
                    FormField::new(
                        "api_key",
                        "API Key",
                        FieldKind::Secret {
                            placeholder: "xai-...".into(),
                        },
                    )
                    .visible_when("credential_mode", CREDENTIAL_INLINE),
                    FormField::new(
                        "env_key",
                        "环境变量名",
                        FieldKind::Text {
                            placeholder: "XAI_API_KEY".into(),
                        },
                    )
                    .visible_when("credential_mode", CREDENTIAL_ENV),
                ],
            ),
            FormSection::new(
                "协议",
                vec![
                    FormField::new(
                        "api_backend",
                        "API Backend",
                        FieldKind::Select {
                            options: vec![
                                SelectOption::new("responses", "OpenAI Responses"),
                                SelectOption::new("chat_completions", "OpenAI Chat Completions"),
                                SelectOption::new("messages", "Anthropic Messages"),
                            ],
                        },
                    )
                    .required(),
                    FormField::new(
                        "context_window",
                        "上下文窗口",
                        FieldKind::Text {
                            placeholder: DEFAULT_CONTEXT_WINDOW.to_string(),
                        },
                    )
                    .required(),
                ],
            )
            .advanced(),
        ]
    }

    fn decode(&self, settings_config: &Value, _meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();
        let config = settings_config
            .get("config")
            .and_then(Value::as_str)
            .and_then(crate::apps::grokbuild::extract_model_config);

        let profile = config
            .as_ref()
            .map(|config| config.profile.as_str())
            .unwrap_or(DEFAULT_MODEL);
        let upstream_model = config
            .as_ref()
            .map(|config| config.model.as_str())
            .unwrap_or(DEFAULT_MODEL);
        set_str(&mut values, "profile", profile);
        set_str(&mut values, "upstream_model", upstream_model);
        set_str(
            &mut values,
            "name",
            config
                .as_ref()
                .map(|config| config.name.as_str())
                .unwrap_or("xAI Grok"),
        );
        set_str(
            &mut values,
            "base_url",
            config
                .as_ref()
                .map(|config| config.base_url.as_str())
                .unwrap_or("https://api.x.ai/v1"),
        );
        let credential_mode = if config
            .as_ref()
            .and_then(|config| config.api_key.as_ref())
            .is_some()
        {
            CREDENTIAL_INLINE
        } else {
            CREDENTIAL_ENV
        };
        set_str(&mut values, "credential_mode", credential_mode);
        set_str(
            &mut values,
            "api_key",
            config
                .as_ref()
                .and_then(|config| config.api_key.as_deref())
                .unwrap_or(""),
        );
        set_str(
            &mut values,
            "env_key",
            config
                .as_ref()
                .and_then(|config| config.env_key.as_deref())
                .unwrap_or("XAI_API_KEY"),
        );
        set_str(
            &mut values,
            "api_backend",
            config
                .as_ref()
                .map(|config| config.api_backend.as_str())
                .unwrap_or(DEFAULT_API_BACKEND),
        );
        set_str(
            &mut values,
            "context_window",
            config
                .as_ref()
                .map(|config| config.context_window.to_string())
                .unwrap_or_else(|| DEFAULT_CONTEXT_WINDOW.to_string()),
        );
        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        let config = update_document(values, prior);
        EncodeResult {
            settings_config: json!({ "config": config.to_string() }),
            meta: prior_meta.cloned(),
        }
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        vec![PreviewFile {
            filename: "~/.grok/config.toml".into(),
            language: Language::Toml,
            content: update_document(values, prior).to_string(),
        }]
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let config = contents.first().cloned().unwrap_or_default();
        crate::apps::grokbuild::validate_config_toml(&config).map_err(|error| error.to_string())?;
        Ok(json!({ "config": config }))
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        for (field, label) in [
            ("profile", "配置名称"),
            ("upstream_model", "上游模型"),
            ("name", "显示名"),
            ("base_url", "Base URL"),
            ("api_backend", "API Backend"),
        ] {
            if str_val(values, field).trim().is_empty() {
                issues.push(ConfigIssue::error(format!("{label}不能为空。")).for_field(field));
            }
        }

        let credential_field = if str_val(values, "credential_mode") == CREDENTIAL_ENV {
            "env_key"
        } else {
            "api_key"
        };
        if str_val(values, credential_field).trim().is_empty() {
            issues.push(
                ConfigIssue::warning("请填写 API Key 或环境变量名。").for_field(credential_field),
            );
        }
        if str_val(values, "context_window")
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0)
            .is_none()
        {
            issues.push(ConfigIssue::error("上下文窗口必须是正整数。").for_field("context_window"));
        }
        issues
    }

    fn presets(&self) -> Vec<Preset> {
        vec![preset(
            "xAI Grok API",
            "xAI Grok",
            "https://api.x.ai/v1",
            DEFAULT_MODEL,
            "XAI_API_KEY",
        )]
    }
}

fn update_document(values: &FormValues, prior: &Value) -> DocumentMut {
    let prior_config = prior.get("config").and_then(Value::as_str).unwrap_or("");
    let mut document = prior_config
        .parse::<DocumentMut>()
        .unwrap_or_else(|_| DocumentMut::new());
    let profile = non_empty(str_val(values, "profile"), DEFAULT_MODEL);
    let previous_profile = document
        .get("models")
        .and_then(|models| models.get("default"))
        .and_then(Item::as_str)
        .map(str::to_string);

    if document.get("models").and_then(Item::as_table).is_none() {
        document["models"] = toml_edit::table();
    }
    if document.get("model").and_then(Item::as_table).is_none() {
        document["model"] = toml_edit::table();
    }
    if previous_profile
        .as_deref()
        .is_some_and(|previous| previous != profile)
        && let Some(models) = document.get_mut("model").and_then(Item::as_table_mut)
        && let Some(previous) = previous_profile
            .as_deref()
            .and_then(|key| models.remove(key))
    {
        models.insert(profile, previous);
    }

    document["models"]["default"] = value(profile);
    let model_tables = document["model"]
        .as_table_mut()
        .expect("Grok model registry is a TOML table");
    if model_tables.get(profile).and_then(Item::as_table).is_none() {
        model_tables.insert(profile, Item::Table(toml_edit::Table::new()));
    }
    let selected_model = model_tables
        .get_mut(profile)
        .and_then(Item::as_table_mut)
        .expect("selected Grok model is a TOML table");
    selected_model.insert(
        "model",
        value(non_empty(str_val(values, "upstream_model"), profile)),
    );
    selected_model.insert("base_url", value(str_val(values, "base_url").trim()));
    selected_model.insert("name", value(str_val(values, "name").trim()));
    selected_model.insert(
        "api_backend",
        value(non_empty(
            str_val(values, "api_backend"),
            DEFAULT_API_BACKEND,
        )),
    );
    selected_model.insert(
        "context_window",
        value(
            str_val(values, "context_window")
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        ),
    );
    if str_val(values, "credential_mode") == CREDENTIAL_ENV {
        selected_model.remove("api_key");
        selected_model.insert(
            "env_key",
            value(non_empty(str_val(values, "env_key"), "XAI_API_KEY")),
        );
    } else {
        selected_model.remove("env_key");
        selected_model.insert("api_key", value(str_val(values, "api_key")));
    }
    document
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let value = value.trim();
    if value.is_empty() { fallback } else { value }
}

fn preset(preset_name: &str, name: &str, base_url: &str, model: &str, env_key: &str) -> Preset {
    let mut values = FormValues::new();
    set_str(&mut values, "profile", DEFAULT_MODEL);
    set_str(&mut values, "upstream_model", model);
    set_str(&mut values, "name", name);
    set_str(&mut values, "base_url", base_url);
    set_str(&mut values, "credential_mode", CREDENTIAL_ENV);
    set_str(&mut values, "api_key", "");
    set_str(&mut values, "env_key", env_key);
    set_str(&mut values, "api_backend", DEFAULT_API_BACKEND);
    set_str(
        &mut values,
        "context_window",
        DEFAULT_CONTEXT_WINDOW.to_string(),
    );
    Preset::new(preset_name, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_native_grok_config() {
        let config = GrokBuildConfig;
        let values = config.decode(&Value::Null, None);
        let encoded = config.encode(&values, &Value::Null, None);
        let text = encoded.settings_config["config"].as_str().unwrap();
        crate::apps::grokbuild::validate_config_toml(text).unwrap();
        assert!(text.contains("[model.\"grok-4.5\"]"));
        assert!(text.contains("env_key = \"XAI_API_KEY\""));
    }

    #[test]
    fn presets_only_include_official_xai() {
        let names: Vec<String> = GrokBuildConfig
            .presets()
            .into_iter()
            .map(|preset| preset.name)
            .collect();
        assert_eq!(names, ["xAI Grok API"]);
        assert!(
            GrokBuildConfig
                .presets()
                .iter()
                .all(|preset| str_val(&preset.values, "base_url") != "https://openrouter.ai/api/v1")
        );
    }
}
