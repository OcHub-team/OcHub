//! Open, string-backed identity of a managed app.
//!
//! Replaces the closed [`crate::AppType`](crate::app_type::AppType) enum at
//! every public boundary (HTTP routes, serde payloads, settings keys, DB TEXT
//! columns, registry keys). Built-in apps keep `AppType` as an internal
//! dispatch key; user-defined manifest apps only ever exist as an `AppId`.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::error::AppError;

/// Well-known ids of the built-in apps.
pub mod builtin {
    pub const CLAUDE: &str = "claude";
    pub const CLAUDE_DESKTOP: &str = "claude-desktop";
    pub const CODEX: &str = "codex";
    pub const GROKBUILD: &str = "grokbuild";
    pub const OPENCODE: &str = "opencode";
    pub const OPENCLAW: &str = "openclaw";
    pub const HERMES: &str = "hermes";
}

/// Validated, normalized app identifier. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId(Arc<str>);

impl AppId {
    /// Validate and normalize an id.
    ///
    /// Accepted shape: lowercase slug `[a-z0-9][a-z0-9_-]*`, max 32 chars.
    /// Legacy aliases `claude_desktop` / `claudeDesktop` normalize to
    /// `claude-desktop` so historical payloads and settings keep parsing.
    pub fn parse(s: &str) -> Result<Self, AppError> {
        let s = s.trim();
        // Legacy ClaudeDesktop spellings (settings files, deeplinks, API payloads).
        if s == "claude_desktop" || s == "claudeDesktop" {
            return Ok(Self(Arc::from(builtin::CLAUDE_DESKTOP)));
        }
        if matches!(s, "grok-build" | "grok_build" | "grok") {
            return Ok(Self(Arc::from(builtin::GROKBUILD)));
        }
        let valid_len = !s.is_empty() && s.len() <= 32;
        let valid_start = s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let valid_chars = s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !(valid_len && valid_start && valid_chars) {
            return Err(AppError::InvalidInput(format!("无效的应用 ID: {s}")));
        }
        Ok(Self(Arc::from(s)))
    }

    /// Construct from a known-good static id (builtin constants, tests).
    pub fn from_static(s: &'static str) -> Self {
        Self(Arc::from(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AppId {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for AppId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl serde::Serialize for AppId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for AppId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builtin_ids() {
        for id in [
            builtin::CLAUDE,
            builtin::CLAUDE_DESKTOP,
            builtin::CODEX,
            builtin::GROKBUILD,
            builtin::OPENCODE,
            builtin::OPENCLAW,
            builtin::HERMES,
        ] {
            assert_eq!(AppId::parse(id).unwrap().as_str(), id);
        }
    }

    #[test]
    fn legacy_claude_desktop_aliases_normalize() {
        assert_eq!(
            AppId::parse("claude_desktop").unwrap().as_str(),
            "claude-desktop"
        );
        assert_eq!(
            AppId::parse("claudeDesktop").unwrap().as_str(),
            "claude-desktop"
        );
    }

    #[test]
    fn grokbuild_aliases_normalize() {
        for alias in ["grok-build", "grok_build", "grok"] {
            assert_eq!(AppId::parse(alias).unwrap().as_str(), "grokbuild");
        }
    }

    #[test]
    fn rejects_invalid_ids() {
        for bad in ["", "-lead", "UPPER", "has space", "汉字", &"a".repeat(33)] {
            assert!(AppId::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn serde_round_trip() {
        let id = AppId::parse("my-app").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"my-app\"");
        let back: AppId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        let legacy: AppId = serde_json::from_str("\"claudeDesktop\"").unwrap();
        assert_eq!(legacy.as_str(), "claude-desktop");
    }
}
