//! Grok Build live configuration management.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::AppError;
use crate::model::Provider;

pub const DEFAULT_MODEL: &str = "grok-4.5";
pub const DEFAULT_API_BACKEND: &str = "responses";
pub const DEFAULT_CONTEXT_WINDOW: i64 = 500_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokModelConfig {
    pub profile: String,
    pub model: String,
    pub base_url: String,
    pub name: String,
    pub api_key: Option<String>,
    pub env_key: Option<String>,
    pub api_backend: String,
    pub context_window: i64,
}

pub fn get_grok_config_dir() -> PathBuf {
    crate::settings::get_grokbuild_override_dir()
        .unwrap_or_else(|| crate::paths::get_home_dir().join(".grok"))
}

pub fn get_grok_config_path() -> PathBuf {
    get_grok_config_dir().join("config.toml")
}

pub fn validate_config_toml_syntax(config_toml: &str) -> Result<(), AppError> {
    toml::from_str::<toml::Table>(config_toml)
        .map(|_| ())
        .map_err(config_parse_error)
}

fn config_parse_error(error: impl std::fmt::Display) -> AppError {
    AppError::localized_ja(
        "grokbuild.config.parse_failed",
        format!("Grok Build config.toml 格式错误: {error}"),
        format!("Invalid Grok Build config.toml: {error}"),
        format!("Grok Build の config.toml の形式が無効です: {error}"),
    )
}

fn required_string<'a>(table: &'a toml::value::Table, key: &str) -> Result<&'a str, AppError> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::localized_ja(
                "grokbuild.config.required_field",
                format!("Grok Build 配置缺少有效的 {key} 字段"),
                format!("Grok Build configuration requires a valid {key} field"),
                format!("Grok Build の設定には有効な {key} フィールドが必要です"),
            )
        })
}

fn optional_string(table: &toml::value::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn validate_config_toml(config_toml: &str) -> Result<(), AppError> {
    let root = toml::from_str::<toml::Table>(config_toml).map_err(config_parse_error)?;
    let models = root
        .get("models")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            AppError::localized_ja(
                "grokbuild.config.models_missing",
                "Grok Build 配置缺少 [models]",
                "Grok Build configuration is missing [models]",
                "Grok Build の設定に [models] がありません",
            )
        })?;
    let profile = required_string(models, "default")?;
    let model = root
        .get("model")
        .and_then(toml::Value::as_table)
        .and_then(|models| models.get(profile))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            AppError::localized_ja(
                "grokbuild.config.model_missing",
                format!("Grok Build 配置缺少 [model.\"{profile}\"]"),
                format!("Grok Build configuration is missing [model.\"{profile}\"]"),
                format!("Grok Build の設定に [model.\"{profile}\"] がありません"),
            )
        })?;

    required_string(model, "model")?;
    required_string(model, "base_url")?;
    required_string(model, "name")?;
    required_string(model, "api_backend")?;
    if optional_string(model, "api_key").is_none() && optional_string(model, "env_key").is_none() {
        return Err(AppError::localized_ja(
            "grokbuild.config.credentials_required",
            "Grok Build 配置缺少有效的 api_key 或 env_key 字段",
            "Grok Build configuration requires a valid api_key or env_key field",
            "Grok Build の設定には有効な api_key または env_key フィールドが必要です",
        ));
    }
    model
        .get("context_window")
        .and_then(toml::Value::as_integer)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AppError::localized_ja(
                "grokbuild.config.context_window_invalid",
                "Grok Build context_window 必须是正整数",
                "Grok Build context_window must be a positive integer",
                "Grok Build の context_window には正の整数を指定してください",
            )
        })?;
    Ok(())
}

pub fn extract_model_config(config_toml: &str) -> Option<GrokModelConfig> {
    let root = toml::from_str::<toml::Table>(config_toml).ok()?;
    let profile = root
        .get("models")?
        .as_table()?
        .get("default")?
        .as_str()?
        .trim();
    let model = root.get("model")?.as_table()?.get(profile)?.as_table()?;

    Some(GrokModelConfig {
        profile: profile.to_string(),
        model: model.get("model")?.as_str()?.trim().to_string(),
        base_url: model
            .get("base_url")?
            .as_str()?
            .trim_end_matches('/')
            .to_string(),
        name: model.get("name")?.as_str()?.trim().to_string(),
        api_key: optional_string(model, "api_key"),
        env_key: optional_string(model, "env_key"),
        api_backend: model.get("api_backend")?.as_str()?.trim().to_string(),
        context_window: model.get("context_window")?.as_integer()?,
    })
}

pub fn extract_credentials(config_toml: &str) -> Option<(String, String)> {
    let config = extract_model_config(config_toml)?;
    let api_key = config
        .api_key
        .or_else(|| {
            config
                .env_key
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            std::env::var("XAI_API_KEY")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })?;
    Some((config.base_url, api_key))
}

pub fn strip_grok_mcp_servers_from_settings(settings: &mut Value) -> Result<(), AppError> {
    let Some(config_text) = settings
        .get("config")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    let mut document = config_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(config_parse_error)?;
    if document.as_table_mut().remove("mcp_servers").is_some() {
        settings["config"] = Value::String(document.to_string());
    }
    Ok(())
}

pub fn read_grok_live_settings() -> Result<Value, AppError> {
    let path = get_grok_config_path();
    if !path.exists() {
        return Err(AppError::localized_ja(
            "grokbuild.config.file_missing",
            "Grok Build 配置文件不存在",
            "Grok Build configuration file does not exist",
            "Grok Build の設定ファイルがありません",
        ));
    }
    let config = fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))?;
    // The official xAI OAuth profile intentionally uses an empty config and
    // lets Grok Build supply its built-in model catalogue.
    validate_config_toml_syntax(&config)?;
    Ok(json!({ "config": config }))
}

pub fn write_grok_provider_live(provider: &Provider) -> Result<(), AppError> {
    let config = provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::localized_ja(
                "grokbuild.config.config_missing",
                "Grok Build 配置缺少 config 字段",
                "Grok Build configuration is missing the config field",
                "Grok Build の設定に config フィールドがありません",
            )
        })?;
    if provider.category.as_deref() == Some("official") {
        validate_config_toml_syntax(config)?;
    } else {
        validate_config_toml(config)?;
    }
    crate::paths::write_text_file(&get_grok_config_path(), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> &'static str {
        r#"[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
name = "xAI"
env_key = "XAI_API_KEY"
api_backend = "responses"
context_window = 500000
"#
    }

    #[test]
    fn validates_and_extracts_native_config() {
        validate_config_toml(valid_config()).unwrap();
        let config = extract_model_config(valid_config()).unwrap();
        assert_eq!(config.profile, "grok-4.5");
        assert_eq!(config.base_url, "https://api.x.ai/v1");
        assert_eq!(config.env_key.as_deref(), Some("XAI_API_KEY"));
    }

    #[test]
    fn rejects_missing_credentials() {
        let config = valid_config().replace("env_key = \"XAI_API_KEY\"\n", "");
        assert!(validate_config_toml(&config).is_err());
    }

    #[test]
    fn validation_errors_include_chinese_english_and_japanese() {
        let cases = [
            ("".to_string(), "models_missing", "[models]"),
            ("[models]".to_string(), "required_field", "default"),
            (
                "[models]\ndefault = \"用户配置-{profile}\"".to_string(),
                "model_missing",
                "用户配置-{profile}",
            ),
            (
                valid_config().replace("model = \"grok-4.5\"", "model = \"\""),
                "required_field",
                "model",
            ),
            (
                valid_config().replace("env_key = \"XAI_API_KEY\"\n", ""),
                "credentials_required",
                "api_key",
            ),
            (
                valid_config().replace("context_window = 500000", "context_window = -1"),
                "context_window_invalid",
                "context_window",
            ),
        ];

        // Inspect each supplied locale without mutating the process-wide locale;
        // AppError's display tests cover choosing the installed language.
        for (config, expected_key, parameter) in cases {
            let AppError::Localized { key, zh, en, ja } =
                validate_config_toml(&config).unwrap_err()
            else {
                panic!("Grok Build validation must provide localized messages");
            };
            assert_eq!(key, format!("grokbuild.config.{expected_key}"));
            for message in [&zh, &en, &ja.unwrap()] {
                assert!(message.contains(parameter), "{key}: {message}");
            }
            assert!(zh.contains("配置") || zh.contains("正整数"));
            assert!(en.starts_with("Grok Build "));
        }
    }

    #[test]
    fn parse_error_translations_preserve_the_original_toml_excerpt() {
        let config = "name = \"用户文件-{error}\" trailing";
        for error in [
            validate_config_toml_syntax(config).unwrap_err(),
            validate_config_toml(config).unwrap_err(),
        ] {
            let AppError::Localized { key, zh, en, ja } = error else {
                panic!("Grok Build parsing must provide localized messages");
            };
            assert_eq!(key, "grokbuild.config.parse_failed");
            assert!(en.starts_with("Invalid Grok Build config.toml: "));
            let ja = ja.unwrap();
            assert!(ja.starts_with("Grok Build の config.toml の形式が無効です: "));
            for message in [&zh, &en, &ja] {
                assert!(message.contains("用户文件-{error}"));
            }
        }
    }
}
