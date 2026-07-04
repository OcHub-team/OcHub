//! The set of AI coding tools RouteDeck can manage, and their switching
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
    Codex,
    Gemini,
    OpenCode,
    OpenClaw,
    Hermes,
}

impl AppType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppType::Claude => "claude",
            AppType::ClaudeDesktop => "claude-desktop",
            AppType::Codex => "codex",
            AppType::Gemini => "gemini",
            AppType::OpenCode => "opencode",
            AppType::OpenClaw => "openclaw",
            AppType::Hermes => "hermes",
        }
    }

    /// Whether this app uses *additive* mode.
    ///
    /// - Switch mode (`false`): only the current provider is written to the live
    ///   config (Claude, Claude Desktop, Codex, Gemini).
    /// - Additive mode (`true`): all providers are written to the live config
    ///   (OpenCode, OpenClaw, Hermes).
    pub fn is_additive_mode(&self) -> bool {
        matches!(
            self,
            AppType::OpenCode | AppType::OpenClaw | AppType::Hermes
        )
    }

    /// Iterate over every app type.
    pub fn all() -> impl Iterator<Item = AppType> {
        [
            AppType::Claude,
            AppType::ClaudeDesktop,
            AppType::Codex,
            AppType::Gemini,
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
            "codex" => Ok(AppType::Codex),
            "gemini" => Ok(AppType::Gemini),
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
        assert!(!AppType::Gemini.is_additive_mode());
        assert!(!AppType::ClaudeDesktop.is_additive_mode());
    }

    #[test]
    fn serde_claude_desktop_kebab() {
        let v = serde_json::to_string(&AppType::ClaudeDesktop).unwrap();
        assert_eq!(v, "\"claude-desktop\"");
        let back: AppType = serde_json::from_str("\"claude_desktop\"").unwrap();
        assert_eq!(back, AppType::ClaudeDesktop);
    }
}
