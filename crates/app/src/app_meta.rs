//! UI-side adapter over the core plugin registry.
//!
//! The single place the GPUI crate derives app lists, labels, colors, and
//! icons from — replaces the per-view hardcoded `[AppType; N]` arrays and
//! duplicated label functions.

use std::collections::HashMap;
use std::sync::LazyLock;

use gpui::SharedString;
use ochub_core::plugin::{self, AppPlugin};
use ochub_core::AppType;

use crate::icons::IconName;

#[derive(Clone)]
struct BuiltinMeta {
    label: SharedString,
    accent: u32,
    icon: Option<IconName>,
}

/// Builtin registry metadata is immutable after startup (manifest plugins
/// cannot replace a builtin id). Cache it once so every sidebar/card frame
/// avoids a registry lock and display-name allocation.
static BUILTIN_META: LazyLock<HashMap<AppType, BuiltinMeta>> = LazyLock::new(|| {
    AppType::all()
        .map(|app| {
            let metadata = plugin::get_plugin(&app.app_id())
                .map(|plugin| BuiltinMeta {
                    label: SharedString::from(plugin.display_name().to_string()),
                    accent: plugin.accent_color(),
                    icon: builtin_icon(plugin.icon_id()),
                })
                .unwrap_or_else(|| BuiltinMeta {
                    label: SharedString::new_static(app.as_str()),
                    accent: 0x888888,
                    icon: None,
                });
            (app, metadata)
        })
        .collect()
});

/// Enabled builtin apps in registry order.
pub fn enabled_app_types() -> Vec<AppType> {
    plugin::enabled_plugins()
        .iter()
        .filter_map(|p| AppType::from_app_id(p.id()))
        .collect()
}

fn enabled_apps_with(filter: impl Fn(&dyn AppPlugin) -> bool) -> Vec<AppType> {
    plugin::enabled_plugins()
        .iter()
        .filter(|p| filter(p.as_ref()))
        .filter_map(|p| AppType::from_app_id(p.id()))
        .collect()
}

/// Enabled apps that support MCP sync.
pub fn enabled_mcp_apps() -> Vec<AppType> {
    enabled_apps_with(|p| p.supports_mcp())
}

/// Enabled apps that support skills.
pub fn enabled_skill_apps() -> Vec<AppType> {
    enabled_apps_with(|p| p.supports_skills())
}

/// Display label from the registry (falls back to the raw id).
pub fn label(app: AppType) -> SharedString {
    BUILTIN_META
        .get(&app)
        .map(|metadata| metadata.label.clone())
        .unwrap_or_else(|| SharedString::new_static(app.as_str()))
}

/// Accent color from the registry.
pub fn accent(app: AppType) -> u32 {
    BUILTIN_META
        .get(&app)
        .map(|metadata| metadata.accent)
        .unwrap_or(0x888888)
}

/// Map a plugin icon key to a bundled icon; `None` = render a letter avatar.
pub fn builtin_icon(icon_id: &str) -> Option<IconName> {
    Some(match icon_id {
        "claude" => IconName::AgentClaude,
        "claude-code" => IconName::AgentClaudeCode,
        "codex" => IconName::AgentCodex,
        "grok" => IconName::AgentGrokBuild,
        "hermes" => IconName::AgentHermes,
        "openclaw" => IconName::AgentOpenClaw,
        "opencode" => IconName::AgentOpenCode,
        _ => return None,
    })
}

/// Icon for a builtin app.
pub fn icon(app: AppType) -> Option<IconName> {
    BUILTIN_META.get(&app).and_then(|metadata| metadata.icon)
}
