//! One-click app configuration: point a managed app at the local gateway.
//!
//! For each supported app this creates (or refreshes) a "Local Gateway"
//! provider entry — reusing the normal provider machinery so the change is
//! visible, switchable, and revertible in the regular provider list — and then
//! switches the app to it. Each app gets its own gateway API key so usage rows
//! are attributed per app.

use serde::Serialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::gateway::types::GatewayKey;
use crate::model::Provider;
use crate::services::provider::ProviderService;

/// Fixed provider id for gateway entries (per app list).
pub const GATEWAY_PROVIDER_ID: &str = "local-gateway";
const GATEWAY_PROVIDER_NAME: &str = "Local Gateway";

/// Result surfaced to the UI after a one-click apply (or for manual clients).
#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub base_url: String,
    pub key_name: String,
    pub key_secret: String,
    /// True when an app config was written (false for the generic info case).
    pub applied: bool,
}

/// Which apps support one-click gateway configuration.
pub fn supported_apps() -> &'static [AppType] {
    &[AppType::Claude, AppType::ClaudeDesktop, AppType::Codex]
}

/// Ensure a gateway key named after `label` exists, creating it if needed.
pub fn ensure_key(state: &AppState, label: &str) -> Result<GatewayKey, AppError> {
    let existing = state.db.get_gateway_keys()?;
    if let Some(k) = existing.into_iter().find(|k| k.name == label && k.enabled) {
        return Ok(k);
    }
    let key = GatewayKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: label.to_string(),
        key: crate::gateway::generate_key_secret(),
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.db.upsert_gateway_key(&key)?;
    Ok(key)
}

fn gateway_settings_for(
    app_type: AppType,
    base_url: &str,
    key: &str,
) -> Result<serde_json::Value, AppError> {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => Ok(json!({
            "env": {
                "ANTHROPIC_BASE_URL": base_url,
                "ANTHROPIC_AUTH_TOKEN": key,
            }
        })),
        AppType::Codex => {
            let toml = format!(
                concat!(
                    "model_provider = \"{id}\"\n",
                    "disable_response_storage = true\n",
                    "\n",
                    "[model_providers.{id}]\n",
                    "name = \"{name}\"\n",
                    "base_url = \"{base}/v1\"\n",
                    "wire_api = \"responses\"\n",
                    "env_key = \"OPENAI_API_KEY\"\n",
                ),
                id = GATEWAY_PROVIDER_ID,
                name = GATEWAY_PROVIDER_NAME,
                base = base_url,
            );
            Ok(json!({
                "auth": { "OPENAI_API_KEY": key },
                "config": toml,
            }))
        }
        other => Err(AppError::Config(format!(
            "one-click gateway config is not supported for {}",
            other.as_str()
        ))),
    }
}

/// Create/refresh the gateway provider entry for `app_type` and switch to it.
///
/// `base_url` is the running gateway origin (e.g. `http://127.0.0.1:4180`).
pub fn apply_to_app(
    state: &AppState,
    app_type: AppType,
    base_url: &str,
) -> Result<ApplyResult, AppError> {
    let key = ensure_key(state, app_type.as_str())?;
    let settings = gateway_settings_for(app_type, base_url, &key.key)?;

    let provider = Provider {
        id: GATEWAY_PROVIDER_ID.to_string(),
        name: GATEWAY_PROVIDER_NAME.to_string(),
        settings_config: settings,
        website_url: None,
        category: Some("gateway".to_string()),
        created_at: Some(chrono::Utc::now().timestamp()),
        sort_index: None,
        notes: Some("Managed by the local gateway one-click setup".to_string()),
        meta: None,
        icon: None,
        icon_color: None,
    };

    let existing = state
        .db
        .get_provider_by_id(GATEWAY_PROVIDER_ID, app_type.as_str())?;
    if existing.is_some() {
        ProviderService::update(state, app_type, Some(GATEWAY_PROVIDER_ID), provider)?;
    } else {
        ProviderService::add(state, app_type, provider, false)?;
    }
    ProviderService::switch(state, app_type, GATEWAY_PROVIDER_ID)?;

    Ok(ApplyResult {
        base_url: base_url.to_string(),
        key_name: key.name,
        key_secret: key.key,
        applied: true,
    })
}

/// Connection info for clients we don't manage (generic chat-dialect tools):
/// creates/reuses a key and returns the endpoint details for copy-paste.
pub fn generic_client_info(state: &AppState, base_url: &str) -> Result<ApplyResult, AppError> {
    let key = ensure_key(state, "generic-client")?;
    Ok(ApplyResult {
        base_url: format!("{base_url}/v1"),
        key_name: key.name,
        key_secret: key.key,
        applied: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_shapes_per_app() {
        let claude =
            gateway_settings_for(AppType::Claude, "http://127.0.0.1:4180", "rd-k").unwrap();
        assert_eq!(claude["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4180");
        assert_eq!(claude["env"]["ANTHROPIC_AUTH_TOKEN"], "rd-k");

        let codex = gateway_settings_for(AppType::Codex, "http://127.0.0.1:4180", "rd-k").unwrap();
        assert_eq!(codex["auth"]["OPENAI_API_KEY"], "rd-k");
        let toml = codex["config"].as_str().unwrap();
        assert!(toml.contains("model_provider = \"local-gateway\""));
        assert!(toml.contains("base_url = \"http://127.0.0.1:4180/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));

        assert!(gateway_settings_for(AppType::OpenCode, "x", "y").is_err());
    }
}
