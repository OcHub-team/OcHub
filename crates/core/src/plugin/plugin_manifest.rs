//! [`AppPlugin`] wrapper around a checked [`AppManifest`].
//!
//! Turns a manifest into a live plugin: metadata/capabilities read straight off
//! the manifest, the provider codec is the generic [`ManifestCodec`], and the
//! live-config surface writes each declared file (honoring `replace` /
//! `merge_shallow`, `clear_when`-emptied stores, per-file modes, and
//! `absent_preserves`) then runs the manifest's post-write hooks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::app_id::AppId;
use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;
use crate::provider_config::AppConfig;

use super::capabilities::LiveConfigOps;
use super::hooks::HookRegistry;
use super::manifest::{AppManifest, FileSpec, ManifestError, WriteMode};
use super::manifest_codec::ManifestCodec;
use super::{AppMode, AppPlugin};

/// Where a manifest plugin was loaded from.
#[derive(Debug, Clone)]
pub enum ManifestSource {
    /// Embedded in the binary (a built-in expressed as a manifest).
    BuiltIn,
    /// Loaded from a user file under `~/.ochub/apps`.
    User(PathBuf),
}

/// A plugin whose entire behavior is described by a manifest.
pub struct ManifestPlugin {
    manifest: Arc<AppManifest>,
    hooks: Arc<HookRegistry>,
    source: ManifestSource,
    id: AppId,
}

impl ManifestPlugin {
    /// Build a plugin from a manifest, running [`AppManifest::check`] first.
    pub fn from_manifest(
        manifest: AppManifest,
        hooks: Arc<HookRegistry>,
        source: ManifestSource,
    ) -> Result<Arc<Self>, ManifestError> {
        manifest.check(&hooks)?;
        let id = manifest.app_id()?;
        Ok(Arc::new(Self {
            manifest: Arc::new(manifest),
            hooks,
            source,
            id,
        }))
    }

    pub fn manifest(&self) -> &Arc<AppManifest> {
        &self.manifest
    }

    /// The source file for a user plugin (`None` for built-ins).
    pub fn source_path(&self) -> Option<&Path> {
        match &self.source {
            ManifestSource::User(path) => Some(path.as_path()),
            ManifestSource::BuiltIn => None,
        }
    }

    /// The provider-editor codec for this manifest.
    pub fn codec(&self) -> ManifestCodec {
        ManifestCodec::new(self.manifest.clone(), self.hooks.clone())
    }

    /// Resolve the config dir: settings override for this id, else the manifest
    /// default (expanding a leading `~/`).
    fn resolve_config_dir(&self) -> PathBuf {
        if let Some(dir) = crate::settings::get_settings().app_config_dir_override(self.id.as_str())
        {
            return dir;
        }
        expand_tilde(self.manifest.config_dir_default())
    }
}

impl AppPlugin for ManifestPlugin {
    fn id(&self) -> &AppId {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.manifest.app.name
    }

    fn icon_id(&self) -> &str {
        self.manifest.icon_key()
    }

    fn accent_color(&self) -> u32 {
        self.manifest.accent_u32()
    }

    fn sort_order(&self) -> i32 {
        self.manifest.app.sort_order
    }

    fn enabled_by_default(&self) -> bool {
        self.manifest.app.enabled_by_default
    }

    fn is_user_manifest(&self) -> bool {
        matches!(self.source, ManifestSource::User(_))
    }

    fn mode(&self) -> AppMode {
        self.manifest.mode()
    }

    fn config_dir(&self) -> Result<PathBuf, AppError> {
        Ok(self.resolve_config_dir())
    }

    fn provider_config(&self) -> Option<Box<dyn AppConfig>> {
        Some(Box::new(self.codec()))
    }

    fn live(&self) -> &dyn LiveConfigOps {
        self
    }

    // v1 manifest apps focus on switch-mode provider config; the optional
    // capabilities stay off.
    fn supports_mcp(&self) -> bool {
        false
    }

    fn supports_skills(&self) -> bool {
        false
    }
}

impl LiveConfigOps for ManifestPlugin {
    fn write_live(&self, _db: &Database, provider: &Provider) -> Result<(), AppError> {
        // 1. live_validate precondition.
        if let Some(name) = &self.manifest.hooks.live_validate
            && let Some(hook) = self.hooks.live_validate(name)
        {
            hook(provider)?;
        }

        let config_dir = self.resolve_config_dir();

        // 2. Write each declared file in order.
        for file in &self.manifest.files {
            self.write_file(&config_dir, file, provider)?;
        }

        // 3. post_write side effects, in declared order.
        for name in &self.manifest.hooks.post_write {
            if let Some(hook) = self.hooks.post_write(name) {
                hook(provider, &config_dir)?;
            }
        }

        Ok(())
    }

    fn remove_from_live(&self, _provider_id: &str) -> Result<(), AppError> {
        // Additive removal is not supported for v1 manifest apps; switch-mode
        // apps never remove a single provider (same as the built-ins).
        Err(AppError::InvalidInput(format!(
            "应用 {} 不支持从 live 配置中移除单个供应商",
            self.id
        )))
    }

    fn read_live(&self) -> Result<Value, AppError> {
        let config_dir = self.resolve_config_dir();
        let mut result = Map::new();
        for file in &self.manifest.files {
            let path = config_dir.join(&file.path);
            if !path.exists() {
                continue;
            }
            let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
            let value = super::format::parse(file.format, &content, &file.id)
                .map_err(AppError::InvalidInput)?;
            result.insert(file.store_key.clone(), value);
        }
        Ok(Value::Object(result))
    }
}

impl ManifestPlugin {
    fn write_file(
        &self,
        config_dir: &Path,
        file: &FileSpec,
        provider: &Provider,
    ) -> Result<(), AppError> {
        let path = config_dir.join(&file.path);
        let store = provider.settings_config.get(&file.store_key);

        match file.write {
            WriteMode::Replace => {
                // The store already reflects form-level clear_when (encode emptied
                // it), so live write simply serializes whatever is stored — an
                // empty store yields an empty file (Gemini's OAuth `.env`).
                let store_value = store.cloned().unwrap_or_else(|| Value::Object(Map::new()));
                let content = super::format::serialize(file.format, &store_value)
                    .map_err(AppError::InvalidInput)?;
                write_leaf(
                    &path,
                    content.as_bytes(),
                    file.dir_mode_u32(),
                    file.file_mode_u32(),
                    file.atomic,
                )
            }
            WriteMode::MergeShallow => {
                // absent_preserves: a missing/null store leaves the file untouched.
                if file.absent_preserves && matches!(store, None | Some(Value::Null)) {
                    return Ok(());
                }
                let store_obj = store.and_then(Value::as_object);

                let mut merged = read_existing(&path, file)?;
                let merged_obj = merged.as_object_mut().ok_or_else(|| {
                    AppError::InvalidInput(format!("文件 {} 的现有内容不是对象", file.id))
                })?;
                if let Some(obj) = store_obj {
                    for (k, v) in obj {
                        merged_obj.insert(k.clone(), v.clone());
                    }
                }

                if matches!(file.format, super::manifest::FileFormat::Json) {
                    // Match the native codec's canonical (sorted, atomic) JSON write.
                    crate::paths::write_json_file(&path, &merged)
                } else {
                    let content = super::format::serialize(file.format, &merged)
                        .map_err(AppError::InvalidInput)?;
                    write_leaf(
                        &path,
                        content.as_bytes(),
                        file.dir_mode_u32(),
                        file.file_mode_u32(),
                        file.atomic,
                    )
                }
            }
        }
    }
}

/// Read the existing file as a Value (default empty object when absent).
fn read_existing(path: &Path, file: &FileSpec) -> Result<Value, AppError> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;
    if content.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    super::format::parse(file.format, &content, &file.id).map_err(AppError::InvalidInput)
}

/// Write a leaf file, creating the parent dir and applying unix modes.
fn write_leaf(
    path: &Path,
    bytes: &[u8],
    _dir_mode: Option<u32>,
    _file_mode: Option<u32>,
    atomic: bool,
) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        #[cfg(unix)]
        if let Some(mode) = _dir_mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(mode);
            fs::set_permissions(parent, perms).map_err(|e| AppError::io(parent, e))?;
        }
    }

    if atomic {
        crate::paths::atomic_write(path, bytes)?;
    } else {
        fs::write(path, bytes).map_err(|e| AppError::io(path, e))?;
    }

    #[cfg(unix)]
    if let Some(mode) = _file_mode {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms).map_err(|e| AppError::io(path, e))?;
    }

    Ok(())
}

/// Expand a leading `~/` (or bare `~`) against the resolved home dir.
fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return crate::paths::get_home_dir();
    }
    if let Some(stripped) = trimmed.strip_prefix("~/") {
        return crate::paths::get_home_dir().join(stripped);
    }
    PathBuf::from(trimmed)
}
