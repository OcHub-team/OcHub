//! The app plugin system.
//!
//! Every managed app — the six built-ins and future user-defined manifest
//! apps — is described by one [`AppPlugin`] behind the process-wide
//! [`registry`]. Per-app *data* (labels, dirs, modes, capability flags) lives
//! on the trait; per-app *leaf writers* live behind [`LiveConfigOps`];
//! stateful orchestration (switching transactions, MCP/skill
//! sync) stays in `services` and consults the registry for iteration and
//! enable/disable gating.

mod builtin;
pub mod capabilities;
pub mod format;
pub mod hooks;
pub mod loader;
pub mod manifest;
pub mod manifest_codec;
pub mod plugin_manifest;
pub mod registry;

#[cfg(test)]
mod manifest_tests;

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

pub use capabilities::LiveConfigOps;
pub use hooks::HookRegistry;
pub use loader::{
    ManifestLoadError, load_and_register_user_plugins, manifest_load_errors, reload_user_plugins,
    user_plugins_dir,
};
pub use manifest::{AppManifest, ManifestError};
pub use manifest_codec::ManifestCodec;
pub use plugin_manifest::{ManifestPlugin, ManifestSource};
pub use registry::{
    all_plugins, all_plugins_snapshot, enabled_plugins, ensure_app_enabled,
    ensure_app_type_enabled, get_plugin, is_app_enabled, is_app_type_enabled, register_plugin,
    unregister_plugin,
};

use crate::app_id::AppId;
use crate::error::AppError;
use crate::provider_config::AppConfig;

/// Process-wide registry of the native hooks manifests may reference.
pub fn builtin_hooks() -> Arc<HookRegistry> {
    static HOOKS: LazyLock<Arc<HookRegistry>> = LazyLock::new(|| Arc::new(HookRegistry::builtin()));
    HOOKS.clone()
}

/// How providers map onto the app's live config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Only the current provider is written to the live config.
    Switch,
    /// All providers are written to the live config.
    Additive,
}

/// Everything OcHub needs to know about one managed app.
///
/// Object-safe; implementable both by the built-in native plugins and by the
/// generic manifest-driven plugin.
pub trait AppPlugin: Send + Sync + 'static {
    // ---- identity & UI metadata ----
    fn id(&self) -> &AppId;
    fn display_name(&self) -> &str;
    /// Symbolic icon key the UI crate maps to a bundled icon; empty string
    /// means "no bundled icon" and the UI renders a letter avatar.
    fn icon_id(&self) -> &str {
        ""
    }
    /// 0xRRGGBB accent color.
    fn accent_color(&self) -> u32;
    /// Display order; built-ins use 0..=60, user plugins append after.
    fn sort_order(&self) -> i32;
    /// Whether the app is enabled when the settings map has no entry for it.
    fn enabled_by_default(&self) -> bool {
        true
    }
    /// True for plugins loaded from a user manifest file.
    fn is_user_manifest(&self) -> bool {
        false
    }

    // ---- switching semantics ----
    fn mode(&self) -> AppMode;

    // ---- paths ----
    /// The app's live config directory (honors any user override).
    fn config_dir(&self) -> Result<PathBuf, AppError>;

    // ---- provider editor codec ----
    fn provider_config(&self) -> Option<Box<dyn AppConfig>>;

    // ---- live-config operations ----
    fn live(&self) -> &dyn LiveConfigOps;

    // ---- optional capabilities ----
    fn supports_mcp(&self) -> bool {
        false
    }
    fn supports_skills(&self) -> bool {
        false
    }
}
