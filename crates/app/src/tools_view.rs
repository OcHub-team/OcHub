//! Advanced utility panel for features that used to live as scattered Tauri
//! commands: config folders, OMO files, Claude MCP, and app-level helper
//! toggles.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Context, Entity, Focusable, FontWeight, ListAlignment, ListState,
    SharedString, Window,
};
use ochub_core::apps::{claude_desktop, claude_plugin, codex, hermes, openclaw, opencode};
use ochub_core::services::OmoService;
use ochub_core::{AppError, AppState, AppType};
use serde_json::Value;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone)]
struct ConfigRow {
    app: AppType,
    label: gpui::SharedString,
    exists: bool,
    path: String,
}

#[derive(Clone)]
struct BackupRow {
    filename: String,
    size_bytes: u64,
    created_at: String,
}

/// 破坏性操作确认目标（删除数据库备份、恢复数据库、删除环境变量冲突、
/// 导入 SQL、恢复 Codex 历史、禁用 OMO），`Some` 时展示确认模态。
#[derive(Clone)]
enum ConfirmAction {
    RestoreDbBackup(String),
    DeleteDbBackup(String),
    DeleteEnvConflicts,
    ImportSql,
    RestoreCodexHistory,
    DisableOmo { slim: bool },
}

/// Number of top-level blocks rendered by [`ToolsView::render_block`] into the
/// virtualized list (stats, 配置目录, 应用辅助, CLI 工具维护, 环境变量冲突,
/// 配置导入导出与数据库备份, 高级项目).
const TOOLS_BLOCK_COUNT: usize = 7;

pub struct ToolsView {
    app: Arc<AppState>,
    config_rows: Vec<ConfigRow>,
    auto_launch: Option<bool>,
    tool_versions: Vec<ochub_core::session_manager::ToolVersion>,
    tool_installations: Vec<ochub_core::session_manager::ToolInstallationReport>,
    tool_busy: bool,
    env_app: AppType,
    env_conflicts: Vec<ochub_core::EnvConflict>,
    db_backups: Vec<BackupRow>,
    show_all_backups: bool,
    show_advanced_tools: bool,
    export_sql_path: Entity<TextInput>,
    import_sql_path: Entity<TextInput>,
    env_restore_path: Entity<TextInput>,
    backup_rename: Entity<TextInput>,
    mcp_command: Entity<TextInput>,
    openclaw_default_model_json: Entity<TextInput>,
    openclaw_env_json: Entity<TextInput>,
    openclaw_tools_json: Entity<TextInput>,
    hermes_model_json: Entity<TextInput>,
    hermes_memory_content: Entity<TextInput>,
    hermes_user_memory_content: Entity<TextInput>,
    hermes_limits: Option<hermes::HermesMemoryLimits>,
    /// 待确认的破坏性操作；`Some` 时展示确认模态。
    confirm: Option<ConfirmAction>,
    status: Option<SharedString>,
    /// Drives the virtualized page body (one item per top-level block).
    list_state: ListState,
}

impl ToolsView {
    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            window.play_system_bell();
            return;
        }
        let focused = |input: &Entity<TextInput>, cx: &App| {
            input.read(cx).focus_handle(cx).is_focused(window)
        };
        if focused(&self.openclaw_default_model_json, cx) {
            self.save_openclaw_default_model(cx);
        } else if focused(&self.openclaw_env_json, cx) {
            self.save_openclaw_env(cx);
        } else if focused(&self.openclaw_tools_json, cx) {
            self.save_openclaw_tools(cx);
        } else if focused(&self.hermes_model_json, cx) {
            self.save_hermes_model(cx);
        } else if focused(&self.hermes_memory_content, cx) {
            self.save_hermes_memory(hermes::MemoryKind::Memory, cx);
        } else if focused(&self.hermes_user_memory_content, cx) {
            self.save_hermes_memory(hermes::MemoryKind::User, cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.take().is_some() {
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let mcp_command = cx.new(|cx| TextInput::new(cx, "npx"));
        let openclaw_default_model_json = cx.new(|cx| {
            TextInput::new(cx, r#"{"primary":"anthropic/claude-sonnet-4"}"#).multiline(true)
        });
        let openclaw_env_json =
            cx.new(|cx| TextInput::new(cx, r#"{"OPENAI_API_KEY":"..."}"#).multiline(true));
        let openclaw_tools_json = cx.new(|cx| {
            TextInput::new(cx, r#"{"profile":"coding","allow":[],"deny":[]}"#).multiline(true)
        });
        let hermes_model_json = cx.new(|cx| {
            TextInput::new(
                cx,
                r#"{"provider":"openrouter","default":"anthropic/claude-sonnet-4"}"#,
            )
            .multiline(true)
        });
        let hermes_memory_content =
            cx.new(|cx| TextInput::new(cx, "Hermes MEMORY.md").multiline(true));
        let hermes_user_memory_content =
            cx.new(|cx| TextInput::new(cx, "Hermes USER.md").multiline(true));
        let export_sql_path = cx.new(|cx| TextInput::new(cx, "~/.ochub/exports/OcHub.sql"));
        let import_sql_path = cx.new(|cx| TextInput::new(cx, "/path/to/OcHub.sql"));
        let env_restore_path =
            cx.new(|cx| TextInput::new(cx, "~/.ochub/backups/env-backup-YYYYMMDD.json"));
        let backup_rename = cx.new(|cx| TextInput::new(cx, "backup-name"));

        let mut this = Self {
            app,
            config_rows: Vec::new(),
            auto_launch: None,
            tool_versions: Vec::new(),
            tool_installations: Vec::new(),
            tool_busy: false,
            env_app: AppType::Claude,
            env_conflicts: Vec::new(),
            db_backups: Vec::new(),
            show_all_backups: false,
            show_advanced_tools: false,
            export_sql_path,
            import_sql_path,
            env_restore_path,
            backup_rename,
            mcp_command,
            openclaw_default_model_json,
            openclaw_env_json,
            openclaw_tools_json,
            hermes_model_json,
            hermes_memory_content,
            hermes_user_memory_content,
            hermes_limits: None,
            confirm: None,
            status: None,
            list_state: ListState::new(TOOLS_BLOCK_COUNT, ListAlignment::Top, px(600.)),
        };
        this.reload();
        this.refresh_advanced_configs(cx);
        this
    }

    pub fn reload(&mut self) {
        self.config_rows = Self::all_apps()
            .into_iter()
            .map(|(app, label)| {
                let (exists, path) = match config_status(&self.app, app) {
                    Ok((exists, path)) => (exists, path),
                    Err(err) => (false, err.to_string()),
                };
                ConfigRow {
                    app,
                    label,
                    exists,
                    path,
                }
            })
            .collect();
        self.auto_launch = ochub_core::autostart::is_enabled().ok();
        self.db_backups = load_db_backup_rows().unwrap_or_default();
    }

    fn refresh_advanced_configs(&mut self, cx: &mut Context<Self>) {
        let openclaw_default_model = openclaw::get_default_model()
            .ok()
            .flatten()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| "{}".to_string());
        self.openclaw_default_model_json.update(cx, |input, cx| {
            input.set_content(openclaw_default_model, cx)
        });

        let openclaw_env = openclaw::get_env_config()
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value.vars).ok())
            .unwrap_or_else(|| "{}".to_string());
        self.openclaw_env_json
            .update(cx, |input, cx| input.set_content(openclaw_env, cx));

        let openclaw_tools = openclaw::get_tools_config()
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| "{}".to_string());
        self.openclaw_tools_json
            .update(cx, |input, cx| input.set_content(openclaw_tools, cx));

        let hermes_model = hermes::get_model_config()
            .ok()
            .flatten()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| "{}".to_string());
        self.hermes_model_json
            .update(cx, |input, cx| input.set_content(hermes_model, cx));

        let hermes_memory = hermes::read_memory(hermes::MemoryKind::Memory).unwrap_or_default();
        self.hermes_memory_content
            .update(cx, |input, cx| input.set_content(hermes_memory, cx));
        let hermes_user = hermes::read_memory(hermes::MemoryKind::User).unwrap_or_default();
        self.hermes_user_memory_content
            .update(cx, |input, cx| input.set_content(hermes_user, cx));
        self.hermes_limits = hermes::read_memory_limits().ok();
    }

    fn all_apps() -> Vec<(AppType, gpui::SharedString)> {
        crate::app_meta::enabled_app_types()
            .into_iter()
            .map(|app| (app, crate::app_meta::label(app)))
            .collect()
    }

    fn set_status(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(msg.into());
        self.list_state.remeasure();
        cx.notify();
    }

    fn open_path_action(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match open_path(&path) {
            Ok(()) => self.set_status(format!("已打开 {}", path.display()), cx),
            Err(err) => self.set_status(format!("打开失败: {err}"), cx),
        }
    }

    fn open_config_dir(&mut self, app: AppType, cx: &mut Context<Self>) {
        match config_dir(app).and_then(|path| {
            std::fs::create_dir_all(&path).map_err(|e| AppError::io(&path, e))?;
            Ok(path)
        }) {
            Ok(path) => self.open_path_action(path, cx),
            Err(err) => self.set_status(format!("打开配置目录失败: {err}"), cx),
        }
    }

    fn refresh_tool_versions(&mut self, cx: &mut Context<Self>) {
        if self.tool_busy {
            return;
        }
        self.tool_busy = true;
        self.status = Some(SharedString::from("正在探测 CLI 工具版本..."));
        self.list_state.remeasure();
        cx.notify();

        let task = cx.background_spawn(async move {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("创建异步运行时失败: {e}"))
                .and_then(|runtime| {
                    runtime.block_on(ochub_core::session_manager::get_tool_versions(None, None))
                })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.tool_busy = false;
                match result {
                    Ok(versions) => {
                        let count = versions.len();
                        this.tool_versions = versions;
                        this.status = Some(SharedString::from(format!(
                            "已刷新 {count} 个 CLI 工具版本"
                        )));
                    }
                    Err(err) => {
                        this.status =
                            Some(SharedString::from(format!("CLI 工具版本探测失败: {err}")));
                    }
                }
                this.list_state.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn probe_cli_installations(&mut self, cx: &mut Context<Self>) {
        match ochub_core::session_manager::probe_tool_installations(cli_tool_ids()) {
            Ok(reports) => {
                let conflicts = reports.iter().filter(|report| report.is_conflict).count();
                let count = reports.len();
                self.tool_installations = reports;
                self.set_status(
                    format!("已扫描 {count} 个 CLI 工具，冲突 {conflicts} 个"),
                    cx,
                );
            }
            Err(err) => self.set_status(format!("CLI 安装分布扫描失败: {err}"), cx),
        }
    }

    fn run_cli_lifecycle(&mut self, action: &'static str, cx: &mut Context<Self>) {
        if self.tool_busy {
            return;
        }
        self.tool_busy = true;
        self.status = Some(SharedString::from(match action {
            "install" => "正在安装缺失的 CLI 工具...",
            "update" => "正在更新 CLI 工具...",
            _ => "正在执行 CLI 工具操作...",
        }));
        self.list_state.remeasure();
        cx.notify();

        let tools = cli_tool_ids();
        let task = cx.background_spawn(async move {
            ochub_core::session_manager::run_tool_lifecycle_action(tools, action.to_string(), None)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.tool_busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(()) => match action {
                        "install" => "CLI 工具安装命令已执行".to_string(),
                        "update" => "CLI 工具更新命令已执行".to_string(),
                        _ => "CLI 工具操作已执行".to_string(),
                    },
                    Err(err) => format!("CLI 工具操作失败: {err}"),
                }));
                this.list_state.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_env_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        self.env_app = app;
        self.scan_env_conflicts(cx);
    }

    fn scan_env_conflicts(&mut self, cx: &mut Context<Self>) {
        match ochub_core::check_env_conflicts(self.env_app.as_str()) {
            Ok(conflicts) => {
                let count = conflicts.len();
                self.env_conflicts = conflicts;
                self.set_status(
                    format!("{} 环境变量冲突 {count} 个", env_app_label(self.env_app)),
                    cx,
                );
            }
            Err(err) => self.set_status(format!("环境变量冲突扫描失败: {err}"), cx),
        }
    }

    fn delete_env_conflicts(&mut self, cx: &mut Context<Self>) {
        if self.env_conflicts.is_empty() {
            self.set_status("当前没有可删除的环境变量冲突", cx);
            return;
        }
        match ochub_core::delete_env_vars(self.env_conflicts.clone()) {
            Ok(backup) => {
                self.env_restore_path.update(cx, |input, cx| {
                    input.set_content(backup.backup_path.clone(), cx)
                });
                self.env_conflicts.clear();
                self.set_status(
                    format!(
                        "环境变量冲突已处理，备份 {}，{} 项",
                        backup.backup_path,
                        backup.conflicts.len()
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(format!("删除环境变量冲突失败: {err}"), cx),
        }
    }

    fn restore_env_backup(&mut self, cx: &mut Context<Self>) {
        let raw = self.env_restore_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status("请输入环境变量备份路径", cx);
            return;
        };
        match ochub_core::restore_env_backup(path.to_string_lossy().to_string()) {
            Ok(()) => self.set_status("环境变量备份已恢复", cx),
            Err(err) => self.set_status(format!("恢复环境变量备份失败: {err}"), cx),
        }
    }

    fn refresh_db_backups(&mut self, cx: &mut Context<Self>) {
        match load_db_backup_rows() {
            Ok(backups) => {
                let count = backups.len();
                self.db_backups = backups;
                self.set_status(format!("已刷新 {count} 个数据库备份"), cx);
            }
            Err(err) => self.set_status(format!("刷新数据库备份失败: {err}"), cx),
        }
    }

    fn create_db_backup(&mut self, cx: &mut Context<Self>) {
        match self.app.db.create_backup_file() {
            Ok(filename) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(format!("数据库备份已创建: {filename}"), cx);
            }
            Err(err) => self.set_status(format!("创建数据库备份失败: {err}"), cx),
        }
    }

    fn restore_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        match self.app.db.restore_from_backup(&filename) {
            Ok(backup_id) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(format!("已恢复 {filename}，当前库安全备份 {backup_id}"), cx);
            }
            Err(err) => self.set_status(format!("恢复数据库备份失败: {err}"), cx),
        }
    }

    fn rename_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        let new_name = self.backup_rename.read(cx).content().trim().to_string();
        if new_name.is_empty() {
            self.set_status("请输入新的备份名称", cx);
            return;
        }
        match ochub_core::Database::rename_backup(&filename, &new_name) {
            Ok(renamed) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(format!("数据库备份已重命名为 {renamed}"), cx);
            }
            Err(err) => self.set_status(format!("重命名数据库备份失败: {err}"), cx),
        }
    }

    fn delete_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        match ochub_core::Database::delete_backup(&filename) {
            Ok(()) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(format!("数据库备份已删除: {filename}"), cx);
            }
            Err(err) => self.set_status(format!("删除数据库备份失败: {err}"), cx),
        }
    }

    fn export_sql(&mut self, cx: &mut Context<Self>) {
        let raw = self.export_sql_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status("请输入 SQL 导出路径", cx);
            return;
        };
        match self.app.db.export_sql(&path) {
            Ok(()) => self.set_status(format!("SQL 已导出到 {}", path.display()), cx),
            Err(err) => self.set_status(format!("SQL 导出失败: {err}"), cx),
        }
    }

    fn import_sql(&mut self, cx: &mut Context<Self>) {
        let raw = self.import_sql_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status("请输入 SQL 导入路径", cx);
            return;
        };
        match self.app.db.import_sql(&path) {
            Ok(backup_id) => {
                let sync_warning =
                    match ochub_core::services::ProviderService::sync_current_to_live(&self.app) {
                        Ok(()) => String::new(),
                        Err(err) => format!("；应用当前供应商到工具配置时警告: {err}"),
                    };
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(
                    format!("SQL 已导入，导入前备份 {backup_id}{sync_warning}"),
                    cx,
                );
            }
            Err(err) => self.set_status(format!("SQL 导入失败: {err}"), cx),
        }
    }

    fn toggle_auto_launch(&mut self, cx: &mut Context<Self>) {
        let target = !self.auto_launch.unwrap_or(false);
        let silent = ochub_core::settings::get_settings().silent_startup;
        match ochub_core::autostart::set_enabled(target, silent) {
            Ok(()) => {
                self.auto_launch = Some(target);
                // The settings page shows the same OS state; keep the stored
                // flag in step so the two surfaces cannot disagree.
                if let Err(err) =
                    ochub_core::settings::mutate_settings(|s| s.launch_on_startup = target)
                {
                    log::warn!("failed to persist launch_on_startup: {err}");
                }
                self.set_status(
                    if target {
                        "已启用开机启动"
                    } else {
                        "已关闭开机启动"
                    },
                    cx,
                );
            }
            Err(err) => self.set_status(format!("{err}"), cx),
        }
    }

    fn read_omo(&mut self, slim: bool, cx: &mut Context<Self>) {
        let result = if slim {
            OmoService::read_local_file(&ochub_core::services::omo::SLIM)
        } else {
            OmoService::read_local_file(&ochub_core::services::omo::STANDARD)
        };
        match result {
            Ok(data) => self.set_status(
                format!(
                    "{}: {}",
                    if slim { "OMO Slim" } else { "OMO" },
                    data.file_path
                ),
                cx,
            ),
            Err(err) => self.set_status(format!("读取 OMO 失败: {err}"), cx),
        }
    }

    fn disable_omo(&mut self, slim: bool, cx: &mut Context<Self>) {
        let category = if slim { "omo-slim" } else { "omo" };
        let providers = self.app.db.get_all_providers("opencode");
        match providers {
            Ok(providers) => {
                for (id, provider) in &providers {
                    if provider.category.as_deref() == Some(category) {
                        let _ = self
                            .app
                            .db
                            .clear_omo_provider_current("opencode", id, category);
                    }
                }
                let result = if slim {
                    OmoService::delete_config_file(&ochub_core::services::omo::SLIM)
                } else {
                    OmoService::delete_config_file(&ochub_core::services::omo::STANDARD)
                };
                match result {
                    Ok(()) => self.set_status("已禁用 OMO 配置", cx),
                    Err(err) => self.set_status(format!("禁用 OMO 失败: {err}"), cx),
                }
            }
            Err(err) => self.set_status(format!("读取 OpenCode 供应商失败: {err}"), cx),
        }
    }

    fn validate_mcp_command(&mut self, cx: &mut Context<Self>) {
        let cmd = self.mcp_command.read(cx).content().trim().to_string();
        match ochub_core::mcp::validate_command_in_path(&cmd) {
            Ok(true) => self.set_status(format!("{cmd} 可用"), cx),
            Ok(false) => self.set_status(format!("{cmd} 不在 PATH 中"), cx),
            Err(err) => self.set_status(format!("校验失败: {err}"), cx),
        }
    }

    fn read_claude_mcp(&mut self, cx: &mut Context<Self>) {
        match ochub_core::mcp::read_mcp_json() {
            Ok(Some(content)) => {
                self.set_status(format!("Claude MCP 配置 {} 字符", content.len()), cx)
            }
            Ok(None) => self.set_status("Claude MCP 配置不存在", cx),
            Err(err) => self.set_status(format!("读取 Claude MCP 失败: {err}"), cx),
        }
    }

    fn apply_claude_plugin(&mut self, official: bool, cx: &mut Context<Self>) {
        let result = if official {
            claude_plugin::clear_claude_config()
        } else {
            claude_plugin::write_claude_config()
        };
        match result {
            Ok(changed) => self.set_status(format!("Claude 插件配置已处理，变更={changed}"), cx),
            Err(err) => self.set_status(format!("Claude 插件配置失败: {err}"), cx),
        }
    }

    fn mark_claude_onboarding(&mut self, completed: bool, cx: &mut Context<Self>) {
        let result = if completed {
            ochub_core::mcp::set_has_completed_onboarding()
        } else {
            ochub_core::mcp::clear_has_completed_onboarding()
        };
        match result {
            Ok(changed) => self.set_status(format!("Claude 引导状态已处理，变更={changed}"), cx),
            Err(err) => self.set_status(format!("Claude 引导状态写入失败: {err}"), cx),
        }
    }

    fn check_codex_unify_backup(&mut self, cx: &mut Context<Self>) {
        let exists =
            ochub_core::services::codex_history_migration::has_codex_official_history_unify_backup(
            );
        self.set_status(
            if exists {
                "当前 Codex 配置目录存在可恢复的统一历史备份"
            } else {
                "当前 Codex 配置目录没有可恢复的统一历史备份"
            },
            cx,
        );
    }

    fn restore_codex_unified_history(&mut self, cx: &mut Context<Self>) {
        match ochub_core::services::codex_history_migration::restore_codex_official_history_from_backups()
        {
            Ok(outcome) => {
                if let Some(reason) = outcome.skipped_reason {
                    self.set_status(format!("Codex 历史恢复已跳过: {reason}"), cx);
                } else {
                    self.set_status(
                        format!(
                            "Codex 历史已恢复：{} 个 jsonl 文件，{} 行 state 记录",
                            outcome.restored_jsonl_files, outcome.restored_state_rows
                        ),
                        cx,
                    );
                }
            }
            Err(err) => self.set_status(format!("Codex 历史恢复失败: {err}"), cx),
        }
    }

    fn refresh_sync_status(&mut self, s3: bool, cx: &mut Context<Self>) {
        if s3 {
            match ochub_core::settings::get_s3_sync_settings() {
                Some(settings) => self.set_status(
                    format_sync_status(
                        "S3",
                        settings.enabled,
                        settings.auto_sync,
                        &settings.status,
                    ),
                    cx,
                ),
                None => self.set_status("S3 同步尚未配置", cx),
            }
        } else {
            match ochub_core::settings::get_webdav_sync_settings() {
                Some(settings) => self.set_status(
                    format_sync_status(
                        "WebDAV",
                        settings.enabled,
                        settings.auto_sync,
                        &settings.status,
                    ),
                    cx,
                ),
                None => self.set_status("WebDAV 同步尚未配置", cx),
            }
        }
    }

    fn openclaw_health(&mut self, cx: &mut Context<Self>) {
        match openclaw::scan_openclaw_config_health() {
            Ok(health) => {
                self.set_status(format!("OpenClaw 配置检查：{} 个警告", health.len()), cx)
            }
            Err(err) => self.set_status(format!("OpenClaw 检查失败: {err}"), cx),
        }
    }

    fn hermes_summary(&mut self, cx: &mut Context<Self>) {
        match hermes::get_model_config() {
            Ok(Some(config)) => self.set_status(
                format!(
                    "Hermes 供应商={} 模型={}",
                    config.provider.unwrap_or_else(|| "未设置".to_string()),
                    config.default.unwrap_or_else(|| "未设置".to_string())
                ),
                cx,
            ),
            Ok(None) => self.set_status("Hermes 模型配置未初始化", cx),
            Err(err) => self.set_status(format!("Hermes 配置读取失败: {err}"), cx),
        }
    }

    fn save_openclaw_default_model(&mut self, cx: &mut Context<Self>) {
        let raw = self
            .openclaw_default_model_json
            .read(cx)
            .content()
            .to_string();
        match serde_json::from_str::<openclaw::OpenClawDefaultModel>(&raw)
            .map_err(|e| AppError::Message(format!("默认模型 JSON 无效: {e}")))
            .and_then(|model| openclaw::set_default_model(&model))
        {
            Ok(outcome) => self.set_status(openclaw_outcome_message("默认模型已保存", outcome), cx),
            Err(err) => self.set_status(format!("保存 OpenClaw 默认模型失败: {err}"), cx),
        }
    }

    fn save_openclaw_env(&mut self, cx: &mut Context<Self>) {
        let raw = self.openclaw_env_json.read(cx).content().to_string();
        match serde_json::from_str::<std::collections::HashMap<String, Value>>(&raw)
            .map(|vars| openclaw::OpenClawEnvConfig { vars })
            .map_err(|e| AppError::Message(format!("环境变量 JSON 无效: {e}")))
            .and_then(|env| openclaw::set_env_config(&env))
        {
            Ok(outcome) => self.set_status(openclaw_outcome_message("环境变量已保存", outcome), cx),
            Err(err) => self.set_status(format!("保存 OpenClaw 环境变量失败: {err}"), cx),
        }
    }

    fn save_openclaw_tools(&mut self, cx: &mut Context<Self>) {
        let raw = self.openclaw_tools_json.read(cx).content().to_string();
        match serde_json::from_str::<openclaw::OpenClawToolsConfig>(&raw)
            .map_err(|e| AppError::Message(format!("工具配置 JSON 无效: {e}")))
            .and_then(|tools| openclaw::set_tools_config(&tools))
        {
            Ok(outcome) => self.set_status(openclaw_outcome_message("工具配置已保存", outcome), cx),
            Err(err) => self.set_status(format!("保存 OpenClaw 工具配置失败: {err}"), cx),
        }
    }

    fn save_hermes_model(&mut self, cx: &mut Context<Self>) {
        let raw = self.hermes_model_json.read(cx).content().to_string();
        match serde_json::from_str::<hermes::HermesModelConfig>(&raw)
            .map_err(|e| AppError::Message(format!("模型配置 JSON 无效: {e}")))
            .and_then(|model| hermes::set_model_config(&model))
        {
            Ok(outcome) => {
                self.set_status(hermes_outcome_message("Hermes 模型配置已保存", outcome), cx)
            }
            Err(err) => self.set_status(format!("保存 Hermes 模型配置失败: {err}"), cx),
        }
    }

    fn save_hermes_memory(&mut self, kind: hermes::MemoryKind, cx: &mut Context<Self>) {
        let content = match kind {
            hermes::MemoryKind::Memory => self.hermes_memory_content.read(cx).content().to_string(),
            hermes::MemoryKind::User => self
                .hermes_user_memory_content
                .read(cx)
                .content()
                .to_string(),
        };
        match hermes::write_memory(kind, &content) {
            Ok(()) => self.set_status("Hermes 记忆已保存", cx),
            Err(err) => self.set_status(format!("保存 Hermes 记忆失败: {err}"), cx),
        }
    }

    fn toggle_hermes_memory(&mut self, kind: hermes::MemoryKind, cx: &mut Context<Self>) {
        let limits = self
            .hermes_limits
            .clone()
            .unwrap_or_else(hermes::HermesMemoryLimits::default);
        let target = match kind {
            hermes::MemoryKind::Memory => !limits.memory_enabled,
            hermes::MemoryKind::User => !limits.user_enabled,
        };
        match hermes::set_memory_enabled(kind, target) {
            Ok(outcome) => {
                self.hermes_limits = hermes::read_memory_limits().ok();
                self.set_status(hermes_outcome_message("Hermes 记忆开关已保存", outcome), cx);
            }
            Err(err) => self.set_status(format!("切换 Hermes 记忆失败: {err}"), cx),
        }
    }

    fn toggle_advanced_tools(&mut self, cx: &mut Context<Self>) {
        self.show_advanced_tools = !self.show_advanced_tools;
        self.list_state.remeasure();
        cx.notify();
    }

    fn toggle_all_backups(&mut self, cx: &mut Context<Self>) {
        self.show_all_backups = !self.show_all_backups;
        self.list_state.remeasure();
        cx.notify();
    }

    fn hermes_limits_row(limits: Option<hermes::HermesMemoryLimits>) -> impl IntoElement {
        let text = limits
            .map(|limits| {
                format!(
                    "MEMORY {} / {} 字符；USER {} / {} 字符",
                    if limits.memory_enabled {
                        "已启用"
                    } else {
                        "已关闭"
                    },
                    limits.memory,
                    if limits.user_enabled {
                        "已启用"
                    } else {
                        "已关闭"
                    },
                    limits.user
                )
            })
            .unwrap_or_else(|| "Hermes 记忆限制未初始化".to_string());
        components::card()
            .text_color(theme::muted())
            .text_xs()
            .child(SharedString::from(text))
    }

    fn render_config_row(&self, row: &ConfigRow, cx: &mut Context<Self>) -> impl IntoElement {
        let app = row.app;
        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.label.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(row.path.clone())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(components::badge(
                        if row.exists {
                            BadgeTone::Success
                        } else {
                            BadgeTone::Warning
                        },
                        if row.exists { "存在" } else { "未初始化" },
                    ))
                    .child(
                        components::button(
                            format!("open-config-{}", row.app.as_str()),
                            "打开",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_config_dir(app, cx);
                            },
                        )),
                    ),
            )
    }

    fn render_tool_version_row(
        version: &ochub_core::session_manager::ToolVersion,
    ) -> impl IntoElement {
        let local = version
            .version
            .clone()
            .unwrap_or_else(|| "未安装".to_string());
        let latest = version
            .latest_version
            .clone()
            .unwrap_or_else(|| "未知".to_string());
        let mut detail = format!("当前 {local} · 最新 {latest} · 环境 {}", version.env_type);
        if let Some(error) = &version.error {
            detail.push_str(&format!(" · {error}"));
        }
        if version.installed_but_broken {
            detail.push_str(" · 可执行文件异常");
        }
        components::card()
            .gap_1()
            .p_3()
            .child(
                div()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(version.name.clone())),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(SharedString::from(detail)),
            )
    }

    fn render_install_report_row(
        report: &ochub_core::session_manager::ToolInstallationReport,
    ) -> impl IntoElement {
        let summary = format!(
            "{} 个安装位置 · {} · {} · 命令: {}",
            report.installs.len(),
            if report.is_conflict {
                "存在冲突"
            } else {
                "无冲突"
            },
            if report.needs_confirmation {
                "需要确认"
            } else {
                "可直接执行"
            },
            report.command
        );
        let installs = report
            .installs
            .iter()
            .map(|install| {
                let version = install.version.as_deref().unwrap_or("未知版本");
                let error = install
                    .error
                    .as_deref()
                    .map(|err| format!(" · {err}"))
                    .unwrap_or_default();
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(SharedString::from(format!(
                        "{} · {version} · {} · {}{}",
                        install.path,
                        install.source,
                        if install.is_path_default {
                            "PATH 默认"
                        } else {
                            "非默认"
                        },
                        error
                    )))
            })
            .collect::<Vec<_>>();
        components::card()
            .gap_1()
            .p_3()
            .child(
                div()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(report.tool.clone())),
            )
            .child(
                div()
                    .text_color(if report.is_conflict {
                        theme::yellow()
                    } else {
                        theme::muted()
                    })
                    .text_xs()
                    .child(SharedString::from(summary)),
            )
            .children(installs)
    }

    fn render_env_conflict_row(conflict: &ochub_core::EnvConflict) -> impl IntoElement {
        components::card()
            .gap_1()
            .p_3()
            .child(
                div()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(conflict.var_name.clone())),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(SharedString::from(format!(
                        "{} · {} · {}",
                        conflict.source_type, conflict.source_path, conflict.var_value
                    ))),
            )
    }

    fn render_backup_row(&self, backup: &BackupRow, cx: &mut Context<Self>) -> impl IntoElement {
        let rename_name = backup.filename.clone();
        let restore_target = ConfirmAction::RestoreDbBackup(backup.filename.clone());
        let delete_target = ConfirmAction::DeleteDbBackup(backup.filename.clone());
        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(backup.filename.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(format!(
                                "{} · {}",
                                format_bytes(backup.size_bytes),
                                backup.created_at
                            ))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            format!("db-backup-restore-{}", backup.filename),
                            "恢复",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(restore_target.clone());
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        components::button(
                            format!("db-backup-rename-{}", backup.filename),
                            "重命名",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.rename_db_backup(rename_name.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            format!("db-backup-delete-{}", backup.filename),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm = Some(delete_target.clone());
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    /// Render one top-level page block as a virtualized list item. Only the
    /// on-screen blocks (plus overdraw) are built each frame — see
    /// [`crate::layout::wide_virtual_body`]. Each item carries its own bottom
    /// spacing (the list draws no inter-item gap).
    fn render_block(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let block = div().w_full().pb_4();
        match ix {
            0 => {
                let configured_count = self.config_rows.iter().filter(|row| row.exists).count();
                let total_configs = self.config_rows.len();
                let backup_count = self.db_backups.len();
                let env_conflict_count = self.env_conflicts.len();
                let env_app_name = env_app_label(self.env_app);
                block
                    .child(
                        div()
                            .grid()
                            .grid_cols(3)
                            .gap_3()
                            .w_full()
                            .child(components::stat_tile(
                                None,
                                theme::accent(),
                                "配置目录",
                                format!("{configured_count}/{total_configs}"),
                                "已初始化应用",
                            ))
                            .child(components::stat_tile(
                                None,
                                theme::green(),
                                "数据库备份",
                                backup_count.to_string(),
                                "可恢复快照",
                            ))
                            .child(components::stat_tile(
                                None,
                                if env_conflict_count == 0 {
                                    theme::teal()
                                } else {
                                    theme::yellow()
                                },
                                "环境冲突",
                                env_conflict_count.to_string(),
                                env_app_name,
                            )),
                    )
                    .into_any_element()
            }
            1 => {
                let config_rows: Vec<_> = self
                    .config_rows
                    .iter()
                    .map(|row| self.render_config_row(row, cx))
                    .collect();
                block
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .child(layout::section_header(
                                "配置目录",
                                "各应用配置目录的初始化状态，可直接打开对应目录。",
                            ))
                            .children(config_rows),
                    )
                    .into_any_element()
            }
            2 => block
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .w_full()
                        .child(layout::section_header(
                            "应用辅助",
                            "应用级辅助开关与 OcHub 数据目录。",
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    components::button(
                                        "auto-launch",
                                        if self.auto_launch.unwrap_or(false) {
                                            "关闭开机自启"
                                        } else {
                                            "启用开机自启"
                                        },
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.toggle_auto_launch(cx);
                                        },
                                    )),
                                )
                                .child(
                                    components::button(
                                        "open-app-config",
                                        "打开 OcHub 数据目录",
                                        ButtonTone::Neutral,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.open_path_action(
                                                ochub_core::paths::get_app_config_dir(),
                                                cx,
                                            );
                                        },
                                    )),
                                ),
                        ),
                )
                .into_any_element(),
            3 => {
                let tool_version_rows: Vec<_> = self
                    .tool_versions
                    .iter()
                    .map(Self::render_tool_version_row)
                    .collect();
                let tool_install_rows: Vec<_> = self
                    .tool_installations
                    .iter()
                    .map(Self::render_install_report_row)
                    .collect();
                block
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .child(layout::section_header(
                                "CLI 工具维护",
                                "探测 CLI 工具版本与安装分布，执行安装与更新。",
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        components::button(
                                            "cli-refresh-versions",
                                            if self.tool_busy {
                                                "处理中..."
                                            } else {
                                                "刷新版本"
                                            },
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.refresh_tool_versions(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "cli-probe-installations",
                                            "扫描安装位置",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.probe_cli_installations(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "cli-install-tools",
                                            "安装缺失工具",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_cli_lifecycle("install", cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "cli-update-tools",
                                            "更新全部工具",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_cli_lifecycle("update", cx);
                                            }),
                                        ),
                                    ),
                            )
                            .when(self.tool_versions.is_empty(), |s| {
                                s.child(components::empty_state(
                                    IconName::Terminal,
                                    "尚未刷新 CLI 版本",
                                    "点击“刷新版本”探测各 CLI 工具的本地与最新版本。",
                                    None,
                                ))
                            })
                            .children(tool_version_rows)
                            .when(self.tool_installations.is_empty(), |s| {
                                s.child(components::empty_state(
                                    IconName::Search,
                                    "尚未扫描安装位置",
                                    "点击“扫描安装位置”查看 CLI 安装分布与冲突。",
                                    None,
                                ))
                            })
                            .children(tool_install_rows),
                    )
                    .into_any_element()
            }
            4 => {
                let env_rows: Vec<_> = self
                    .env_conflicts
                    .iter()
                    .map(Self::render_env_conflict_row)
                    .collect();
                let env_selected = match self.env_app {
                    AppType::Codex => 1,
                    _ => 0,
                };
                let on_env_select = cx.listener(|this, ix: &usize, _window, cx| {
                    let app = match ix {
                        1 => AppType::Codex,
                        _ => AppType::Claude,
                    };
                    this.select_env_app(app, cx);
                });
                block
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .child(layout::section_header(
                                "环境变量冲突",
                                "按应用扫描环境变量冲突；删除前会自动备份，可从备份恢复。",
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(components::segmented(
                                        "tools-env-app",
                                        &["Claude", "Codex"],
                                        env_selected,
                                        move |ix, window, cx| on_env_select(&ix, window, cx),
                                    ))
                                    .child(
                                        components::button(
                                            "env-scan",
                                            "扫描冲突",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.scan_env_conflicts(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "env-delete",
                                            "删除并备份",
                                            ButtonTone::Danger,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.confirm =
                                                    Some(ConfirmAction::DeleteEnvConflicts);
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            )
                            .when(self.env_conflicts.is_empty(), |s| {
                                s.child(components::empty_state(
                                    IconName::Check,
                                    "当前没有已扫描出的冲突",
                                    "选择应用后点击“扫描冲突”检查环境变量。",
                                    None,
                                ))
                            })
                            .children(env_rows)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.env_restore_path.clone())
                                    .child(
                                        components::button(
                                            "env-restore",
                                            "恢复备份",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.restore_env_backup(cx);
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            5 => {
                let backup_count = self.db_backups.len();
                let visible_backup_limit = if self.show_all_backups {
                    backup_count
                } else {
                    backup_count.min(3)
                };
                let hidden_backup_count = backup_count.saturating_sub(visible_backup_limit);
                let backup_rows: Vec<_> = self
                    .db_backups
                    .iter()
                    .take(visible_backup_limit)
                    .map(|backup| self.render_backup_row(backup, cx))
                    .collect();
                block
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .child(layout::section_header(
                                "配置导入导出与数据库备份",
                                "SQL 导入导出，以及数据库快照的创建、恢复、重命名与删除。",
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.export_sql_path.clone())
                                    .child(
                                        components::button(
                                            "config-export-sql",
                                            "导出 SQL",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.export_sql(cx);
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.import_sql_path.clone())
                                    .child(
                                        components::button(
                                            "config-import-sql",
                                            "导入 SQL",
                                            ButtonTone::Danger,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.confirm = Some(ConfirmAction::ImportSql);
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        components::button(
                                            "db-backup-refresh",
                                            "刷新备份",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.refresh_db_backups(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "db-backup-create",
                                            "创建备份",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.create_db_backup(cx);
                                            }),
                                        ),
                                    )
                                    .child(self.backup_rename.clone()),
                            )
                            .when(self.db_backups.is_empty(), |s| {
                                s.child(components::empty_state(
                                    IconName::Archive,
                                    "暂无数据库备份",
                                    "点击“创建备份”生成第一个数据库快照。",
                                    None,
                                ))
                            })
                            .children(backup_rows)
                            .when(hidden_backup_count > 0, |s| {
                                s.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .gap_3()
                                        .w_full()
                                        .child(div().text_color(theme::muted()).text_xs().child(
                                            SharedString::from(format!(
                                                "还有 {hidden_backup_count} 个历史备份已收起。"
                                            )),
                                        ))
                                        .child(
                                            components::button(
                                                "db-backup-show-all",
                                                "显示全部备份",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.toggle_all_backups(cx);
                                                }),
                                            ),
                                        ),
                                )
                            })
                            .when(self.show_all_backups && backup_count > 3, |s| {
                                s.child(
                                    div().flex().w_full().justify_end().child(
                                        components::button(
                                            "db-backup-collapse",
                                            "收起历史备份",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.toggle_all_backups(cx);
                                            }),
                                        ),
                                    ),
                                )
                            }),
                    )
                    .into_any_element()
            }
            6 => {
                block
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        components::card().child(
                            components::disclosure(
                                "tools-advanced",
                                "高级项目",
                                "同步、Codex 历史、OMO、MCP 与 JSON 配置。",
                                self.show_advanced_tools,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_advanced_tools(cx);
                            })),
                        ),
                    )
                    .when(self.show_advanced_tools, |s| {
                        s.child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("同步状态"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "sync-webdav-status",
                                                "刷新 WebDAV 状态",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.refresh_sync_status(false, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "sync-s3-status",
                                                "刷新 S3 状态",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.refresh_sync_status(true, cx);
                                                },
                                            )),
                                        ),
                                ),
                        )
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Codex 历史"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "codex-unify-backup-check",
                                                "检查统一历史备份",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.check_codex_unify_backup(cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "codex-unify-restore",
                                                "恢复官方历史备份",
                                                ButtonTone::Danger,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.confirm =
                                                        Some(ConfirmAction::RestoreCodexHistory);
                                                    cx.notify();
                                                },
                                            )),
                                        ),
                                ),
                        )
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("OMO"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "omo-read",
                                                "读取 OMO",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.read_omo(false, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "omo-disable",
                                                "禁用 OMO",
                                                ButtonTone::Danger,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.confirm =
                                                        Some(ConfirmAction::DisableOmo {
                                                            slim: false,
                                                        });
                                                    cx.notify();
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "omo-slim-read",
                                                "读取 OMO Slim",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.read_omo(true, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "omo-slim-disable",
                                                "禁用 OMO Slim",
                                                ButtonTone::Danger,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.confirm =
                                                        Some(ConfirmAction::DisableOmo {
                                                            slim: true,
                                                        });
                                                    cx.notify();
                                                },
                                            )),
                                        ),
                                ),
                        )
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Claude MCP 与插件"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(self.mcp_command.clone())
                                        .child(
                                            components::button(
                                                "mcp-validate",
                                                "校验命令",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.validate_mcp_command(cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "mcp-read",
                                                "读取 MCP 配置",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.read_claude_mcp(cx);
                                                },
                                            )),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "claude-plugin-apply",
                                                "应用 OcHub 插件",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.apply_claude_plugin(false, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "claude-plugin-clear",
                                                "恢复官方 Claude 配置",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.apply_claude_plugin(true, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "claude-onboarding-skip",
                                                "跳过 Claude 引导",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.mark_claude_onboarding(true, cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "claude-onboarding-clear",
                                                "恢复 Claude 引导",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.mark_claude_onboarding(false, cx);
                                                },
                                            )),
                                        ),
                                ),
                        )
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("OpenClaw 高级配置"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "openclaw-health",
                                                "检查 OpenClaw 配置",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.openclaw_health(cx);
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "openclaw-refresh-advanced",
                                                "重读 OpenClaw/Hermes",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.refresh_advanced_configs(cx);
                                                    this.set_status("已重读高级配置", cx);
                                                },
                                            )),
                                        ),
                                )
                                .child(components::card().child(components::field(
                                    "默认模型 JSON",
                                    false,
                                    Some(
                                        "agents.defaults.model；支持 primary、fallbacks 和额外字段。"
                                            .into(),
                                    ),
                                    self.openclaw_default_model_json.clone(),
                                )))
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "openclaw-save-default-model",
                                            "保存默认模型",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.save_openclaw_default_model(cx);
                                            },
                                        )),
                                    ),
                                )
                                .child(components::card().child(components::field(
                                    "环境变量 JSON",
                                    false,
                                    Some("openclaw.json 的 env 节点；对象键会原样写入。".into()),
                                    self.openclaw_env_json.clone(),
                                )))
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "openclaw-save-env",
                                            "保存环境变量",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.save_openclaw_env(cx);
                                            },
                                        )),
                                    ),
                                )
                                .child(components::card().child(components::field(
                                    "工具配置 JSON",
                                    false,
                                    Some(
                                        "tools 节点；支持 profile、allow、deny 和额外字段。".into(),
                                    ),
                                    self.openclaw_tools_json.clone(),
                                )))
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "openclaw-save-tools",
                                            "保存工具配置",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.save_openclaw_tools(cx);
                                            },
                                        )),
                                    ),
                                ),
                        )
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Hermes 模型与记忆"),
                                )
                                .child(
                                    div().flex().flex_row().flex_wrap().gap_2().child(
                                        components::button(
                                            "hermes-summary",
                                            "读取 Hermes 模型配置",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.hermes_summary(cx);
                                            },
                                        )),
                                    ),
                                )
                                .child(components::card().child(components::field(
                                    "模型配置 JSON",
                                    false,
                                    Some("Hermes config.yaml 的 model 节点。".into()),
                                    self.hermes_model_json.clone(),
                                )))
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "hermes-save-model",
                                            "保存模型配置",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.save_hermes_model(cx);
                                            },
                                        )),
                                    ),
                                )
                                .child(Self::hermes_limits_row(self.hermes_limits.clone()))
                                .child(components::card().child(components::field(
                                    "MEMORY.md",
                                    false,
                                    Some("Hermes agent 记忆内容。".into()),
                                    self.hermes_memory_content.clone(),
                                )))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "hermes-save-memory",
                                                "保存 MEMORY",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.save_hermes_memory(
                                                        hermes::MemoryKind::Memory,
                                                        cx,
                                                    );
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "hermes-toggle-memory",
                                                "切换 MEMORY 启用",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.toggle_hermes_memory(
                                                        hermes::MemoryKind::Memory,
                                                        cx,
                                                    );
                                                },
                                            )),
                                        ),
                                )
                                .child(components::card().child(components::field(
                                    "USER.md",
                                    false,
                                    Some("Hermes user profile 记忆内容。".into()),
                                    self.hermes_user_memory_content.clone(),
                                )))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "hermes-save-user-memory",
                                                "保存 USER",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.save_hermes_memory(
                                                        hermes::MemoryKind::User,
                                                        cx,
                                                    );
                                                },
                                            )),
                                        )
                                        .child(
                                            components::button(
                                                "hermes-toggle-user-memory",
                                                "切换 USER 启用",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.toggle_hermes_memory(
                                                        hermes::MemoryKind::User,
                                                        cx,
                                                    );
                                                },
                                            )),
                                        ),
                                ),
                        )
                    })
                    .into_any_element()
            }
            _ => gpui::Empty.into_any_element(),
        }
    }
}

impl Render for ToolsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Only what the confirm modal (rendered on the page root, outside the
        // virtualized list) needs; every section builds its own data inside
        // `render_block`.
        let env_conflict_count = self.env_conflicts.len();
        let env_app_name = env_app_label(self.env_app);

        layout::page()
            .relative()
            .child(
                layout::page_header(
                    "高级工具",
                    Some("常用维护优先显示，深层配置按需展开。".into()),
                )
                .child(
                    components::icon_button_tone(
                        "tools-refresh",
                        "刷新",
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.reload();
                        this.refresh_advanced_configs(cx);
                        this.set_status("已刷新", cx);
                    })),
                ),
            )
            .child(layout::wide_virtual_body(
                "tools-body",
                gpui::list(
                    self.list_state.clone(),
                    cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
                ),
                &self.list_state,
            ))
            .when_some(self.confirm.clone(), |root, action| {
                let (title, message, confirm_label) = match &action {
                    ConfirmAction::RestoreDbBackup(name) => (
                        "恢复数据库备份",
                        format!(
                            "确定用备份「{name}」覆盖当前数据库吗？当前数据库会先自动创建安全备份。"
                        ),
                        "恢复",
                    ),
                    ConfirmAction::DeleteDbBackup(name) => (
                        "删除数据库备份",
                        format!("确定删除数据库备份「{name}」吗？此操作不可撤销。"),
                        "删除",
                    ),
                    ConfirmAction::DeleteEnvConflicts => (
                        "删除环境变量冲突",
                        format!(
                            "确定删除 {env_app_name} 扫描出的 {env_conflict_count} 个环境变量冲突吗？删除前会自动备份。"
                        ),
                        "删除并备份",
                    ),
                    ConfirmAction::ImportSql => (
                        "导入 SQL",
                        "确定从指定文件导入 SQL 吗？导入会覆盖当前数据库（导入前自动创建安全备份）。"
                            .to_string(),
                        "导入",
                    ),
                    ConfirmAction::RestoreCodexHistory => (
                        "恢复 Codex 官方历史",
                        "确定从统一历史备份恢复 Codex 官方历史吗？当前的官方历史可能被覆盖。"
                            .to_string(),
                        "恢复",
                    ),
                    ConfirmAction::DisableOmo { slim } => (
                        "禁用 OMO 配置",
                        format!(
                            "确定禁用 {} 吗？将删除对应配置文件并清除相关供应商的当前标记。",
                            if *slim { "OMO Slim" } else { "OMO" }
                        ),
                        "禁用",
                    ),
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(title))
                        .child(
                            components::modal_body().child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(message)),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "tools-confirm-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "tools-confirm-ok",
                                confirm_label,
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm = None;
                                match &action {
                                    ConfirmAction::RestoreDbBackup(name) => {
                                        this.restore_db_backup(name.clone(), cx)
                                    }
                                    ConfirmAction::DeleteDbBackup(name) => {
                                        this.delete_db_backup(name.clone(), cx)
                                    }
                                    ConfirmAction::DeleteEnvConflicts => {
                                        this.delete_env_conflicts(cx)
                                    }
                                    ConfirmAction::ImportSql => this.import_sql(cx),
                                    ConfirmAction::RestoreCodexHistory => {
                                        this.restore_codex_unified_history(cx)
                                    }
                                    ConfirmAction::DisableOmo { slim } => {
                                        this.disable_omo(*slim, cx)
                                    }
                                }
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

fn config_dir(app: AppType) -> Result<PathBuf, AppError> {
    ochub_core::plugin::get_plugin(&app.app_id())
        .ok_or_else(|| AppError::InvalidInput(format!("未知的应用类型: {app}")))?
        .config_dir()
}

fn config_status(app: &Arc<AppState>, app_type: AppType) -> Result<(bool, String), AppError> {
    let (exists, path) = match app_type {
        AppType::Claude => {
            let status = ochub_core::paths::get_claude_config_status();
            (status.exists, status.path)
        }
        AppType::ClaudeDesktop => {
            let status = claude_desktop::get_status(&app.db)?;
            (
                status.configured,
                status.config_library_path.unwrap_or_default(),
            )
        }
        AppType::Codex => {
            let auth_path = codex::get_codex_auth_path();
            let config_text = codex::read_codex_config_text().unwrap_or_default();
            (
                auth_path.exists() || !config_text.trim().is_empty(),
                codex::get_codex_config_dir().to_string_lossy().to_string(),
            )
        }
        AppType::GrokBuild => {
            let config_path = ochub_core::apps::grokbuild::get_grok_config_path();
            (
                config_path.exists(),
                ochub_core::apps::grokbuild::get_grok_config_dir()
                    .to_string_lossy()
                    .to_string(),
            )
        }
        AppType::OpenCode => {
            let config_path = opencode::get_opencode_config_path();
            (
                config_path.exists(),
                opencode::get_opencode_dir().to_string_lossy().to_string(),
            )
        }
        AppType::OpenClaw => {
            let config_path = openclaw::get_openclaw_config_path();
            (
                config_path.exists(),
                openclaw::get_openclaw_dir().to_string_lossy().to_string(),
            )
        }
        AppType::Hermes => {
            let config_path = hermes::get_hermes_config_path();
            (
                config_path.exists(),
                hermes::get_hermes_dir().to_string_lossy().to_string(),
            )
        }
    };
    Ok((exists, path))
}

fn format_sync_status(
    label: &str,
    enabled: bool,
    auto_sync: bool,
    status: &ochub_core::settings::WebDavSyncStatus,
) -> String {
    let last_sync = status
        .last_sync_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| "从未同步".to_string());
    let error = status
        .last_error
        .as_deref()
        .map(|message| {
            let source = status.last_error_source.as_deref().unwrap_or("未知来源");
            format!("；最近错误({source})：{message}")
        })
        .unwrap_or_default();
    format!(
        "{label}：{}，自动同步{}，最近同步：{last_sync}{error}",
        if enabled { "已启用" } else { "未启用" },
        if auto_sync { "开启" } else { "关闭" },
    )
}

fn openclaw_outcome_message(prefix: &str, outcome: openclaw::OpenClawWriteOutcome) -> String {
    let backup = outcome
        .backup_path
        .map(|path| format!("；备份 {path}"))
        .unwrap_or_default();
    let warnings = if outcome.warnings.is_empty() {
        String::new()
    } else {
        format!("；{} 个警告", outcome.warnings.len())
    };
    format!("{prefix}{backup}{warnings}")
}

fn hermes_outcome_message(prefix: &str, outcome: hermes::HermesWriteOutcome) -> String {
    let backup = outcome
        .backup_path
        .map(|path| format!("；备份 {path}"))
        .unwrap_or_default();
    format!("{prefix}{backup}")
}

fn cli_tool_ids() -> Vec<String> {
    ["claude", "codex", "opencode", "openclaw", "hermes"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn env_app_label(app: AppType) -> &'static str {
    match app {
        AppType::Claude => "Claude",
        AppType::Codex => "Codex",
        _ => app.as_str(),
    }
}

fn expand_user_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest));
    }
    Some(PathBuf::from(trimmed))
}

fn load_db_backup_rows() -> Result<Vec<BackupRow>, AppError> {
    let value = serde_json::to_value(ochub_core::Database::list_backups()?)
        .map_err(|e| AppError::Message(format!("序列化数据库备份失败: {e}")))?;
    let Some(array) = value.as_array() else {
        return Ok(Vec::new());
    };
    Ok(array
        .iter()
        .filter_map(|item| {
            Some(BackupRow {
                filename: item.get("filename")?.as_str()?.to_string(),
                size_bytes: item.get("sizeBytes")?.as_u64().unwrap_or_default(),
                created_at: item
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

fn open_path(path: &Path) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("explorer");
        cmd.arg(path);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    let status = cmd
        .status()
        .map_err(|e| AppError::Message(format!("打开路径失败: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!("打开路径失败: {status}")))
    }
}

crate::notifications::impl_status_toasts!(ToolsView);
