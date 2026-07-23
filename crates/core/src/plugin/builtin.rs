//! The six built-in app plugins.
//!
//! Thin data + delegation shims: all real behavior stays in `crate::apps`,
//! `crate::services::provider::live`, and `crate::provider_config`. Each
//! built-in keeps its [`AppType`] as the internal dispatch key.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::app_id::AppId;
use crate::app_type::AppType;
use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;
use crate::provider_config::AppConfig;

use super::capabilities::LiveConfigOps;
use super::{AppMode, AppPlugin};

struct BuiltinSpec {
    app: AppType,
    name: &'static str,
    /// Icon key; the UI crate owns the key -> asset mapping.
    icon: &'static str,
    accent: u32,
    sort: i32,
    enabled_by_default: bool,
    mode: AppMode,
    mcp: bool,
    skills: bool,
}

fn builtin_specs() -> Vec<BuiltinSpec> {
    vec![
        BuiltinSpec {
            app: AppType::Claude,
            name: "Claude Code",
            icon: "claude-code",
            accent: 0xd97757,
            sort: 0,
            enabled_by_default: true,
            mode: AppMode::Switch,
            mcp: true,
            skills: true,
        },
        BuiltinSpec {
            app: AppType::ClaudeDesktop,
            name: "Claude Desktop",
            icon: "claude",
            accent: 0xbd5d3a,
            sort: 10,
            enabled_by_default: true,
            mode: AppMode::Switch,
            mcp: false,
            skills: false,
        },
        BuiltinSpec {
            app: AppType::Codex,
            name: "Codex",
            icon: "codex",
            accent: 0x0d0d0d,
            sort: 20,
            enabled_by_default: true,
            mode: AppMode::Switch,
            mcp: true,
            skills: true,
        },
        BuiltinSpec {
            app: AppType::OpenCode,
            name: "OpenCode",
            icon: "opencode",
            accent: 0x211e1e,
            sort: 40,
            enabled_by_default: true,
            mode: AppMode::Additive,
            mcp: true,
            skills: true,
        },
        BuiltinSpec {
            app: AppType::OpenClaw,
            name: "OpenClaw",
            icon: "openclaw",
            accent: 0xe23b3b,
            sort: 50,
            enabled_by_default: true,
            mode: AppMode::Additive,
            mcp: false,
            skills: false,
        },
        BuiltinSpec {
            app: AppType::Hermes,
            name: "Hermes",
            icon: "hermes",
            accent: 0x2b2b33,
            sort: 60,
            enabled_by_default: false,
            mode: AppMode::Additive,
            mcp: true,
            skills: true,
        },
    ]
}

pub(super) fn builtin_plugins() -> Vec<Arc<dyn AppPlugin>> {
    builtin_specs()
        .into_iter()
        .map(|spec| {
            Arc::new(BuiltinPlugin {
                id: spec.app.app_id(),
                spec,
            }) as Arc<dyn AppPlugin>
        })
        .collect()
}

struct BuiltinPlugin {
    id: AppId,
    spec: BuiltinSpec,
}

impl AppPlugin for BuiltinPlugin {
    fn id(&self) -> &AppId {
        &self.id
    }

    fn display_name(&self) -> &str {
        self.spec.name
    }

    fn icon_id(&self) -> &str {
        self.spec.icon
    }

    fn accent_color(&self) -> u32 {
        self.spec.accent
    }

    fn sort_order(&self) -> i32 {
        self.spec.sort
    }

    fn enabled_by_default(&self) -> bool {
        self.spec.enabled_by_default
    }

    fn mode(&self) -> AppMode {
        self.spec.mode
    }

    fn config_dir(&self) -> Result<PathBuf, AppError> {
        Ok(match self.spec.app {
            AppType::Claude => crate::paths::get_claude_config_dir(),
            AppType::ClaudeDesktop => crate::apps::claude_desktop::get_config_library_path()?,
            AppType::Codex => crate::apps::codex::get_codex_config_dir(),
            AppType::OpenCode => crate::apps::opencode::get_opencode_dir(),
            AppType::OpenClaw => crate::apps::openclaw::get_openclaw_dir(),
            AppType::Hermes => crate::apps::hermes::get_hermes_dir(),
        })
    }

    fn provider_config(&self) -> Option<Box<dyn AppConfig>> {
        crate::provider_config::config_for(self.spec.app)
    }

    fn live(&self) -> &dyn LiveConfigOps {
        self
    }

    fn supports_mcp(&self) -> bool {
        self.spec.mcp
    }

    fn supports_skills(&self) -> bool {
        self.spec.skills
    }
}

impl LiveConfigOps for BuiltinPlugin {
    fn write_live(&self, db: &Database, provider: &Provider) -> Result<(), AppError> {
        use crate::services::provider::live;
        match self.spec.app {
            AppType::Claude => live::write_claude_live_snapshot(provider),
            AppType::ClaudeDesktop => crate::apps::claude_desktop::apply_provider(db, provider),
            AppType::Codex => live::write_codex_live_snapshot(provider),
            AppType::OpenCode => live::write_opencode_live_snapshot(provider),
            AppType::OpenClaw => live::write_openclaw_live_snapshot(provider),
            AppType::Hermes => live::write_hermes_live_snapshot(provider),
        }
    }

    fn remove_from_live(&self, provider_id: &str) -> Result<(), AppError> {
        use crate::services::provider::live;
        match self.spec.app {
            AppType::OpenCode => live::remove_opencode_provider_from_live(provider_id),
            AppType::OpenClaw => live::remove_openclaw_provider_from_live(provider_id),
            AppType::Hermes => live::remove_hermes_provider_from_live(provider_id),
            _ => Err(AppError::InvalidInput(format!(
                "应用 {} 不支持从 live 配置中移除单个供应商",
                self.id
            ))),
        }
    }

    fn read_live(&self) -> Result<Value, AppError> {
        crate::services::provider::live::read_live_settings(self.spec.app)
    }
}
