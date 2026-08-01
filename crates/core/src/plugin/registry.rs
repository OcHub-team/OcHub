//! Process-wide plugin registry.
//!
//! Seeded with the built-in plugins on first access; the manifest loader
//! registers user plugins at startup / on manual reload. Lookups clone `Arc`s
//! and drop the lock immediately — never hold the guard across `.await`.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use crate::app_id::AppId;
use crate::error::AppError;

use super::AppPlugin;
use super::builtin::builtin_plugins;

pub struct PluginRegistry {
    plugins: BTreeMap<AppId, Arc<dyn AppPlugin>>,
    builtin_ids: Vec<AppId>,
    sorted: Arc<[Arc<dyn AppPlugin>]>,
}

impl PluginRegistry {
    fn with_builtins() -> Self {
        let mut plugins = BTreeMap::new();
        let mut builtin_ids = Vec::new();
        for plugin in builtin_plugins() {
            builtin_ids.push(plugin.id().clone());
            plugins.insert(plugin.id().clone(), plugin);
        }
        let mut registry = Self {
            plugins,
            builtin_ids,
            sorted: Arc::from([]),
        };
        registry.rebuild_sorted();
        registry
    }

    fn is_builtin(&self, id: &AppId) -> bool {
        self.builtin_ids.contains(id)
    }

    pub fn register(&mut self, plugin: Arc<dyn AppPlugin>) -> Result<(), AppError> {
        let id = plugin.id().clone();
        if self.plugins.contains_key(&id) {
            return Err(AppError::InvalidInput(format!(
                "应用 ID 冲突: {id} 已被注册"
            )));
        }
        self.plugins.insert(id, plugin);
        self.rebuild_sorted();
        Ok(())
    }

    pub fn unregister(&mut self, id: &AppId) -> Result<(), AppError> {
        if self.is_builtin(id) {
            return Err(AppError::InvalidInput(format!("内置应用不可注销: {id}")));
        }
        if self.plugins.remove(id).is_some() {
            self.rebuild_sorted();
        }
        Ok(())
    }

    pub fn get(&self, id: &AppId) -> Option<Arc<dyn AppPlugin>> {
        self.plugins.get(id).cloned()
    }

    fn rebuild_sorted(&mut self) {
        let mut all: Vec<_> = self.plugins.values().cloned().collect();
        all.sort_by(|a, b| {
            a.sort_order()
                .cmp(&b.sort_order())
                .then_with(|| a.id().cmp(b.id()))
        });
        self.sorted = all.into();
    }

    fn sorted(&self) -> Arc<[Arc<dyn AppPlugin>]> {
        self.sorted.clone()
    }
}

static REGISTRY: LazyLock<RwLock<PluginRegistry>> =
    LazyLock::new(|| RwLock::new(PluginRegistry::with_builtins()));

/// Look up one plugin by id.
pub fn get_plugin(id: &AppId) -> Option<Arc<dyn AppPlugin>> {
    REGISTRY.read().ok()?.get(id)
}

/// All registered plugins sorted by `(sort_order, id)`.
pub fn all_plugins() -> Vec<Arc<dyn AppPlugin>> {
    all_plugins_snapshot().iter().cloned().collect()
}

/// Cached sorted registry snapshot. Render paths can retain this `Arc` instead
/// of sorting and cloning the full plugin set on every frame.
pub fn all_plugins_snapshot() -> Arc<[Arc<dyn AppPlugin>]> {
    REGISTRY
        .read()
        .map(|registry| registry.sorted())
        .unwrap_or_else(|_| Arc::from([]))
}

/// [`all_plugins`] filtered by the enabled map in settings.
pub fn enabled_plugins() -> Vec<Arc<dyn AppPlugin>> {
    let overrides = crate::settings::enabled_apps_snapshot();
    all_plugins_snapshot()
        .iter()
        .filter(|plugin| {
            overrides
                .as_ref()
                .and_then(|enabled| enabled.get(plugin.id().as_str()).copied())
                .unwrap_or_else(|| plugin.enabled_by_default())
        })
        .cloned()
        .collect()
}

/// Whether the app is currently enabled (settings map, falling back to the
/// plugin's default).
pub fn is_app_enabled(plugin: &dyn AppPlugin) -> bool {
    crate::settings::app_enabled_override(plugin.id().as_str())
        .unwrap_or_else(|| plugin.enabled_by_default())
}

/// The single enable/disable enforcement helper: resolves the plugin and
/// rejects disabled apps.
pub fn ensure_app_enabled(id: &AppId) -> Result<Arc<dyn AppPlugin>, AppError> {
    let plugin =
        get_plugin(id).ok_or_else(|| AppError::InvalidInput(format!("未知的应用类型: {id}")))?;
    if is_app_enabled(plugin.as_ref()) {
        Ok(plugin)
    } else {
        Err(AppError::AppDisabled(id.to_string()))
    }
}

/// Enabled check for a builtin [`AppType`](crate::app_type::AppType).
pub fn is_app_type_enabled(app: &crate::app_type::AppType) -> bool {
    get_plugin(&app.app_id())
        .map(|p| is_app_enabled(p.as_ref()))
        .unwrap_or(false)
}

/// [`ensure_app_enabled`] for a builtin [`AppType`](crate::app_type::AppType).
pub fn ensure_app_type_enabled(app: &crate::app_type::AppType) -> Result<(), AppError> {
    ensure_app_enabled(&app.app_id()).map(|_| ())
}

/// Extension point for the manifest loader.
pub fn register_plugin(plugin: Arc<dyn AppPlugin>) -> Result<(), AppError> {
    REGISTRY
        .write()
        .map_err(|e| AppError::Lock(e.to_string()))?
        .register(plugin)
}

pub fn unregister_plugin(id: &AppId) -> Result<(), AppError> {
    REGISTRY
        .write()
        .map_err(|e| AppError::Lock(e.to_string()))?
        .unregister(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_id::builtin;

    #[test]
    fn seeds_builtins_in_sidebar_order() {
        let ids: Vec<String> = all_plugins()
            .iter()
            .map(|p| p.id().as_str().to_string())
            .collect();
        // All builtins stay in sidebar order (user plugins may follow).
        let expected = [
            builtin::CLAUDE,
            builtin::CLAUDE_DESKTOP,
            builtin::CODEX,
            builtin::GROKBUILD,
            builtin::KIMI_CODE,
            builtin::OPENCODE,
            builtin::OPENCLAW,
            builtin::HERMES,
        ];
        let positions: Vec<usize> = expected
            .iter()
            .map(|id| ids.iter().position(|x| x == id).expect("builtin missing"))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "order: {ids:?}");
    }

    #[test]
    fn unknown_lookup_is_none() {
        assert!(get_plugin(&AppId::parse("no-such-app").unwrap()).is_none());
    }

    #[test]
    fn builtin_unregister_refused_and_collision_rejected() {
        let claude = AppId::from_static(builtin::CLAUDE);
        assert!(unregister_plugin(&claude).is_err());
        let dup = get_plugin(&claude).unwrap();
        assert!(register_plugin(dup).is_err());
    }

    #[test]
    fn enabled_gating_follows_settings_map() {
        let _guard = crate::test_support::env_lock();
        let temp = tempfile::tempdir().unwrap();
        crate::test_support::set_var("OCHUB_TEST_HOME", temp.path());
        crate::settings::reload_settings().unwrap();

        // Defaults: hermes off, everything else on.
        let claude = AppId::from_static(builtin::CLAUDE);
        let hermes = AppId::from_static(builtin::HERMES);
        assert!(ensure_app_enabled(&claude).is_ok());
        assert!(matches!(
            ensure_app_enabled(&hermes),
            Err(AppError::AppDisabled(_))
        ));

        // Toggle claude off / hermes on via the settings map.
        crate::settings::mutate_settings(|s| {
            s.set_app_enabled("claude", false);
            s.set_app_enabled("hermes", true);
        })
        .unwrap();
        assert!(matches!(
            ensure_app_enabled(&claude),
            Err(AppError::AppDisabled(_))
        ));
        assert!(ensure_app_enabled(&hermes).is_ok());
        assert!(!enabled_plugins().iter().any(|p| p.id() == &claude));

        // The write chokepoint rejects a disabled app.
        let app_type = crate::app_type::AppType::Claude;
        assert!(matches!(
            ensure_app_type_enabled(&app_type),
            Err(AppError::AppDisabled(_))
        ));

        crate::test_support::remove_var("OCHUB_TEST_HOME");
        crate::settings::reload_settings().ok();
    }
}
