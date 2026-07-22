//! Loading user manifest plugins from `~/.ochub/apps/*.toml`.
//!
//! Every failure — unreadable file, TOML/parse error, semantic check failure, or
//! an id that collides with a built-in or an already-loaded manifest — becomes a
//! [`ManifestLoadError`] entry rather than a panic, so one bad file never blocks
//! the rest. The last load's errors are cached for the UI to display.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};

use serde::Serialize;

use super::hooks::HookRegistry;
use super::manifest::AppManifest;
use super::plugin_manifest::{ManifestPlugin, ManifestSource};
use super::{all_plugins, register_plugin, unregister_plugin, AppPlugin};

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
        Err(_) => {
            return LoadedPlugins { plugins, errors };
        }
    };

    let mut files: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    files.sort();

    for path in files {
        let path_str = path.display().to_string();
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

/// Unregister all currently-registered user-manifest plugins, then reload.
pub fn reload_user_plugins() -> Vec<ManifestLoadError> {
    for plugin in all_plugins() {
        if plugin.is_user_manifest() {
            let _ = unregister_plugin(plugin.id());
        }
    }
    load_and_register_user_plugins()
}

/// The cached error list from the most recent load/reload.
pub fn manifest_load_errors() -> Vec<ManifestLoadError> {
    LOAD_ERRORS.read().map(|g| g.clone()).unwrap_or_default()
}
