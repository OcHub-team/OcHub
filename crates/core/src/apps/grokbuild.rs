//! Grok Build live configuration management.

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

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
        .map_err(|error| AppError::Config(format!("Grok Build config.toml 格式错误: {error}")))
}

fn required_string<'a>(table: &'a toml::value::Table, key: &str) -> Result<&'a str, AppError> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Config(format!("Grok Build 配置缺少有效的 {key} 字段")))
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
    let root = toml::from_str::<toml::Table>(config_toml)
        .map_err(|error| AppError::Config(format!("Grok Build config.toml 格式错误: {error}")))?;
    let models = root
        .get("models")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| AppError::Config("Grok Build 配置缺少 [models]".into()))?;
    let profile = required_string(models, "default")?;
    let model = root
        .get("model")
        .and_then(toml::Value::as_table)
        .and_then(|models| models.get(profile))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| AppError::Config(format!("Grok Build 配置缺少 [model.\"{profile}\"]")))?;

    required_string(model, "model")?;
    required_string(model, "base_url")?;
    required_string(model, "name")?;
    required_string(model, "api_backend")?;
    if optional_string(model, "api_key").is_none() && optional_string(model, "env_key").is_none() {
        return Err(AppError::Config(
            "Grok Build 配置缺少有效的 api_key 或 env_key 字段".into(),
        ));
    }
    model
        .get("context_window")
        .and_then(toml::Value::as_integer)
        .filter(|value| *value > 0)
        .ok_or_else(|| AppError::Config("Grok Build context_window 必须是正整数".into()))?;
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
        .map_err(|error| AppError::Config(format!("Grok Build config.toml 格式错误: {error}")))?;
    if document.as_table_mut().remove("mcp_servers").is_some() {
        settings["config"] = Value::String(document.to_string());
    }
    Ok(())
}

pub fn read_grok_live_settings() -> Result<Value, AppError> {
    let path = get_grok_config_path();
    if !path.exists() {
        return Err(AppError::Config("Grok Build 配置文件不存在".into()));
    }
    let config = fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))?;
    validate_config_toml(&config)?;
    Ok(json!({ "config": config }))
}

pub fn write_grok_provider_live(provider: &Provider) -> Result<(), AppError> {
    let config = provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Config("Grok Build 配置缺少 config 字段".into()))?;
    validate_config_toml(config)?;
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
}
