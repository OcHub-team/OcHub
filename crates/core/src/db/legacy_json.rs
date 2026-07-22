//! Minimal standalone port of the legacy `config.json` (`MultiAppConfig`) shape
//! and the domain structs the DB layer reads/writes.
//!
//! cc-switch kept these in `app_config.rs`, `prompt.rs`, and `services/skill.rs`.
//! Those modules pull in a lot of Tauri/service machinery that ochub-core does not
//! need yet, so we re-define only the data structures required to:
//!   * migrate a legacy `config.json` into SQLite (`db::migration`)
//!   * back the MCP / Prompt / Skill DAOs
//!
//! Field names, serde renames, defaults, and `Default` impls match the
//! reference 1:1 so legacy `config.json` files still deserialize correctly and
//! SQLite payloads stay byte-compatible.

#![allow(dead_code)]

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::app_type::AppType;
use crate::model::ProviderManager;

// ============================================================================
// MCP
// ============================================================================

/// MCP 服务器应用状态（标记应用到哪些客户端）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpApps {
    #[serde(default)]
    pub claude: bool,
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub gemini: bool,
    #[serde(default)]
    pub opencode: bool,
    #[serde(default)]
    pub hermes: bool,
}

impl McpApps {
    pub fn is_enabled_for(&self, app: &AppType) -> bool {
        match app {
            AppType::Claude => self.claude,
            AppType::Codex => self.codex,
            AppType::Gemini => self.gemini,
            AppType::OpenCode => self.opencode,
            AppType::OpenClaw => false,
            AppType::Hermes => self.hermes,
            AppType::ClaudeDesktop => false,
        }
    }

    pub fn set_enabled_for(&mut self, app: &AppType, enabled: bool) {
        match app {
            AppType::Claude => self.claude = enabled,
            AppType::Codex => self.codex = enabled,
            AppType::Gemini => self.gemini = enabled,
            AppType::OpenCode => self.opencode = enabled,
            AppType::OpenClaw => {}
            AppType::Hermes => self.hermes = enabled,
            AppType::ClaudeDesktop => {}
        }
    }

    pub fn enabled_apps(&self) -> Vec<AppType> {
        let mut apps = Vec::new();
        if self.claude {
            apps.push(AppType::Claude);
        }
        if self.codex {
            apps.push(AppType::Codex);
        }
        if self.gemini {
            apps.push(AppType::Gemini);
        }
        if self.opencode {
            apps.push(AppType::OpenCode);
        }
        if self.hermes {
            apps.push(AppType::Hermes);
        }
        apps
    }

    pub fn is_empty(&self) -> bool {
        !self.claude && !self.codex && !self.gemini && !self.opencode && !self.hermes
    }
}

/// MCP 服务器定义（v3.7.0 统一结构）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub server: serde_json::Value,
    pub apps: McpApps,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// MCP 配置：单客户端维度（v3.6.x 及以前，保留用于向后兼容）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, serde_json::Value>,
}

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// MCP 根配置（v3.7.0 新旧结构并存）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRoot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<HashMap<String, McpServer>>,

    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub claude: McpConfig,
    #[serde(
        rename = "claude-desktop",
        alias = "claudeDesktop",
        alias = "claude_desktop",
        default,
        skip_serializing_if = "McpConfig::is_empty"
    )]
    pub claude_desktop: McpConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub codex: McpConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub gemini: McpConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub opencode: McpConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub openclaw: McpConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub hermes: McpConfig,
}

impl Default for McpRoot {
    fn default() -> Self {
        Self {
            servers: Some(HashMap::new()),
            claude: McpConfig::default(),
            claude_desktop: McpConfig::default(),
            codex: McpConfig::default(),
            gemini: McpConfig::default(),
            opencode: McpConfig::default(),
            openclaw: McpConfig::default(),
            hermes: McpConfig::default(),
        }
    }
}

// ============================================================================
// Prompts
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

/// Prompt 配置：单客户端维度
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptConfig {
    #[serde(default)]
    pub prompts: HashMap<String, Prompt>,
}

/// Prompt 根：按客户端分开维护
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptRoot {
    #[serde(default)]
    pub claude: PromptConfig,
    #[serde(
        rename = "claude-desktop",
        alias = "claudeDesktop",
        alias = "claude_desktop",
        default
    )]
    pub claude_desktop: PromptConfig,
    #[serde(default)]
    pub codex: PromptConfig,
    #[serde(default)]
    pub gemini: PromptConfig,
    #[serde(default)]
    pub opencode: PromptConfig,
    #[serde(default)]
    pub openclaw: PromptConfig,
    #[serde(default)]
    pub hermes: PromptConfig,
}

// ============================================================================
// Skills
// ============================================================================

/// Skill 应用启用状态（标记 Skill 应用到哪些客户端）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SkillApps {
    #[serde(default)]
    pub claude: bool,
    #[serde(default)]
    pub codex: bool,
    #[serde(default)]
    pub gemini: bool,
    #[serde(default)]
    pub opencode: bool,
    #[serde(default)]
    pub hermes: bool,
}

impl SkillApps {
    pub fn is_enabled_for(&self, app: &AppType) -> bool {
        match app {
            AppType::Claude => self.claude,
            AppType::Codex => self.codex,
            AppType::Gemini => self.gemini,
            AppType::OpenCode => self.opencode,
            AppType::Hermes => self.hermes,
            AppType::OpenClaw => false,
            AppType::ClaudeDesktop => false,
        }
    }

    pub fn set_enabled_for(&mut self, app: &AppType, enabled: bool) {
        match app {
            AppType::Claude => self.claude = enabled,
            AppType::Codex => self.codex = enabled,
            AppType::Gemini => self.gemini = enabled,
            AppType::OpenCode => self.opencode = enabled,
            AppType::Hermes => self.hermes = enabled,
            AppType::OpenClaw => {}
            AppType::ClaudeDesktop => {}
        }
    }

    pub fn enabled_apps(&self) -> Vec<AppType> {
        let mut apps = Vec::new();
        if self.claude {
            apps.push(AppType::Claude);
        }
        if self.codex {
            apps.push(AppType::Codex);
        }
        if self.gemini {
            apps.push(AppType::Gemini);
        }
        if self.opencode {
            apps.push(AppType::OpenCode);
        }
        if self.hermes {
            apps.push(AppType::Hermes);
        }
        apps
    }

    pub fn is_empty(&self) -> bool {
        !self.claude && !self.codex && !self.gemini && !self.opencode && !self.hermes
    }

    /// Build a `SkillApps` with only the given app enabled.
    pub fn only(app: &AppType) -> Self {
        let mut apps = SkillApps::default();
        apps.set_enabled_for(app, true);
        apps
    }
}

/// 已安装的 Skill（v3.10.0+ 统一结构）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readme_url: Option<String>,
    pub apps: SkillApps,
    pub installed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
}

/// 仓库配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRepo {
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub enabled: bool,
}

/// 技能安装状态（旧版兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillState {
    pub installed: bool,
    #[serde(rename = "installedAt")]
    pub installed_at: DateTime<Utc>,
}

/// 持久化存储结构（仓库配置）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStore {
    /// directory -> 安装状态（旧版兼容，新版不使用）
    #[serde(default)]
    pub skills: HashMap<String, SkillState>,
    /// 仓库列表
    #[serde(default)]
    pub repos: Vec<SkillRepo>,
}

impl Default for SkillStore {
    fn default() -> Self {
        SkillStore {
            skills: HashMap::new(),
            repos: vec![
                SkillRepo {
                    owner: "anthropics".to_string(),
                    name: "skills".to_string(),
                    branch: "main".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "ComposioHQ".to_string(),
                    name: "awesome-claude-skills".to_string(),
                    branch: "master".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "cexll".to_string(),
                    name: "myclaude".to_string(),
                    branch: "master".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "JimLiu".to_string(),
                    name: "baoyu-skills".to_string(),
                    branch: "main".to_string(),
                    enabled: true,
                },
            ],
        }
    }
}

/// 未被 CC Switch 管理的 Skill（扫描应用目录时发现）
///
/// Ported from cc-switch `app_config.rs` (`UnmanagedSkill`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnmanagedSkill {
    pub directory: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub found_in: Vec<String>,
    pub path: String,
}

// ============================================================================
// Common config snippets
// ============================================================================

/// 通用配置片段（按应用分治）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommonConfigSnippets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openclaw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hermes: Option<String>,
}

impl CommonConfigSnippets {
    pub fn get(&self, app: &AppType) -> Option<&String> {
        match app {
            AppType::Claude => self.claude.as_ref(),
            AppType::ClaudeDesktop => None,
            AppType::Codex => self.codex.as_ref(),
            AppType::Gemini => self.gemini.as_ref(),
            AppType::OpenCode => self.opencode.as_ref(),
            AppType::OpenClaw => self.openclaw.as_ref(),
            AppType::Hermes => self.hermes.as_ref(),
        }
    }

    pub fn set(&mut self, app: &AppType, snippet: Option<String>) {
        match app {
            AppType::Claude => self.claude = snippet,
            AppType::ClaudeDesktop => {}
            AppType::Codex => self.codex = snippet,
            AppType::Gemini => self.gemini = snippet,
            AppType::OpenCode => self.opencode = snippet,
            AppType::OpenClaw => self.openclaw = snippet,
            AppType::Hermes => self.hermes = snippet,
        }
    }
}

// ============================================================================
// MultiAppConfig (legacy config.json root)
// ============================================================================

/// 多应用配置结构（向后兼容；用于 JSON → SQLite 迁移）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiAppConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// 应用管理器（claude/codex/...）以 app key 为键
    #[serde(flatten)]
    pub apps: HashMap<String, ProviderManager>,
    #[serde(default)]
    pub mcp: McpRoot,
    #[serde(default)]
    pub prompts: PromptRoot,
    #[serde(default)]
    pub skills: SkillStore,
    #[serde(default)]
    pub common_config_snippets: CommonConfigSnippets,
    /// Claude 通用配置片段（旧字段，用于向后兼容迁移）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_common_config_snippet: Option<String>,
}

fn default_version() -> u32 {
    2
}

impl Default for MultiAppConfig {
    fn default() -> Self {
        let mut apps = HashMap::new();
        apps.insert("claude".to_string(), ProviderManager::default());
        apps.insert("claude-desktop".to_string(), ProviderManager::default());
        apps.insert("codex".to_string(), ProviderManager::default());
        apps.insert("gemini".to_string(), ProviderManager::default());
        apps.insert("opencode".to_string(), ProviderManager::default());
        apps.insert("openclaw".to_string(), ProviderManager::default());
        apps.insert("hermes".to_string(), ProviderManager::default());

        Self {
            version: 2,
            apps,
            mcp: McpRoot::default(),
            prompts: PromptRoot::default(),
            skills: SkillStore::default(),
            common_config_snippets: CommonConfigSnippets::default(),
            claude_common_config_snippet: None,
        }
    }
}
