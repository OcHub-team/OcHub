use serde::{Deserialize, Serialize};

use crate::application::{
    Application, ApplicationError, ApplicationResult, ProviderDetails, ProviderListItem,
    ProviderSwitchPlan,
};
use crate::plugin::AppMode;
use crate::provider_config::Severity;
use crate::services::provider::{
    DriftConflict, DriftResolution, LiveDrift, ProviderService, SwitchResult,
};
use crate::{AppId, AppType, Provider};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSwitchPolicy {
    #[default]
    Abort,
    Preserve,
    Discard,
}

impl Application {
    fn resolve_plugin(
        &self,
        app: &AppId,
    ) -> ApplicationResult<std::sync::Arc<dyn crate::plugin::AppPlugin>> {
        crate::plugin::get_plugin(app).ok_or_else(|| ApplicationError::NotFound {
            kind: "app",
            id: app.to_string(),
        })
    }

    fn builtin_app(&self, app: &AppId, capability: &'static str) -> ApplicationResult<AppType> {
        AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
            app: app.to_string(),
            capability,
        })
    }

    pub fn list_providers(&self, app: &AppId) -> ApplicationResult<Vec<ProviderListItem>> {
        let plugin = self.resolve_plugin(app)?;
        let builtin = AppType::from_app_id(app);
        let current = if plugin.mode() == AppMode::Switch {
            match builtin {
                Some(app_type) => {
                    let current = ProviderService::current(&self.state, app_type)?;
                    (!current.is_empty()).then_some(current)
                }
                None => self.state.db.get_current_provider(app.as_str())?,
            }
        } else {
            None
        };
        let mut providers = self
            .state
            .db
            .get_all_providers(app.as_str())?
            .into_values()
            .map(|provider| {
                let base_url = builtin
                    .map(|app_type| provider.resolve_usage_base_url(&app_type))
                    .unwrap_or_default();
                ProviderListItem {
                    current: current.as_deref() == Some(provider.id.as_str()),
                    app: app.to_string(),
                    id: provider.id,
                    name: provider.name,
                    category: provider.category,
                    website_url: provider.website_url,
                    sort_index: provider.sort_index,
                    live_config_managed: provider
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.live_config_managed),
                    base_url,
                }
            })
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| {
            left.sort_index
                .unwrap_or(usize::MAX)
                .cmp(&right.sort_index.unwrap_or(usize::MAX))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(providers)
    }

    pub fn get_provider(
        &self,
        app: &AppId,
        id: &str,
        show_secrets: bool,
    ) -> ApplicationResult<ProviderDetails> {
        self.resolve_plugin(app)?;
        let mut provider = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        if !show_secrets {
            provider.settings_config = redact_json(&provider.settings_config);
            if let Some(meta) = provider.meta.as_mut()
                && let Some(script) = meta.usage_script.as_mut()
            {
                script.api_key = script.api_key.as_ref().map(|_| "******".to_string());
                script.access_token = script.access_token.as_ref().map(|_| "******".to_string());
            }
        }
        Ok(ProviderDetails {
            app: app.to_string(),
            provider,
        })
    }

    pub fn add_provider(
        &self,
        app: &AppId,
        provider: Provider,
        add_to_live: bool,
    ) -> ApplicationResult<ProviderDetails> {
        let plugin = self.resolve_plugin(app)?;
        if self
            .state
            .db
            .get_provider_by_id(&provider.id, app.as_str())?
            .is_some()
        {
            return Err(ApplicationError::AlreadyExists {
                kind: "provider",
                id: provider.id,
            });
        }
        validate_provider_with_plugin(plugin.as_ref(), &provider)?;
        if let Some(app_type) = AppType::from_app_id(app) {
            ProviderService::add(&self.state, app_type, provider.clone(), add_to_live)?;
        } else {
            self.state.db.save_provider(app.as_str(), &provider)?;
            if add_to_live {
                plugin
                    .live()
                    .write_live(self.state.db.as_ref(), &provider)?;
                if plugin.mode() == AppMode::Switch {
                    self.state
                        .db
                        .set_current_provider(app.as_str(), &provider.id)?;
                }
            }
        }
        self.get_provider(app, &provider.id, false)
    }

    pub fn update_provider(
        &self,
        app: &AppId,
        original_id: &str,
        provider: Provider,
    ) -> ApplicationResult<ProviderDetails> {
        let plugin = self.resolve_plugin(app)?;
        let existing = self
            .state
            .db
            .get_provider_by_id(original_id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: original_id.to_string(),
            })?;
        if original_id != provider.id
            && self
                .state
                .db
                .get_provider_by_id(&provider.id, app.as_str())?
                .is_some()
        {
            return Err(ApplicationError::AlreadyExists {
                kind: "provider",
                id: provider.id,
            });
        }
        validate_provider_with_plugin(plugin.as_ref(), &provider)?;
        if let Some(app_type) = AppType::from_app_id(app) {
            ProviderService::update(&self.state, app_type, Some(original_id), provider.clone())?;
        } else {
            let current = self.state.db.get_current_provider(app.as_str())?;
            let was_current = current.as_deref() == Some(original_id);
            self.state.db.save_provider(app.as_str(), &provider)?;
            if original_id != provider.id {
                self.state.db.delete_provider(app.as_str(), original_id)?;
            }
            let live_managed = plugin.mode() == AppMode::Switch && was_current
                || plugin.mode() == AppMode::Additive
                    && existing
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.live_config_managed)
                        .unwrap_or(false);
            if live_managed {
                plugin
                    .live()
                    .write_live(self.state.db.as_ref(), &provider)?;
            }
            if was_current {
                self.state
                    .db
                    .set_current_provider(app.as_str(), &provider.id)?;
            }
        }
        self.get_provider(app, &provider.id, false)
    }

    pub fn delete_provider(&self, app: &AppId, id: &str) -> ApplicationResult<()> {
        let plugin = self.resolve_plugin(app)?;
        let existing = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        if let Some(app_type) = AppType::from_app_id(app) {
            ProviderService::delete(&self.state, app_type, id)?;
            return Ok(());
        }
        if self.state.db.get_current_provider(app.as_str())?.as_deref() == Some(id) {
            return Err(ApplicationError::ResourceConflict(format!(
                "provider {id} is active for {app}; switch away before deleting it"
            )));
        }
        if plugin.mode() == AppMode::Additive
            && existing
                .meta
                .as_ref()
                .and_then(|meta| meta.live_config_managed)
                .unwrap_or(false)
        {
            return Err(ApplicationError::ResourceConflict(format!(
                "provider {id} is present in the live config for {app}; remove it from live before deleting it"
            )));
        }
        self.state.db.delete_provider(app.as_str(), id)?;
        Ok(())
    }

    pub fn duplicate_provider(&self, app: &AppId, id: &str) -> ApplicationResult<ProviderDetails> {
        let app_type = self.builtin_app(app, "provider.duplicate")?;
        let provider = ProviderService::duplicate(&self.state, app_type, id)?;
        Ok(ProviderDetails {
            app: app.to_string(),
            provider,
        })
    }

    pub fn remove_provider_from_live(&self, app: &AppId, id: &str) -> ApplicationResult<()> {
        if let Some(app_type) = AppType::from_app_id(app) {
            ProviderService::remove_from_live_config(&self.state, app_type, id)?;
        } else {
            let plugin = self.resolve_plugin(app)?;
            if plugin.mode() != AppMode::Additive {
                return Err(ApplicationError::CapabilityUnsupported {
                    app: app.to_string(),
                    capability: "provider.remove-from-live",
                });
            }
            plugin.live().remove_from_live(id)?;
            if let Some(mut provider) = self.state.db.get_provider_by_id(id, app.as_str())? {
                provider
                    .meta
                    .get_or_insert_with(Default::default)
                    .live_config_managed = Some(false);
                self.state.db.save_provider(app.as_str(), &provider)?;
            }
        }
        Ok(())
    }

    pub fn add_provider_to_live(&self, app: &AppId, id: &str) -> ApplicationResult<SwitchResult> {
        if AppType::from_app_id(app).is_some() {
            return self.switch_provider(app, id, ProviderSwitchPolicy::Preserve);
        }
        let plugin = self.resolve_plugin(app)?;
        let mut provider = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        if plugin.mode() == AppMode::Switch {
            return self.switch_provider(app, id, ProviderSwitchPolicy::Preserve);
        }
        plugin
            .live()
            .write_live(self.state.db.as_ref(), &provider)?;
        provider
            .meta
            .get_or_insert_with(Default::default)
            .live_config_managed = Some(true);
        self.state.db.save_provider(app.as_str(), &provider)?;
        Ok(SwitchResult::default())
    }

    pub fn preview_provider_switch(
        &self,
        app: &AppId,
        id: &str,
    ) -> ApplicationResult<ProviderSwitchPlan> {
        let target = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        if let Some(app_type) = AppType::from_app_id(app) {
            let current = ProviderService::current(&self.state, app_type)?;
            let drift = ProviderService::preview_switch(&self.state, app_type, id)?;
            return Ok(ProviderSwitchPlan {
                app: app.to_string(),
                provider_id: id.to_string(),
                current_provider_id: (!current.is_empty()).then_some(current.clone()),
                config_path: crate::services::provider::drift::live_config_label(&app_type),
                would_change: current != id || !drift.is_empty(),
                drift,
            });
        }
        let plugin = self.resolve_plugin(app)?;
        if plugin.mode() != AppMode::Switch {
            return Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "provider.switch",
            });
        }
        let current = self.state.db.get_current_provider(app.as_str())?;
        let live = plugin.live().read_live()?;
        let current_provider = match current.as_deref() {
            Some(current_id) => self.state.db.get_provider_by_id(current_id, app.as_str())?,
            None => None,
        };
        let live_changed = current_provider
            .as_ref()
            .map(|provider| provider.settings_config != live)
            .unwrap_or_else(|| !empty_live_config(&live));
        let drift = if live_changed {
            LiveDrift {
                conflicts: vec![DriftConflict {
                    path: "$".to_string(),
                    live,
                    incoming: target.settings_config.clone(),
                }],
                ..Default::default()
            }
        } else {
            LiveDrift::default()
        };
        Ok(ProviderSwitchPlan {
            app: app.to_string(),
            provider_id: id.to_string(),
            current_provider_id: current.clone(),
            config_path: plugin.config_dir()?.to_string_lossy().into_owned(),
            would_change: current.as_deref() != Some(id) || !drift.is_empty(),
            drift,
        })
    }

    pub fn switch_provider(
        &self,
        app: &AppId,
        id: &str,
        policy: ProviderSwitchPolicy,
    ) -> ApplicationResult<SwitchResult> {
        let plan = self.preview_provider_switch(app, id)?;
        if policy == ProviderSwitchPolicy::Abort && !plan.drift.is_empty() {
            return Err(ApplicationError::ConfigDrift {
                app: app.to_string(),
                path: plan.config_path,
                drift: Box::new(plan.drift),
            });
        }
        if let Some(app_type) = AppType::from_app_id(app) {
            let resolution = match policy {
                ProviderSwitchPolicy::Abort | ProviderSwitchPolicy::Preserve => {
                    DriftResolution::Preserve
                }
                ProviderSwitchPolicy::Discard => DriftResolution::Discard,
            };
            return Ok(ProviderService::switch_with(
                &self.state,
                app_type,
                id,
                resolution,
            )?);
        }
        if policy == ProviderSwitchPolicy::Preserve && !plan.drift.is_empty() {
            return Err(ApplicationError::CapabilityUnsupported {
                app: app.to_string(),
                capability: "provider.switch-preserve-drift",
            });
        }
        let plugin = self.resolve_plugin(app)?;
        let provider = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        plugin
            .live()
            .write_live(self.state.db.as_ref(), &provider)?;
        self.state.db.set_current_provider(app.as_str(), id)?;
        Ok(SwitchResult {
            warnings: Vec::new(),
            drift: (!plan.drift.is_empty()).then_some(plan.drift),
        })
    }

    pub fn import_live_providers(&self, app: &AppId) -> ApplicationResult<usize> {
        let app_type = self.builtin_app(app, "provider.import-live")?;
        Ok(ProviderService::auto_import_live_providers(
            &self.state,
            app_type,
        )?)
    }

    pub fn seed_official_provider(&self, app: &AppId) -> ApplicationResult<bool> {
        let app_type = self.builtin_app(app, "provider.seed-official")?;
        Ok(ProviderService::import_default_config(
            &self.state,
            app_type,
        )?)
    }

    pub fn sync_live_provider(&self, app: &AppId) -> ApplicationResult<()> {
        if let Some(app_type) = AppType::from_app_id(app) {
            ProviderService::sync_current_provider_for_app(&self.state, app_type)?;
        } else {
            let plugin = self.resolve_plugin(app)?;
            let id = self
                .state
                .db
                .get_current_provider(app.as_str())?
                .ok_or_else(|| ApplicationError::NotFound {
                    kind: "current-provider",
                    id: app.to_string(),
                })?;
            let provider = self
                .state
                .db
                .get_provider_by_id(&id, app.as_str())?
                .ok_or_else(|| ApplicationError::NotFound {
                    kind: "provider",
                    id,
                })?;
            plugin
                .live()
                .write_live(self.state.db.as_ref(), &provider)?;
        }
        Ok(())
    }

    pub fn provider_drift(&self, app: &AppId, id: &str) -> ApplicationResult<LiveDrift> {
        Ok(self.preview_provider_switch(app, id)?.drift)
    }
}

fn empty_live_config(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.is_empty(),
        serde_json::Value::Array(items) => items.is_empty(),
        serde_json::Value::String(value) => value.trim().is_empty(),
        _ => false,
    }
}

fn validate_provider_with_plugin(
    plugin: &dyn crate::plugin::AppPlugin,
    provider: &Provider,
) -> ApplicationResult<()> {
    if provider.id.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "provider id cannot be empty".to_string(),
        ));
    }
    if provider.name.trim().is_empty() {
        return Err(ApplicationError::InvalidInput(
            "provider name cannot be empty".to_string(),
        ));
    }
    let codec =
        plugin
            .provider_config()
            .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                app: plugin.id().to_string(),
                capability: "provider.write",
            })?;
    let values = codec.decode(&provider.settings_config, provider.meta.as_ref());
    let errors = codec
        .validate_for_category(&values, provider.category.as_deref())
        .into_iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| {
            issue
                .field
                .map(|field| format!("{field}: {}", issue.message))
                .unwrap_or(issue.message)
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApplicationError::ValidationFailed {
            message: format!("provider {} is invalid", provider.id),
            details: serde_json::json!({ "issues": errors }),
        })
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("authorization")
        || normalized.contains("cookie")
        || normalized == "key"
        || normalized == "api_key"
        || normalized == "apikey"
        || normalized.ends_with("_api_key")
        || normalized.ends_with("_key")
}

/// Whether a value under a secret-looking key can actually hold a credential.
///
/// Numbers and booleans never do, and [`is_secret_key`] matches on the
/// substring `token`, which every token *counter* in the usage schema also
/// carries: `totalInputTokens`, `inputTokens`, `firstTokenMs`. Masking those
/// turned each one into the string `******`, so every remote usage response
/// failed to deserialize into its `u64`/`u32` field and the whole page came
/// back empty. Credentials are always strings here; masking only non-scalars
/// keeps a hypothetical structured secret covered too.
fn is_redactable(value: &serde_json::Value) -> bool {
    !matches!(
        value,
        serde_json::Value::Null | serde_json::Value::Number(_) | serde_json::Value::Bool(_)
    )
}

pub fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let redacted = if is_secret_key(key) && is_redactable(value) {
                        serde_json::Value::String("******".to_string())
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        serde_json::Value::Array(values)
            if values.len() == 2
                && values[0].as_str().is_some_and(is_secret_key)
                && is_redactable(&values[1]) =>
        {
            serde_json::Value::Array(vec![
                values[0].clone(),
                serde_json::Value::String("******".to_string()),
            ])
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_json).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_json;

    #[test]
    fn redacts_nested_secret_values_without_hiding_regular_fields() {
        let input = serde_json::json!({
            "baseUrl": "https://example.com",
            "env": {
                "ANTHROPIC_API_KEY": "sk-secret",
                "MODEL": "claude"
            },
            "headers": {
                "Authorization": "Bearer secret"
            },
            "extraHeaders": [["x-api-key", "header-secret"]]
        });
        assert_eq!(
            redact_json(&input),
            serde_json::json!({
                "baseUrl": "https://example.com",
                "env": {
                    "ANTHROPIC_API_KEY": "******",
                    "MODEL": "claude"
                },
                "headers": {
                    "Authorization": "******"
                },
                "extraHeaders": [["x-api-key", "******"]]
            })
        );
    }

    /// Token *counters* are numbers, and every one of them is named `*Tokens`.
    /// Masking them shipped `"******"` where the client expected `u64`, which
    /// is what emptied the usage page on remote nodes.
    #[test]
    fn keeps_numeric_token_counters_intact_while_masking_string_credentials() {
        let input = serde_json::json!({
            "totalRequests": 42,
            "totalInputTokens": 1234,
            "realTotalTokens": 9999,
            "firstTokenMs": 120,
            "accessToken": "sk-secret",
            "refreshToken": "rt-secret",
            "logs": [{ "inputTokens": 7, "outputTokens": 8, "apiKey": "sk-live" }]
        });
        assert_eq!(
            redact_json(&input),
            serde_json::json!({
                "totalRequests": 42,
                "totalInputTokens": 1234,
                "realTotalTokens": 9999,
                "firstTokenMs": 120,
                "accessToken": "******",
                "refreshToken": "******",
                "logs": [{ "inputTokens": 7, "outputTokens": 8, "apiKey": "******" }]
            })
        );
    }
}
