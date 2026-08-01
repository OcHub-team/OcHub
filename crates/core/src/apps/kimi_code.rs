//! Kimi Code `~/.kimi-code/config.toml` integration.
//!
//! OcHub stores one provider profile as a focused snapshot containing a single
//! `[providers.*]` entry, its `[models.*]` entries, and the active defaults.
//! Applying a profile updates only those keys in the live document so Kimi's
//! permissions, hooks, thinking preferences, and other unrelated settings are
//! preserved (including their comments).

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, Table};

use crate::error::AppError;
use crate::model::Provider;

pub fn get_kimi_code_config_dir() -> PathBuf {
    crate::settings::get_settings()
        .kimi_code_config_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::get_home_dir().join(".kimi-code"))
}

pub fn get_kimi_code_config_path() -> PathBuf {
    get_kimi_code_config_dir().join("config.toml")
}

pub fn read_kimi_code_config_text() -> Result<String, AppError> {
    let path = get_kimi_code_config_path();
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))
}

fn read_config_json() -> Result<Value, AppError> {
    let path = get_kimi_code_config_path();
    let text = read_kimi_code_config_text()?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value =
        toml::from_str::<toml::Value>(&text).map_err(|error| AppError::toml(&path, error))?;
    serde_json::to_value(value).map_err(|source| AppError::JsonSerialize { source })
}

fn active_provider_id(config: &Value) -> Option<&str> {
    let providers = config.get("providers")?.as_object()?;
    if let Some(provider) = config
        .get("default_provider")
        .and_then(Value::as_str)
        .filter(|id| providers.contains_key(*id))
    {
        return Some(provider);
    }
    let model_id = config.get("default_model")?.as_str()?;
    config
        .get("models")?
        .get(model_id)?
        .get("provider")?
        .as_str()
        .filter(|id| providers.contains_key(*id))
}

/// Read the active provider as the same focused snapshot stored by OcHub.
pub fn read_kimi_code_live_snapshot() -> Result<Value, AppError> {
    let config = read_config_json()?;
    let Some(provider_id) = active_provider_id(&config) else {
        return Ok(json!({}));
    };

    let mut snapshot = Map::new();
    if let Some(default_model) = config.get("default_model") {
        snapshot.insert("default_model".into(), default_model.clone());
    }
    if let Some(default_provider) = config.get("default_provider") {
        snapshot.insert("default_provider".into(), default_provider.clone());
    }
    if let Some(provider) = config.get("providers").and_then(|v| v.get(provider_id)) {
        snapshot.insert("providers".into(), json!({ provider_id: provider.clone() }));
    }

    let models = config
        .get("models")
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .filter(|(_, model)| {
                    model.get("provider").and_then(Value::as_str) == Some(provider_id)
                })
                .map(|(id, model)| (id.clone(), model.clone()))
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();
    snapshot.insert("models".into(), Value::Object(models));
    Ok(Value::Object(snapshot))
}

fn json_to_item(value: &Value) -> Result<Item, AppError> {
    let wrapper = json!({ "value": value });
    let encoded = toml::to_string(&wrapper)
        .map_err(|error| AppError::Config(format!("Kimi Code 配置序列化失败: {error}")))?;
    let mut document = encoded
        .parse::<DocumentMut>()
        .map_err(|error| AppError::Config(format!("Kimi Code 配置序列化失败: {error}")))?;
    document
        .as_table_mut()
        .remove("value")
        .ok_or_else(|| AppError::Config("Kimi Code 配置序列化结果为空".to_string()))
}

fn ensure_table(document: &mut DocumentMut, key: &str) {
    if !document.get(key).is_some_and(Item::is_table) {
        document[key] = Item::Table(Table::new());
    }
}

/// Merge a focused provider snapshot into the live Kimi Code TOML document.
pub fn write_kimi_code_live(provider: &Provider) -> Result<(), AppError> {
    let path = get_kimi_code_config_path();
    let existing = read_kimi_code_config_text()?;
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing.parse::<DocumentMut>().map_err(|error| {
            AppError::Config(format!(
                "Kimi Code config.toml 解析失败 ({}): {error}",
                path.display()
            ))
        })?
    };
    let settings = provider
        .settings_config
        .as_object()
        .ok_or_else(|| AppError::InvalidInput("Kimi Code 供应商配置必须是对象".to_string()))?;

    if let Some(providers) = settings.get("providers").and_then(Value::as_object) {
        ensure_table(&mut document, "providers");
        let target = document["providers"].as_table_mut().expect("table ensured");
        for (id, config) in providers {
            target.insert(id, json_to_item(config)?);
        }
    }
    if let Some(models) = settings.get("models").and_then(Value::as_object) {
        ensure_table(&mut document, "models");
        let target = document["models"].as_table_mut().expect("table ensured");
        let incoming_provider_ids = settings
            .get("providers")
            .and_then(Value::as_object)
            .map(|providers| providers.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let incoming_model_ids = models.keys().map(String::as_str).collect::<Vec<_>>();
        let stale = target
            .iter()
            .filter(|(id, model)| {
                !incoming_model_ids.contains(id)
                    && model
                        .get("provider")
                        .and_then(Item::as_str)
                        .is_some_and(|provider| incoming_provider_ids.contains(&provider))
            })
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>();
        for id in stale {
            target.remove(&id);
        }
        for (id, config) in models {
            target.insert(id, json_to_item(config)?);
        }
    }
    for key in ["default_provider", "default_model"] {
        if let Some(value) = settings.get(key) {
            document[key] = json_to_item(value)?;
        }
    }

    crate::paths::write_text_file(&path, &document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    #[test]
    fn active_snapshot_keeps_only_the_selected_provider_models() {
        let config = json!({
            "default_model": "main",
            "providers": { "one": {"type": "openai"}, "two": {"type": "anthropic"} },
            "models": {
                "main": {"provider": "one", "model": "gpt", "max_context_size": 1000},
                "other": {"provider": "two", "model": "claude", "max_context_size": 2000}
            }
        });
        assert_eq!(active_provider_id(&config), Some("one"));
    }

    #[test]
    fn json_item_quotes_dotted_model_aliases() {
        let item = json_to_item(&json!({
            "provider": "openai",
            "model": "gpt-4.1",
            "max_context_size": 128000
        }))
        .unwrap();
        assert!(item.is_table());
    }

    #[test]
    fn live_write_preserves_unrelated_config_and_replaces_stale_models() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        crate::test_support::set_var("OCHUB_TEST_HOME", home.path());
        crate::settings::reload_settings().unwrap();

        let path = get_kimi_code_config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"# keep this comment
default_model = "old"
default_provider = "custom"

[thinking]
enabled = true

[providers.custom]
type = "openai"
base_url = "https://old.example/v1"
api_key = "old"

[models.old]
provider = "custom"
model = "old"
max_context_size = 1000
"#,
        )
        .unwrap();

        let provider = Provider::with_id(
            "profile".into(),
            "Profile".into(),
            json!({
                "default_provider": "custom",
                "default_model": "new",
                "providers": { "custom": {
                    "type": "openai",
                    "base_url": "https://new.example/v1",
                    "api_key": "new-key"
                }},
                "models": { "new": {
                    "provider": "custom",
                    "model": "new-upstream",
                    "max_context_size": 128000
                }}
            }),
            None,
        );
        write_kimi_code_live(&provider).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"));
        let config = toml::from_str::<toml::Value>(&text).unwrap();
        assert_eq!(config["thinking"]["enabled"].as_bool(), Some(true));
        assert!(config["models"].get("old").is_none());
        assert_eq!(
            config["models"]["new"]["model"].as_str(),
            Some("new-upstream")
        );
        assert_eq!(
            read_kimi_code_live_snapshot().unwrap(),
            provider.settings_config
        );

        crate::test_support::remove_var("OCHUB_TEST_HOME");
        crate::settings::reload_settings().ok();
    }
}
