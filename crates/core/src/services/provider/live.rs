//! Live configuration operations
//!
//! Handles reading and writing live configuration files for managed applications.

use serde_json::Value;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::apps::codex::{get_codex_auth_path, get_codex_config_path};
use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;
use crate::paths::{delete_file, get_claude_settings_path, read_json_file, write_json_file};

use super::drift::{self, LiveDrift};
use super::normalize_claude_models_in_value;

pub(crate) fn sanitize_claude_settings_for_live(settings: &Value) -> Value {
    let mut v = settings.clone();
    if let Some(obj) = v.as_object_mut() {
        // Internal-only fields - never write to Claude Code settings.json
        obj.remove("api_format");
        obj.remove("apiFormat");
        obj.remove("openrouter_compat_mode");
        obj.remove("openrouterCompatMode");
    }
    v
}

pub(crate) fn provider_exists_in_live_config(
    app_type: &AppType,
    provider_id: &str,
) -> Result<bool, AppError> {
    match app_type {
        AppType::OpenCode => crate::apps::opencode::get_providers()
            .map(|providers| providers.contains_key(provider_id)),
        AppType::OpenClaw => crate::apps::openclaw::get_providers()
            .map(|providers| providers.contains_key(provider_id)),
        AppType::Hermes => crate::apps::hermes::get_providers()
            .map(|providers| providers.contains_key(provider_id)),
        _ => Ok(false),
    }
}

fn json_is_subset(target: &Value, source: &Value) -> bool {
    match source {
        Value::Object(source_map) => {
            let Some(target_map) = target.as_object() else {
                return false;
            };
            source_map.iter().all(|(key, source_value)| {
                target_map
                    .get(key)
                    .is_some_and(|target_value| json_is_subset(target_value, source_value))
            })
        }
        Value::Array(source_arr) => {
            let Some(target_arr) = target.as_array() else {
                return false;
            };
            json_array_contains_subset(target_arr, source_arr)
        }
        _ => target == source,
    }
}

fn json_array_contains_subset(target_arr: &[Value], source_arr: &[Value]) -> bool {
    let mut matched = vec![false; target_arr.len()];

    source_arr.iter().all(|source_item| {
        if let Some((index, _)) = target_arr.iter().enumerate().find(|(index, target_item)| {
            !matched[*index] && json_is_subset(target_item, source_item)
        }) {
            matched[index] = true;
            true
        } else {
            false
        }
    })
}

fn json_remove_array_items(target_arr: &mut Vec<Value>, source_arr: &[Value]) {
    for source_item in source_arr {
        if let Some(index) = target_arr
            .iter()
            .position(|target_item| json_is_subset(target_item, source_item))
        {
            target_arr.remove(index);
        }
    }
}

fn json_deep_merge(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, source_value) in source_map {
                match target_map.get_mut(key) {
                    Some(target_value) => json_deep_merge(target_value, source_value),
                    None => {
                        target_map.insert(key.clone(), source_value.clone());
                    }
                }
            }
        }
        (target_value, source_value) => {
            *target_value = source_value.clone();
        }
    }
}

fn json_deep_remove(target: &mut Value, source: &Value) {
    let (Some(target_map), Some(source_map)) = (target.as_object_mut(), source.as_object()) else {
        return;
    };

    for (key, source_value) in source_map {
        let mut remove_key = false;

        if let Some(target_value) = target_map.get_mut(key) {
            if source_value.is_object() && target_value.is_object() {
                json_deep_remove(target_value, source_value);
                remove_key = target_value.as_object().is_some_and(|obj| obj.is_empty());
            } else if let (Some(target_arr), Some(source_arr)) =
                (target_value.as_array_mut(), source_value.as_array())
            {
                json_remove_array_items(target_arr, source_arr);
                remove_key = target_arr.is_empty();
            } else if json_is_subset(target_value, source_value) {
                remove_key = true;
            }
        }

        if remove_key {
            target_map.remove(key);
        }
    }
}

fn toml_value_is_subset(target: &toml_edit::Value, source: &toml_edit::Value) -> bool {
    match (target, source) {
        (toml_edit::Value::String(target), toml_edit::Value::String(source)) => {
            target.value() == source.value()
        }
        (toml_edit::Value::Integer(target), toml_edit::Value::Integer(source)) => {
            target.value() == source.value()
        }
        (toml_edit::Value::Float(target), toml_edit::Value::Float(source)) => {
            target.value() == source.value()
        }
        (toml_edit::Value::Boolean(target), toml_edit::Value::Boolean(source)) => {
            target.value() == source.value()
        }
        (toml_edit::Value::Datetime(target), toml_edit::Value::Datetime(source)) => {
            target.value() == source.value()
        }
        (toml_edit::Value::Array(target), toml_edit::Value::Array(source)) => {
            toml_array_contains_subset(target, source)
        }
        (toml_edit::Value::InlineTable(target), toml_edit::Value::InlineTable(source)) => {
            source.iter().all(|(key, source_item)| {
                target
                    .get(key)
                    .is_some_and(|target_item| toml_value_is_subset(target_item, source_item))
            })
        }
        _ => false,
    }
}

fn toml_array_contains_subset(target: &toml_edit::Array, source: &toml_edit::Array) -> bool {
    let mut matched = vec![false; target.len()];
    let target_items: Vec<&toml_edit::Value> = target.iter().collect();

    source.iter().all(|source_item| {
        if let Some((index, _)) = target_items
            .iter()
            .enumerate()
            .find(|(index, target_item)| {
                !matched[*index] && toml_value_is_subset(target_item, source_item)
            })
        {
            matched[index] = true;
            true
        } else {
            false
        }
    })
}

fn toml_remove_array_items(target: &mut toml_edit::Array, source: &toml_edit::Array) {
    for source_item in source.iter() {
        let index = {
            let target_items: Vec<&toml_edit::Value> = target.iter().collect();
            target_items
                .iter()
                .enumerate()
                .find(|(_, target_item)| toml_value_is_subset(target_item, source_item))
                .map(|(index, _)| index)
        };

        if let Some(index) = index {
            target.remove(index);
        }
    }
}

fn toml_item_is_subset(target: &Item, source: &Item) -> bool {
    if let Some(source_table) = source.as_table_like() {
        let Some(target_table) = target.as_table_like() else {
            return false;
        };
        return source_table.iter().all(|(key, source_item)| {
            target_table
                .get(key)
                .is_some_and(|target_item| toml_item_is_subset(target_item, source_item))
        });
    }

    match (target.as_value(), source.as_value()) {
        (Some(target_value), Some(source_value)) => {
            toml_value_is_subset(target_value, source_value)
        }
        _ => false,
    }
}

fn merge_toml_item(target: &mut Item, source: &Item) {
    if let Some(source_table) = source.as_table_like()
        && let Some(target_table) = target.as_table_like_mut()
    {
        merge_toml_table_like(target_table, source_table);
        return;
    }

    *target = source.clone();
}

fn merge_toml_table_like(target: &mut dyn TableLike, source: &dyn TableLike) {
    for (key, source_item) in source.iter() {
        match target.get_mut(key) {
            Some(target_item) => merge_toml_item(target_item, source_item),
            None => {
                target.insert(key, source_item.clone());
            }
        }
    }
}

fn remove_toml_item(target: &mut Item, source: &Item) {
    if let Some(source_table) = source.as_table_like()
        && let Some(target_table) = target.as_table_like_mut()
    {
        remove_toml_table_like(target_table, source_table);
        if target_table.is_empty() {
            *target = Item::None;
        }
        return;
    }

    if let Some(source_value) = source.as_value() {
        let mut remove_item = false;

        if let Some(target_value) = target.as_value_mut() {
            match (target_value, source_value) {
                (toml_edit::Value::Array(target_arr), toml_edit::Value::Array(source_arr)) => {
                    toml_remove_array_items(target_arr, source_arr);
                    remove_item = target_arr.is_empty();
                }
                (target_value, source_value)
                    if toml_value_is_subset(target_value, source_value) =>
                {
                    remove_item = true;
                }
                _ => {}
            }
        }

        if remove_item {
            *target = Item::None;
        }
    }
}

fn remove_toml_table_like(target: &mut dyn TableLike, source: &dyn TableLike) {
    let keys: Vec<String> = source.iter().map(|(key, _)| key.to_string()).collect();

    for key in keys {
        let mut remove_key = false;
        if let (Some(target_item), Some(source_item)) = (target.get_mut(&key), source.get(&key)) {
            remove_toml_item(target_item, source_item);
            remove_key = target_item.is_none()
                || target_item
                    .as_table_like()
                    .is_some_and(|table_like| table_like.is_empty());
        }

        if remove_key {
            target.remove(&key);
        }
    }
}

fn settings_contain_common_config(app_type: &AppType, settings: &Value, snippet: &str) -> bool {
    let trimmed = snippet.trim();
    if trimmed.is_empty() {
        return false;
    }

    match app_type {
        AppType::Claude => match serde_json::from_str::<Value>(trimmed) {
            Ok(source) if source.is_object() => json_is_subset(settings, &source),
            _ => false,
        },
        AppType::Codex => {
            let config_toml = settings.get("config").and_then(Value::as_str).unwrap_or("");
            if config_toml.trim().is_empty() {
                return false;
            }

            let target_doc = match config_toml.parse::<DocumentMut>() {
                Ok(doc) => doc,
                Err(_) => return false,
            };
            let source_doc = match trimmed.parse::<DocumentMut>() {
                Ok(doc) => doc,
                Err(_) => return false,
            };

            toml_item_is_subset(target_doc.as_item(), source_doc.as_item())
        }
        AppType::OpenCode
        | AppType::OpenClaw
        | AppType::Hermes
        | AppType::KimiCode
        | AppType::CherryStudio
        | AppType::GrokBuild
        | AppType::ClaudeDesktop => false,
    }
}

pub(crate) fn provider_uses_common_config(
    app_type: &AppType,
    provider: &Provider,
    snippet: Option<&str>,
) -> bool {
    match provider
        .meta
        .as_ref()
        .and_then(|meta| meta.common_config_enabled)
    {
        Some(explicit) => explicit && snippet.is_some_and(|value| !value.trim().is_empty()),
        None => snippet.is_some_and(|value| {
            settings_contain_common_config(app_type, &provider.settings_config, value)
        }),
    }
}

pub(crate) fn remove_common_config_from_settings(
    app_type: &AppType,
    settings: &Value,
    snippet: &str,
) -> Result<Value, AppError> {
    let trimmed = snippet.trim();
    if trimmed.is_empty() {
        return Ok(settings.clone());
    }

    match app_type {
        AppType::Claude => {
            let source = serde_json::from_str::<Value>(trimmed)
                .map_err(|e| AppError::Message(format!("Invalid Claude common config: {e}")))?;
            let mut result = settings.clone();
            json_deep_remove(&mut result, &source);
            Ok(result)
        }
        AppType::Codex => {
            let mut result = settings.clone();
            let config_toml = settings.get("config").and_then(Value::as_str).unwrap_or("");
            let mut target_doc = if config_toml.trim().is_empty() {
                DocumentMut::new()
            } else {
                config_toml.parse::<DocumentMut>().map_err(|e| {
                    AppError::Message(format!(
                        "Invalid Codex config.toml while removing common config: {e}"
                    ))
                })?
            };
            let source_doc = trimmed.parse::<DocumentMut>().map_err(|e| {
                AppError::Message(format!("Invalid Codex common config snippet: {e}"))
            })?;

            remove_toml_table_like(target_doc.as_table_mut(), source_doc.as_table());
            if let Some(obj) = result.as_object_mut() {
                obj.insert("config".to_string(), Value::String(target_doc.to_string()));
            }
            Ok(result)
        }
        AppType::OpenCode
        | AppType::OpenClaw
        | AppType::Hermes
        | AppType::KimiCode
        | AppType::CherryStudio
        | AppType::GrokBuild
        | AppType::ClaudeDesktop => Ok(settings.clone()),
    }
}

fn apply_common_config_to_settings(
    app_type: &AppType,
    settings: &Value,
    snippet: &str,
) -> Result<Value, AppError> {
    let trimmed = snippet.trim();
    if trimmed.is_empty() {
        return Ok(settings.clone());
    }

    match app_type {
        AppType::Claude => {
            let source = serde_json::from_str::<Value>(trimmed)
                .map_err(|e| AppError::Message(format!("Invalid Claude common config: {e}")))?;
            let mut result = settings.clone();
            json_deep_merge(&mut result, &source);
            Ok(result)
        }
        AppType::Codex => {
            let mut result = settings.clone();
            let config_toml = settings.get("config").and_then(Value::as_str).unwrap_or("");
            let mut target_doc = if config_toml.trim().is_empty() {
                DocumentMut::new()
            } else {
                config_toml.parse::<DocumentMut>().map_err(|e| {
                    AppError::Message(format!(
                        "Invalid Codex config.toml while applying common config: {e}"
                    ))
                })?
            };
            let source_doc = trimmed.parse::<DocumentMut>().map_err(|e| {
                AppError::Message(format!("Invalid Codex common config snippet: {e}"))
            })?;

            merge_toml_table_like(target_doc.as_table_mut(), source_doc.as_table());
            if let Some(obj) = result.as_object_mut() {
                obj.insert("config".to_string(), Value::String(target_doc.to_string()));
            }
            Ok(result)
        }
        AppType::OpenCode
        | AppType::OpenClaw
        | AppType::Hermes
        | AppType::KimiCode
        | AppType::CherryStudio
        | AppType::GrokBuild
        | AppType::ClaudeDesktop => Ok(settings.clone()),
    }
}

pub(crate) fn build_effective_settings_with_common_config(
    db: &Database,
    app_type: &AppType,
    provider: &Provider,
) -> Result<Value, AppError> {
    let snippet = db.get_config_snippet(app_type.as_str())?;
    let mut effective_settings = provider.settings_config.clone();

    if provider_uses_common_config(app_type, provider, snippet.as_deref())
        && let Some(snippet_text) = snippet.as_deref()
    {
        match apply_common_config_to_settings(app_type, &effective_settings, snippet_text) {
            Ok(settings) => effective_settings = settings,
            Err(err) => {
                log::warn!(
                    "Failed to apply common config for {} provider '{}': {err}",
                    app_type.as_str(),
                    provider.id
                );
            }
        }
    }

    Ok(effective_settings)
}

pub(crate) fn write_live_with_common_config(
    db: &Database,
    app_type: &AppType,
    provider: &Provider,
) -> Result<(), AppError> {
    crate::plugin::registry::ensure_app_type_enabled(app_type)?;
    write_live_with_common_config_ungated(db, app_type, provider)
}

/// Internal SSOT rebuild paths bypass the enabled gate so startup repair can
/// restore a disabled application's current provider configuration.
pub(crate) fn write_live_with_common_config_ungated(
    db: &Database,
    app_type: &AppType,
    provider: &Provider,
) -> Result<(), AppError> {
    let mut effective_provider = provider.clone();
    effective_provider.settings_config =
        build_effective_settings_with_common_config(db, app_type, provider)?;

    if matches!(app_type, AppType::ClaudeDesktop) {
        crate::apps::claude_desktop::apply_provider(db, &effective_provider)?;
        log::info!(
            "Claude Desktop 3P profile '{}' written for provider '{}'",
            crate::apps::claude_desktop::PROFILE_ID,
            effective_provider.id
        );
        return Ok(());
    }

    write_live_snapshot(app_type, &effective_provider)?;
    // Re-baseline even on the paths that overwrite without asking, so the next
    // comparison blames the user for the user's edits and nobody else's.
    drift::record_snapshot(app_type, &effective_provider.id);
    Ok(())
}

/// Record an external edit in the log for the paths that have nowhere to show it
/// yet. Conflicts are the half a user has to know about, so they are louder.
pub(crate) fn log_drift(app_type: &AppType, provider: &Provider, report: &LiveDrift) {
    if report.is_empty() {
        return;
    }

    if report.has_conflicts() {
        log::warn!(
            "{} live config was edited outside OcHub; {} change(s) were overwritten by provider '{}': {}",
            app_type.as_str(),
            report.conflicts.len(),
            provider.id,
            report
                .conflicts
                .iter()
                .map(|conflict| conflict.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if !report.preserved.is_empty() || !report.removed.is_empty() {
        log::info!(
            "{} live config: kept {} external edit(s), {} deletion(s) across provider '{}'",
            app_type.as_str(),
            report.preserved.len(),
            report.removed.len(),
            provider.id
        );
    }
}

/// Compare the live file against what OcHub last wrote, and against what it is
/// about to write.
///
/// Returns the merged configuration alongside the report. Splitting this from
/// the write is what lets the UI show the user a diff *before* their file
/// changes, rather than a summary of what already happened to it.
fn resolve_live_write(
    db: &Database,
    app_type: &AppType,
    outgoing: Option<&Provider>,
    incoming: &Provider,
) -> Result<(Value, Value, LiveDrift), AppError> {
    let incoming_settings = build_effective_settings_with_common_config(db, app_type, incoming)?;

    if !drift::tracks_live_drift(app_type) {
        return Ok((
            incoming_settings.clone(),
            incoming_settings,
            LiveDrift::default(),
        ));
    }

    // The baseline is what OcHub last wrote. Falling back to the outgoing
    // provider's effective settings keeps the first switch after an upgrade
    // honest: without it, every edit made before the snapshot existed would
    // look like OcHub's own work and be overwritten without a word.
    let base = match drift::load_snapshot(app_type) {
        Some(snapshot) => Some(snapshot.settings),
        None => outgoing
            .map(|provider| build_effective_settings_with_common_config(db, app_type, provider))
            .transpose()?,
    };

    let (merged, report) = match (base, read_live_settings(*app_type)) {
        (Some(base), Ok(live)) => {
            drift::merge_user_edits(app_type, &base, &live, &incoming_settings)
        }
        // No baseline, or nothing readable on disk: there is no external edit to
        // distinguish, so the new configuration is simply the file.
        _ => (incoming_settings.clone(), LiveDrift::default()),
    };

    Ok((merged, incoming_settings, report))
}

/// What a live write should do with edits made outside OcHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DriftResolution {
    /// Carry the external edits onto the new configuration. The default: an
    /// edit OcHub has no opinion about is the user's, not ours to drop.
    #[default]
    Preserve,
    /// Write the stored configuration as-is, dropping the external edits.
    Discard,
}

/// Report what a live write would change on disk, without writing anything.
pub(crate) fn preview_live_drift(
    db: &Database,
    app_type: &AppType,
    outgoing: Option<&Provider>,
    incoming: &Provider,
) -> Result<LiveDrift, AppError> {
    crate::plugin::registry::ensure_app_type_enabled(app_type)?;
    let (_, _, report) = resolve_live_write(db, app_type, outgoing, incoming)?;
    Ok(report)
}

/// Write `incoming` to the live config, keeping edits made outside OcHub.
///
/// Anything changed on disk since OcHub last wrote is carried onto the new
/// configuration unless `incoming` sets the same key, in which case `incoming`
/// wins and the collision is reported — writing it is the action the user just
/// asked for.
pub(crate) fn write_live_preserving_user_edits(
    db: &Database,
    app_type: &AppType,
    outgoing: Option<&Provider>,
    incoming: &Provider,
) -> Result<LiveDrift, AppError> {
    write_live_resolving_drift(db, app_type, outgoing, incoming, DriftResolution::Preserve)
}

/// Write `incoming` to the live config, resolving any external edit as asked.
///
/// The report describes the edits that were found either way, so a caller that
/// discarded them can still say what it dropped.
pub(crate) fn write_live_resolving_drift(
    db: &Database,
    app_type: &AppType,
    outgoing: Option<&Provider>,
    incoming: &Provider,
    resolution: DriftResolution,
) -> Result<LiveDrift, AppError> {
    crate::plugin::registry::ensure_app_type_enabled(app_type)?;

    if !drift::tracks_live_drift(app_type) {
        write_live_with_common_config_ungated(db, app_type, incoming)?;
        return Ok(LiveDrift::default());
    }

    let (merged, stored, report) = resolve_live_write(db, app_type, outgoing, incoming)?;

    let mut effective_provider = incoming.clone();
    effective_provider.settings_config = match resolution {
        DriftResolution::Preserve => merged,
        DriftResolution::Discard => stored,
    };
    write_live_snapshot(app_type, &effective_provider)?;
    drift::record_snapshot(app_type, &incoming.id);

    Ok(report)
}

pub(crate) fn normalize_provider_common_config_for_storage(
    db: &Database,
    app_type: &AppType,
    provider: &mut Provider,
) -> Result<(), AppError> {
    let uses_common_config = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.common_config_enabled)
        .unwrap_or(false);

    if !uses_common_config {
        return Ok(());
    }

    let Some(snippet) = db.get_config_snippet(app_type.as_str())? else {
        return Ok(());
    };

    if snippet.trim().is_empty() {
        return Ok(());
    }

    match remove_common_config_from_settings(app_type, &provider.settings_config, &snippet) {
        Ok(settings) => provider.settings_config = settings,
        Err(err) => {
            log::warn!(
                "Failed to normalize common config before saving {} provider '{}': {err}",
                app_type.as_str(),
                provider.id
            );
        }
    }

    Ok(())
}

/// Live configuration snapshot for backup/restore
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) enum LiveSnapshot {
    Claude {
        settings: Option<Value>,
    },
    Codex {
        auth: Option<Value>,
        config: Option<String>,
    },
}

impl LiveSnapshot {
    #[allow(dead_code)]
    pub(crate) fn restore(&self) -> Result<(), AppError> {
        match self {
            LiveSnapshot::Claude { settings } => {
                let path = get_claude_settings_path();
                if let Some(value) = settings {
                    write_json_file(&path, value)?;
                } else if path.exists() {
                    delete_file(&path)?;
                }
            }
            LiveSnapshot::Codex { auth, config } => {
                let auth_path = get_codex_auth_path();
                let config_path = get_codex_config_path();
                if let Some(value) = auth {
                    write_json_file(&auth_path, value)?;
                } else if auth_path.exists() {
                    delete_file(&auth_path)?;
                }

                if let Some(text) = config {
                    crate::paths::write_text_file(&config_path, text)?;
                } else if config_path.exists() {
                    delete_file(&config_path)?;
                }
            }
        }
        Ok(())
    }
}

/// Write live configuration snapshot for a provider
pub(crate) fn write_live_snapshot(app_type: &AppType, provider: &Provider) -> Result<(), AppError> {
    match app_type {
        AppType::Claude => write_claude_live_snapshot(provider),
        AppType::ClaudeDesktop => Err(AppError::localized(
            "claude_desktop.live.requires_db_context",
            "Claude Desktop 配置写入需要通过供应商切换流程执行",
            "Claude Desktop configuration must be written through the provider switch flow",
        )),
        AppType::Codex => write_codex_live_snapshot(provider),
        AppType::GrokBuild => write_grokbuild_live_snapshot(provider),
        AppType::KimiCode => write_kimi_code_live_snapshot(provider),
        AppType::CherryStudio => Ok(()),
        AppType::OpenCode => write_opencode_live_snapshot(provider),
        AppType::OpenClaw => write_openclaw_live_snapshot(provider),
        AppType::Hermes => write_hermes_live_snapshot(provider),
    }
}

pub(crate) fn write_claude_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    let path = get_claude_settings_path();
    let settings = sanitize_claude_settings_for_live(&provider.settings_config);
    write_json_file(&path, &settings)
}

pub(crate) fn write_codex_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    let obj = provider
        .settings_config
        .as_object()
        .ok_or_else(|| AppError::Config("Codex 供应商配置必须是 JSON 对象".to_string()))?;
    let auth = obj
        .get("auth")
        .ok_or_else(|| AppError::Config("Codex 供应商配置缺少 'auth' 字段".to_string()))?;
    let config_str = obj.get("config").and_then(|v| v.as_str());

    crate::apps::codex::write_codex_provider_live_with_catalog(
        &provider.settings_config,
        provider.category.as_deref(),
        auth,
        config_str,
    )
}

pub(crate) fn write_grokbuild_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    crate::apps::grokbuild::write_grok_provider_live(provider)
}

pub(crate) fn write_kimi_code_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    crate::apps::kimi_code::write_kimi_code_live(provider)
}

pub(crate) fn write_opencode_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    {
        // OpenCode uses additive mode - write provider to config
        use crate::apps::opencode;
        use crate::model::OpenCodeProviderConfig;

        // Defensive check: if settings_config is a full config structure, extract provider fragment
        let config_to_write = if let Some(obj) = provider.settings_config.as_object() {
            // Detect full config structure (has $schema or top-level provider field)
            if obj.contains_key("$schema") || obj.contains_key("provider") {
                log::warn!(
                    "OpenCode provider '{}' has full config structure in settings_config, attempting to extract fragment",
                    provider.id
                );
                // Try to extract from provider.{id}
                obj.get("provider")
                    .and_then(|p| p.get(&provider.id))
                    .cloned()
                    .unwrap_or_else(|| provider.settings_config.clone())
            } else {
                provider.settings_config.clone()
            }
        } else {
            provider.settings_config.clone()
        };

        // Convert settings_config to OpenCodeProviderConfig
        let opencode_config_result =
            serde_json::from_value::<OpenCodeProviderConfig>(config_to_write.clone());

        match opencode_config_result {
            Ok(config) => {
                opencode::set_typed_provider(&provider.id, &config)?;
                log::info!("OpenCode provider '{}' written to live config", provider.id);
            }
            Err(e) => {
                log::warn!(
                    "Failed to parse OpenCode provider config for '{}': {}",
                    provider.id,
                    e
                );
                // Only write if config looks like a valid provider fragment
                if config_to_write.get("npm").is_some() || config_to_write.get("options").is_some()
                {
                    opencode::set_provider(&provider.id, config_to_write)?;
                    log::info!(
                        "OpenCode provider '{}' written as raw JSON to live config",
                        provider.id
                    );
                } else {
                    return Err(AppError::Message(format!(
                        "OpenCode provider '{}' has invalid config structure for live config (must contain 'npm' or 'options')",
                        provider.id
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn write_openclaw_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    {
        // OpenClaw uses additive mode - write provider to config
        use crate::apps::openclaw;
        use crate::apps::openclaw::OpenClawProviderConfig;

        // Convert settings_config to OpenClawProviderConfig
        let openclaw_config_result =
            serde_json::from_value::<OpenClawProviderConfig>(provider.settings_config.clone());

        match openclaw_config_result {
            Ok(config) => {
                openclaw::set_typed_provider(&provider.id, &config)?;
                // Ensure a usable default model exists, so an added provider is
                // actually selected. Only set it when none is configured yet, to
                // avoid clobbering a user's existing choice.
                if let Some(first) = config.models.first() {
                    let has_default = openclaw::get_default_model()
                        .ok()
                        .flatten()
                        .map(|d| !d.primary.trim().is_empty())
                        .unwrap_or(false);
                    if !has_default {
                        let primary = format!("{}/{}", provider.id, first.id);
                        if let Err(e) =
                            openclaw::set_default_model(&openclaw::OpenClawDefaultModel {
                                primary,
                                fallbacks: Vec::new(),
                                extra: std::collections::HashMap::new(),
                            })
                        {
                            log::warn!("OpenClaw: failed to set default model: {e}");
                        }
                    }
                }
                log::info!("OpenClaw provider '{}' written to live config", provider.id);
            }
            Err(e) => {
                log::warn!(
                    "Failed to parse OpenClaw provider config for '{}': {}",
                    provider.id,
                    e
                );
                // Try to write as raw JSON if it looks valid
                if provider.settings_config.get("baseUrl").is_some()
                    || provider.settings_config.get("api").is_some()
                    || provider.settings_config.get("models").is_some()
                {
                    openclaw::set_provider(&provider.id, provider.settings_config.clone())?;
                    log::info!(
                        "OpenClaw provider '{}' written as raw JSON to live config",
                        provider.id
                    );
                } else {
                    return Err(AppError::Message(format!(
                        "OpenClaw provider '{}' has invalid config structure for live config (must contain 'baseUrl', 'api', or 'models')",
                        provider.id
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn write_hermes_live_snapshot(provider: &Provider) -> Result<(), AppError> {
    crate::apps::hermes::set_provider(&provider.id, provider.settings_config.clone())?;
    log::debug!("Hermes provider '{}' written to live config", provider.id);
    Ok(())
}

/// Sync all providers to live configuration (for additive mode apps)
///
/// Writes all providers from the database to the live configuration file.
/// Used for OpenCode and other additive mode applications.
fn sync_all_providers_to_live(state: &AppState, app_type: &AppType) -> Result<(), AppError> {
    let providers = state.db.get_all_providers(app_type.as_str())?;
    let mut synced_count = 0usize;

    for provider in providers.values() {
        if provider
            .meta
            .as_ref()
            .and_then(|meta| meta.live_config_managed)
            == Some(false)
        {
            continue;
        }

        if let Err(e) = write_live_with_common_config(state.db.as_ref(), app_type, provider) {
            log::warn!(
                "Failed to sync {:?} provider '{}' to live: {e}",
                app_type,
                provider.id
            );
            continue;
        }
        synced_count += 1;
    }

    log::info!("Synced {synced_count} {app_type:?} providers to live config");
    Ok(())
}

pub(crate) fn sync_current_provider_for_app_to_live(
    state: &AppState,
    app_type: &AppType,
) -> Result<(), AppError> {
    if app_type.is_additive_mode() {
        sync_all_providers_to_live(state, app_type)?;
    } else {
        let current_id = match crate::settings::get_effective_current_provider(&state.db, app_type)?
        {
            Some(id) => id,
            None => return Ok(()),
        };

        let providers = state.db.get_all_providers(app_type.as_str())?;
        if let Some(provider) = providers.get(&current_id) {
            let report = write_live_preserving_user_edits(
                state.db.as_ref(),
                app_type,
                Some(provider),
                provider,
            )?;
            log_drift(app_type, provider, &report);
        }
    }

    crate::services::mcp::McpService::sync_all_enabled(state)?;

    Ok(())
}

/// Sync current provider to live configuration
///
/// 使用有效的当前供应商 ID（验证过存在性）。
/// 优先从本地 settings 读取，验证后 fallback 到数据库的 is_current 字段。
/// 这确保了配置导入后无效 ID 会自动 fallback 到数据库。
///
/// For additive mode apps (OpenCode), all providers are synced instead of just the current one.
pub fn sync_current_to_live(state: &AppState) -> Result<(), AppError> {
    // Sync providers based on mode; disabled apps are skipped entirely.
    for app_type in AppType::all() {
        if !crate::plugin::registry::is_app_type_enabled(&app_type) {
            continue;
        }
        if app_type.is_additive_mode() {
            // Additive mode: sync ALL providers
            sync_all_providers_to_live(state, &app_type)?;
        } else {
            sync_current_provider_for_app_to_live(state, &app_type)?;
        }
    }

    // MCP sync
    crate::services::mcp::McpService::sync_all_enabled(state)?;

    // Skills need no per-switch sync: the skills CLI installs directly into
    // each agent's own directory.

    Ok(())
}

/// One-time upgrade repair for tools that may still point at the retired local
/// routing listener. Only applications that could be rewritten by
/// that legacy feature are touched; additive app configs, MCP, and skills stay
/// unchanged.
pub(crate) fn restore_live_after_legacy_local_routing(state: &AppState) -> Result<(), AppError> {
    for app_type in [AppType::Claude, AppType::ClaudeDesktop, AppType::Codex] {
        let Some(current_id) =
            crate::settings::get_effective_current_provider(&state.db, &app_type)?
        else {
            continue;
        };
        let providers = state.db.get_all_providers(app_type.as_str())?;
        if let Some(provider) = providers.get(&current_id) {
            write_live_with_common_config_ungated(&state.db, &app_type, provider)?;
        }
    }
    Ok(())
}

/// Read current live settings for an app type
pub fn read_live_settings(app_type: AppType) -> Result<Value, AppError> {
    match app_type {
        AppType::Codex => {
            let mut result = crate::apps::codex::read_codex_live_settings()?;
            // `modelCatalog` is an OcHub private field that lives only in
            // the DB SSOT plus the `ochub-model-catalog.json` projection
            // file — it is never inlined into `auth.json` or `config.toml`.
            // Reverse-parse the projection so the edit form for the active
            // Codex provider doesn't see an empty mapping table.
            if let Ok(Some(model_catalog)) =
                crate::apps::codex::read_codex_model_catalog_simplified_from_live()
                && let Some(obj) = result.as_object_mut()
            {
                obj.insert("modelCatalog".to_string(), model_catalog);
            }
            Ok(result)
        }
        AppType::GrokBuild => crate::apps::grokbuild::read_grok_live_settings(),
        AppType::KimiCode => crate::apps::kimi_code::read_kimi_code_live_snapshot(),
        AppType::CherryStudio => Ok(Value::Null),
        AppType::Claude => {
            let path = get_claude_settings_path();
            if !path.exists() {
                return Err(AppError::localized(
                    "claude.live.missing",
                    "Claude Code 配置文件不存在",
                    "Claude settings file is missing",
                ));
            }
            read_json_file(&path)
        }
        AppType::ClaudeDesktop => Err(AppError::localized(
            "claude_desktop.live.read_unsupported",
            "Claude Desktop 3P 配置不支持作为通用 live 配置导入，请使用“从 Claude 导入兼容供应商”。",
            "Claude Desktop 3P configuration cannot be imported as a generic live config. Use 'Import compatible providers from Claude' instead.",
        )),
        AppType::OpenCode => {
            use crate::apps::opencode::{get_opencode_config_path, read_opencode_config};

            let config_path = get_opencode_config_path();
            if !config_path.exists() {
                return Err(AppError::localized(
                    "opencode.config.missing",
                    "OpenCode 配置文件不存在",
                    "OpenCode configuration file not found",
                ));
            }

            let config = read_opencode_config()?;
            Ok(config)
        }
        AppType::OpenClaw => {
            use crate::apps::openclaw::{get_openclaw_config_path, read_openclaw_config};

            let config_path = get_openclaw_config_path();
            if !config_path.exists() {
                return Err(AppError::localized(
                    "openclaw.config.missing",
                    "OpenClaw 配置文件不存在",
                    "OpenClaw configuration file not found",
                ));
            }

            let config = read_openclaw_config()?;
            Ok(config)
        }
        AppType::Hermes => {
            let config_path = crate::apps::hermes::get_hermes_config_path();
            if !config_path.exists() {
                return Err(AppError::localized(
                    "hermes.config.missing",
                    "Hermes 配置文件不存在",
                    "Hermes configuration file not found",
                ));
            }
            let yaml_config = crate::apps::hermes::read_hermes_config()?;
            let config = crate::apps::hermes::yaml_to_json(&yaml_config)?;
            Ok(config)
        }
    }
}

/// Import default configuration from live files.
///
/// Returns `Ok(true)` if a provider was actually imported,
/// `Ok(false)` if skipped (providers already exist for this app).
pub fn import_default_config(state: &AppState, app_type: AppType) -> Result<bool, AppError> {
    // Additive mode apps (OpenCode, OpenClaw) should use their dedicated
    // import_xxx_providers_from_live functions, not this generic default config import
    if app_type.is_additive_mode() {
        return Ok(false);
    }

    // 允许只有官方 seed 预设时继续导入 live。自动发现会额外确认 OcHub
    // 尚未管理当前供应商，避免用户主动删除导入项后又被重新创建。
    if state.db.has_non_official_seed_provider(app_type.as_str())? {
        return Ok(false);
    }

    let settings_config = match app_type {
        AppType::Codex => crate::apps::codex::read_codex_live_settings()?,
        AppType::GrokBuild => crate::apps::grokbuild::read_grok_live_settings()?,
        AppType::KimiCode => crate::apps::kimi_code::read_kimi_code_live_snapshot()?,
        AppType::CherryStudio => return Ok(false),
        AppType::Claude => {
            let settings_path = get_claude_settings_path();
            if !settings_path.exists() {
                return Err(AppError::localized(
                    "claude.live.missing",
                    "Claude Code 配置文件不存在",
                    "Claude settings file is missing",
                ));
            }
            let mut v = read_json_file::<Value>(&settings_path)?;
            let _ = normalize_claude_models_in_value(&mut v);
            v
        }
        AppType::ClaudeDesktop => {
            return Err(AppError::localized(
                "claude_desktop.import_unsupported",
                "Claude Desktop 3P 配置不能通过通用导入读取，请使用“从 Claude 导入兼容供应商”。",
                "Claude Desktop 3P config cannot be imported through the generic import flow. Use 'Import compatible providers from Claude' instead.",
            ));
        }
        // OpenCode, OpenClaw and Hermes use additive mode and are handled by early return above
        AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => {
            unreachable!("additive mode apps are handled by early return")
        }
    };

    if matches!(app_type, AppType::KimiCode)
        && settings_config
            .get("providers")
            .and_then(Value::as_object)
            .is_none_or(serde_json::Map::is_empty)
    {
        return Ok(false);
    }

    if crate::official_auth::live_looks_like_official_oauth(app_type, &settings_config) {
        return Ok(false);
    }

    let mut provider = Provider::with_id(
        "default".to_string(),
        "default".to_string(),
        settings_config,
        None,
    );
    provider.category = Some(
        if matches!(app_type, AppType::Codex) {
            let config_text = provider
                .settings_config
                .get("config")
                .and_then(Value::as_str);
            let has_provider_key = crate::apps::codex::extract_codex_api_key(
                provider.settings_config.get("auth"),
                config_text,
            )
            .is_some();
            let has_login_material = provider
                .settings_config
                .get("auth")
                .is_some_and(crate::apps::codex::codex_auth_has_login_material);

            if has_login_material && !has_provider_key {
                "official"
            } else {
                "custom"
            }
        } else {
            "custom"
        }
        .to_string(),
    );

    state.db.save_provider(app_type.as_str(), &provider)?;
    state
        .db
        .set_current_provider(app_type.as_str(), &provider.id)?;
    crate::settings::set_current_provider(&app_type, Some(provider.id.as_str()))?;

    Ok(true) // 真正导入了
}

/// Decide whether automatic discovery should import the current live config as
/// `default`.
///
/// Official seeds do not block discovery because they may have been added
/// before the user installs or configures the corresponding tool. Once OcHub
/// has a current provider or any non-seed provider, the live file is considered
/// managed and is never imported over the stored state.
pub fn should_auto_import_default_config(
    state: &AppState,
    app_type: &AppType,
) -> Result<bool, AppError> {
    if app_type.is_additive_mode() || matches!(app_type, AppType::ClaudeDesktop) {
        return Ok(false);
    }

    if state.db.get_current_provider(app_type.as_str())?.is_some() {
        return Ok(false);
    }

    Ok(!state.db.has_non_official_seed_provider(app_type.as_str())?)
}

/// Discover providers from the selected tool's live configuration.
///
/// The operation is idempotent:
/// - switch-mode tools import one `default` provider only while unmanaged;
/// - additive-mode tools import only provider ids missing from the OcHub DB;
/// - existing OcHub providers are never overwritten.
pub fn auto_import_live_providers(state: &AppState, app_type: AppType) -> Result<usize, AppError> {
    match app_type {
        AppType::Claude | AppType::Codex | AppType::GrokBuild | AppType::KimiCode => {
            if should_auto_import_default_config(state, &app_type)?
                && import_default_config(state, app_type)?
            {
                Ok(1)
            } else {
                Ok(0)
            }
        }
        AppType::ClaudeDesktop => Ok(0),
        AppType::CherryStudio => Ok(0),
        AppType::OpenCode => import_opencode_providers_from_live(state),
        AppType::OpenClaw => import_openclaw_providers_from_live(state),
        AppType::Hermes => import_hermes_providers_from_live(state),
    }
}

/// Remove an OpenCode provider from the live configuration
///
/// This is specific to OpenCode's additive mode - removing a provider
/// from the opencode.json file.
pub(crate) fn remove_opencode_provider_from_live(provider_id: &str) -> Result<(), AppError> {
    use crate::apps::opencode;

    // Check if OpenCode config directory exists
    if !opencode::get_opencode_dir().exists() {
        log::debug!("OpenCode config directory doesn't exist, skipping removal of '{provider_id}'");
        return Ok(());
    }

    opencode::remove_provider(provider_id)?;
    log::info!("OpenCode provider '{provider_id}' removed from live config");

    Ok(())
}

/// Import all providers from OpenCode live config to database
///
/// This imports existing providers from ~/.config/opencode/opencode.json
/// into the OcHub database. Each provider found will be added to the
/// database with is_current set to false.
pub fn import_opencode_providers_from_live(state: &AppState) -> Result<usize, AppError> {
    use crate::apps::opencode;

    let providers = opencode::get_typed_providers()?;
    if providers.is_empty() {
        return Ok(0);
    }

    let mut imported = 0;
    let existing_ids = state.db.get_provider_ids("opencode")?;

    for (id, config) in providers {
        // Skip if already exists in database
        if existing_ids.contains(&id) {
            log::debug!("OpenCode provider '{id}' already exists in database, skipping");
            continue;
        }

        // Convert to Value for settings_config
        let settings_config = match serde_json::to_value(&config) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("Failed to serialize OpenCode provider '{id}': {e}");
                continue;
            }
        };

        // Create provider
        let mut provider = Provider::with_id(
            id.clone(),
            config.name.clone().unwrap_or_else(|| id.clone()),
            settings_config,
            None,
        );
        provider.meta = Some(crate::model::ProviderMeta {
            live_config_managed: Some(true),
            ..Default::default()
        });

        // Save to database
        if let Err(e) = state.db.save_provider("opencode", &provider) {
            log::warn!("Failed to import OpenCode provider '{id}': {e}");
            continue;
        }

        imported += 1;
        log::info!("Imported OpenCode provider '{id}' from live config");
    }

    Ok(imported)
}

/// Import all providers from OpenClaw live config to database
///
/// This imports existing providers from ~/.openclaw/openclaw.json
/// into the OcHub database. Each provider found will be added to the
/// database with is_current set to false.
pub fn import_openclaw_providers_from_live(state: &AppState) -> Result<usize, AppError> {
    use crate::apps::openclaw;

    let providers = openclaw::get_typed_providers()?;
    if providers.is_empty() {
        return Ok(0);
    }

    let mut imported = 0;
    let existing_ids = state.db.get_provider_ids("openclaw")?;

    for (id, config) in providers {
        // Validate: skip entries with empty id or no models
        if id.trim().is_empty() {
            log::warn!("Skipping OpenClaw provider with empty id");
            continue;
        }
        if config.models.is_empty() {
            log::warn!("Skipping OpenClaw provider '{id}': no models defined");
            continue;
        }

        // Skip if already exists in database
        if existing_ids.contains(&id) {
            log::debug!("OpenClaw provider '{id}' already exists in database, skipping");
            continue;
        }

        // Convert to Value for settings_config
        let settings_config = match serde_json::to_value(&config) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("Failed to serialize OpenClaw provider '{id}': {e}");
                continue;
            }
        };

        // Determine display name: use first model name if available, otherwise use id
        let display_name = config
            .models
            .first()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| id.clone());

        // Create provider
        let mut provider = Provider::with_id(id.clone(), display_name, settings_config, None);
        provider.meta = Some(crate::model::ProviderMeta {
            live_config_managed: Some(true),
            ..Default::default()
        });

        // Save to database
        if let Err(e) = state.db.save_provider("openclaw", &provider) {
            log::warn!("Failed to import OpenClaw provider '{id}': {e}");
            continue;
        }

        imported += 1;
        log::info!("Imported OpenClaw provider '{id}' from live config");
    }

    Ok(imported)
}

/// Import all providers from Hermes live config to database
///
/// This imports existing providers from ~/.hermes/config.yaml
/// into the OcHub database. Each provider found will be added to the
/// database with is_current set to false.
pub fn import_hermes_providers_from_live(state: &AppState) -> Result<usize, AppError> {
    use crate::apps::hermes;

    let providers = hermes::get_providers()?;
    if providers.is_empty() {
        return Ok(0);
    }

    let mut imported = 0;
    let existing_ids = state.db.get_provider_ids("hermes")?;

    for (name, config) in providers {
        // Validate: skip entries with empty name
        if name.trim().is_empty() {
            log::warn!("Skipping Hermes provider with empty name");
            continue;
        }

        // Skip if already exists in database
        if existing_ids.contains(&name) {
            log::debug!("Hermes provider '{name}' already exists in database, skipping");
            continue;
        }

        // Create provider
        let mut provider = Provider::with_id(name.clone(), name.clone(), config, None);
        provider.meta = Some(crate::model::ProviderMeta {
            live_config_managed: Some(true),
            ..Default::default()
        });

        // Save to database
        if let Err(e) = state.db.save_provider("hermes", &provider) {
            log::warn!("Failed to import Hermes provider '{name}': {e}");
            continue;
        }

        imported += 1;
        log::info!("Imported Hermes provider '{name}' from live config");
    }

    Ok(imported)
}

/// Remove a Hermes provider from live config
///
/// This removes a specific provider from ~/.hermes/config.yaml
/// without affecting other providers in the file.
pub fn remove_hermes_provider_from_live(provider_id: &str) -> Result<(), AppError> {
    use crate::apps::hermes;

    // Check if Hermes config directory exists
    if !hermes::get_hermes_dir().exists() {
        log::debug!("Hermes config directory doesn't exist, skipping removal of '{provider_id}'");
        return Ok(());
    }

    hermes::remove_provider(provider_id)?;
    log::info!("Hermes provider '{provider_id}' removed from live config");

    Ok(())
}

/// Remove an OpenClaw provider from live config
///
/// This removes a specific provider from ~/.openclaw/openclaw.json
/// without affecting other providers in the file.
pub fn remove_openclaw_provider_from_live(provider_id: &str) -> Result<(), AppError> {
    use crate::apps::openclaw;

    // Check if OpenClaw config directory exists
    if !openclaw::get_openclaw_dir().exists() {
        log::debug!("OpenClaw config directory doesn't exist, skipping removal of '{provider_id}'");
        return Ok(());
    }

    openclaw::remove_provider(provider_id)?;
    log::info!("OpenClaw provider '{provider_id}' removed from live config");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;

    #[test]
    fn claude_common_config_apply_and_remove_roundtrip_for_non_overlapping_fields() {
        let settings = json!({
            "env": {
                "ANTHROPIC_API_KEY": "sk-test"
            }
        });
        let snippet = r#"{
  "includeCoAuthoredBy": false,
  "env": {
    "CLAUDE_CODE_USE_BEDROCK": "1"
  }
}"#;

        let applied =
            apply_common_config_to_settings(&AppType::Claude, &settings, snippet).unwrap();
        assert_eq!(applied["includeCoAuthoredBy"], json!(false));
        assert_eq!(applied["env"]["CLAUDE_CODE_USE_BEDROCK"], json!("1"));

        let stripped =
            remove_common_config_from_settings(&AppType::Claude, &applied, snippet).unwrap();
        assert_eq!(stripped, settings);
    }

    #[test]
    fn codex_common_config_apply_and_remove_roundtrip_for_non_overlapping_fields() {
        let settings = json!({
            "auth": {
                "OPENAI_API_KEY": "sk-test"
            },
            "config": "model_provider = \"openai\"\n[general]\nmodel = \"gpt-5\"\n"
        });
        let snippet = "[shared]\nreasoning = \"medium\"\n";

        let applied = apply_common_config_to_settings(&AppType::Codex, &settings, snippet).unwrap();
        let applied_config = applied["config"].as_str().unwrap_or_default();
        assert!(applied_config.contains("[shared]"));
        assert!(applied_config.contains("reasoning = \"medium\""));

        let stripped =
            remove_common_config_from_settings(&AppType::Codex, &applied, snippet).unwrap();
        assert_eq!(stripped, settings);
    }

    #[test]
    fn explicit_common_config_flag_overrides_legacy_subset_detection() {
        let mut provider = Provider::with_id(
            "claude-test".to_string(),
            "Claude Test".to_string(),
            json!({
                "includeCoAuthoredBy": false
            }),
            None,
        );
        provider.meta = Some(crate::model::ProviderMeta {
            common_config_enabled: Some(false),
            ..Default::default()
        });

        assert!(
            !provider_uses_common_config(
                &AppType::Claude,
                &provider,
                Some(r#"{ "includeCoAuthoredBy": false }"#),
            ),
            "explicit false should win over legacy subset detection"
        );
    }

    #[test]
    fn claude_common_config_array_subset_detection_and_strip_preserve_extra_items() {
        let settings = json!({
            "allowedTools": ["tool1", "tool2"]
        });
        let snippet = r#"{
  "allowedTools": ["tool1"]
}"#;

        assert!(
            settings_contain_common_config(&AppType::Claude, &settings, snippet),
            "array subset should be detected for legacy providers"
        );

        let stripped =
            remove_common_config_from_settings(&AppType::Claude, &settings, snippet).unwrap();
        assert_eq!(
            stripped,
            json!({
                "allowedTools": ["tool2"]
            })
        );
    }

    #[test]
    fn codex_common_config_array_subset_detection_and_strip_preserve_extra_items() {
        let settings = json!({
            "auth": {},
            "config": "allowed_tools = [\"tool1\", \"tool2\"]\n"
        });
        let snippet = "allowed_tools = [\"tool1\"]\n";

        assert!(
            settings_contain_common_config(&AppType::Codex, &settings, snippet),
            "TOML array subset should be detected for legacy providers"
        );

        let stripped =
            remove_common_config_from_settings(&AppType::Codex, &settings, snippet).unwrap();
        assert_eq!(stripped["auth"], json!({}));
        let stripped_config = stripped["config"].as_str().unwrap_or_default();
        let parsed = stripped_config
            .parse::<DocumentMut>()
            .expect("stripped codex config should remain valid TOML");
        let allowed_tools = parsed["allowed_tools"]
            .as_array()
            .expect("allowed_tools should remain an array");
        let values: Vec<&str> = allowed_tools
            .iter()
            .map(|value| value.as_str().expect("tool id should be string"))
            .collect();
        assert_eq!(values, vec!["tool2"]);
    }

    #[test]
    fn automatic_default_import_allows_unmanaged_official_seed_only() {
        let db = Arc::new(Database::memory().expect("memory db"));
        let state = AppState::new(db);

        state
            .db
            .init_default_official_providers()
            .expect("seed official providers");

        assert!(
            should_auto_import_default_config(&state, &AppType::Claude)
                .expect("claude discovery policy")
        );
        assert!(
            should_auto_import_default_config(&state, &AppType::Codex)
                .expect("codex discovery policy")
        );
        assert!(
            !should_auto_import_default_config(&state, &AppType::ClaudeDesktop)
                .expect("desktop discovery policy")
        );
    }

    #[test]
    fn automatic_default_import_stops_once_ochub_manages_live_config() {
        let db = Arc::new(Database::memory().expect("memory db"));
        let state = AppState::new(db);

        state
            .db
            .init_default_official_providers()
            .expect("seed official providers");
        state
            .db
            .set_current_provider("claude", "claude-official")
            .expect("set current provider");

        assert!(
            !should_auto_import_default_config(&state, &AppType::Claude)
                .expect("managed discovery policy")
        );

        let custom = Provider::with_id(
            "custom".to_string(),
            "Custom".to_string(),
            json!({"auth": {}, "config": ""}),
            None,
        );
        state
            .db
            .save_provider("codex", &custom)
            .expect("save non-seed provider");

        assert!(
            !should_auto_import_default_config(&state, &AppType::Codex)
                .expect("non-seed discovery policy")
        );
        assert!(
            !should_auto_import_default_config(&state, &AppType::OpenCode)
                .expect("additive discovery policy")
        );
    }

    /// Scoped `OCHUB_TEST_HOME` for tests that touch real config paths.
    struct TestHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl TestHome {
        fn new() -> Self {
            let guard = crate::test_support::env_lock();
            let dir = tempfile::tempdir().expect("temp home");
            crate::test_support::set_var("OCHUB_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");
            Self {
                _guard: guard,
                _dir: dir,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            crate::test_support::remove_var("OCHUB_TEST_HOME");
            crate::settings::reload_settings().ok();
        }
    }

    fn claude_provider(id: &str, base_url: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            id.to_string(),
            json!({ "env": { "ANTHROPIC_BASE_URL": base_url } }),
            None,
        )
    }

    #[test]
    fn a_hand_edit_survives_a_switch_without_being_absorbed_by_the_outgoing_provider() {
        let _home = TestHome::new();
        let state = AppState::new(Arc::new(Database::memory().expect("memory db")));

        for provider in [
            claude_provider("alpha", "https://alpha.example"),
            claude_provider("beta", "https://beta.example"),
        ] {
            state
                .db
                .save_provider("claude", &provider)
                .expect("save provider");
        }

        // OcHub writes alpha, which establishes the baseline.
        crate::services::provider::ProviderService::switch(&state, AppType::Claude, "alpha")
            .expect("switch to alpha");

        // The user edits settings.json directly, adding something OcHub has no
        // opinion about.
        let path = get_claude_settings_path();
        let mut live: Value = read_json_file(&path).expect("read live");
        live.as_object_mut()
            .expect("live object")
            .insert("hooks".to_string(), json!({ "PreToolUse": [] }));
        write_json_file(&path, &live).expect("hand edit");

        let result =
            crate::services::provider::ProviderService::switch(&state, AppType::Claude, "beta")
                .expect("switch to beta");

        // The edit is carried onto beta's configuration ...
        let after: Value = read_json_file(&path).expect("read live after switch");
        assert_eq!(after["hooks"], json!({ "PreToolUse": [] }));
        assert_eq!(
            after["env"]["ANTHROPIC_BASE_URL"],
            json!("https://beta.example")
        );

        // ... and is not absorbed into the provider being left behind.
        let alpha = state
            .db
            .get_provider_by_id("alpha", "claude")
            .expect("load alpha")
            .expect("alpha exists");
        assert!(
            alpha.settings_config.get("hooks").is_none(),
            "a file-level edit must not become a property of the outgoing provider"
        );

        let drift = result.drift.expect("switch reports the external edit");
        assert_eq!(drift.preserved, vec!["hooks".to_string()]);
        assert!(!drift.has_conflicts());
    }

    #[test]
    fn saving_the_current_provider_keeps_an_external_edit_and_reports_a_collision() {
        let _home = TestHome::new();
        let state = AppState::new(Arc::new(Database::memory().expect("memory db")));

        let alpha = claude_provider("alpha", "https://alpha.example");
        state
            .db
            .save_provider("claude", &alpha)
            .expect("save provider");
        crate::services::provider::ProviderService::switch(&state, AppType::Claude, "alpha")
            .expect("switch to alpha");

        // One edit OcHub does not own, one it does.
        let path = get_claude_settings_path();
        let mut live: Value = read_json_file(&path).expect("read live");
        {
            let live = live.as_object_mut().expect("live object");
            live.insert("statusLine".to_string(), json!({ "type": "command" }));
            live["env"]["ANTHROPIC_BASE_URL"] = json!("https://hand.example");
        }
        write_json_file(&path, &live).expect("hand edit");

        // Saving an edit to the current provider used to overwrite the file.
        let mut edited = alpha.clone();
        edited.settings_config =
            json!({ "env": { "ANTHROPIC_BASE_URL": "https://edited.example" } });
        crate::services::provider::ProviderService::update(
            &state,
            AppType::Claude,
            Some("alpha"),
            edited,
        )
        .expect("update alpha");

        let after: Value = read_json_file(&path).expect("read live after update");
        assert_eq!(
            after["statusLine"],
            json!({ "type": "command" }),
            "an untouched external edit must survive a provider save"
        );
        assert_eq!(
            after["env"]["ANTHROPIC_BASE_URL"],
            json!("https://edited.example"),
            "the value the user just typed wins the collision"
        );
    }

    fn codex_provider(id: &str, config: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            id.to_string(),
            json!({ "auth": {}, "config": config }),
            None,
        )
    }

    #[test]
    fn a_hand_edited_codex_key_survives_a_switch_that_changes_a_different_key() {
        let _home = TestHome::new();
        let state = AppState::new(Arc::new(Database::memory().expect("memory db")));

        for provider in [
            codex_provider("alpha", "model = \"gpt-5\"\nmodel_provider = \"alpha\"\n"),
            codex_provider("beta", "model = \"gpt-5\"\nmodel_provider = \"beta\"\n"),
        ] {
            state
                .db
                .save_provider("codex", &provider)
                .expect("save provider");
        }

        crate::services::provider::ProviderService::switch(&state, AppType::Codex, "alpha")
            .expect("switch to alpha");

        // The user edits config.toml directly: one key beta also sets, one it
        // knows nothing about.
        let path = get_codex_config_path();
        let edited = format!(
            "{}\napproval_policy = \"never\"\n\n[mcp_servers.mine]\ncommand = \"x\"\n",
            std::fs::read_to_string(&path)
                .expect("read live")
                .replace("model_provider = \"alpha\"", "model_provider = \"hand\"")
        );
        std::fs::write(&path, edited).expect("hand edit");

        let result =
            crate::services::provider::ProviderService::switch(&state, AppType::Codex, "beta")
                .expect("switch to beta");

        let after = std::fs::read_to_string(&path).expect("read live after switch");
        assert!(
            after.contains("approval_policy = \"never\""),
            "an edit beta says nothing about must survive: {after}"
        );
        assert!(
            after.contains("[mcp_servers.mine]"),
            "a hand-added table must survive: {after}"
        );
        assert!(after.contains("model_provider = \"beta\""));

        // The whole file used to be one unresolvable conflict; now only the key
        // both sides set is reported.
        let drift = result.drift.expect("switch reports the external edit");
        assert_eq!(
            drift.conflicts.len(),
            1,
            "unexpected conflicts: {:?}",
            drift.conflicts
        );
        assert_eq!(drift.conflicts[0].path, "config.model_provider");
    }

    #[test]
    fn an_mcp_sync_after_a_switch_is_not_reported_as_an_external_edit() {
        let _home = TestHome::new();
        let state = AppState::new(Arc::new(Database::memory().expect("memory db")));

        for provider in [
            codex_provider("alpha", "model = \"gpt-5\"\nmodel_provider = \"alpha\"\n"),
            codex_provider("beta", "model = \"gpt-5\"\nmodel_provider = \"beta\"\n"),
        ] {
            state
                .db
                .save_provider("codex", &provider)
                .expect("save provider");
        }

        // MCP servers are synced into the same config.toml a switch writes, and
        // the sync runs after the switch has taken its baseline.
        let mut server = crate::db::legacy_json::McpServer {
            id: "local".to_string(),
            name: "local".to_string(),
            server: json!({ "command": "x", "args": [] }),
            apps: Default::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        };
        server.apps.set_enabled_for(&AppType::Codex, true);
        state.db.save_mcp_server(&server).expect("save mcp server");

        crate::services::provider::ProviderService::switch(&state, AppType::Codex, "alpha")
            .expect("switch to alpha");
        assert!(
            std::fs::read_to_string(get_codex_config_path())
                .expect("read live")
                .contains("[mcp_servers.local]"),
            "the sync must have written to the file the baseline covers"
        );

        let result =
            crate::services::provider::ProviderService::switch(&state, AppType::Codex, "beta")
                .expect("switch to beta");

        assert!(
            result.drift.is_none(),
            "OcHub's own MCP sync must not come back as somebody else's edit: {:?}",
            result.drift
        );
    }

    #[test]
    fn an_untouched_live_file_switches_without_reporting_drift() {
        let _home = TestHome::new();
        let state = AppState::new(Arc::new(Database::memory().expect("memory db")));

        for provider in [
            claude_provider("alpha", "https://alpha.example"),
            claude_provider("beta", "https://beta.example"),
        ] {
            state
                .db
                .save_provider("claude", &provider)
                .expect("save provider");
        }

        crate::services::provider::ProviderService::switch(&state, AppType::Claude, "alpha")
            .expect("switch to alpha");
        let result =
            crate::services::provider::ProviderService::switch(&state, AppType::Claude, "beta")
                .expect("switch to beta");

        assert!(
            result.drift.is_none(),
            "a file only OcHub has written must not look edited"
        );
    }
}
