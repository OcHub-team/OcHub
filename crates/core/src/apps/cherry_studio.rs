//! Cherry Studio provider import through its public custom-URL protocol.

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::model::Provider;

pub const DEFAULT_PROVIDER_TYPE: &str = "openai";
pub const SUPPORTED_PROVIDER_TYPES: &[&str] = &[
    "openai",
    "openai-response",
    "anthropic",
    "gemini",
    "vertexai",
    "vertex-anthropic",
    "ollama",
];

pub fn get_cherry_studio_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".cherrystudio")
}

pub fn build_provider_import_deeplink(provider: &Provider) -> Result<String, AppError> {
    let settings = provider
        .settings_config
        .as_object()
        .ok_or_else(|| AppError::Config("Cherry Studio 导入配置必须是 JSON 对象".to_string()))?;
    let base_url = required_string(settings.get("base_url"), "Base URL")?;
    let api_key = required_string(settings.get("api_key"), "API Key")?;
    let provider_type = settings
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PROVIDER_TYPE);
    if !SUPPORTED_PROVIDER_TYPES.contains(&provider_type) {
        return Err(AppError::InvalidInput(format!(
            "Cherry Studio 不支持协议类型：{provider_type}"
        )));
    }

    let payload = json!({
        "id": provider.id,
        "name": provider.name,
        "baseUrl": base_url,
        "apiKey": api_key,
        "type": provider_type,
    });
    let bytes = serde_json::to_vec(&payload)
        .map_err(|error| AppError::Config(format!("序列化 Cherry Studio 导入参数失败: {error}")))?;
    let data = STANDARD.encode(bytes).replace('+', "_").replace('/', "-");
    Ok(format!("cherrystudio://providers/api-keys?v=1&data={data}"))
}

fn required_string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str, AppError> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::InvalidInput(format!("Cherry Studio {label} 不能为空")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeplink_round_trips_through_cherry_url_safe_base64() {
        let provider = Provider::with_id(
            "relay-main".into(),
            "Relay Main".into(),
            json!({
                "type": "anthropic",
                "base_url": "https://relay.example/v1",
                "api_key": "sk-test+/value",
            }),
            None,
        );

        let link = build_provider_import_deeplink(&provider).unwrap();
        let data = link.split("data=").nth(1).unwrap();
        let standard = data.replace('_', "+").replace('-', "/");
        let decoded = STANDARD.decode(standard).unwrap();
        let payload: Value = serde_json::from_slice(&decoded).unwrap();

        assert_eq!(payload["id"], "relay-main");
        assert_eq!(payload["name"], "Relay Main");
        assert_eq!(payload["baseUrl"], "https://relay.example/v1");
        assert_eq!(payload["apiKey"], "sk-test+/value");
        assert_eq!(payload["type"], "anthropic");
    }

    #[test]
    fn deeplink_rejects_unknown_provider_type() {
        let provider = Provider::with_id(
            "bad".into(),
            "Bad".into(),
            json!({
                "type": "unknown",
                "base_url": "https://relay.example/v1",
                "api_key": "sk-test",
            }),
            None,
        );
        assert!(build_provider_import_deeplink(&provider).is_err());
    }
}
