//! UI-side adapter over the core plugin registry.
//!
//! The single place the GPUI crate derives app lists, labels, colors, and
//! icons from — replaces the per-view hardcoded `[AppType; N]` arrays and
//! duplicated label functions.

use gpui::SharedString;
use ochub_core::plugin::{self, AppPlugin};
use ochub_core::AppType;

use crate::icons::IconName;

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

/// Enabled apps that support prompt files.
pub fn enabled_prompt_apps() -> Vec<AppType> {
    enabled_apps_with(|p| p.prompt_filename().is_some())
}

/// Display label from the registry (falls back to the raw id).
pub fn label(app: AppType) -> SharedString {
    plugin::get_plugin(&app.app_id())
        .map(|p| SharedString::from(p.display_name().to_string()))
        .unwrap_or_else(|| SharedString::from(app.as_str().to_string()))
}

/// Accent color from the registry.
pub fn accent(app: AppType) -> u32 {
    plugin::get_plugin(&app.app_id())
        .map(|p| p.accent_color())
        .unwrap_or(0x888888)
}

/// Map a plugin icon key to a bundled icon; `None` = render a letter avatar.
pub fn builtin_icon(icon_id: &str) -> Option<IconName> {
    Some(match icon_id {
        "claude" => IconName::AgentClaude,
        "claude-code" => IconName::AgentClaudeCode,
        "codex" => IconName::AgentCodex,
        "gemini" => IconName::AgentGemini,
        "hermes" => IconName::AgentHermes,
        "openclaw" => IconName::AgentOpenClaw,
        "opencode" => IconName::AgentOpenCode,
        _ => return None,
    })
}

/// Icon for a builtin app.
pub fn icon(app: AppType) -> Option<IconName> {
    plugin::get_plugin(&app.app_id()).and_then(|p| builtin_icon(p.icon_id()))
}
