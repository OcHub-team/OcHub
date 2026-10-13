//! Loading user manifest plugins from `~/.ochub/apps/*.toml`.
//!
//! Every failure — unreadable file, TOML/parse error, semantic check failure, or
//! an id that collides with a built-in or an already-loaded manifest — becomes a
//! [`ManifestLoadError`] entry rather than a panic, so one bad file never blocks
//! the rest. The last load's errors are cached for the UI to display.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};

use serde::Serialize;

use super::hooks::HookRegistry;
use super::manifest::AppManifest;
use super::plugin_manifest::{ManifestPlugin, ManifestSource};
use super::{AppPlugin, all_plugins, register_plugin};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// The directory user manifest plugins live in (`~/.ochub/apps`).
pub fn user_plugins_dir() -> PathBuf {
    crate::paths::get_app_config_dir().join("apps")
}

/// One manifest that failed to load, for UI display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestLoadError {
    pub path: String,
    pub message: String,
}

impl ManifestLoadError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// The outcome of scanning the user plugins dir: parsed+checked plugins plus a
/// per-file error list. Registration collisions are detected later.
pub struct LoadedPlugins {
    pub plugins: Vec<Arc<ManifestPlugin>>,
    pub errors: Vec<ManifestLoadError>,
}

/// Parse and check every `*.toml` under the user plugins dir (sorted by name).
/// A missing dir yields an empty result; every failure is an error entry.
pub fn load_user_manifests(hooks: Arc<HookRegistry>) -> LoadedPlugins {
    let dir = user_plugins_dir();
    let mut plugins = Vec::new();
    let mut errors = Vec::new();

    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedPlugins { plugins, errors };
        }
        Err(error) => {
            errors.push(ManifestLoadError::new(
                dir.display().to_string(),
                error.to_string(),
            ));
            return LoadedPlugins { plugins, errors };
        }
    };

    let mut files = Vec::new();
    for entry in read {
        match entry {
            Ok(entry)
                if entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("toml") =>
            {
                files.push(entry.path())
            }
            Ok(_) => {}
            Err(error) => errors.push(ManifestLoadError::new(
                dir.display().to_string(),
                error.to_string(),
            )),
        }
    }
    files.sort();

    for path in files {
        let path_str = path.display().to_string();
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES => {}
            Ok(_) => {
                errors.push(ManifestLoadError::new(
                    path_str,
                    "manifest must be a regular file no larger than 1 MiB",
                ));
                continue;
            }
            Err(error) => {
                errors.push(ManifestLoadError::new(path_str, error.to_string()));
                continue;
            }
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                errors.push(ManifestLoadError::new(path_str, e.to_string()));
                continue;
            }
        };
        let manifest = match AppManifest::parse(&content) {
            Ok(manifest) => manifest,
            Err(e) => {
                errors.push(ManifestLoadError::new(path_str, e.to_string()));
                continue;
            }
        };
        match ManifestPlugin::from_manifest(manifest, hooks.clone(), ManifestSource::User(path)) {
            Ok(plugin) => plugins.push(plugin),
            Err(e) => errors.push(ManifestLoadError::new(path_str, e.to_string())),
        }
    }

    LoadedPlugins { plugins, errors }
}

static LOAD_ERRORS: LazyLock<RwLock<Vec<ManifestLoadError>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Load user manifests and register them, caching the error list for the UI.
/// An id colliding with a built-in or an earlier manifest is an error entry.
pub fn load_and_register_user_plugins() -> Vec<ManifestLoadError> {
    let loaded = load_user_manifests(super::builtin_hooks());
    let mut errors = loaded.errors;

    for plugin in loaded.plugins {
        let path = plugin
            .source_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if let Err(e) = register_plugin(plugin as Arc<dyn AppPlugin>) {
            errors.push(ManifestLoadError::new(path, e.to_string()));
        }
    }

    if let Ok(mut guard) = LOAD_ERRORS.write() {
        *guard = errors.clone();
    }
    errors
}

/// Load and validate a candidate set before atomically replacing user plugins.
/// A malformed update keeps every previous adapter available; callers can show
/// errors and retry after repairing files. Deleted files disappear on success.
pub fn reload_user_plugins() -> Vec<ManifestLoadError> {
    let loaded = load_user_manifests(super::builtin_hooks());
    let mut errors = loaded.errors;
    let mut ids = all_plugins()
        .into_iter()
        .filter(|plugin| !plugin.is_user_manifest())
        .map(|plugin| plugin.id().clone())
        .collect::<BTreeSet<_>>();
    for plugin in &loaded.plugins {
        if !ids.insert(plugin.id().clone()) {
            errors.push(ManifestLoadError::new(
                plugin
                    .source_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                format!("应用 ID 冲突: {} 已被注册", plugin.id()),
            ));
        }
    }
    if errors.is_empty()
        && let Err(error) = super::registry::replace_user_plugins(
            loaded
                .plugins
                .into_iter()
                .map(|plugin| plugin as Arc<dyn AppPlugin>)
                .collect(),
        )
    {
        errors.push(ManifestLoadError::new(
            user_plugins_dir().display().to_string(),
            error.to_string(),
        ));
    }
    if let Ok(mut guard) = LOAD_ERRORS.write() {
        *guard = errors.clone();
    }
    errors
}

/// The cached error list from the most recent load/reload.
pub fn manifest_load_errors() -> Vec<ManifestLoadError> {
    LOAD_ERRORS.read().map(|g| g.clone()).unwrap_or_default()
}
