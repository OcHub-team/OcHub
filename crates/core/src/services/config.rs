//! Common-config-snippet management + config.json backups.
//!
//! Ported from cc-switch `src-tauri/src/services/config.rs` (backups) and the
//! common-config-snippet command surface in `src-tauri/src/commands/config.rs`.
//!
//! The snippet *extraction* / *merge* logic itself lives on
//! `ProviderService::{extract_common_config_snippet,
//! extract_common_config_snippet_from_settings, migrate_legacy_common_config_usage,
//! sync_current_provider_for_app}` (already ported); `ConfigService` is the
//! transport-agnostic wrapper the UI commands used to call.
//!
//! The reference `services/config.rs` also had
//! `sync_current_providers_to_live(&mut MultiAppConfig)` /
//! `sync_current_provider_for_app(&mut MultiAppConfig, ...)` for the legacy JSON
//! `MultiAppConfig`. OCHUB uses SQLite as SSOT and routes live writes
//! through `ProviderService::sync_current_provider_for_app(state, app)` instead.

use std::fs;
use std::path::Path;

use chrono::Utc;

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::services::provider::ProviderService;

const MAX_BACKUPS: usize = 10;

/// 配置导入导出 + 通用配置片段相关业务逻辑
pub struct ConfigService;

impl ConfigService {
    // ========================================================================
    // 通用配置片段（common config snippet）
    // ========================================================================

    /// 校验通用配置片段格式（claude/omo/omo-slim 为 JSON，codex 为 TOML）。
    pub fn validate_common_config_snippet(app_type: &str, snippet: &str) -> Result<(), AppError> {
        if snippet.trim().is_empty() {
            return Ok(());
        }

        match app_type {
            "claude" | "omo" | "omo-slim" => {
                serde_json::from_str::<serde_json::Value>(snippet)
                    .map_err(|e| AppError::Config(format!("无效的 JSON 格式: {e}")))?;
            }
            "codex" => {
                snippet
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|e| AppError::Config(format!("无效的 TOML 格式: {e}")))?;
            }
            _ => {}
        }

        Ok(())
    }

    /// 读取指定应用的通用配置片段。
    pub fn get_common_config_snippet(
        state: &AppState,
        app_type: &str,
    ) -> Result<Option<String>, AppError> {
        state.db.get_config_snippet(app_type)
    }

    /// 写入指定应用的通用配置片段。
    ///
    /// 与 cc-switch `commands::config::set_common_config_snippet` 行为一致：
    /// - 校验片段格式
    /// - 对 claude/codex，先迁移旧 inline snippet 用法，再写库并重新同步
    ///   当前供应商到 live 配置
    /// - 对 omo / omo-slim，若已有当前供应商则重写其 live 配置文件
    pub fn set_common_config_snippet(
        state: &AppState,
        app_type: &str,
        snippet: String,
    ) -> Result<(), AppError> {
        let is_cleared = snippet.trim().is_empty();
        let old_snippet = state.db.get_config_snippet(app_type)?;

        Self::validate_common_config_snippet(app_type, &snippet)?;

        let value = if is_cleared { None } else { Some(snippet) };

        if matches!(app_type, "claude" | "codex") {
            if let Some(legacy_snippet) = old_snippet
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                let app = app_type.parse::<AppType>()?;
                ProviderService::migrate_legacy_common_config_usage(state, app, legacy_snippet)?;
            }
        }

        state.db.set_config_snippet(app_type, value)?;
        state.db.set_config_snippet_cleared(app_type, is_cleared)?;

        if matches!(app_type, "claude" | "codex") {
            let app = app_type.parse::<AppType>()?;
            ProviderService::sync_current_provider_for_app(state, app)?;
        }

        if app_type == "omo"
            && state
                .db
                .get_current_omo_provider("opencode", "omo")?
                .is_some()
        {
            crate::services::OmoService::write_config_to_file(
                state,
                &crate::services::omo::STANDARD,
            )?;
        }
        if app_type == "omo-slim"
            && state
                .db
                .get_current_omo_provider("opencode", "omo-slim")?
                .is_some()
        {
            crate::services::OmoService::write_config_to_file(state, &crate::services::omo::SLIM)?;
        }

        Ok(())
    }

    /// 从指定应用的当前供应商配置中提取通用配置片段。
    ///
    /// 若提供 `settings_config` JSON，则直接从该 settings 提取；否则从当前供应商提取。
    pub fn extract_common_config_snippet(
        state: &AppState,
        app: AppType,
        settings_config: Option<&serde_json::Value>,
    ) -> Result<String, AppError> {
        if let Some(settings) = settings_config {
            return ProviderService::extract_common_config_snippet_from_settings(app, settings);
        }
        ProviderService::extract_common_config_snippet(state, app)
    }

    // ========================================================================
    // config.json 备份
    // ========================================================================

    /// 为当前 config.json 创建备份，返回备份 ID（若文件不存在则返回空字符串）。
    pub fn create_backup(config_path: &Path) -> Result<String, AppError> {
        if !config_path.exists() {
            return Ok(String::new());
        }

        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let backup_id = format!("backup_{timestamp}");

        let backup_dir = config_path
            .parent()
            .ok_or_else(|| AppError::Config("Invalid config path".into()))?
            .join("backups");

        fs::create_dir_all(&backup_dir).map_err(|e| AppError::io(&backup_dir, e))?;

        let backup_path = backup_dir.join(format!("{backup_id}.json"));
        let contents = fs::read(config_path).map_err(|e| AppError::io(config_path, e))?;
        fs::write(&backup_path, contents).map_err(|e| AppError::io(&backup_path, e))?;

        Self::cleanup_old_backups(&backup_dir, MAX_BACKUPS)?;

        Ok(backup_id)
    }

    fn cleanup_old_backups(backup_dir: &Path, retain: usize) -> Result<(), AppError> {
        if retain == 0 {
            return Ok(());
        }

        let entries = match fs::read_dir(backup_dir) {
            Ok(iter) => iter
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .map(|ext| ext == "json")
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>(),
            Err(_) => return Ok(()),
        };

        if entries.len() <= retain {
            return Ok(());
        }

        let remove_count = entries.len().saturating_sub(retain);
        let mut sorted = entries;

        sorted.sort_by(|a, b| {
            let a_time = a.metadata().and_then(|m| m.modified()).ok();
            let b_time = b.metadata().and_then(|m| m.modified()).ok();
            a_time.cmp(&b_time)
        });

        for entry in sorted.into_iter().take(remove_count) {
            if let Err(err) = fs::remove_file(entry.path()) {
                log::warn!(
                    "Failed to remove old backup {}: {}",
                    entry.path().display(),
                    err
                );
            }
        }

        Ok(())
    }
}
