//! Advanced utility panel for features that used to live as scattered Tauri
//! commands: config folders, OMO files, OpenClaw workspace memory, Claude MCP,
//! and app-level helper toggles.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use gpui::{div, prelude::*, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::apps::{claude_desktop, claude_plugin, codex, hermes, openclaw, opencode};
use routedeck_core::services::{OmoService, WorkspaceService};
use routedeck_core::{AppError, AppState, AppType};
use serde_json::Value;

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone)]
struct ConfigRow {
    app: AppType,
    label: &'static str,
    exists: bool,
    path: String,
}

#[derive(Clone)]
struct BackupRow {
    filename: String,
    size_bytes: u64,
    created_at: String,
}

pub struct ToolsView {
    app: Arc<AppState>,
    config_rows: Vec<ConfigRow>,
    auto_launch: Option<bool>,
    tool_versions: Vec<routedeck_core::session_manager::ToolVersion>,
    tool_installations: Vec<routedeck_core::session_manager::ToolInstallationReport>,
    tool_busy: bool,
    env_app: AppType,
    env_conflicts: Vec<routedeck_core::EnvConflict>,
    db_backups: Vec<BackupRow>,
    show_all_backups: bool,
    show_advanced_tools: bool,
    memory_files: Vec<routedeck_core::services::DailyMemoryFileInfo>,
    memory_results: Vec<routedeck_core::services::DailyMemorySearchResult>,
    export_sql_path: Entity<TextInput>,
    import_sql_path: Entity<TextInput>,
    env_restore_path: Entity<TextInput>,
    backup_rename: Entity<TextInput>,
    workspace_file: Entity<TextInput>,
    workspace_content: Entity<TextInput>,
    memory_file: Entity<TextInput>,
    memory_content: Entity<TextInput>,
    memory_query: Entity<TextInput>,
    mcp_command: Entity<TextInput>,
    openclaw_default_model_json: Entity<TextInput>,
    openclaw_env_json: Entity<TextInput>,
    openclaw_tools_json: Entity<TextInput>,
    hermes_model_json: Entity<TextInput>,
    hermes_memory_content: Entity<TextInput>,
    hermes_user_memory_content: Entity<TextInput>,
    hermes_limits: Option<hermes::HermesMemoryLimits>,
    status: Option<SharedString>,
}

impl ToolsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let workspace_file = cx.new(|cx| {
            let mut input = TextInput::new(cx, "AGENTS.md");
            input.set_content("AGENTS.md", cx);
            input
        });
        let workspace_content = cx.new(|cx| TextInput::new(cx, "文件内容"));
        let memory_file = cx.new(|cx| TextInput::new(cx, "YYYY-MM-DD.md"));
        let memory_content = cx.new(|cx| TextInput::new(cx, "记忆内容"));
        let memory_query = cx.new(|cx| TextInput::new(cx, "搜索每日记忆"));
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
        let export_sql_path =
            cx.new(|cx| TextInput::new(cx, "~/.cc-switch/exports/RouteDeck.sql"));
        let import_sql_path = cx.new(|cx| TextInput::new(cx, "/path/to/RouteDeck.sql"));
        let env_restore_path =
            cx.new(|cx| TextInput::new(cx, "~/.cc-switch/backups/env-backup-YYYYMMDD.json"));
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
            memory_files: Vec::new(),
            memory_results: Vec::new(),
            export_sql_path,
            import_sql_path,
            env_restore_path,
            backup_rename,
            workspace_file,
            workspace_content,
            memory_file,
            memory_content,
            memory_query,
            mcp_command,
            openclaw_default_model_json,
            openclaw_env_json,
            openclaw_tools_json,
            hermes_model_json,
            hermes_memory_content,
            hermes_user_memory_content,
            hermes_limits: None,
            status: None,
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
        self.auto_launch = auto_launch_handle()
            .and_then(|handle| {
                handle
                    .is_enabled()
                    .map_err(|e| AppError::Message(e.to_string()))
            })
            .ok();
        self.memory_files = WorkspaceService::list_daily_memory_files().unwrap_or_default();
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

    fn all_apps() -> [(AppType, &'static str); 6] {
        [
            (AppType::Claude, "Claude Code"),
            (AppType::ClaudeDesktop, "Claude Desktop"),
            (AppType::Codex, "Codex"),
            (AppType::OpenCode, "OpenCode"),
            (AppType::OpenClaw, "OpenClaw"),
            (AppType::Hermes, "Hermes"),
        ]
    }

    fn set_status(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(msg.into());
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
        cx.notify();

        let task = cx.background_spawn(async move {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("创建异步运行时失败: {e}"))
                .and_then(|runtime| {
                    runtime.block_on(routedeck_core::session_manager::get_tool_versions(None, None))
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
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn probe_cli_installations(&mut self, cx: &mut Context<Self>) {
        match routedeck_core::session_manager::probe_tool_installations(cli_tool_ids()) {
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
        cx.notify();

        let tools = cli_tool_ids();
        let task = cx.background_spawn(async move {
            routedeck_core::session_manager::run_tool_lifecycle_action(tools, action.to_string(), None)
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
        match routedeck_core::check_env_conflicts(self.env_app.as_str()) {
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
        match routedeck_core::delete_env_vars(self.env_conflicts.clone()) {
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
        match routedeck_core::restore_env_backup(path.to_string_lossy().to_string()) {
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
        match routedeck_core::Database::rename_backup(&filename, &new_name) {
            Ok(renamed) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(format!("数据库备份已重命名为 {renamed}"), cx);
            }
            Err(err) => self.set_status(format!("重命名数据库备份失败: {err}"), cx),
        }
    }

    fn delete_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        match routedeck_core::Database::delete_backup(&filename) {
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
                    match routedeck_core::services::ProviderService::sync_current_to_live(&self.app) {
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
        match auto_launch_handle() {
            Ok(handle) => {
                let target = !self.auto_launch.unwrap_or(false);
                let result = if target {
                    handle.enable()
                } else {
                    handle.disable()
                };
                match result {
                    Ok(()) => {
                        self.auto_launch = Some(target);
                        self.set_status(
                            if target {
                                "已启用开机自启"
                            } else {
                                "已关闭开机自启"
                            },
                            cx,
                        );
                    }
                    Err(err) => self.set_status(format!("开机自启设置失败: {err}"), cx),
                }
            }
            Err(err) => self.set_status(format!("开机自启不可用: {err}"), cx),
        }
    }

    fn read_omo(&mut self, slim: bool, cx: &mut Context<Self>) {
        let result = if slim {
            OmoService::read_local_file(&routedeck_core::services::omo::SLIM)
        } else {
            OmoService::read_local_file(&routedeck_core::services::omo::STANDARD)
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
                    OmoService::delete_config_file(&routedeck_core::services::omo::SLIM)
                } else {
                    OmoService::delete_config_file(&routedeck_core::services::omo::STANDARD)
                };
                match result {
                    Ok(()) => self.set_status("已禁用 OMO 配置", cx),
                    Err(err) => self.set_status(format!("禁用 OMO 失败: {err}"), cx),
                }
            }
            Err(err) => self.set_status(format!("读取 OpenCode 供应商失败: {err}"), cx),
        }
    }

    fn load_workspace_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.workspace_file.read(cx).content().trim().to_string();
        match WorkspaceService::read_workspace_file(&filename) {
            Ok(Some(content)) => {
                self.workspace_content
                    .update(cx, |input, cx| input.set_content(content, cx));
                self.set_status(format!("已读取 {filename}"), cx);
            }
            Ok(None) => self.set_status(format!("{filename} 尚不存在"), cx),
            Err(err) => self.set_status(format!("读取工作区文件失败: {err}"), cx),
        }
    }

    fn save_workspace_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.workspace_file.read(cx).content().trim().to_string();
        let content = self.workspace_content.read(cx).content().to_string();
        match WorkspaceService::write_workspace_file(&filename, &content) {
            Ok(()) => self.set_status(format!("已保存 {filename}"), cx),
            Err(err) => self.set_status(format!("保存工作区文件失败: {err}"), cx),
        }
    }

    fn load_memory_file(&mut self, filename: Option<String>, cx: &mut Context<Self>) {
        let filename =
            filename.unwrap_or_else(|| self.memory_file.read(cx).content().trim().to_string());
        if filename.is_empty() {
            self.set_status("请输入每日记忆文件名", cx);
            return;
        }
        match WorkspaceService::read_daily_memory_file(&filename) {
            Ok(Some(content)) => {
                self.memory_file
                    .update(cx, |input, cx| input.set_content(filename.clone(), cx));
                self.memory_content
                    .update(cx, |input, cx| input.set_content(content, cx));
                self.set_status(format!("已读取 {filename}"), cx);
            }
            Ok(None) => self.set_status(format!("{filename} 尚不存在"), cx),
            Err(err) => self.set_status(format!("读取每日记忆失败: {err}"), cx),
        }
    }

    fn save_memory_file(&mut self, cx: &mut Context<Self>) {
        let filename = self.memory_file.read(cx).content().trim().to_string();
        let content = self.memory_content.read(cx).content().to_string();
        match WorkspaceService::write_daily_memory_file(&filename, &content) {
            Ok(()) => {
                self.memory_files = WorkspaceService::list_daily_memory_files().unwrap_or_default();
                self.set_status(format!("已保存 {filename}"), cx);
            }
            Err(err) => self.set_status(format!("保存每日记忆失败: {err}"), cx),
        }
    }

    fn delete_memory_file(&mut self, filename: String, cx: &mut Context<Self>) {
        match WorkspaceService::delete_daily_memory_file(&filename) {
            Ok(()) => {
                self.memory_files = WorkspaceService::list_daily_memory_files().unwrap_or_default();
                self.set_status(format!("已删除 {filename}"), cx);
            }
            Err(err) => self.set_status(format!("删除每日记忆失败: {err}"), cx),
        }
    }

    fn search_memory(&mut self, cx: &mut Context<Self>) {
        let query = self.memory_query.read(cx).content().trim().to_string();
        match WorkspaceService::search_daily_memory_files(&query) {
            Ok(results) => {
                let count = results.len();
                self.memory_results = results;
                self.set_status(format!("找到 {count} 个结果"), cx);
            }
            Err(err) => self.set_status(format!("搜索失败: {err}"), cx),
        }
    }

    fn open_workspace_dir(&mut self, memory: bool, cx: &mut Context<Self>) {
        match WorkspaceService::ensure_directory_for_subdir(if memory {
            "memory"
        } else {
            "workspace"
        }) {
            Ok(path) => self.open_path_action(path, cx),
            Err(err) => self.set_status(format!("打开工作区失败: {err}"), cx),
        }
    }

    fn validate_mcp_command(&mut self, cx: &mut Context<Self>) {
        let cmd = self.mcp_command.read(cx).content().trim().to_string();
        match routedeck_core::mcp::validate_command_in_path(&cmd) {
            Ok(true) => self.set_status(format!("{cmd} 可用"), cx),
            Ok(false) => self.set_status(format!("{cmd} 不在 PATH 中"), cx),
            Err(err) => self.set_status(format!("校验失败: {err}"), cx),
        }
    }

    fn read_claude_mcp(&mut self, cx: &mut Context<Self>) {
        match routedeck_core::mcp::read_mcp_json() {
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
            routedeck_core::mcp::set_has_completed_onboarding()
        } else {
            routedeck_core::mcp::clear_has_completed_onboarding()
        };
        match result {
            Ok(changed) => self.set_status(format!("Claude 引导状态已处理，变更={changed}"), cx),
            Err(err) => self.set_status(format!("Claude 引导状态写入失败: {err}"), cx),
        }
    }

    fn check_codex_unify_backup(&mut self, cx: &mut Context<Self>) {
        let exists =
            routedeck_core::services::codex_history_migration::has_codex_official_history_unify_backup();
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
        match routedeck_core::services::codex_history_migration::restore_codex_official_history_from_backups()
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
            match routedeck_core::settings::get_s3_sync_settings() {
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
            match routedeck_core::settings::get_webdav_sync_settings() {
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
        cx.notify();
    }

    fn toggle_all_backups(&mut self, cx: &mut Context<Self>) {
        self.show_all_backups = !self.show_all_backups;
        cx.notify();
    }

    fn header(title: &str) -> impl IntoElement {
        div()
            .text_color(theme::c(theme::TEXT))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child(SharedString::from(title.to_string()))
    }

    fn labeled_editor(
        label: &'static str,
        description: &'static str,
        input: Entity<TextInput>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child(description),
                    ),
            )
            .child(input)
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
        div()
            .p_4()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .text_color(theme::c(theme::MUTED))
            .text_xs()
            .child(SharedString::from(text))
    }

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary)
    }

    fn overview_tile(
        label: &'static str,
        value: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        tone: u32,
    ) -> impl IntoElement {
        let value = value.into();
        let detail = detail.into();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_4()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(gpui::px(8.))
                            .h(gpui::px(8.))
                            .rounded_full()
                            .bg(theme::c(tone)),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    ),
            )
            .child(
                div()
                    .text_color(theme::c(theme::TEXT))
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child(value),
            )
            .child(
                div()
                    .text_color(theme::c(theme::SUBTEXT))
                    .text_xs()
                    .child(detail),
            )
    }

    fn advanced_preview(showing: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_4()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if showing {
                                "高级项目已展开"
                            } else {
                                "高级项目已收起"
                            }),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child("同步、Codex 历史、OMO、工作区、记忆、MCP 与 JSON 配置。"),
                    ),
            )
            .child(
                Self::action_button(
                    "tools-advanced-toggle",
                    if showing {
                        "收起高级项目"
                    } else {
                        "展开高级项目"
                    },
                    false,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_advanced_tools(cx);
                }))
                .aria_expanded(showing),
            )
    }

    fn render_config_row(&self, row: &ConfigRow, cx: &mut Context<Self>) -> impl IntoElement {
        let app = row.app;
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.label),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
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
                    .child(
                        div()
                            .px_2()
                            .rounded_md()
                            .bg(theme::c(if row.exists {
                                theme::GREEN
                            } else {
                                theme::YELLOW
                            }))
                            .text_color(theme::c(theme::ACCENT_TEXT))
                            .text_xs()
                            .child(if row.exists { "存在" } else { "未初始化" }),
                    )
                    .child(
                        Self::action_button(
                            format!("open-config-{}", row.app.as_str()),
                            "打开",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_config_dir(app, cx);
                            },
                        )),
                    ),
            )
    }

    fn render_memory_row(
        &self,
        file: &routedeck_core::services::DailyMemoryFileInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let read_name = file.filename.clone();
        let delete_name = file.filename.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(file.filename.clone())),
                    )
                    .child(div().text_color(theme::c(theme::MUTED)).text_xs().child(
                        SharedString::from(format!("{} 字节 · {}", file.size_bytes, file.preview)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Self::action_button(
                            format!("memory-read-{}", file.filename),
                            "读取",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.load_memory_file(Some(read_name.clone()), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("memory-delete-{}", file.filename),
                            "删除",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_memory_file(delete_name.clone(), cx);
                            },
                        )),
                    ),
            )
    }

    fn render_tool_version_row(
        version: &routedeck_core::session_manager::ToolVersion,
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
        div()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .text_color(theme::c(theme::TEXT))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(version.name.clone())),
            )
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(SharedString::from(detail)),
            )
    }

    fn render_install_report_row(
        report: &routedeck_core::session_manager::ToolInstallationReport,
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
                    .text_color(theme::c(theme::MUTED))
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
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .text_color(theme::c(theme::TEXT))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(report.tool.clone())),
            )
            .child(
                div()
                    .text_color(theme::c(if report.is_conflict {
                        theme::YELLOW
                    } else {
                        theme::MUTED
                    }))
                    .text_xs()
                    .child(SharedString::from(summary)),
            )
            .children(installs)
    }

    fn render_env_conflict_row(conflict: &routedeck_core::EnvConflict) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .text_color(theme::c(theme::TEXT))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(conflict.var_name.clone())),
            )
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(SharedString::from(format!(
                        "{} · {} · {}",
                        conflict.source_type, conflict.source_path, conflict.var_value
                    ))),
            )
    }

    fn render_backup_row(&self, backup: &BackupRow, cx: &mut Context<Self>) -> impl IntoElement {
        let restore_name = backup.filename.clone();
        let rename_name = backup.filename.clone();
        let delete_name = backup.filename.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(backup.filename.clone())),
                    )
                    .child(div().text_color(theme::c(theme::MUTED)).text_xs().child(
                        SharedString::from(format!(
                            "{} · {}",
                            format_bytes(backup.size_bytes),
                            backup.created_at
                        )),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Self::action_button(
                            format!("db-backup-restore-{}", backup.filename),
                            "恢复",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.restore_db_backup(restore_name.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("db-backup-rename-{}", backup.filename),
                            "重命名",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.rename_db_backup(rename_name.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            format!("db-backup-delete-{}", backup.filename),
                            "删除",
                            false,
                        )
                        .text_color(theme::c(theme::RED))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_db_backup(delete_name.clone(), cx);
                            },
                        )),
                    ),
            )
    }
}

impl Render for ToolsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let configured_count = self.config_rows.iter().filter(|row| row.exists).count();
        let total_configs = self.config_rows.len();
        let backup_count = self.db_backups.len();
        let visible_backup_limit = if self.show_all_backups {
            backup_count
        } else {
            backup_count.min(3)
        };
        let hidden_backup_count = backup_count.saturating_sub(visible_backup_limit);
        let env_conflict_count = self.env_conflicts.len();
        let memory_count = self.memory_files.len();
        let config_rows: Vec<_> = self
            .config_rows
            .iter()
            .map(|row| self.render_config_row(row, cx))
            .collect();
        let memory_rows: Vec<_> = self
            .memory_files
            .iter()
            .map(|file| self.render_memory_row(file, cx))
            .collect();
        let search_rows: Vec<_> = self
            .memory_results
            .iter()
            .map(|result| {
                div()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(theme::c(theme::SURFACE))
                    .border_1()
                    .border_color(theme::c(theme::BORDER))
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(format!(
                                "{} · {} 次",
                                result.filename, result.match_count
                            ))),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child(SharedString::from(result.snippet.clone())),
                    )
            })
            .collect();
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
        let env_rows: Vec<_> = self
            .env_conflicts
            .iter()
            .map(Self::render_env_conflict_row)
            .collect();
        let backup_rows: Vec<_> = self
            .db_backups
            .iter()
            .take(visible_backup_limit)
            .map(|backup| self.render_backup_row(backup, cx))
            .collect();

        layout::page()
            .child(
                layout::page_header(
                    "高级工具",
                    Some("常用维护优先显示，深层配置按需展开。".into()),
                )
                .child(
                    Self::action_button("tools-refresh", "刷新", false).on_click(cx.listener(
                        |this, _event, _window, cx| {
                            this.reload();
                            this.refresh_advanced_configs(cx);
                            this.set_status("已刷新", cx);
                        },
                    )),
                ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::c(theme::TEAL))
                        .text_xs()
                        .child(status),
                )
            })
            .child(
                div()
                    .id("tools-body")
                    .flex()
                    .flex_col()
                    .gap_5()
                    .p_6()
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .grid()
                            .grid_cols(4)
                            .gap_3()
                            .child(Self::overview_tile(
                                "配置目录",
                                format!("{configured_count}/{total_configs}"),
                                "已初始化应用",
                                theme::ACCENT,
                            ))
                            .child(Self::overview_tile(
                                "数据库备份",
                                backup_count.to_string(),
                                "可恢复快照",
                                theme::GREEN,
                            ))
                            .child(Self::overview_tile(
                                "环境冲突",
                                env_conflict_count.to_string(),
                                env_app_label(self.env_app),
                                if env_conflict_count == 0 {
                                    theme::TEAL
                                } else {
                                    theme::YELLOW
                                },
                            ))
                            .child(Self::overview_tile(
                                "每日记忆",
                                memory_count.to_string(),
                                "工作区文件",
                                theme::MAUVE,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("配置目录"))
                            .children(config_rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("应用辅助"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button(
                                            "auto-launch",
                                            if self.auto_launch.unwrap_or(false) {
                                                "关闭开机自启"
                                            } else {
                                                "启用开机自启"
                                            },
                                            true,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.toggle_auto_launch(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "open-app-config",
                                            "打开 RouteDeck 数据目录",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.open_path_action(
                                                    routedeck_core::paths::get_app_config_dir(),
                                                    cx,
                                                );
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("CLI 工具维护"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button(
                                            "cli-refresh-versions",
                                            if self.tool_busy {
                                                "处理中..."
                                            } else {
                                                "刷新版本"
                                            },
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.refresh_tool_versions(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "cli-probe-installations",
                                            "扫描安装位置",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.probe_cli_installations(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "cli-install-tools",
                                            "安装缺失工具",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_cli_lifecycle("install", cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "cli-update-tools",
                                            "更新全部工具",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_cli_lifecycle("update", cx);
                                            }),
                                        ),
                                    ),
                            )
                            .when(self.tool_versions.is_empty(), |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("尚未刷新 CLI 版本"),
                                )
                            })
                            .children(tool_version_rows)
                            .when(self.tool_installations.is_empty(), |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("尚未扫描安装位置"),
                                )
                            })
                            .children(tool_install_rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("环境变量冲突"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button(
                                            "env-app-claude",
                                            if self.env_app == AppType::Claude {
                                                "Claude ✓"
                                            } else {
                                                "Claude"
                                            },
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.select_env_app(AppType::Claude, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "env-app-codex",
                                            if self.env_app == AppType::Codex {
                                                "Codex ✓"
                                            } else {
                                                "Codex"
                                            },
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.select_env_app(AppType::Codex, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button("env-scan", "扫描冲突", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.scan_env_conflicts(cx);
                                            })),
                                    )
                                    .child(
                                        Self::action_button("env-delete", "删除并备份", false)
                                            .text_color(theme::c(theme::RED))
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.delete_env_conflicts(cx);
                                            })),
                                    ),
                            )
                            .when(self.env_conflicts.is_empty(), |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("当前没有已扫描出的冲突"),
                                )
                            })
                            .children(env_rows)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.env_restore_path.clone())
                                    .child(
                                        Self::action_button("env-restore", "恢复备份", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.restore_env_backup(cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(Self::header("配置导入导出与数据库备份"))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.export_sql_path.clone())
                                    .child(
                                        Self::action_button("config-export-sql", "导出 SQL", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.export_sql(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(self.import_sql_path.clone())
                                    .child(
                                        Self::action_button("config-import-sql", "导入 SQL", false)
                                            .text_color(theme::c(theme::RED))
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.import_sql(cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button("db-backup-refresh", "刷新备份", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.refresh_db_backups(cx);
                                            })),
                                    )
                                    .child(
                                        Self::action_button("db-backup-create", "创建备份", true)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.create_db_backup(cx);
                                            })),
                                    )
                                    .child(self.backup_rename.clone()),
                            )
                            .when(self.db_backups.is_empty(), |s| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::MUTED))
                                        .text_xs()
                                        .child("暂无数据库备份"),
                                )
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
                                        .child(
                                            div()
                                                .text_color(theme::c(theme::MUTED))
                                                .text_xs()
                                                .child(SharedString::from(format!(
                                                    "还有 {hidden_backup_count} 个历史备份已收起。"
                                                ))),
                                        )
                                        .child(
                                            Self::action_button(
                                                "db-backup-show-all",
                                                "显示全部备份",
                                                false,
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
                                    div().flex().justify_end().child(
                                        Self::action_button(
                                            "db-backup-collapse",
                                            "收起历史备份",
                                            false,
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
                    .child(Self::advanced_preview(self.show_advanced_tools, cx))
                    .when(self.show_advanced_tools, |s| {
                        s.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("同步状态"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button(
                                                "sync-webdav-status",
                                                "刷新 WebDAV 状态",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.refresh_sync_status(false, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "sync-s3-status",
                                                "刷新 S3 状态",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.refresh_sync_status(true, cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("Codex 历史"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button(
                                                "codex-unify-backup-check",
                                                "检查统一历史备份",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.check_codex_unify_backup(cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "codex-unify-restore",
                                                "恢复官方历史备份",
                                                false,
                                            )
                                            .text_color(theme::c(theme::RED))
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.restore_codex_unified_history(cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("OMO"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button("omo-read", "读取 OMO", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.read_omo(false, cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button("omo-disable", "禁用 OMO", false)
                                                .text_color(theme::c(theme::RED))
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.disable_omo(false, cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button(
                                                "omo-slim-read",
                                                "读取 OMO Slim",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.read_omo(true, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "omo-slim-disable",
                                                "禁用 OMO Slim",
                                                false,
                                            )
                                            .text_color(theme::c(theme::RED))
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.disable_omo(true, cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("OpenClaw 工作区"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(self.workspace_file.clone())
                                        .child(
                                            Self::action_button("workspace-load", "读取", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.load_workspace_file(cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button("workspace-save", "保存", true)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.save_workspace_file(cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button(
                                                "workspace-open",
                                                "打开目录",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.open_workspace_dir(false, cx);
                                                }),
                                            ),
                                        ),
                                )
                                .child(self.workspace_content.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("每日记忆"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(self.memory_file.clone())
                                        .child(
                                            Self::action_button("memory-load", "读取", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.load_memory_file(None, cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button("memory-save", "保存", true)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.save_memory_file(cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button("memory-open", "打开目录", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.open_workspace_dir(true, cx);
                                                    },
                                                )),
                                        ),
                                )
                                .child(self.memory_content.clone())
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(self.memory_query.clone())
                                        .child(
                                            Self::action_button("memory-search", "搜索", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.search_memory(cx);
                                                    },
                                                )),
                                        ),
                                )
                                .children(memory_rows)
                                .children(search_rows),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("Claude MCP 与插件"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(self.mcp_command.clone())
                                        .child(
                                            Self::action_button("mcp-validate", "校验命令", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.validate_mcp_command(cx);
                                                    },
                                                )),
                                        )
                                        .child(
                                            Self::action_button("mcp-read", "读取 MCP 配置", false)
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
                                            Self::action_button(
                                                "claude-plugin-apply",
                                                "应用 RouteDeck 插件",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.apply_claude_plugin(false, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "claude-plugin-clear",
                                                "恢复官方 Claude 配置",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.apply_claude_plugin(true, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "claude-onboarding-skip",
                                                "跳过 Claude 引导",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.mark_claude_onboarding(true, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "claude-onboarding-clear",
                                                "恢复 Claude 引导",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.mark_claude_onboarding(false, cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("OpenClaw 高级配置"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button(
                                                "openclaw-health",
                                                "检查 OpenClaw 配置",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.openclaw_health(cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "openclaw-refresh-advanced",
                                                "重读 OpenClaw/Hermes",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.refresh_advanced_configs(cx);
                                                    this.set_status("已重读高级配置", cx);
                                                }),
                                            ),
                                        ),
                                )
                                .child(Self::labeled_editor(
                                    "默认模型 JSON",
                                    "agents.defaults.model；支持 primary、fallbacks 和额外字段。",
                                    self.openclaw_default_model_json.clone(),
                                ))
                                .child(
                                    Self::action_button(
                                        "openclaw-save-default-model",
                                        "保存默认模型",
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.save_openclaw_default_model(cx);
                                        },
                                    )),
                                )
                                .child(Self::labeled_editor(
                                    "环境变量 JSON",
                                    "openclaw.json 的 env 节点；对象键会原样写入。",
                                    self.openclaw_env_json.clone(),
                                ))
                                .child(
                                    Self::action_button("openclaw-save-env", "保存环境变量", true)
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.save_openclaw_env(cx);
                                        })),
                                )
                                .child(Self::labeled_editor(
                                    "工具配置 JSON",
                                    "tools 节点；支持 profile、allow、deny 和额外字段。",
                                    self.openclaw_tools_json.clone(),
                                ))
                                .child(
                                    Self::action_button(
                                        "openclaw-save-tools",
                                        "保存工具配置",
                                        true,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.save_openclaw_tools(cx);
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(Self::header("Hermes 模型与记忆"))
                                .child(
                                    div().flex().flex_row().flex_wrap().gap_2().child(
                                        Self::action_button(
                                            "hermes-summary",
                                            "读取 Hermes 模型配置",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.hermes_summary(cx);
                                            }),
                                        ),
                                    ),
                                )
                                .child(Self::labeled_editor(
                                    "模型配置 JSON",
                                    "Hermes config.yaml 的 model 节点。",
                                    self.hermes_model_json.clone(),
                                ))
                                .child(
                                    Self::action_button("hermes-save-model", "保存模型配置", true)
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.save_hermes_model(cx);
                                        })),
                                )
                                .child(Self::hermes_limits_row(self.hermes_limits.clone()))
                                .child(Self::labeled_editor(
                                    "MEMORY.md",
                                    "Hermes agent 记忆内容。",
                                    self.hermes_memory_content.clone(),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button(
                                                "hermes-save-memory",
                                                "保存 MEMORY",
                                                true,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.save_hermes_memory(
                                                        hermes::MemoryKind::Memory,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "hermes-toggle-memory",
                                                "切换 MEMORY 启用",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.toggle_hermes_memory(
                                                        hermes::MemoryKind::Memory,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        ),
                                )
                                .child(Self::labeled_editor(
                                    "USER.md",
                                    "Hermes user profile 记忆内容。",
                                    self.hermes_user_memory_content.clone(),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            Self::action_button(
                                                "hermes-save-user-memory",
                                                "保存 USER",
                                                true,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.save_hermes_memory(
                                                        hermes::MemoryKind::User,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        )
                                        .child(
                                            Self::action_button(
                                                "hermes-toggle-user-memory",
                                                "切换 USER 启用",
                                                false,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.toggle_hermes_memory(
                                                        hermes::MemoryKind::User,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        ),
                                ),
                        )
                    }),
            )
    }
}

fn config_dir(app: AppType) -> Result<PathBuf, AppError> {
    Ok(match app {
        AppType::Claude => routedeck_core::paths::get_claude_config_dir(),
        AppType::ClaudeDesktop => claude_desktop::get_config_library_path()?,
        AppType::Codex => codex::get_codex_config_dir(),
        AppType::OpenCode => opencode::get_opencode_dir(),
        AppType::OpenClaw => openclaw::get_openclaw_dir(),
        AppType::Hermes => hermes::get_hermes_dir(),
    })
}

fn config_status(app: &Arc<AppState>, app_type: AppType) -> Result<(bool, String), AppError> {
    let (exists, path) = match app_type {
        AppType::Claude => {
            let status = routedeck_core::paths::get_claude_config_status();
            (status.exists, status.path)
        }
        AppType::ClaudeDesktop => {
            let status = claude_desktop::get_status(&app.db, false)?;
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
    status: &routedeck_core::settings::WebDavSyncStatus,
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
    let value = serde_json::to_value(routedeck_core::Database::list_backups()?)
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

#[cfg(target_os = "macos")]
fn macos_app_bundle_path(exe_path: &Path) -> Option<PathBuf> {
    let path_str = exe_path.to_string_lossy();
    path_str.find(".app/Contents/MacOS/").map(|app_pos| {
        let app_bundle_end = app_pos + 4;
        PathBuf::from(&path_str[..app_bundle_end])
    })
}

fn auto_launch_handle() -> Result<AutoLaunch, AppError> {
    let exe_path =
        std::env::current_exe().map_err(|e| AppError::Message(format!("无法获取应用路径: {e}")))?;

    #[cfg(target_os = "macos")]
    let app_path = macos_app_bundle_path(&exe_path).unwrap_or(exe_path);

    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    AutoLaunchBuilder::new()
        .set_app_name("RouteDeck")
        .set_app_path(&app_path.to_string_lossy())
        .build()
        .map_err(|e| AppError::Message(format!("创建 AutoLaunch 失败: {e}")))
}
