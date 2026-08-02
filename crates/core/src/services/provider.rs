//! Provider service module
//!
//! Handles provider CRUD operations, switching, and configuration management.
//! Ported from cc-switch `services/provider/mod.rs`.

pub mod drift;
mod endpoints;
pub(crate) mod live;
mod usage;

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::db::{validate_cost_multiplier, validate_pricing_source};
use crate::error::AppError;
use crate::model::{Provider, UsageResult};
use crate::settings::CustomEndpoint;

// Re-export sub-module functions for external access
pub use live::{
    auto_import_live_providers, import_default_config, import_hermes_providers_from_live,
    import_openclaw_providers_from_live, import_opencode_providers_from_live, read_live_settings,
    should_auto_import_default_config, sync_current_to_live,
};

// Internal re-exports (pub(crate)). These mirror the cc-switch re-export seams;
// some are not yet called inside ochub-core but are kept as the public-within-crate
// surface for callers ported in later phases.
#[allow(unused_imports)]
pub(crate) use live::sanitize_claude_settings_for_live;
#[allow(unused_imports)]
pub(crate) use live::{
    build_effective_settings_with_common_config, normalize_provider_common_config_for_storage,
    provider_exists_in_live_config, write_live_preserving_user_edits,
    write_live_with_common_config,
};

pub use drift::{DriftConflict, LiveDrift, LiveSnapshot};
pub use live::DriftResolution;

// Internal re-exports
use live::{
    remove_hermes_provider_from_live, remove_openclaw_provider_from_live,
    remove_opencode_provider_from_live, sync_current_provider_for_app_to_live,
};
use usage::validate_usage_script;

/// 统一会话开关变更后，立即按新开关状态重写当前官方 Codex 供应商的
/// live 配置，使开关即时生效（无需等下一次切换）。
pub fn reapply_current_codex_official_live(state: &AppState) -> Result<bool, AppError> {
    let current_id = ProviderService::current(state, AppType::Codex)?;
    if current_id.is_empty() {
        return Ok(false);
    }
    let providers = state.db.get_all_providers(AppType::Codex.as_str())?;
    let Some(provider) = providers.get(&current_id) else {
        return Ok(false);
    };
    if provider.category.as_deref() != Some("official") {
        return Ok(false);
    }

    live::write_live_with_common_config(&state.db, &AppType::Codex, provider)?;
    Ok(true)
}

/// Provider business logic service
pub struct ProviderService;

/// Result of a provider switch operation, including any non-fatal warnings
#[derive(Debug, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchResult {
    pub warnings: Vec<String>,
    /// What was changed in the live config outside OcHub, if anything. Present
    /// so the caller can show it; the switch itself has already resolved it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift: Option<LiveDrift>,
}

impl ProviderService {
    fn normalize_provider_if_claude(app_type: &AppType, provider: &mut Provider) {
        if matches!(app_type, AppType::Claude) {
            let mut v = provider.settings_config.clone();
            if normalize_claude_models_in_value(&mut v) {
                provider.settings_config = v;
            }
        }
    }

    /// Check whether a provider exists in live config, tolerating parse errors
    /// only for providers that are explicitly marked as DB-only.
    fn check_live_config_exists(
        app_type: &AppType,
        provider_id: &str,
        live_config_managed: Option<bool>,
    ) -> Result<bool, AppError> {
        if live_config_managed == Some(false) {
            Ok(provider_exists_in_live_config(app_type, provider_id).unwrap_or(false))
        } else {
            provider_exists_in_live_config(app_type, provider_id)
        }
    }

    /// Store account material the tool refreshed on its own back onto the
    /// provider being switched away from.
    ///
    /// Codex rewrites `auth.json` when its OAuth token is refreshed. That token
    /// belongs to the account that was active, so unlike the rest of the file it
    /// cannot be carried onto the next provider — it has to be captured here or
    /// it is lost. Only a provider that actually owns the live `auth.json` may
    /// claim it; a config-only provider is merely borrowing whichever account
    /// happens to be logged in.
    fn capture_outgoing_account_state(
        state: &AppState,
        app_type: &AppType,
        outgoing: &Provider,
    ) -> Result<(), AppError> {
        if !matches!(app_type, AppType::Codex) {
            return Ok(());
        }

        let Ok(live) = read_live_settings(*app_type) else {
            return Ok(());
        };
        let Some(live_auth) = live.get("auth") else {
            return Ok(());
        };

        let stored_auth = outgoing.settings_config.get("auth");
        if stored_auth == Some(live_auth) {
            return Ok(());
        }

        let owns_live_auth = crate::apps::codex::codex_provider_owns_live_auth(
            outgoing.category.as_deref(),
            stored_auth.unwrap_or(&Value::Null),
            outgoing
                .settings_config
                .get("config")
                .and_then(Value::as_str)
                .unwrap_or(""),
            crate::settings::preserve_codex_official_auth_on_switch(),
        );
        if !owns_live_auth {
            return Ok(());
        }

        let mut captured = live_auth.clone();

        // The writer strips `OPENAI_API_KEY` out of a non-official provider's
        // live `auth.json` and projects it into the config's bearer token, so
        // taking the live object verbatim would delete the stored key.
        if let (Some(captured), Some(stored_key)) = (
            captured.as_object_mut(),
            stored_auth.and_then(crate::apps::codex::extract_codex_auth_api_key),
        ) {
            captured
                .entry("OPENAI_API_KEY".to_string())
                .or_insert(Value::String(stored_key));
        }

        let mut updated = outgoing.clone();
        let Some(settings) = updated.settings_config.as_object_mut() else {
            return Ok(());
        };
        settings.insert("auth".to_string(), captured);
        state.db.save_provider(app_type.as_str(), &updated)
    }

    fn provider_live_config_managed(provider: &Provider) -> Option<bool> {
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.live_config_managed)
    }

    fn set_provider_live_config_managed(provider: &mut Provider, managed: bool) {
        provider
            .meta
            .get_or_insert_with(Default::default)
            .live_config_managed = Some(managed);
    }

    /// List all providers for an app type
    pub fn list(
        state: &AppState,
        app_type: AppType,
    ) -> Result<IndexMap<String, Provider>, AppError> {
        state.db.get_all_providers(app_type.as_str())
    }

    /// Get current provider ID
    ///
    /// 对于累加模式应用（OpenCode, OpenClaw），不存在"当前供应商"概念，直接返回空字符串。
    pub fn current(state: &AppState, app_type: AppType) -> Result<String, AppError> {
        // Additive mode apps have no "current" provider concept
        if app_type.is_additive_mode() {
            return Ok(String::new());
        }
        crate::settings::get_effective_current_provider(&state.db, &app_type)
            .map(|opt| opt.unwrap_or_default())
    }

    /// Add a new provider
    pub fn add(
        state: &AppState,
        app_type: AppType,
        provider: Provider,
        add_to_live: bool,
    ) -> Result<bool, AppError> {
        let mut provider = provider;
        // Normalize Claude model keys
        Self::normalize_provider_if_claude(&app_type, &mut provider);
        Self::validate_provider_settings(&app_type, &provider)?;
        normalize_provider_common_config_for_storage(state.db.as_ref(), &app_type, &mut provider)?;
        if app_type.is_additive_mode() {
            Self::set_provider_live_config_managed(&mut provider, add_to_live);
        }

        // Save to database
        state.db.save_provider(app_type.as_str(), &provider)?;

        // Additive mode apps (OpenCode, OpenClaw): optionally write to live config.
        if app_type.is_additive_mode() {
            // OMO / OMO Slim providers use exclusive mode and write to dedicated config file.
            if matches!(app_type, AppType::OpenCode)
                && matches!(provider.category.as_deref(), Some("omo") | Some("omo-slim"))
            {
                // Do not auto-enable newly added OMO / OMO Slim providers.
                return Ok(true);
            }
            if !add_to_live {
                return Ok(true);
            }
            write_live_with_common_config(state.db.as_ref(), &app_type, &provider)?;
            return Ok(true);
        }

        // For other apps: Check if sync is needed
        let current = state.db.get_current_provider(app_type.as_str())?;
        if current.is_none() {
            // No current provider, set as current and sync
            state
                .db
                .set_current_provider(app_type.as_str(), &provider.id)?;
            write_live_with_common_config(state.db.as_ref(), &app_type, &provider)?;
        }

        Ok(true)
    }

    /// Duplicate a provider: same configuration, `-copy` suffixed name, fresh id.
    ///
    /// The copy is a draft. It never reaches the live config and never becomes
    /// the current provider, so the button cannot change which provider a tool
    /// is talking to. It inherits the source's `sort_index` and gets a newer
    /// `created_at`, which is what makes the list — ordered by `sort_index`,
    /// then `created_at` — render it directly below its source.
    pub fn duplicate(state: &AppState, app_type: AppType, id: &str) -> Result<Provider, AppError> {
        let providers = state.db.get_all_providers(app_type.as_str())?;
        let source = providers.get(id).ok_or_else(|| {
            AppError::Message(format!(
                "供应商「{}」在应用「{}」中不存在",
                id,
                app_type.as_str()
            ))
        })?;

        let mut copy = source.clone();
        copy.name = next_copy_label(&source.name, |candidate| {
            providers.values().any(|other| other.name == candidate)
        });
        // In additive mode the id is the provider key written into the tool's
        // own config, so the copy needs a readable key of its own; elsewhere the
        // id is opaque and a fresh uuid is all it has to be.
        copy.id = if app_type.is_additive_mode() {
            next_copy_label(&source.id, |candidate| providers.contains_key(candidate))
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        copy.created_at = Some(chrono::Utc::now().timestamp_millis());
        if app_type.is_additive_mode() {
            Self::set_provider_live_config_managed(&mut copy, false);
        }

        state.db.save_provider(app_type.as_str(), &copy)?;
        Ok(copy)
    }

    /// Update a provider
    pub fn update(
        state: &AppState,
        app_type: AppType,
        original_id: Option<&str>,
        provider: Provider,
    ) -> Result<bool, AppError> {
        let mut provider = provider;
        let original_id = original_id.unwrap_or(provider.id.as_str()).to_string();
        let provider_id_changed = original_id != provider.id;
        let existing_provider = state
            .db
            .get_provider_by_id(&original_id, app_type.as_str())?;
        // Normalize Claude model keys
        Self::normalize_provider_if_claude(&app_type, &mut provider);
        Self::validate_provider_settings(&app_type, &provider)?;
        normalize_provider_common_config_for_storage(state.db.as_ref(), &app_type, &mut provider)?;

        if provider_id_changed {
            if !app_type.is_additive_mode() {
                return Err(AppError::Message(
                    "Only additive-mode providers support changing provider key".to_string(),
                ));
            }

            let Some(existing_provider) = existing_provider else {
                return Err(AppError::Message(format!(
                    "Original provider '{}' does not exist in app '{}'",
                    original_id,
                    app_type.as_str()
                )));
            };

            if matches!(app_type, AppType::OpenCode)
                && matches!(
                    existing_provider.category.as_deref(),
                    Some("omo") | Some("omo-slim")
                )
            {
                return Err(AppError::Message(
                    "Provider key cannot be changed for OMO/OMO Slim providers".to_string(),
                ));
            }

            let original_in_live = Self::check_live_config_exists(
                &app_type,
                &original_id,
                Self::provider_live_config_managed(&existing_provider),
            )?;
            if original_in_live {
                return Err(AppError::Message(
                    "Provider key cannot be changed after the provider has been added to the app config"
                        .to_string(),
                ));
            }

            let next_id_in_live = Self::check_live_config_exists(
                &app_type,
                &provider.id,
                Self::provider_live_config_managed(&existing_provider),
            )?;
            if state
                .db
                .get_provider_by_id(&provider.id, app_type.as_str())?
                .is_some()
                || next_id_in_live
            {
                return Err(AppError::Message(format!(
                    "Provider '{}' already exists in app '{}'",
                    provider.id,
                    app_type.as_str()
                )));
            }

            Self::set_provider_live_config_managed(&mut provider, false);
            state.db.save_provider(app_type.as_str(), &provider)?;
            state.db.delete_provider(app_type.as_str(), &original_id)?;

            if crate::settings::get_current_provider(&app_type).as_deref() == Some(&original_id) {
                crate::settings::set_current_provider(&app_type, Some(provider.id.as_str()))?;
            }

            return Ok(true);
        }

        // Additive mode apps (OpenCode, OpenClaw): only sync to live when the provider
        // already exists in live config.
        if app_type.is_additive_mode() {
            let omo_variant = if matches!(app_type, AppType::OpenCode) {
                match provider.category.as_deref() {
                    Some("omo") => Some(&crate::services::omo::STANDARD),
                    Some("omo-slim") => Some(&crate::services::omo::SLIM),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(variant) = omo_variant {
                let is_current = state.db.is_omo_provider_current(
                    app_type.as_str(),
                    &provider.id,
                    variant.category,
                )?;
                if is_current {
                    crate::services::OmoService::write_provider_config_to_file(&provider, variant)?;
                }
                if let Err(err) = state.db.save_provider(app_type.as_str(), &provider) {
                    if is_current
                        && let Err(rollback_err) =
                            crate::services::OmoService::write_config_to_file(state, variant)
                    {
                        log::warn!(
                            "Failed to roll back {} config after DB save error: {}",
                            variant.label,
                            rollback_err
                        );
                    }
                    return Err(err);
                }
                return Ok(true);
            }
            let live_config_managed = Self::check_live_config_exists(
                &app_type,
                &provider.id,
                Self::provider_live_config_managed(&provider).or_else(|| {
                    existing_provider
                        .as_ref()
                        .and_then(Self::provider_live_config_managed)
                }),
            )?;
            Self::set_provider_live_config_managed(&mut provider, live_config_managed);

            state.db.save_provider(app_type.as_str(), &provider)?;

            if !live_config_managed {
                return Ok(true);
            }
            write_live_with_common_config(state.db.as_ref(), &app_type, &provider)?;
            return Ok(true);
        }

        // Save to database
        state.db.save_provider(app_type.as_str(), &provider)?;

        // For other apps: Check if this is current provider (use effective current, not just DB)
        let effective_current =
            crate::settings::get_effective_current_provider(&state.db, &app_type)?;
        let is_current = effective_current.as_deref() == Some(provider.id.as_str());

        if is_current {
            // Saving an edit here used to overwrite the file outright, which is
            // how a hand-edited `settings.json` disappeared without the switch
            // flow ever being involved.
            let drift = write_live_preserving_user_edits(
                state.db.as_ref(),
                &app_type,
                existing_provider.as_ref(),
                &provider,
            )?;
            live::log_drift(&app_type, &provider, &drift);
            crate::services::mcp::McpService::sync_all_enabled(state)?;
        }

        Ok(true)
    }

    /// Delete a provider
    pub fn delete(state: &AppState, app_type: AppType, id: &str) -> Result<(), AppError> {
        // Additive mode apps - no current provider concept
        if app_type.is_additive_mode() {
            let existing = state.db.get_provider_by_id(id, app_type.as_str())?;

            if matches!(app_type, AppType::OpenCode) {
                let provider_category = existing.as_ref().and_then(|p| p.category.clone());
                let omo_variant = match provider_category.as_deref() {
                    Some("omo") => Some(&crate::services::omo::STANDARD),
                    Some("omo-slim") => Some(&crate::services::omo::SLIM),
                    _ => None,
                };
                if let Some(variant) = omo_variant {
                    let was_current = state.db.is_omo_provider_current(
                        app_type.as_str(),
                        id,
                        variant.category,
                    )?;
                    state.db.delete_provider(app_type.as_str(), id)?;
                    if was_current {
                        crate::services::OmoService::delete_config_file(variant)?;
                    }
                    return Ok(());
                }
            }

            let live_managed = existing
                .as_ref()
                .and_then(Self::provider_live_config_managed);
            if Self::check_live_config_exists(&app_type, id, live_managed)? {
                match app_type {
                    AppType::OpenCode => remove_opencode_provider_from_live(id)?,
                    AppType::OpenClaw => remove_openclaw_provider_from_live(id)?,
                    AppType::Hermes => remove_hermes_provider_from_live(id)?,
                    _ => {}
                }
            }
            state.db.delete_provider(app_type.as_str(), id)?;
            return Ok(());
        }

        // For other apps: Check both local settings and database
        let local_current = crate::settings::get_current_provider(&app_type);
        let db_current = state.db.get_current_provider(app_type.as_str())?;

        if local_current.as_deref() == Some(id) || db_current.as_deref() == Some(id) {
            return Err(AppError::Message(
                "无法删除当前正在使用的供应商".to_string(),
            ));
        }

        state.db.delete_provider(app_type.as_str(), id)
    }

    /// Remove provider from live config only (for additive mode apps like OpenCode, OpenClaw)
    pub fn remove_from_live_config(
        state: &AppState,
        app_type: AppType,
        id: &str,
    ) -> Result<(), AppError> {
        match app_type {
            AppType::OpenCode => {
                let provider_category = state
                    .db
                    .get_provider_by_id(id, app_type.as_str())?
                    .and_then(|p| p.category);

                let omo_variant = match provider_category.as_deref() {
                    Some("omo") => Some(&crate::services::omo::STANDARD),
                    Some("omo-slim") => Some(&crate::services::omo::SLIM),
                    _ => None,
                };
                if let Some(variant) = omo_variant {
                    state
                        .db
                        .clear_omo_provider_current(app_type.as_str(), id, variant.category)?;
                    let still_has_current = state
                        .db
                        .get_current_omo_provider("opencode", variant.category)?
                        .is_some();
                    if still_has_current {
                        crate::services::OmoService::write_config_to_file(state, variant)?;
                    } else {
                        crate::services::OmoService::delete_config_file(variant)?;
                    }
                } else {
                    remove_opencode_provider_from_live(id)?;
                }
            }
            AppType::OpenClaw => {
                remove_openclaw_provider_from_live(id)?;
            }
            AppType::Hermes => {
                remove_hermes_provider_from_live(id)?;
            }
            _ => {
                return Err(AppError::Message(format!(
                    "App {} does not support remove from live config",
                    app_type.as_str()
                )));
            }
        }

        if let Some(mut provider) = state.db.get_provider_by_id(id, app_type.as_str())? {
            Self::set_provider_live_config_managed(&mut provider, false);
            state.db.save_provider(app_type.as_str(), &provider)?;
        }

        Ok(())
    }

    /// What a switch would change on disk, without changing it.
    ///
    /// The UI asks this first so an external edit can be shown to the user
    /// *before* their file is touched, rather than reported afterwards.
    pub fn preview_switch(
        state: &AppState,
        app_type: AppType,
        id: &str,
    ) -> Result<LiveDrift, AppError> {
        crate::plugin::registry::ensure_app_type_enabled(&app_type)?;

        let providers = state.db.get_all_providers(app_type.as_str())?;
        let Some(provider) = providers.get(id) else {
            return Ok(LiveDrift::default());
        };
        if app_type.is_additive_mode() {
            return Ok(LiveDrift::default());
        }

        let outgoing = crate::settings::get_effective_current_provider(&state.db, &app_type)?
            .and_then(|current_id| providers.get(&current_id).cloned());

        live::preview_live_drift(state.db.as_ref(), &app_type, outgoing.as_ref(), provider)
    }

    /// Switch to a provider, keeping any edit made outside OcHub.
    pub fn switch(state: &AppState, app_type: AppType, id: &str) -> Result<SwitchResult, AppError> {
        Self::switch_with(state, app_type, id, DriftResolution::Preserve)
    }

    /// Switch to a provider, resolving an external edit as the caller decided.
    pub fn switch_with(
        state: &AppState,
        app_type: AppType,
        id: &str,
        resolution: DriftResolution,
    ) -> Result<SwitchResult, AppError> {
        crate::plugin::registry::ensure_app_type_enabled(&app_type)?;

        // Check if provider exists
        let providers = state.db.get_all_providers(app_type.as_str())?;
        providers
            .get(id)
            .ok_or_else(|| AppError::Message(format!("供应商 {id} 不存在")))?;

        Self::switch_normal(state, app_type, id, &providers, resolution)
    }

    /// Switch flow with a live-config write.
    fn switch_normal(
        state: &AppState,
        app_type: AppType,
        id: &str,
        providers: &indexmap::IndexMap<String, Provider>,
        resolution: DriftResolution,
    ) -> Result<SwitchResult, AppError> {
        let provider = providers
            .get(id)
            .ok_or_else(|| AppError::Message(format!("供应商 {id} 不存在")))?;

        // OMO ↔ OMO Slim are mutually exclusive.
        if matches!(app_type, AppType::OpenCode) {
            let omo_pair = match provider.category.as_deref() {
                Some("omo") => Some((&crate::services::omo::STANDARD, &crate::services::omo::SLIM)),
                Some("omo-slim") => {
                    Some((&crate::services::omo::SLIM, &crate::services::omo::STANDARD))
                }
                _ => None,
            };
            if let Some((enable, disable)) = omo_pair {
                state
                    .db
                    .set_omo_provider_current(app_type.as_str(), id, enable.category)?;
                crate::services::OmoService::write_config_to_file(state, enable)?;
                let _ = crate::services::OmoService::delete_config_file(disable);
                return Ok(SwitchResult::default());
            }
        }

        let mut result = SwitchResult::default();

        let current_id = crate::settings::get_effective_current_provider(&state.db, &app_type)?;
        let outgoing = current_id
            .as_deref()
            .and_then(|current_id| providers.get(current_id));

        // The live file is no longer read back wholesale into the provider we
        // are leaving — an edit made outside OcHub is a property of the file,
        // not of that provider, and `write_live_preserving_user_edits` carries
        // it forward instead. Account state is the exception: it belongs to the
        // account that was active and cannot follow the switch.
        if let (Some(current_id), Some(outgoing)) = (current_id.as_deref(), outgoing)
            && current_id != id
            && !app_type.is_additive_mode()
            && let Err(e) = Self::capture_outgoing_account_state(state, &app_type, outgoing)
        {
            log::warn!("Backfill failed: {e}");
            result
                .warnings
                .push(format!("backfill_failed:{current_id}"));
        }

        // Additive mode apps skip setting is_current (no such concept)
        if !app_type.is_additive_mode() {
            // Update local settings (device-level, takes priority)
            crate::settings::set_current_provider(&app_type, Some(id))?;

            // Update database is_current (as default for new devices)
            state.db.set_current_provider(app_type.as_str(), id)?;
        }

        // Sync to live, resolving any external edit the way the caller decided.
        let drift = live::write_live_resolving_drift(
            state.db.as_ref(),
            &app_type,
            outgoing,
            provider,
            resolution,
        )?;
        live::log_drift(&app_type, provider, &drift);
        if !drift.is_empty() {
            result.drift = Some(drift);
        }

        // Hermes is additive: update top-level `model:` section to point at this provider.
        if matches!(app_type, AppType::Hermes)
            && let Err(e) =
                crate::apps::hermes::apply_switch_defaults(&provider.id, &provider.settings_config)
        {
            log::warn!(
                "Failed to update Hermes model defaults after switching to '{}': {e}",
                provider.id
            );
            result
                .warnings
                .push(format!("hermes_model_defaults_failed:{}", provider.id));
        }

        // For additive-mode providers that were DB-only, flip the live_config_managed flag.
        if app_type.is_additive_mode() && Self::provider_live_config_managed(provider) != Some(true)
        {
            let mut updated = provider.clone();
            Self::set_provider_live_config_managed(&mut updated, true);
            if let Err(e) = state.db.save_provider(app_type.as_str(), &updated) {
                let rollback_result = match app_type {
                    AppType::OpenCode => remove_opencode_provider_from_live(&provider.id),
                    AppType::OpenClaw => remove_openclaw_provider_from_live(&provider.id),
                    AppType::Hermes => remove_hermes_provider_from_live(&provider.id),
                    _ => Ok(()),
                };

                match rollback_result {
                    Ok(()) => {
                        return Err(AppError::Message(format!(
                            "Failed to persist live_config_managed for '{}' after writing live config; live changes were rolled back: {e}",
                            provider.id
                        )));
                    }
                    Err(rollback_err) => {
                        return Err(AppError::Message(format!(
                            "Failed to persist live_config_managed for '{}' after writing live config: {e}; additionally failed to roll back live config: {rollback_err}",
                            provider.id
                        )));
                    }
                }
            }
        }

        // Sync MCP
        crate::services::mcp::McpService::sync_all_enabled(state)?;

        Ok(result)
    }

    /// Sync current provider to live configuration (re-export)
    pub fn sync_current_to_live(state: &AppState) -> Result<(), AppError> {
        sync_current_to_live(state)
    }

    pub fn sync_current_provider_for_app(
        state: &AppState,
        app_type: AppType,
    ) -> Result<(), AppError> {
        sync_current_provider_for_app_to_live(state, &app_type)
    }

    pub fn migrate_legacy_common_config_usage(
        state: &AppState,
        app_type: AppType,
        legacy_snippet: &str,
    ) -> Result<(), AppError> {
        if app_type.is_additive_mode() || legacy_snippet.trim().is_empty() {
            return Ok(());
        }

        let providers = state.db.get_all_providers(app_type.as_str())?;

        for provider in providers.values() {
            if provider
                .meta
                .as_ref()
                .and_then(|meta| meta.common_config_enabled)
                .is_some()
            {
                continue;
            }

            if !live::provider_uses_common_config(&app_type, provider, Some(legacy_snippet)) {
                continue;
            }

            let mut updated_provider = provider.clone();
            updated_provider
                .meta
                .get_or_insert_with(Default::default)
                .common_config_enabled = Some(true);

            match live::remove_common_config_from_settings(
                &app_type,
                &updated_provider.settings_config,
                legacy_snippet,
            ) {
                Ok(settings) => updated_provider.settings_config = settings,
                Err(err) => {
                    log::warn!(
                        "Failed to normalize legacy common config for {} provider '{}': {err}",
                        app_type.as_str(),
                        updated_provider.id
                    );
                }
            }

            state
                .db
                .save_provider(app_type.as_str(), &updated_provider)?;
        }

        Ok(())
    }

    pub fn migrate_legacy_common_config_usage_if_needed(
        state: &AppState,
        app_type: AppType,
    ) -> Result<(), AppError> {
        if app_type.is_additive_mode() {
            return Ok(());
        }

        let Some(snippet) = state.db.get_config_snippet(app_type.as_str())? else {
            return Ok(());
        };

        if snippet.trim().is_empty() {
            return Ok(());
        }

        Self::migrate_legacy_common_config_usage(state, app_type, &snippet)
    }

    /// Extract common config snippet from current provider
    pub fn extract_common_config_snippet(
        state: &AppState,
        app_type: AppType,
    ) -> Result<String, AppError> {
        // Get current provider
        let current_id = Self::current(state, app_type)?;
        if current_id.is_empty() {
            return Err(AppError::Message("No current provider".to_string()));
        }

        let providers = state.db.get_all_providers(app_type.as_str())?;
        let provider = providers
            .get(&current_id)
            .ok_or_else(|| AppError::Message(format!("Provider {current_id} not found")))?;

        match app_type {
            AppType::Claude => Self::extract_claude_common_config(&provider.settings_config),
            AppType::ClaudeDesktop => Ok(String::new()),
            AppType::CherryStudio => Ok(String::new()),
            AppType::Codex => Self::extract_codex_common_config(&provider.settings_config),
            AppType::GrokBuild => Ok(String::new()),
            AppType::KimiCode => Ok(String::new()),
            AppType::OpenCode => Self::extract_opencode_common_config(&provider.settings_config),
            AppType::OpenClaw => Self::extract_openclaw_common_config(&provider.settings_config),
            AppType::Hermes => Ok(String::new()), // Hermes doesn't use common config snippets
        }
    }

    /// Extract common config snippet from a config value (e.g. editor content).
    pub fn extract_common_config_snippet_from_settings(
        app_type: AppType,
        settings_config: &Value,
    ) -> Result<String, AppError> {
        match app_type {
            AppType::Claude => Self::extract_claude_common_config(settings_config),
            AppType::ClaudeDesktop => Ok(String::new()),
            AppType::CherryStudio => Ok(String::new()),
            AppType::Codex => Self::extract_codex_common_config(settings_config),
            AppType::GrokBuild => Ok(String::new()),
            AppType::KimiCode => Ok(String::new()),
            AppType::OpenCode => Self::extract_opencode_common_config(settings_config),
            AppType::OpenClaw => Self::extract_openclaw_common_config(settings_config),
            AppType::Hermes => Ok(String::new()),
        }
    }

    /// Extract common config for Claude (JSON format)
    fn extract_claude_common_config(settings: &Value) -> Result<String, AppError> {
        let mut config = settings.clone();

        const ENV_EXCLUDES: &[&str] = &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_REASONING_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            "ANTHROPIC_BASE_URL",
        ];

        const TOP_LEVEL_EXCLUDES: &[&str] = &["apiBaseUrl", "primaryModel", "smallFastModel"];

        if let Some(env) = config.get_mut("env").and_then(|v| v.as_object_mut()) {
            for key in ENV_EXCLUDES {
                env.remove(*key);
            }
            if env.is_empty() {
                config.as_object_mut().map(|obj| obj.remove("env"));
            }
        }

        if let Some(obj) = config.as_object_mut() {
            for key in TOP_LEVEL_EXCLUDES {
                obj.remove(*key);
            }
        }

        if config.as_object().is_none_or(|obj| obj.is_empty()) {
            return Ok("{}".to_string());
        }

        serde_json::to_string_pretty(&config)
            .map_err(|e| AppError::Message(format!("Serialization failed: {e}")))
    }

    /// Extract common config for Codex (TOML format)
    fn extract_codex_common_config(settings: &Value) -> Result<String, AppError> {
        let config_toml = settings
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if config_toml.is_empty() {
            return Ok(String::new());
        }

        let mut doc = config_toml
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| AppError::Message(format!("TOML parse error: {e}")))?;

        let root = doc.as_table_mut();
        root.remove("model");
        root.remove("model_provider");
        root.remove("base_url");
        root.remove("model_providers");

        let mut cleaned = String::new();
        let mut blank_run = 0usize;
        for line in doc.to_string().lines() {
            if line.trim().is_empty() {
                blank_run += 1;
                if blank_run <= 1 {
                    cleaned.push('\n');
                }
                continue;
            }
            blank_run = 0;
            cleaned.push_str(line);
            cleaned.push('\n');
        }

        Ok(cleaned.trim().to_string())
    }

    /// Extract common config for OpenCode (JSON format)
    fn extract_opencode_common_config(settings: &Value) -> Result<String, AppError> {
        let mut config = settings.clone();

        if let Some(obj) = config.as_object_mut()
            && let Some(options) = obj.get_mut("options").and_then(|v| v.as_object_mut())
        {
            options.remove("apiKey");
            options.remove("baseURL");
        }

        if config.is_null() || (config.is_object() && config.as_object().unwrap().is_empty()) {
            return Ok("{}".to_string());
        }

        serde_json::to_string_pretty(&config)
            .map_err(|e| AppError::Message(format!("Serialization failed: {e}")))
    }

    /// Extract common config for OpenClaw (JSON format)
    fn extract_openclaw_common_config(settings: &Value) -> Result<String, AppError> {
        let mut config = settings.clone();

        if let Some(obj) = config.as_object_mut() {
            obj.remove("apiKey");
            obj.remove("baseUrl");
        }

        if config.is_null() || (config.is_object() && config.as_object().unwrap().is_empty()) {
            return Ok("{}".to_string());
        }

        serde_json::to_string_pretty(&config)
            .map_err(|e| AppError::Message(format!("Serialization failed: {e}")))
    }

    /// Import default configuration from live files (re-export)
    pub fn import_default_config(state: &AppState, app_type: AppType) -> Result<bool, AppError> {
        import_default_config(state, app_type)
    }

    pub fn should_auto_import_default_config(
        state: &AppState,
        app_type: &AppType,
    ) -> Result<bool, AppError> {
        should_auto_import_default_config(state, app_type)
    }

    /// Discover providers from the tool's live configuration without
    /// overwriting anything already managed by OcHub.
    pub fn auto_import_live_providers(
        state: &AppState,
        app_type: AppType,
    ) -> Result<usize, AppError> {
        auto_import_live_providers(state, app_type)
    }

    /// Read current live settings (re-export)
    pub fn read_live_settings(app_type: AppType) -> Result<Value, AppError> {
        read_live_settings(app_type)
    }

    /// Get custom endpoints list (re-export)
    pub fn get_custom_endpoints(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
    ) -> Result<Vec<CustomEndpoint>, AppError> {
        endpoints::get_custom_endpoints(state, app_type, provider_id)
    }

    /// Add custom endpoint (re-export)
    pub fn add_custom_endpoint(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
        url: String,
    ) -> Result<(), AppError> {
        endpoints::add_custom_endpoint(state, app_type, provider_id, url)
    }

    /// Remove custom endpoint (re-export)
    pub fn remove_custom_endpoint(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
        url: String,
    ) -> Result<(), AppError> {
        endpoints::remove_custom_endpoint(state, app_type, provider_id, url)
    }

    /// Update endpoint last used timestamp (re-export)
    pub fn update_endpoint_last_used(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
        url: String,
    ) -> Result<(), AppError> {
        endpoints::update_endpoint_last_used(state, app_type, provider_id, url)
    }

    /// Update provider sort order
    pub fn update_sort_order(
        state: &AppState,
        app_type: AppType,
        updates: Vec<ProviderSortUpdate>,
    ) -> Result<bool, AppError> {
        let mut providers = state.db.get_all_providers(app_type.as_str())?;

        for update in updates {
            if let Some(provider) = providers.get_mut(&update.id) {
                provider.sort_index = Some(update.sort_index);
                state.db.save_provider(app_type.as_str(), provider)?;
            }
        }

        Ok(true)
    }

    /// Query provider usage (re-export)
    pub async fn query_usage(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
    ) -> Result<UsageResult, AppError> {
        usage::query_usage(state, app_type, provider_id).await
    }

    /// Test usage script (re-export)
    #[allow(clippy::too_many_arguments)]
    pub async fn test_usage_script(
        state: &AppState,
        app_type: AppType,
        provider_id: &str,
        script_code: &str,
        timeout: u64,
        api_key: Option<&str>,
        base_url: Option<&str>,
        access_token: Option<&str>,
        user_id: Option<&str>,
        template_type: Option<&str>,
    ) -> Result<UsageResult, AppError> {
        usage::test_usage_script(
            state,
            app_type,
            provider_id,
            script_code,
            timeout,
            api_key,
            base_url,
            access_token,
            user_id,
            template_type,
        )
        .await
    }

    fn validate_provider_settings(app_type: &AppType, provider: &Provider) -> Result<(), AppError> {
        match app_type {
            AppType::Claude => {
                if !provider.settings_config.is_object() {
                    return Err(AppError::localized(
                        "provider.claude.settings.not_object",
                        "Claude 配置必须是 JSON 对象",
                        "Claude configuration must be a JSON object",
                    ));
                }
            }
            AppType::ClaudeDesktop => {
                crate::apps::claude_desktop::validate_provider(provider)?;
            }
            AppType::CherryStudio => {
                crate::apps::cherry_studio::build_provider_import_deeplink(provider)?;
            }
            AppType::Codex => {
                let settings = provider.settings_config.as_object().ok_or_else(|| {
                    AppError::localized(
                        "provider.codex.settings.not_object",
                        "Codex 配置必须是 JSON 对象",
                        "Codex configuration must be a JSON object",
                    )
                })?;

                let auth = settings.get("auth").ok_or_else(|| {
                    AppError::localized(
                        "provider.codex.auth.missing",
                        format!("供应商 {} 缺少 auth 配置", provider.id),
                        format!("Provider {} is missing auth configuration", provider.id),
                    )
                })?;
                if !auth.is_object() {
                    return Err(AppError::localized(
                        "provider.codex.auth.not_object",
                        format!("供应商 {} 的 auth 配置必须是 JSON 对象", provider.id),
                        format!(
                            "Provider {} auth configuration must be a JSON object",
                            provider.id
                        ),
                    ));
                }

                if let Some(config_value) = settings.get("config") {
                    if !(config_value.is_string() || config_value.is_null()) {
                        return Err(AppError::localized(
                            "provider.codex.config.invalid_type",
                            "Codex config 字段必须是字符串",
                            "Codex config field must be a string",
                        ));
                    }
                    if let Some(cfg_text) = config_value.as_str() {
                        crate::apps::codex::validate_config_toml(cfg_text)?;
                    }
                }
            }
            AppType::GrokBuild => {
                let config = provider
                    .settings_config
                    .get("config")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AppError::Config("Grok Build 配置缺少 config 字段".to_string())
                    })?;
                if provider.category.as_deref() == Some("official") {
                    crate::apps::grokbuild::validate_config_toml_syntax(config)?;
                } else {
                    crate::apps::grokbuild::validate_config_toml(config)?;
                }
            }
            AppType::KimiCode => {
                let settings = provider.settings_config.as_object().ok_or_else(|| {
                    AppError::Config("Kimi Code 配置必须是 JSON 对象".to_string())
                })?;
                if !settings.get("providers").is_some_and(Value::is_object)
                    || !settings.get("models").is_some_and(Value::is_object)
                {
                    return Err(AppError::Config(
                        "Kimi Code 配置缺少 providers 或 models".to_string(),
                    ));
                }
            }
            AppType::OpenCode => {
                if !provider.settings_config.is_object() {
                    return Err(AppError::localized(
                        "provider.opencode.settings.not_object",
                        "OpenCode 配置必须是 JSON 对象",
                        "OpenCode configuration must be a JSON object",
                    ));
                }
            }
            AppType::OpenClaw => {
                if !provider.settings_config.is_object() {
                    return Err(AppError::localized(
                        "provider.openclaw.settings.not_object",
                        "OpenClaw 配置必须是 JSON 对象",
                        "OpenClaw configuration must be a JSON object",
                    ));
                }
            }
            AppType::Hermes => {
                if !provider.settings_config.is_object() {
                    return Err(AppError::localized(
                        "provider.hermes.settings.not_object",
                        "Hermes 配置必须是 JSON 对象",
                        "Hermes configuration must be a JSON object",
                    ));
                }
            }
        }

        // Validate and clean UsageScript configuration (common for all app types)
        if let Some(meta) = &provider.meta {
            if let Some(multiplier) = meta.cost_multiplier.as_deref() {
                validate_cost_multiplier(multiplier)?;
            }
            if let Some(source) = meta.pricing_model_source.as_deref() {
                validate_pricing_source(source)?;
            }
            if let Some(usage_script) = &meta.usage_script {
                validate_usage_script(usage_script)?;
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    fn extract_credentials(
        provider: &Provider,
        app_type: &AppType,
    ) -> Result<(String, String), AppError> {
        match app_type {
            AppType::Claude => {
                let env = provider
                    .settings_config
                    .get("env")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.claude.env.missing",
                            "配置格式错误: 缺少 env",
                            "Invalid configuration: missing env section",
                        )
                    })?;

                let api_key = env
                    .get("ANTHROPIC_AUTH_TOKEN")
                    .or_else(|| env.get("ANTHROPIC_API_KEY"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.claude.api_key.missing",
                            "缺少 API Key",
                            "API key is missing",
                        )
                    })?
                    .to_string();

                let base_url = env
                    .get("ANTHROPIC_BASE_URL")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.claude.base_url.missing",
                            "缺少 ANTHROPIC_BASE_URL 配置",
                            "Missing ANTHROPIC_BASE_URL configuration",
                        )
                    })?
                    .to_string();

                Ok((api_key, base_url))
            }
            AppType::ClaudeDesktop => {
                let credentials =
                    crate::apps::claude_desktop::direct_gateway_credentials(provider)?;
                Ok((credentials.api_key, credentials.base_url))
            }
            AppType::CherryStudio => Ok((
                provider
                    .settings_config
                    .get("api_key")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                provider
                    .settings_config
                    .get("base_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )),
            AppType::Codex => {
                let _auth = provider
                    .settings_config
                    .get("auth")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.codex.auth.missing",
                            "配置格式错误: 缺少 auth",
                            "Invalid configuration: missing auth section",
                        )
                    })?;

                let config_toml = provider
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let api_key = crate::apps::codex::extract_codex_api_key(
                    provider.settings_config.get("auth"),
                    Some(config_toml),
                )
                .ok_or_else(|| {
                    AppError::localized(
                        "provider.codex.api_key.missing",
                        "缺少 API Key",
                        "API key is missing",
                    )
                })?;

                let base_url = if config_toml.contains("base_url") {
                    let re = Regex::new(r#"base_url\s*=\s*["']([^"']+)["']"#).map_err(|e| {
                        AppError::localized(
                            "provider.regex_init_failed",
                            format!("正则初始化失败: {e}"),
                            format!("Failed to initialize regex: {e}"),
                        )
                    })?;
                    re.captures(config_toml)
                        .and_then(|caps| caps.get(1))
                        .map(|m| m.as_str().to_string())
                        .ok_or_else(|| {
                            AppError::localized(
                                "provider.codex.base_url.invalid",
                                "config.toml 中 base_url 格式错误",
                                "base_url in config.toml has invalid format",
                            )
                        })?
                } else {
                    return Err(AppError::localized(
                        "provider.codex.base_url.missing",
                        "config.toml 中缺少 base_url 配置",
                        "base_url is missing from config.toml",
                    ));
                };

                Ok((api_key, base_url))
            }
            AppType::GrokBuild => {
                let config = provider
                    .settings_config
                    .get("config")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AppError::Config("Grok Build 配置缺少 config 字段".to_string())
                    })?;
                let (base_url, api_key) = crate::apps::grokbuild::extract_credentials(config)
                    .ok_or_else(|| {
                        AppError::Config("Grok Build 配置缺少 Base URL 或 API Key".to_string())
                    })?;
                Ok((api_key, base_url))
            }
            AppType::KimiCode => {
                let provider = provider
                    .settings_config
                    .get("providers")
                    .and_then(Value::as_object)
                    .and_then(|providers| providers.values().next())
                    .ok_or_else(|| AppError::Config("Kimi Code 配置缺少 provider".to_string()))?;
                Ok((
                    provider
                        .get("api_key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    provider
                        .get("base_url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            }
            AppType::OpenCode => {
                let options = provider
                    .settings_config
                    .get("options")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.opencode.options.missing",
                            "配置格式错误: 缺少 options",
                            "Invalid configuration: missing options section",
                        )
                    })?;

                let api_key = options
                    .get("apiKey")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.opencode.api_key.missing",
                            "缺少 API Key",
                            "API key is missing",
                        )
                    })?
                    .to_string();

                let base_url = options
                    .get("baseURL")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                Ok((api_key, base_url))
            }
            AppType::OpenClaw | AppType::Hermes => {
                let api_key = provider
                    .settings_config
                    .get("apiKey")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::localized(
                            "provider.openclaw.api_key.missing",
                            "缺少 API Key",
                            "API key is missing",
                        )
                    })?
                    .to_string();

                let base_url = provider
                    .settings_config
                    .get("baseUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                Ok((api_key, base_url))
            }
        }
    }
}

/// `base` → `base-copy`, falling back to `base-copy-2`, `-3`… while `taken`
/// says the candidate is already in use.
///
/// Used for both the name and — in additive mode — the id of a duplicate, so
/// duplicating the same provider twice cannot silently overwrite the first copy
/// or leave two cards spelled identically.
fn next_copy_label(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let first = format!("{base}-copy");
    if !taken(&first) {
        return first;
    }
    (2u32..)
        .map(|nth| format!("{base}-copy-{nth}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or(first)
}

/// Normalize Claude model keys in a JSON value
///
/// Reads old key (ANTHROPIC_SMALL_FAST_MODEL), writes new keys (DEFAULT_*), and deletes old key.
pub(crate) fn normalize_claude_models_in_value(settings: &mut Value) -> bool {
    let mut changed = false;
    let env = match settings.get_mut("env").and_then(|v| v.as_object_mut()) {
        Some(obj) => obj,
        None => return changed,
    };

    let model = env
        .get("ANTHROPIC_MODEL")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let small_fast = env
        .get("ANTHROPIC_SMALL_FAST_MODEL")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let current_haiku = env
        .get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let current_sonnet = env
        .get("ANTHROPIC_DEFAULT_SONNET_MODEL")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let current_opus = env
        .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let target_haiku = current_haiku
        .or_else(|| small_fast.clone())
        .or_else(|| model.clone());
    let target_sonnet = current_sonnet
        .or_else(|| model.clone())
        .or_else(|| small_fast.clone());
    let target_opus = current_opus
        .or_else(|| model.clone())
        .or_else(|| small_fast.clone());

    if env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").is_none()
        && let Some(v) = target_haiku
    {
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            Value::String(v),
        );
        changed = true;
    }
    if env.get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none()
        && let Some(v) = target_sonnet
    {
        env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
            Value::String(v),
        );
        changed = true;
    }
    if env.get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none()
        && let Some(v) = target_opus
    {
        env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), Value::String(v));
        changed = true;
    }

    if env.remove("ANTHROPIC_SMALL_FAST_MODEL").is_some() {
        changed = true;
    }

    changed
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderSortUpdate {
    pub id: String,
    #[serde(rename = "sortIndex")]
    pub sort_index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use serde_json::json;
    use std::sync::Arc;

    fn state() -> AppState {
        AppState::new(Arc::new(Database::memory().expect("memory db")))
    }

    fn claude_provider(id: &str, name: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            name.to_string(),
            json!({ "env": { "ANTHROPIC_BASE_URL": "https://alpha.example" } }),
            Some("https://alpha.example".to_string()),
        )
    }

    #[test]
    fn duplicate_copies_the_configuration_under_a_copy_suffixed_name() {
        let state = state();
        let mut source = claude_provider("alpha", "Alpha");
        source.sort_index = Some(3);
        source.notes = Some("keep me".to_string());
        state.db.save_provider("claude", &source).expect("save");

        let copy = ProviderService::duplicate(&state, AppType::Claude, "alpha").expect("duplicate");

        assert_eq!(copy.name, "Alpha-copy");
        assert_ne!(copy.id, "alpha", "a copy must not overwrite its source");
        assert_eq!(copy.settings_config, source.settings_config);
        assert_eq!(copy.website_url, source.website_url);
        assert_eq!(copy.notes, source.notes);
        // Same slot, newer timestamp: that is what puts the copy directly below
        // its source in the list.
        assert_eq!(copy.sort_index, Some(3));
        assert!(copy.created_at > source.created_at);

        // The copy is inert: nothing became current just because it was made.
        assert_eq!(
            state.db.get_current_provider("claude").expect("current"),
            None
        );
        assert_eq!(state.db.get_all_providers("claude").expect("list").len(), 2);
    }

    #[test]
    fn duplicating_twice_yields_two_distinct_copies() {
        let state = state();
        state
            .db
            .save_provider("claude", &claude_provider("alpha", "Alpha"))
            .expect("save");

        ProviderService::duplicate(&state, AppType::Claude, "alpha").expect("first copy");
        let second = ProviderService::duplicate(&state, AppType::Claude, "alpha").expect("second");

        assert_eq!(second.name, "Alpha-copy-2");
        let providers = state.db.get_all_providers("claude").expect("list");
        assert_eq!(providers.len(), 3);
    }

    #[test]
    fn an_additive_copy_gets_a_readable_key_and_stays_out_of_the_tool_config() {
        let state = state();
        let mut source = Provider::with_id(
            "myco".to_string(),
            "MyCo".to_string(),
            json!({ "npm": "@ai-sdk/openai-compatible" }),
            None,
        );
        ProviderService::set_provider_live_config_managed(&mut source, true);
        state.db.save_provider("opencode", &source).expect("save");

        let copy =
            ProviderService::duplicate(&state, AppType::OpenCode, "myco").expect("duplicate");

        assert_eq!(copy.id, "myco-copy");
        assert_eq!(
            ProviderService::provider_live_config_managed(&copy),
            Some(false),
            "a fresh copy must not claim to be in the tool config"
        );
    }

    #[test]
    fn duplicating_an_unknown_provider_is_an_error() {
        let state = state();
        assert!(ProviderService::duplicate(&state, AppType::Claude, "ghost").is_err());
    }
}
