//! [`AppPlugin`] wrapper around a checked [`AppManifest`].
//!
//! Turns a manifest into a live plugin: metadata/capabilities read straight off
//! the manifest, the provider codec is the generic [`ManifestCodec`], and the
//! live-config surface prepares all declared files (honoring `replace` /
//! `merge_shallow`, per-file modes, and `absent_preserves`) for the host's
//! recoverable writer. Preview and execution consume the same prepared bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::app_id::AppId;
use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;
use crate::provider_config::AppConfig;
use crate::services::config_commit::{PreparedConfigWrite, PreparedFileWrite, execute_files};

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
    configuration_revision: String,
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
        // The checked structure, rather than file mtime or path, identifies
        // the adapter. Cosmetic comments do not invalidate an editing session.
        let configuration_revision = Sha256::digest(format!("{manifest:?}").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Arc::new(Self {
            manifest: Arc::new(manifest),
            hooks,
            source,
            id,
            configuration_revision,
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
            .with_preview_root(self.resolve_config_dir())
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

    fn manifest_path(&self) -> Option<&Path> {
        self.source_path()
    }

    fn configuration_revision(&self) -> Option<&str> {
        Some(&self.configuration_revision)
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
    fn prepare_write(
        &self,
        _db: &Database,
        provider: &Provider,
    ) -> Result<Option<PreparedConfigWrite>, AppError> {
        self.prepare_config_write(provider).map(Some)
    }

    fn write_live(&self, _db: &Database, provider: &Provider) -> Result<(), AppError> {
        execute_files(&self.prepare_config_write(provider)?)
    }

    fn remove_from_live(&self, _provider_id: &str) -> Result<(), AppError> {
        Err(AppError::InvalidInput(format!(
            "应用 {} 不支持从 live 配置中移除单个供应商",
            self.id
        )))
    }

    fn read_live(&self) -> Result<Value, AppError> {
        let config_dir = self.resolve_config_dir();
        let mut result = Map::new();
        for file in &self.manifest.files {
            let path = checked_file_path(&config_dir, file)?;
            let Some(content) = read_file_bytes(&path)? else {
                continue;
            };
            let value = parse_file_bytes(file, &content)?;
            result.insert(file.store_key.clone(), value);
        }
        Ok(Value::Object(result))
    }
}

impl ManifestPlugin {
    fn prepare_config_write(&self, provider: &Provider) -> Result<PreparedConfigWrite, AppError> {
        if let Some(name) = &self.manifest.hooks.live_validate
            && let Some(hook) = self.hooks.live_validate(name)
        {
            hook(provider)?;
        }
        let config_dir = self.resolve_config_dir();
        let mut files = Vec::with_capacity(self.manifest.files.len());
        for file in &self.manifest.files {
            let path = checked_file_path(&config_dir, file)?;
            let store = provider.settings_config.get(&file.store_key);
            // Skipped files have no operation, including permission changes.
            // The same missing-store rule applies to replace and merge modes.
            if file.absent_preserves && matches!(store, None | Some(Value::Null)) {
                continue;
            }
            let before = read_file_bytes(&path)?;
            let value = match file.write {
                WriteMode::Replace => store.cloned().unwrap_or_else(|| Value::Object(Map::new())),
                WriteMode::MergeShallow => {
                    let mut merged = match before.as_deref() {
                        Some(bytes) => parse_file_bytes(file, bytes)?,
                        None => Value::Object(Map::new()),
                    };
                    let merged_obj = merged.as_object_mut().ok_or_else(|| {
                        AppError::InvalidInput(format!("文件 {} 的现有内容不是对象", file.id))
                    })?;
                    if let Some(store) = store.filter(|value| !value.is_null()) {
                        let store_obj = store.as_object().ok_or_else(|| {
                            AppError::InvalidInput(format!("文件 {} 的候选内容不是对象", file.id))
                        })?;
                        merged_obj.extend(
                            store_obj
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        );
                    }
                    merged
                }
            };
            let after = super::format::serialize(file.format, &value)
                .map_err(AppError::InvalidInput)?
                .into_bytes();
            files.push(PreparedFileWrite {
                path,
                before,
                after: Some(after),
                file_mode: file.file_mode_u32(),
                dir_mode: file.dir_mode_u32(),
            });
        }
        Ok(PreparedConfigWrite { files })
    }
}

fn read_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn parse_file_bytes(file: &FileSpec, bytes: &[u8]) -> Result<Value, AppError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|_| AppError::InvalidInput(format!("文件 {} 不是 UTF-8 文本", file.id)))?;
    if content.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    super::format::parse(file.format, content, &file.id).map_err(AppError::InvalidInput)
}

/// A root may itself be a user-selected symlink. Symlinks below that root are
/// refused so declared files cannot be redirected outside the selected scope.
fn checked_file_path(config_dir: &Path, file: &FileSpec) -> Result<PathBuf, AppError> {
    super::manifest::validate_file_path(&file.path)
        .map_err(|error| AppError::InvalidInput(error.to_string()))?;
    let mut path = absolute_root(config_dir)?;
    let components = Path::new(&file.path).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(AppError::InvalidInput(format!(
                        "插件配置路径不能经过符号链接: {}",
                        path.display()
                    )));
                }
                let last = index + 1 == components.len();
                if (!last && !metadata.is_dir()) || (last && !metadata.is_file()) {
                    return Err(AppError::InvalidInput(format!(
                        "插件配置目标的文件类型不匹配: {}",
                        path.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::io(&path, error)),
        }
    }
    Ok(path)
}

fn absolute_root(root: &Path) -> Result<PathBuf, AppError> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AppError::io(root, error))?
            .join(root)
    };
    // Canonicalize the existing prefix without creating directories during
    // preview. Resolve a user-selected symlink root before adding file paths.
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| AppError::io(existing, error))?;
                missing.push(name.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| AppError::InvalidInput("配置目录没有可解析的父目录".into()))?;
            }
            Err(error) => return Err(AppError::io(existing, error)),
        }
    }
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
