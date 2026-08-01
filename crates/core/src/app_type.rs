//! The set of AI coding tools OcHub can manage, and their switching
//! semantics. Ported from cc-switch `app_config.rs` `AppType`.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// One managed application / CLI tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    Claude,
    #[serde(
        rename = "claude-desktop",
        alias = "claude_desktop",
        alias = "claudeDesktop"
    )]
    ClaudeDesktop,
    #[serde(
        rename = "cherry-studio",
        alias = "cherry_studio",
        alias = "cherryStudio",
        alias = "cherry"
    )]
    CherryStudio,
    Codex,
    #[serde(
        rename = "grokbuild",
        alias = "grok-build",
        alias = "grok_build",
        alias = "grok"
    )]
    GrokBuild,
    #[serde(rename = "kimi-code", alias = "kimi_code", alias = "kimi")]
    KimiCode,
    OpenCode,
    OpenClaw,
    Hermes,
}

impl AppType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppType::Claude => "claude",
            AppType::ClaudeDesktop => "claude-desktop",
            AppType::CherryStudio => "cherry-studio",
            AppType::Codex => "codex",
            AppType::GrokBuild => "grokbuild",
            AppType::KimiCode => "kimi-code",
            AppType::OpenCode => "opencode",
            AppType::OpenClaw => "openclaw",
            AppType::Hermes => "hermes",
        }
    }

    /// Whether this app uses *additive* mode.
    ///
    /// - Switch mode (`false`): only the current provider is written to the live
    ///   config (Claude, Claude Desktop, Codex).
    /// - Additive mode (`true`): all providers are written to the live config
    ///   (OpenCode, OpenClaw, Hermes).
    pub fn is_additive_mode(&self) -> bool {
        matches!(
            self,
            AppType::OpenCode | AppType::OpenClaw | AppType::Hermes
        )
    }

    /// The open [`AppId`](crate::app_id::AppId) of this builtin app.
    pub fn app_id(&self) -> crate::app_id::AppId {
        crate::app_id::AppId::from_static(self.as_str())
    }

    /// Resolve a builtin from an open id. `None` = user-defined manifest app.
    pub fn from_app_id(id: &crate::app_id::AppId) -> Option<AppType> {
        id.as_str().parse().ok()
    }

    /// Iterate over every app type.
    pub fn all() -> impl Iterator<Item = AppType> {
        [
            AppType::Claude,
            AppType::ClaudeDesktop,
            AppType::CherryStudio,
            AppType::Codex,
            AppType::GrokBuild,
            AppType::KimiCode,
            AppType::OpenCode,
            AppType::OpenClaw,
            AppType::Hermes,
        ]
        .into_iter()
    }
}

impl FromStr for AppType {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(AppType::Claude),
            "claude-desktop" | "claude_desktop" | "claudeDesktop" => Ok(AppType::ClaudeDesktop),
            "cherry-studio" | "cherry_studio" | "cherryStudio" | "cherry" => {
                Ok(AppType::CherryStudio)
            }
            "codex" => Ok(AppType::Codex),
            "grokbuild" | "grok-build" | "grok_build" | "grok" => Ok(AppType::GrokBuild),
            "kimi-code" | "kimi_code" | "kimi" => Ok(AppType::KimiCode),
            "opencode" => Ok(AppType::OpenCode),
            "openclaw" => Ok(AppType::OpenClaw),
            "hermes" => Ok(AppType::Hermes),
            other => Err(AppError::InvalidInput(format!("未知的应用类型: {other}"))),
        }
    }
}

impl std::fmt::Display for AppType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_str() {
        for app in AppType::all() {
            assert_eq!(AppType::from_str(app.as_str()).unwrap(), app);
        }
    }

    #[test]
    fn additive_set() {
        assert!(AppType::OpenCode.is_additive_mode());
        assert!(AppType::OpenClaw.is_additive_mode());
        assert!(AppType::Hermes.is_additive_mode());
        assert!(!AppType::Claude.is_additive_mode());
        assert!(!AppType::Codex.is_additive_mode());
        assert!(!AppType::GrokBuild.is_additive_mode());
        assert!(!AppType::ClaudeDesktop.is_additive_mode());
    }

    #[test]
    fn serde_claude_desktop_kebab() {
        let v = serde_json::to_string(&AppType::ClaudeDesktop).unwrap();
        assert_eq!(v, "\"claude-desktop\"");
        let back: AppType = serde_json::from_str("\"claude_desktop\"").unwrap();
        assert_eq!(back, AppType::ClaudeDesktop);
    }

    #[test]
    fn grokbuild_aliases_normalize() {
        for alias in ["grokbuild", "grok-build", "grok_build", "grok"] {
            assert_eq!(AppType::from_str(alias).unwrap(), AppType::GrokBuild);
        }
        assert_eq!(
            serde_json::to_string(&AppType::GrokBuild).unwrap(),
            "\"grokbuild\""
        );
    }
}
