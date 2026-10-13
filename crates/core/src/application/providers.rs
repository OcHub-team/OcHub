use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::{
    Application, ApplicationError, ApplicationResult, ConfigDiffFile, ProviderDetails,
    ProviderListItem, ProviderSnapshot, ProviderSwitchPlan,
};
use crate::plugin::AppMode;
use crate::provider_config::Severity;
use crate::services::provider::{
    DriftConflict, LiveDrift, ProviderService, ProviderVersionResolution, SwitchResult,
};
use crate::{AppId, AppType, Provider};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSwitchPolicy {
    #[default]
    Abort,
    #[serde(alias = "preserve")]
    Save,
    #[serde(alias = "discard")]
    Revert,
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

    fn provider_list_state(
        &self,
        app: &AppId,
    ) -> ApplicationResult<(Option<AppType>, Option<String>)> {
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
        Ok((builtin, current))
    }

    pub fn list_providers(&self, app: &AppId) -> ApplicationResult<Vec<ProviderListItem>> {
        let (builtin, current) = self.provider_list_state(app)?;
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

    /// Load the local provider page in one batch instead of listing then fetching
    /// each record again. URL resolution precedes the usual secret redaction.
    pub fn provider_snapshot(&self, app: &AppId) -> ApplicationResult<ProviderSnapshot> {
        let (builtin, current) = self.provider_list_state(app)?;
        let mut providers = self
            .state
            .db
            .get_all_providers(app.as_str())?
            .into_values()
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| {
            left.sort_index
                .unwrap_or(usize::MAX)
                .cmp(&right.sort_index.unwrap_or(usize::MAX))
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut base_urls = std::collections::BTreeMap::new();
        for provider in &mut providers {
            let base_url = builtin
                .map(|app_type| provider.resolve_usage_base_url(&app_type))
                .unwrap_or_default();
            base_urls.insert(provider.id.clone(), base_url);
            redact_provider(app, provider);
        }
        Ok(ProviderSnapshot {
            providers,
            base_urls,
            current,
        })
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
            redact_provider(app, &mut provider);
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
        // The explicit application path commits files before publishing the new record.
        let builtin = AppType::from_app_id(app);
        let legacy_special = builtin == Some(AppType::CherryStudio)
            || (builtin == Some(AppType::OpenCode)
                && matches!(provider.category.as_deref(), Some("omo" | "omo-slim")));
        let should_apply = add_to_live
            && (plugin.mode() == AppMode::Additive
                || builtin.is_none()
                || self.state.db.get_current_provider(app.as_str())?.is_none());
        if should_apply && !legacy_special {
            self.apply_provider_candidate(
                app,
                None,
                provider.clone(),
                ProviderSwitchPolicy::Revert,
            )?;
            return self.get_provider(app, &provider.id, false);
        }
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

    pub fn save_provider_draft(
        &self,
        app: &AppId,
        original_id: &str,
        provider: Provider,
    ) -> ApplicationResult<ProviderDetails> {
        if AppType::from_app_id(app) != Some(AppType::Codex) || original_id != provider.id {
            return Err(ApplicationError::InvalidInput(
                "Draft updates require an unchanged Codex connection ID".into(),
            ));
        }
        let plugin = self.resolve_plugin(app)?;
        validate_provider_with_plugin(plugin.as_ref(), &provider)?;
        ProviderService::save_codex_draft(&self.state, provider.clone())?;
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
        let mut provider = provider;
        super::config_commits::restore_provider_input_secrets(
            AppType::from_app_id(app),
            plugin.as_ref(),
            &mut provider,
            &existing,
        );
        validate_provider_with_plugin(plugin.as_ref(), &provider)?;
        let builtin = AppType::from_app_id(app);
        let legacy_special = builtin == Some(AppType::CherryStudio);
        let current = if let Some(app_type) = builtin {
            crate::settings::get_effective_current_provider(&self.state.db, &app_type)?
        } else {
            self.state.db.get_current_provider(app.as_str())?
        };
        let omo = builtin == Some(AppType::OpenCode)
            && matches!(provider.category.as_deref(), Some("omo" | "omo-slim"));
        let managed = if let Some(builtin) = builtin {
            ProviderService::is_managed_in_live(&self.state, builtin, &existing)?
        } else if plugin.mode() == AppMode::Switch {
            current.as_deref() == Some(original_id)
        } else if omo {
            self.state.db.is_omo_provider_current(
                app.as_str(),
                original_id,
                provider.category.as_deref().unwrap_or_default(),
            )?
        } else {
            existing
                .meta
                .as_ref()
                .and_then(|m| m.live_config_managed)
                .unwrap_or(false)
        };
        if managed && !legacy_special {
            if let Some(builtin) = builtin.filter(|_| plugin.mode() == AppMode::Switch) {
                // Existing update callers expect unrelated live edits to survive.
                let (merged, _, _) = crate::services::provider::live::resolve_live_write(
                    &self.state.db,
                    &builtin,
                    Some(&existing),
                    &provider,
                )?;
                provider.settings_config = merged;
                crate::services::provider::live::normalize_provider_common_config_for_storage(
                    &self.state.db,
                    &builtin,
                    &mut provider,
                )?;
            }
            self.apply_provider_candidate(
                app,
                Some(&existing),
                provider.clone(),
                ProviderSwitchPolicy::Save,
            )?;
            return self.get_provider(app, &provider.id, false);
        }
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
            return self.switch_provider(app, id, ProviderSwitchPolicy::Save);
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
        if plugin.mode() == AppMode::Switch {
            return self.switch_provider(app, id, ProviderSwitchPolicy::Save);
        }
        self.apply_provider_candidate(
            app,
            Some(&provider),
            provider.clone(),
            ProviderSwitchPolicy::Save,
        )
    }

    pub fn preview_provider_switch(
        &self,
        app: &AppId,
        id: &str,
    ) -> ApplicationResult<ProviderSwitchPlan> {
        let _target = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.to_string(),
            })?;
        if let Some(app_type) = AppType::from_app_id(app) {
            let current = ProviderService::current(&self.state, app_type)?;
            let preview = ProviderService::preview_switch(&self.state, app_type, id)?;
            let diff_files = config_diff_files(app_type, &preview.stored, &preview.live);
            return Ok(ProviderSwitchPlan {
                app: app.to_string(),
                provider_id: id.to_string(),
                current_provider_id: (!current.is_empty()).then_some(current.clone()),
                config_path: crate::services::provider::drift::live_config_label(&app_type),
                would_change: current != id || !preview.drift.is_empty(),
                drift: preview.drift,
                diff_files,
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
                    live: live.clone(),
                    incoming: current_provider
                        .as_ref()
                        .map(|provider| provider.settings_config.clone())
                        .unwrap_or(Value::Null),
                }],
                ..Default::default()
            }
        } else {
            LiveDrift::default()
        };
        let config_path = plugin.config_dir()?.to_string_lossy().into_owned();
        let diff_files = current_provider
            .as_ref()
            .filter(|_| live_changed)
            .map(|provider| {
                vec![ConfigDiffFile {
                    path: config_path.clone(),
                    before: pretty_json(&provider.settings_config),
                    after: pretty_json(&live),
                }]
            })
            .unwrap_or_default();
        Ok(ProviderSwitchPlan {
            app: app.to_string(),
            provider_id: id.to_string(),
            current_provider_id: current.clone(),
            config_path,
            would_change: current.as_deref() != Some(id) || !drift.is_empty(),
            drift,
            diff_files,
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
        let provider = self
            .state
            .db
            .get_provider_by_id(id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: id.into(),
            })?;
        let builtin = AppType::from_app_id(app);
        let legacy_special = builtin == Some(AppType::CherryStudio);
        if !legacy_special {
            let mut target = provider.clone();
            if policy == ProviderSwitchPolicy::Save
                && plan.current_provider_id.as_deref() == Some(id)
                && !plan.drift.is_empty()
            {
                target.settings_config = if let Some(builtin) = builtin {
                    crate::services::provider::live::read_live_settings(builtin)?
                } else {
                    self.resolve_plugin(app)?.live().read_live()?
                };
                if let Some(builtin) = builtin {
                    crate::services::provider::live::normalize_provider_common_config_for_storage(
                        &self.state.db,
                        &builtin,
                        &mut target,
                    )?;
                }
            }
            let mut result =
                self.apply_stored_provider_candidate(app, &provider, target, policy)?;
            result.drift = (!plan.drift.is_empty()).then_some(plan.drift);
            return Ok(result);
        }
        if let Some(app_type) = AppType::from_app_id(app) {
            let resolution = match policy {
                ProviderSwitchPolicy::Abort | ProviderSwitchPolicy::Save => {
                    ProviderVersionResolution::Save
                }
                ProviderSwitchPolicy::Revert => ProviderVersionResolution::Revert,
            };
            return Ok(ProviderService::switch_with(
                &self.state,
                app_type,
                id,
                resolution,
            )?);
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
        if policy == ProviderSwitchPolicy::Save
            && !plan.drift.is_empty()
            && let Some(current_id) = plan.current_provider_id.as_deref()
            && let Some(mut current) = self.state.db.get_provider_by_id(current_id, app.as_str())?
        {
            current.settings_config = plugin.live().read_live()?;
            self.state.db.save_provider(app.as_str(), &current)?;
            if current.id == provider.id {
                provider = current;
            }
        }
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

fn redact_provider(app: &AppId, provider: &mut Provider) {
    provider.settings_config = redact_json(&provider.settings_config);
    if let Some(codec) = crate::plugin::get_plugin(app).and_then(|plugin| plugin.provider_config())
    {
        for path in codec.secret_paths() {
            let pointer = path
                .iter()
                .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
                .collect::<String>();
            if let Some(value) = provider.settings_config.pointer_mut(&pointer)
                && !value.is_null()
            {
                *value = Value::String("******".into());
            }
        }
    }
    if let Some(meta) = provider.meta.as_mut()
        && let Some(script) = meta.usage_script.as_mut()
    {
        script.api_key = script.api_key.as_ref().map(|_| "******".to_string());
        script.access_token = script.access_token.as_ref().map(|_| "******".to_string());
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn pretty_toml_value(value: &Value) -> String {
    toml::to_string_pretty(value).unwrap_or_else(|_| pretty_json(value))
}

fn config_diff_files(app_type: AppType, stored: &Value, live: &Value) -> Vec<ConfigDiffFile> {
    if stored.is_null() || live.is_null() || stored == live {
        return Vec::new();
    }

    let mut files = Vec::new();
    let mut push = |path: std::path::PathBuf, before: String, after: String| {
        if before != after {
            files.push(ConfigDiffFile {
                path: crate::paths::abbreviate_home(&path),
                before,
                after,
            });
        }
    };

    match app_type {
        AppType::Codex => {
            let auth_text = |value: &Value| {
                value
                    .get("authRaw")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| pretty_json(value.get("auth").unwrap_or(&Value::Null)))
            };
            push(
                crate::apps::codex::get_codex_auth_path(),
                auth_text(stored),
                auth_text(live),
            );
            push(
                crate::apps::codex::get_codex_config_path(),
                stored
                    .get("config")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                live.get("config")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        AppType::GrokBuild => push(
            crate::apps::grokbuild::get_grok_config_path(),
            stored
                .get("config")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            live.get("config")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        AppType::KimiCode => push(
            crate::apps::kimi_code::get_kimi_code_config_path(),
            pretty_toml_value(stored),
            pretty_toml_value(live),
        ),
        _ => push(
            crate::paths::get_claude_settings_path(),
            pretty_json(stored),
            pretty_json(live),
        ),
    }
    files
}

pub(crate) fn empty_live_config(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.values().all(empty_live_config),
        serde_json::Value::Array(items) => items.is_empty(),
        serde_json::Value::String(value) => value.trim().is_empty(),
        _ => false,
    }
}

pub(crate) fn validate_provider_with_plugin(
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

pub(crate) fn is_secret_key(key: &str) -> bool {
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
    fn provider_snapshot_preserves_order_current_urls_and_redacts_without_mutating_storage() {
        use crate::application::Application;
        use crate::{AppId, AppState, Database, Provider};
        use serde_json::json;
        use std::sync::Arc;

        let _home = crate::test_support::TestHome::new();
        let db = Arc::new(Database::memory().unwrap());
        for (id, name, sort) in [
            ("z", "Zulu", Some(0)),
            ("a", "Alpha", Some(0)),
            ("n", "No sort", None),
        ] {
            let mut provider = Provider::with_id(
                id.into(),
                name.into(),
                json!({
                    "env": {"ANTHROPIC_AUTH_TOKEN":"fixture-secret", "ANTHROPIC_BASE_URL":"https://example.invalid"}
                }),
                None,
            );
            provider.sort_index = sort;
            provider.meta = Some(crate::ProviderMeta {
                usage_script: Some(serde_json::from_value(json!({
                    "enabled":false, "language":"javascript", "code":"", "apiKey":"fixture-script-key", "accessToken":"fixture-access-token"
                })).unwrap()),
                ..Default::default()
            });
            db.save_provider("claude", &provider).unwrap();
        }
        db.set_current_provider("claude", "z").unwrap();
        crate::settings::set_current_provider(&crate::AppType::Claude, Some("a")).unwrap();
        let application = Application::from_state(Arc::new(AppState::new(db.clone())));
        let app = AppId::parse("claude").unwrap();
        let snapshot = application.provider_snapshot(&app).unwrap();
        let listed = application.list_providers(&app).unwrap();
        assert_eq!(
            snapshot
                .providers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "z", "n"]
        );
        assert_eq!(
            snapshot.providers.iter().map(|p| &p.id).collect::<Vec<_>>(),
            listed.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
        assert_eq!(snapshot.current.as_deref(), Some("a"));
        assert_eq!(snapshot.base_urls["a"], "https://example.invalid");
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for secret in [
            "fixture-secret",
            "fixture-script-key",
            "fixture-access-token",
        ] {
            assert!(!serialized.contains(secret));
        }
        assert_eq!(
            snapshot.providers[0].settings_config["env"]["ANTHROPIC_AUTH_TOKEN"],
            "******"
        );
        assert_eq!(
            application
                .get_provider(&app, "a", true)
                .unwrap()
                .provider
                .settings_config["env"]["ANTHROPIC_AUTH_TOKEN"],
            "fixture-secret"
        );
        assert_eq!(
            db.get_provider_by_id("a", "claude")
                .unwrap()
                .unwrap()
                .meta
                .unwrap()
                .usage_script
                .unwrap()
                .api_key
                .as_deref(),
            Some("fixture-script-key")
        );

        // A stale local selection must still be cleared and fall back to the DB.
        crate::settings::set_current_provider(&crate::AppType::Claude, Some("missing")).unwrap();
        assert_eq!(
            application
                .provider_snapshot(&app)
                .unwrap()
                .current
                .as_deref(),
            Some("z")
        );
        assert_eq!(
            crate::settings::get_current_provider(&crate::AppType::Claude),
            None
        );
        let empty = application
            .provider_snapshot(&AppId::parse("opencode").unwrap())
            .unwrap();
        assert!(
            empty.providers.is_empty() && empty.base_urls.is_empty() && empty.current.is_none()
        );
    }

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
