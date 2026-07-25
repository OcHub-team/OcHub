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
use crate::i18n::{k, raw, t, Key};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
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

/// A destructive action awaiting confirmation: delete or restore a database
/// backup, delete the environment variable conflicts, import SQL, restore the
/// Codex history, or disable OMO. `Some` puts the confirm modal on screen.
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
/// virtualized list: stats, config folders, app helpers, CLI tool maintenance,
/// environment variable conflicts, import/export plus backups, and advanced.
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
    /// The destructive action awaiting confirmation; `Some` shows the modal.
    confirm: Option<ConfirmAction>,
    status: Option<SharedString>,
    /// Severity of `status`, forwarded to the toast host alongside it.
    status_level: Option<NotificationLevel>,
    /// Drives the virtualized page body (one item per top-level block).
    list_state: ListState,
}

impl ToolsView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        self.list_state.remeasure();
        cx.notify();
    }

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
            status_level: None,
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

    /// Every status toast carries its severity explicitly. Inferring it from
    /// the wording mis-reads several of these messages (a scan that reports
    /// zero conflicts is not a warning) and stops working entirely once the
    /// copy is translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        msg: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(msg.into());
        self.status_level = Some(level);
        self.list_state.remeasure();
        cx.notify();
    }

    fn open_path_action(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match open_path(&path) {
            Ok(()) => self.set_status(
                NotificationLevel::Success,
                tf!(k::TOOLS_PATH_OPENED, path = path.display()),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_PATH_OPEN_FAILED, error = err),
                cx,
            ),
        }
    }

    fn open_config_dir(&mut self, app: AppType, cx: &mut Context<Self>) {
        match config_dir(app).and_then(|path| {
            std::fs::create_dir_all(&path).map_err(|e| AppError::io(&path, e))?;
            Ok(path)
        }) {
            Ok(path) => self.open_path_action(path, cx),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_CONFIG_DIR_OPEN_FAILED, error = err),
                cx,
            ),
        }
    }

    fn refresh_tool_versions(&mut self, cx: &mut Context<Self>) {
        if self.tool_busy {
            return;
        }
        self.tool_busy = true;
        self.set_status(
            NotificationLevel::Info,
            t(k::TOOLS_CLI_VERSIONS_PROBING),
            cx,
        );

        let task = cx.background_spawn(crate::core_async::run(
            ochub_core::session_manager::get_tool_versions(None, None),
        ));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.tool_busy = false;
                match result {
                    Ok(versions) => {
                        let count = versions.len();
                        this.tool_versions = versions;
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::TOOLS_CLI_VERSIONS_REFRESHED, count = count),
                            cx,
                        );
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::TOOLS_CLI_VERSIONS_FAILED, error = err),
                        cx,
                    ),
                }
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
                    if conflicts == 0 {
                        NotificationLevel::Success
                    } else {
                        NotificationLevel::Warning
                    },
                    tf!(
                        k::TOOLS_CLI_INSTALLS_SCANNED,
                        count = count,
                        conflicts = conflicts
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_CLI_INSTALLS_SCAN_FAILED, error = err),
                cx,
            ),
        }
    }

    fn run_cli_lifecycle(&mut self, action: &'static str, cx: &mut Context<Self>) {
        if self.tool_busy {
            return;
        }
        self.tool_busy = true;
        self.set_status(
            NotificationLevel::Info,
            match action {
                "install" => t(k::TOOLS_CLI_INSTALLING),
                "update" => t(k::TOOLS_CLI_UPDATING),
                _ => t(k::TOOLS_CLI_RUNNING),
            },
            cx,
        );

        let tools = cli_tool_ids();
        let task = cx.background_spawn(async move {
            ochub_core::session_manager::run_tool_lifecycle_action(tools, action.to_string(), None)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.tool_busy = false;
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        match action {
                            "install" => t(k::TOOLS_CLI_INSTALL_COMMAND_RAN),
                            "update" => t(k::TOOLS_CLI_UPDATE_COMMAND_RAN),
                            _ => t(k::TOOLS_CLI_COMMAND_RAN),
                        },
                        cx,
                    ),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::TOOLS_CLI_COMMAND_FAILED, error = err),
                        cx,
                    ),
                }
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
                    if count == 0 {
                        NotificationLevel::Success
                    } else {
                        NotificationLevel::Warning
                    },
                    tf!(
                        k::TOOLS_ENV_SCANNED,
                        app = env_app_label(self.env_app),
                        count = count
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_ENV_SCAN_FAILED, error = err),
                cx,
            ),
        }
    }

    fn delete_env_conflicts(&mut self, cx: &mut Context<Self>) {
        if self.env_conflicts.is_empty() {
            self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_ENV_NONE_TO_DELETE),
                cx,
            );
            return;
        }
        match ochub_core::delete_env_vars(self.env_conflicts.clone()) {
            Ok(backup) => {
                self.env_restore_path.update(cx, |input, cx| {
                    input.set_content(backup.backup_path.clone(), cx)
                });
                self.env_conflicts.clear();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(
                        k::TOOLS_ENV_DELETED,
                        path = backup.backup_path,
                        count = backup.conflicts.len()
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_ENV_DELETE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn restore_env_backup(&mut self, cx: &mut Context<Self>) {
        let raw = self.env_restore_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_ENV_RESTORE_PATH_REQUIRED),
                cx,
            );
            return;
        };
        match ochub_core::restore_env_backup(path.to_string_lossy().to_string()) {
            Ok(()) => self.set_status(NotificationLevel::Success, t(k::TOOLS_ENV_RESTORED), cx),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_ENV_RESTORE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn refresh_db_backups(&mut self, cx: &mut Context<Self>) {
        match load_db_backup_rows() {
            Ok(backups) => {
                let count = backups.len();
                self.db_backups = backups;
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::TOOLS_DB_BACKUPS_REFRESHED, count = count),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_DB_BACKUPS_REFRESH_FAILED, error = err),
                cx,
            ),
        }
    }

    fn create_db_backup(&mut self, cx: &mut Context<Self>) {
        match self.app.db.create_backup_file() {
            Ok(filename) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::TOOLS_DB_BACKUP_CREATED, filename = filename),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_DB_BACKUP_CREATE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn restore_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        match self.app.db.restore_from_backup(&filename) {
            Ok(backup_id) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(
                        k::TOOLS_DB_BACKUP_RESTORED,
                        filename = filename,
                        backup = backup_id
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_DB_BACKUP_RESTORE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn rename_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        let new_name = self.backup_rename.read(cx).content().trim().to_string();
        if new_name.is_empty() {
            self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_DB_RENAME_REQUIRED),
                cx,
            );
            return;
        }
        match ochub_core::Database::rename_backup(&filename, &new_name) {
            Ok(renamed) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::TOOLS_DB_BACKUP_RENAMED, name = renamed),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_DB_BACKUP_RENAME_FAILED, error = err),
                cx,
            ),
        }
    }

    fn delete_db_backup(&mut self, filename: String, cx: &mut Context<Self>) {
        match ochub_core::Database::delete_backup(&filename) {
            Ok(()) => {
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::TOOLS_DB_BACKUP_DELETED, filename = filename),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_DB_BACKUP_DELETE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn export_sql(&mut self, cx: &mut Context<Self>) {
        let raw = self.export_sql_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_SQL_EXPORT_PATH_REQUIRED),
                cx,
            );
            return;
        };
        match self.app.db.export_sql(&path) {
            Ok(()) => self.set_status(
                NotificationLevel::Success,
                tf!(k::TOOLS_SQL_EXPORTED, path = path.display()),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_SQL_EXPORT_FAILED, error = err),
                cx,
            ),
        }
    }

    fn import_sql(&mut self, cx: &mut Context<Self>) {
        let raw = self.import_sql_path.read(cx).content().trim().to_string();
        let Some(path) = expand_user_path(&raw) else {
            self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_SQL_IMPORT_PATH_REQUIRED),
                cx,
            );
            return;
        };
        match self.app.db.import_sql(&path) {
            Ok(backup_id) => {
                let sync_warning =
                    ochub_core::services::ProviderService::sync_current_to_live(&self.app).err();
                self.db_backups = load_db_backup_rows().unwrap_or_default();
                // The import itself succeeded either way; the re-apply warning
                // is the caveat that downgrades the toast. Each outcome is its
                // own sentence — a translation cannot glue a clause on the end.
                match sync_warning {
                    None => self.set_status(
                        NotificationLevel::Success,
                        tf!(k::TOOLS_SQL_IMPORTED, backup = backup_id),
                        cx,
                    ),
                    Some(err) => self.set_status(
                        NotificationLevel::Warning,
                        tf!(
                            k::TOOLS_SQL_IMPORTED_WITH_WARNING,
                            backup = backup_id,
                            error = err
                        ),
                        cx,
                    ),
                }
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_SQL_IMPORT_FAILED, error = err),
                cx,
            ),
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
                    NotificationLevel::Success,
                    if target {
                        t(k::TOOLS_AUTOSTART_ENABLED)
                    } else {
                        t(k::TOOLS_AUTOSTART_DISABLED)
                    },
                    cx,
                );
            }
            Err(err) => self.set_status(NotificationLevel::Error, format!("{err}"), cx),
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
                NotificationLevel::Info,
                format!(
                    "{}: {}",
                    if slim { "OMO Slim" } else { "OMO" },
                    data.file_path
                ),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OMO_READ_FAILED, error = err),
                cx,
            ),
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
                    Ok(()) => {
                        self.set_status(NotificationLevel::Success, t(k::TOOLS_OMO_DISABLED), cx)
                    }
                    Err(err) => self.set_status(
                        NotificationLevel::Error,
                        tf!(k::TOOLS_OMO_DISABLE_FAILED, error = err),
                        cx,
                    ),
                }
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OMO_PROVIDERS_READ_FAILED, error = err),
                cx,
            ),
        }
    }

    fn validate_mcp_command(&mut self, cx: &mut Context<Self>) {
        let cmd = self.mcp_command.read(cx).content().trim().to_string();
        match ochub_core::mcp::validate_command_in_path(&cmd) {
            Ok(true) => self.set_status(
                NotificationLevel::Success,
                tf!(k::TOOLS_MCP_COMMAND_AVAILABLE, command = cmd),
                cx,
            ),
            Ok(false) => self.set_status(
                NotificationLevel::Warning,
                tf!(k::TOOLS_MCP_COMMAND_MISSING, command = cmd),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_MCP_VALIDATE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn read_claude_mcp(&mut self, cx: &mut Context<Self>) {
        match ochub_core::mcp::read_mcp_json() {
            Ok(Some(content)) => self.set_status(
                NotificationLevel::Info,
                tf!(k::TOOLS_MCP_CONFIG_CHARS, count = content.len()),
                cx,
            ),
            Ok(None) => self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_MCP_CONFIG_MISSING),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_MCP_READ_FAILED, error = err),
                cx,
            ),
        }
    }

    fn apply_claude_plugin(&mut self, official: bool, cx: &mut Context<Self>) {
        let result = if official {
            claude_plugin::clear_claude_config()
        } else {
            claude_plugin::write_claude_config()
        };
        match result {
            Ok(changed) => self.set_status(
                NotificationLevel::Success,
                tf!(k::TOOLS_CLAUDE_PLUGIN_APPLIED, changed = changed),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_CLAUDE_PLUGIN_FAILED, error = err),
                cx,
            ),
        }
    }

    fn mark_claude_onboarding(&mut self, completed: bool, cx: &mut Context<Self>) {
        let result = if completed {
            ochub_core::mcp::set_has_completed_onboarding()
        } else {
            ochub_core::mcp::clear_has_completed_onboarding()
        };
        match result {
            Ok(changed) => self.set_status(
                NotificationLevel::Success,
                tf!(k::TOOLS_CLAUDE_ONBOARDING_APPLIED, changed = changed),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_CLAUDE_ONBOARDING_FAILED, error = err),
                cx,
            ),
        }
    }

    fn check_codex_unify_backup(&mut self, cx: &mut Context<Self>) {
        let exists =
            ochub_core::services::codex_history_migration::has_codex_official_history_unify_backup(
            );
        // A check that comes back empty is a caveat, not plain information:
        // there is nothing the restore button can act on.
        self.set_status(
            if exists {
                NotificationLevel::Info
            } else {
                NotificationLevel::Warning
            },
            if exists {
                t(k::TOOLS_CODEX_UNIFY_BACKUP_PRESENT)
            } else {
                t(k::TOOLS_CODEX_UNIFY_BACKUP_ABSENT)
            },
            cx,
        );
    }

    fn restore_codex_unified_history(&mut self, cx: &mut Context<Self>) {
        match ochub_core::services::codex_history_migration::restore_codex_official_history_from_backups()
        {
            Ok(outcome) => {
                if let Some(reason) = outcome.skipped_reason {
                    self.set_status(
                        NotificationLevel::Warning,
                        tf!(k::TOOLS_CODEX_RESTORE_SKIPPED, reason = reason),
                        cx,
                    );
                } else {
                    self.set_status(
                        NotificationLevel::Success,
                        tf!(
                            k::TOOLS_CODEX_RESTORED,
                            files = outcome.restored_jsonl_files,
                            rows = outcome.restored_state_rows
                        ),
                        cx,
                    );
                }
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_CODEX_RESTORE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn refresh_sync_status(&mut self, s3: bool, cx: &mut Context<Self>) {
        if s3 {
            match ochub_core::settings::get_s3_sync_settings() {
                Some(settings) => self.set_status(
                    NotificationLevel::Info,
                    format_sync_status(
                        "S3",
                        settings.enabled,
                        settings.auto_sync,
                        &settings.status,
                    ),
                    cx,
                ),
                None => self.set_status(
                    NotificationLevel::Warning,
                    tf!(k::TOOLS_SYNC_NOT_CONFIGURED, provider = "S3"),
                    cx,
                ),
            }
        } else {
            match ochub_core::settings::get_webdav_sync_settings() {
                Some(settings) => self.set_status(
                    NotificationLevel::Info,
                    format_sync_status(
                        "WebDAV",
                        settings.enabled,
                        settings.auto_sync,
                        &settings.status,
                    ),
                    cx,
                ),
                None => self.set_status(
                    NotificationLevel::Warning,
                    tf!(k::TOOLS_SYNC_NOT_CONFIGURED, provider = "WebDAV"),
                    cx,
                ),
            }
        }
    }

    fn openclaw_health(&mut self, cx: &mut Context<Self>) {
        match openclaw::scan_openclaw_config_health() {
            Ok(health) => self.set_status(
                if health.is_empty() {
                    NotificationLevel::Success
                } else {
                    NotificationLevel::Warning
                },
                tf!(k::TOOLS_OPENCLAW_HEALTH, count = health.len()),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OPENCLAW_HEALTH_FAILED, error = err),
                cx,
            ),
        }
    }

    fn hermes_summary(&mut self, cx: &mut Context<Self>) {
        match hermes::get_model_config() {
            Ok(Some(config)) => self.set_status(
                NotificationLevel::Info,
                tf!(
                    k::TOOLS_HERMES_SUMMARY,
                    provider = config
                        .provider
                        .unwrap_or_else(|| raw(k::TOOLS_HERMES_UNSET).to_string()),
                    model = config
                        .default
                        .unwrap_or_else(|| raw(k::TOOLS_HERMES_UNSET).to_string())
                ),
                cx,
            ),
            Ok(None) => self.set_status(
                NotificationLevel::Warning,
                t(k::TOOLS_HERMES_MODEL_UNINITIALIZED),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_HERMES_READ_FAILED, error = err),
                cx,
            ),
        }
    }

    fn save_openclaw_default_model(&mut self, cx: &mut Context<Self>) {
        let raw = self
            .openclaw_default_model_json
            .read(cx)
            .content()
            .to_string();
        match serde_json::from_str::<openclaw::OpenClawDefaultModel>(&raw)
            .map_err(|e| {
                AppError::Message(tf!(k::TOOLS_OPENCLAW_DEFAULT_MODEL_JSON_INVALID, error = e))
            })
            .and_then(|model| openclaw::set_default_model(&model))
        {
            Ok(outcome) => {
                let level = openclaw_outcome_level(&outcome);
                let message = openclaw_outcome_message(
                    OutcomeKeys {
                        plain: k::TOOLS_OPENCLAW_DEFAULT_MODEL_SAVED,
                        backup: k::TOOLS_OPENCLAW_DEFAULT_MODEL_SAVED_BACKUP,
                        warnings: k::TOOLS_OPENCLAW_DEFAULT_MODEL_SAVED_WARNINGS,
                        backup_warnings: k::TOOLS_OPENCLAW_DEFAULT_MODEL_SAVED_BACKUP_WARNINGS,
                    },
                    outcome,
                );
                self.set_status(level, message, cx)
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OPENCLAW_DEFAULT_MODEL_SAVE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn save_openclaw_env(&mut self, cx: &mut Context<Self>) {
        let raw = self.openclaw_env_json.read(cx).content().to_string();
        match serde_json::from_str::<std::collections::HashMap<String, Value>>(&raw)
            .map(|vars| openclaw::OpenClawEnvConfig { vars })
            .map_err(|e| AppError::Message(tf!(k::TOOLS_OPENCLAW_ENV_JSON_INVALID, error = e)))
            .and_then(|env| openclaw::set_env_config(&env))
        {
            Ok(outcome) => {
                let level = openclaw_outcome_level(&outcome);
                let message = openclaw_outcome_message(
                    OutcomeKeys {
                        plain: k::TOOLS_OPENCLAW_ENV_SAVED,
                        backup: k::TOOLS_OPENCLAW_ENV_SAVED_BACKUP,
                        warnings: k::TOOLS_OPENCLAW_ENV_SAVED_WARNINGS,
                        backup_warnings: k::TOOLS_OPENCLAW_ENV_SAVED_BACKUP_WARNINGS,
                    },
                    outcome,
                );
                self.set_status(level, message, cx)
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OPENCLAW_ENV_SAVE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn save_openclaw_tools(&mut self, cx: &mut Context<Self>) {
        let raw = self.openclaw_tools_json.read(cx).content().to_string();
        match serde_json::from_str::<openclaw::OpenClawToolsConfig>(&raw)
            .map_err(|e| AppError::Message(tf!(k::TOOLS_OPENCLAW_TOOLS_JSON_INVALID, error = e)))
            .and_then(|tools| openclaw::set_tools_config(&tools))
        {
            Ok(outcome) => {
                let level = openclaw_outcome_level(&outcome);
                let message = openclaw_outcome_message(
                    OutcomeKeys {
                        plain: k::TOOLS_OPENCLAW_TOOLS_SAVED,
                        backup: k::TOOLS_OPENCLAW_TOOLS_SAVED_BACKUP,
                        warnings: k::TOOLS_OPENCLAW_TOOLS_SAVED_WARNINGS,
                        backup_warnings: k::TOOLS_OPENCLAW_TOOLS_SAVED_BACKUP_WARNINGS,
                    },
                    outcome,
                );
                self.set_status(level, message, cx)
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_OPENCLAW_TOOLS_SAVE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn save_hermes_model(&mut self, cx: &mut Context<Self>) {
        let raw = self.hermes_model_json.read(cx).content().to_string();
        match serde_json::from_str::<hermes::HermesModelConfig>(&raw)
            .map_err(|e| AppError::Message(tf!(k::TOOLS_HERMES_MODEL_JSON_INVALID, error = e)))
            .and_then(|model| hermes::set_model_config(&model))
        {
            Ok(outcome) => self.set_status(
                NotificationLevel::Success,
                hermes_outcome_message(
                    k::TOOLS_HERMES_MODEL_SAVED,
                    k::TOOLS_HERMES_MODEL_SAVED_BACKUP,
                    outcome,
                ),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_HERMES_MODEL_SAVE_FAILED, error = err),
                cx,
            ),
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
            Ok(()) => self.set_status(
                NotificationLevel::Success,
                t(k::TOOLS_HERMES_MEMORY_SAVED),
                cx,
            ),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_HERMES_MEMORY_SAVE_FAILED, error = err),
                cx,
            ),
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
                self.set_status(
                    NotificationLevel::Success,
                    hermes_outcome_message(
                        k::TOOLS_HERMES_MEMORY_TOGGLE_SAVED,
                        k::TOOLS_HERMES_MEMORY_TOGGLE_SAVED_BACKUP,
                        outcome,
                    ),
                    cx,
                );
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::TOOLS_HERMES_MEMORY_TOGGLE_FAILED, error = err),
                cx,
            ),
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
                // One whole sentence per on/off combination: the two switches
                // cannot be dropped into a template as translated fragments.
                let key = match (limits.memory_enabled, limits.user_enabled) {
                    (true, true) => k::TOOLS_HERMES_LIMITS_BOTH_ON,
                    (true, false) => k::TOOLS_HERMES_LIMITS_MEMORY_ON,
                    (false, true) => k::TOOLS_HERMES_LIMITS_USER_ON,
                    (false, false) => k::TOOLS_HERMES_LIMITS_BOTH_OFF,
                };
                tf!(key, memory = limits.memory, user = limits.user)
            })
            .unwrap_or_else(|| raw(k::TOOLS_HERMES_LIMITS_UNINITIALIZED).to_string());
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
                        if row.exists {
                            t(k::TOOLS_CONFIG_EXISTS)
                        } else {
                            t(k::TOOLS_CONFIG_MISSING)
                        },
                    ))
                    .child(
                        components::button(
                            format!("open-config-{}", row.app.as_str()),
                            t(k::TOOLS_ACTION_OPEN),
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
            .unwrap_or_else(|| raw(k::TOOLS_VERSION_NOT_INSTALLED).to_string());
        let latest = version
            .latest_version
            .clone()
            .unwrap_or_else(|| raw(k::TOOLS_VERSION_UNKNOWN).to_string());
        // The error and the broken-executable note used to be appended as
        // clauses; each combination is now its own sentence in the catalog.
        let detail = match (&version.error, version.installed_but_broken) {
            (None, false) => tf!(
                k::TOOLS_VERSION_DETAIL,
                local = local,
                latest = latest,
                env = version.env_type
            ),
            (None, true) => tf!(
                k::TOOLS_VERSION_DETAIL_BROKEN,
                local = local,
                latest = latest,
                env = version.env_type
            ),
            (Some(error), false) => tf!(
                k::TOOLS_VERSION_DETAIL_ERROR,
                local = local,
                latest = latest,
                env = version.env_type,
                error = error
            ),
            (Some(error), true) => tf!(
                k::TOOLS_VERSION_DETAIL_ERROR_BROKEN,
                local = local,
                latest = latest,
                env = version.env_type,
                error = error
            ),
        };
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
        // Conflict and confirmation each flip a word in the middle of the
        // sentence, so the four readings are four catalog entries.
        let summary_key = match (report.is_conflict, report.needs_confirmation) {
            (true, true) => k::TOOLS_INSTALL_SUMMARY_CONFLICT_CONFIRM,
            (true, false) => k::TOOLS_INSTALL_SUMMARY_CONFLICT_DIRECT,
            (false, true) => k::TOOLS_INSTALL_SUMMARY_CLEAN_CONFIRM,
            (false, false) => k::TOOLS_INSTALL_SUMMARY_CLEAN_DIRECT,
        };
        let summary = tf!(
            summary_key,
            count = report.installs.len(),
            command = report.command
        );
        let installs = report
            .installs
            .iter()
            .map(|install| {
                let version = install
                    .version
                    .as_deref()
                    .unwrap_or_else(|| raw(k::TOOLS_INSTALL_UNKNOWN_VERSION));
                let line = match (install.is_path_default, install.error.as_deref()) {
                    (true, None) => tf!(
                        k::TOOLS_INSTALL_ENTRY_DEFAULT,
                        path = install.path,
                        version = version,
                        source = install.source
                    ),
                    (true, Some(error)) => tf!(
                        k::TOOLS_INSTALL_ENTRY_DEFAULT_ERROR,
                        path = install.path,
                        version = version,
                        source = install.source,
                        error = error
                    ),
                    (false, None) => tf!(
                        k::TOOLS_INSTALL_ENTRY_OTHER,
                        path = install.path,
                        version = version,
                        source = install.source
                    ),
                    (false, Some(error)) => tf!(
                        k::TOOLS_INSTALL_ENTRY_OTHER_ERROR,
                        path = install.path,
                        version = version,
                        source = install.source,
                        error = error
                    ),
                };
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(SharedString::from(line))
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
                            t(k::TOOLS_ACTION_RESTORE),
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
                            t(k::TOOLS_ACTION_RENAME),
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
                            t(k::TOOLS_ACTION_DELETE),
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
                                t(k::TOOLS_STAT_CONFIG_DIRS_LABEL),
                                format!("{configured_count}/{total_configs}"),
                                t(k::TOOLS_STAT_CONFIG_DIRS_DETAIL),
                            ))
                            .child(components::stat_tile(
                                None,
                                theme::green(),
                                t(k::TOOLS_STAT_BACKUPS_LABEL),
                                backup_count.to_string(),
                                t(k::TOOLS_STAT_BACKUPS_DETAIL),
                            ))
                            .child(components::stat_tile(
                                None,
                                if env_conflict_count == 0 {
                                    theme::teal()
                                } else {
                                    theme::yellow()
                                },
                                t(k::TOOLS_STAT_ENV_CONFLICTS_LABEL),
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
                                t(k::TOOLS_SECTION_CONFIG_TITLE),
                                t(k::TOOLS_SECTION_CONFIG_DESC),
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
                            t(k::TOOLS_SECTION_APP_TITLE),
                            t(k::TOOLS_SECTION_APP_DESC),
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
                                            t(k::TOOLS_APP_AUTOSTART_DISABLE)
                                        } else {
                                            t(k::TOOLS_APP_AUTOSTART_ENABLE)
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
                                        t(k::TOOLS_APP_OPEN_DATA_DIR),
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
                                t(k::TOOLS_SECTION_CLI_TITLE),
                                t(k::TOOLS_SECTION_CLI_DESC),
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
                                                t(k::TOOLS_CLI_BUSY)
                                            } else {
                                                t(k::TOOLS_CLI_REFRESH_VERSIONS)
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
                                            t(k::TOOLS_CLI_SCAN_INSTALLS),
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
                                            t(k::TOOLS_CLI_INSTALL_MISSING),
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
                                            t(k::TOOLS_CLI_UPDATE_ALL),
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
                                    t(k::TOOLS_CLI_VERSIONS_EMPTY_TITLE),
                                    t(k::TOOLS_CLI_VERSIONS_EMPTY_DESC),
                                    None,
                                ))
                            })
                            .children(tool_version_rows)
                            .when(self.tool_installations.is_empty(), |s| {
                                s.child(components::empty_state(
                                    IconName::Search,
                                    t(k::TOOLS_CLI_INSTALLS_EMPTY_TITLE),
                                    t(k::TOOLS_CLI_INSTALLS_EMPTY_DESC),
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
                                t(k::TOOLS_SECTION_ENV_TITLE),
                                t(k::TOOLS_SECTION_ENV_DESC),
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
                                            t(k::TOOLS_ENV_SCAN),
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
                                            t(k::TOOLS_ENV_DELETE),
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
                                    t(k::TOOLS_ENV_EMPTY_TITLE),
                                    t(k::TOOLS_ENV_EMPTY_DESC),
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
                                            t(k::TOOLS_ENV_RESTORE),
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
                                t(k::TOOLS_SECTION_DATA_TITLE),
                                t(k::TOOLS_SECTION_DATA_DESC),
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
                                            t(k::TOOLS_SQL_EXPORT),
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
                                            t(k::TOOLS_SQL_IMPORT),
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
                                            t(k::TOOLS_DB_REFRESH),
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
                                            t(k::TOOLS_DB_CREATE),
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
                                    t(k::TOOLS_DB_EMPTY_TITLE),
                                    t(k::TOOLS_DB_EMPTY_DESC),
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
                                            SharedString::from(tf!(
                                                k::TOOLS_DB_HIDDEN_COUNT,
                                                count = hidden_backup_count
                                            )),
                                        ))
                                        .child(
                                            components::button(
                                                "db-backup-show-all",
                                                t(k::TOOLS_DB_SHOW_ALL),
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
                                            t(k::TOOLS_DB_COLLAPSE),
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
            6 => block
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    components::card().child(
                        components::disclosure(
                            "tools-advanced",
                            t(k::TOOLS_ADVANCED_TITLE),
                            t(k::TOOLS_ADVANCED_DESC),
                            self.show_advanced_tools,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.toggle_advanced_tools(cx);
                            },
                        )),
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
                                    .child(t(k::TOOLS_ADVANCED_SYNC_TITLE)),
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
                                            t(k::TOOLS_ADVANCED_SYNC_WEBDAV),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.refresh_sync_status(false, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "sync-s3-status",
                                            t(k::TOOLS_ADVANCED_SYNC_S3),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
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
                        components::card()
                            .gap_3()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::TOOLS_ADVANCED_CODEX_TITLE)),
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
                                            t(k::TOOLS_ADVANCED_CODEX_CHECK),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.check_codex_unify_backup(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "codex-unify-restore",
                                            t(k::TOOLS_ADVANCED_CODEX_RESTORE),
                                            ButtonTone::Danger,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.confirm =
                                                    Some(ConfirmAction::RestoreCodexHistory);
                                                cx.notify();
                                            }),
                                        ),
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
                                            t(k::TOOLS_ADVANCED_OMO_READ),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.read_omo(false, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "omo-disable",
                                            t(k::TOOLS_ADVANCED_OMO_DISABLE),
                                            ButtonTone::Danger,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.confirm =
                                                    Some(ConfirmAction::DisableOmo { slim: false });
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "omo-slim-read",
                                            t(k::TOOLS_ADVANCED_OMO_READ_SLIM),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.read_omo(true, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "omo-slim-disable",
                                            t(k::TOOLS_ADVANCED_OMO_DISABLE_SLIM),
                                            ButtonTone::Danger,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.confirm =
                                                    Some(ConfirmAction::DisableOmo { slim: true });
                                                cx.notify();
                                            }),
                                        ),
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
                                    .child(t(k::TOOLS_ADVANCED_MCP_TITLE)),
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
                                            t(k::TOOLS_ADVANCED_MCP_VALIDATE),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.validate_mcp_command(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "mcp-read",
                                            t(k::TOOLS_ADVANCED_MCP_READ),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.read_claude_mcp(cx);
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
                                            "claude-plugin-apply",
                                            t(k::TOOLS_ADVANCED_CLAUDE_APPLY_PLUGIN),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.apply_claude_plugin(false, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "claude-plugin-clear",
                                            t(k::TOOLS_ADVANCED_CLAUDE_RESTORE_OFFICIAL),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.apply_claude_plugin(true, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "claude-onboarding-skip",
                                            t(k::TOOLS_ADVANCED_CLAUDE_SKIP_ONBOARDING),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.mark_claude_onboarding(true, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "claude-onboarding-clear",
                                            t(k::TOOLS_ADVANCED_CLAUDE_RESTORE_ONBOARDING),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
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
                        components::card()
                            .gap_3()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::TOOLS_ADVANCED_OPENCLAW_TITLE)),
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
                                            t(k::TOOLS_ADVANCED_OPENCLAW_CHECK),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.openclaw_health(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::button(
                                            "openclaw-refresh-advanced",
                                            t(k::TOOLS_ADVANCED_OPENCLAW_RELOAD),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.refresh_advanced_configs(cx);
                                                this.set_status(
                                                    NotificationLevel::Success,
                                                    t(k::TOOLS_ADVANCED_RELOADED),
                                                    cx,
                                                );
                                            }),
                                        ),
                                    ),
                            )
                            .child(components::card().child(components::field(
                                t(k::TOOLS_ADVANCED_OPENCLAW_DEFAULT_MODEL_LABEL),
                                false,
                                Some(t(k::TOOLS_ADVANCED_OPENCLAW_DEFAULT_MODEL_HELP)),
                                self.openclaw_default_model_json.clone(),
                            )))
                            .child(
                                div().flex().flex_row().justify_end().child(
                                    components::button(
                                        "openclaw-save-default-model",
                                        t(k::TOOLS_ADVANCED_OPENCLAW_SAVE_DEFAULT_MODEL),
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
                                t(k::TOOLS_ADVANCED_OPENCLAW_ENV_LABEL),
                                false,
                                Some(t(k::TOOLS_ADVANCED_OPENCLAW_ENV_HELP)),
                                self.openclaw_env_json.clone(),
                            )))
                            .child(
                                div().flex().flex_row().justify_end().child(
                                    components::button(
                                        "openclaw-save-env",
                                        t(k::TOOLS_ADVANCED_OPENCLAW_SAVE_ENV),
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
                                t(k::TOOLS_ADVANCED_OPENCLAW_TOOLS_LABEL),
                                false,
                                Some(t(k::TOOLS_ADVANCED_OPENCLAW_TOOLS_HELP)),
                                self.openclaw_tools_json.clone(),
                            )))
                            .child(
                                div().flex().flex_row().justify_end().child(
                                    components::button(
                                        "openclaw-save-tools",
                                        t(k::TOOLS_ADVANCED_OPENCLAW_SAVE_TOOLS),
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
                                    .child(t(k::TOOLS_ADVANCED_HERMES_TITLE)),
                            )
                            .child(
                                div().flex().flex_row().flex_wrap().gap_2().child(
                                    components::button(
                                        "hermes-summary",
                                        t(k::TOOLS_ADVANCED_HERMES_READ_MODEL),
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
                                t(k::TOOLS_ADVANCED_HERMES_MODEL_LABEL),
                                false,
                                Some(t(k::TOOLS_ADVANCED_HERMES_MODEL_HELP)),
                                self.hermes_model_json.clone(),
                            )))
                            .child(
                                div().flex().flex_row().justify_end().child(
                                    components::button(
                                        "hermes-save-model",
                                        t(k::TOOLS_ADVANCED_HERMES_SAVE_MODEL),
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
                                Some(t(k::TOOLS_ADVANCED_HERMES_MEMORY_HELP)),
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
                                            t(k::TOOLS_ADVANCED_HERMES_SAVE_MEMORY),
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
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
                                        components::button(
                                            "hermes-toggle-memory",
                                            t(k::TOOLS_ADVANCED_HERMES_TOGGLE_MEMORY),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
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
                            .child(components::card().child(components::field(
                                "USER.md",
                                false,
                                Some(t(k::TOOLS_ADVANCED_HERMES_USER_MEMORY_HELP)),
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
                                            t(k::TOOLS_ADVANCED_HERMES_SAVE_USER_MEMORY),
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
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
                                        components::button(
                                            "hermes-toggle-user-memory",
                                            t(k::TOOLS_ADVANCED_HERMES_TOGGLE_USER_MEMORY),
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
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
                })
                .into_any_element(),
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
                layout::page_header(t(k::TOOLS_PAGE_TITLE), Some(t(k::TOOLS_PAGE_SUBTITLE))).child(
                    components::icon_button_tone(
                        "tools-refresh",
                        t(k::TOOLS_ACTION_REFRESH),
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.reload();
                        this.refresh_advanced_configs(cx);
                        this.set_status(
                            NotificationLevel::Success,
                            t(k::TOOLS_STATUS_REFRESHED),
                            cx,
                        );
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
                        t(k::TOOLS_CONFIRM_RESTORE_BACKUP_TITLE),
                        tf!(k::TOOLS_CONFIRM_RESTORE_BACKUP_MESSAGE, name = name),
                        t(k::TOOLS_ACTION_RESTORE),
                    ),
                    ConfirmAction::DeleteDbBackup(name) => (
                        t(k::TOOLS_CONFIRM_DELETE_BACKUP_TITLE),
                        tf!(k::TOOLS_CONFIRM_DELETE_BACKUP_MESSAGE, name = name),
                        t(k::TOOLS_ACTION_DELETE),
                    ),
                    ConfirmAction::DeleteEnvConflicts => (
                        t(k::TOOLS_CONFIRM_DELETE_ENV_TITLE),
                        tf!(
                            k::TOOLS_CONFIRM_DELETE_ENV_MESSAGE,
                            app = env_app_name,
                            count = env_conflict_count
                        ),
                        t(k::TOOLS_ENV_DELETE),
                    ),
                    ConfirmAction::ImportSql => (
                        t(k::TOOLS_SQL_IMPORT),
                        raw(k::TOOLS_CONFIRM_IMPORT_SQL_MESSAGE).to_string(),
                        t(k::TOOLS_ACTION_IMPORT),
                    ),
                    ConfirmAction::RestoreCodexHistory => (
                        t(k::TOOLS_CONFIRM_RESTORE_CODEX_TITLE),
                        raw(k::TOOLS_CONFIRM_RESTORE_CODEX_MESSAGE).to_string(),
                        t(k::TOOLS_ACTION_RESTORE),
                    ),
                    ConfirmAction::DisableOmo { slim } => (
                        t(k::TOOLS_CONFIRM_DISABLE_OMO_TITLE),
                        tf!(
                            k::TOOLS_CONFIRM_DISABLE_OMO_MESSAGE,
                            name = if *slim { "OMO Slim" } else { "OMO" }
                        ),
                        t(k::TOOLS_ACTION_DISABLE),
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
                                t(k::TOOLS_ACTION_CANCEL),
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
        .ok_or_else(|| AppError::InvalidInput(tf!(k::TOOLS_CONFIG_DIR_UNKNOWN_APP, app = app)))?
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

/// One catalog entry per reading, rather than a template with "enabled" and
/// "auto-sync on" dropped into it: those are clauses, and a translation has to
/// be free to reorder or reword the whole sentence.
fn format_sync_status(
    provider: &str,
    enabled: bool,
    auto_sync: bool,
    status: &ochub_core::settings::WebDavSyncStatus,
) -> String {
    let last_sync = status
        .last_sync_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| raw(k::TOOLS_SYNC_NEVER).to_string());
    match status.last_error.as_deref() {
        Some(message) => {
            let source = status
                .last_error_source
                .as_deref()
                .unwrap_or_else(|| raw(k::TOOLS_SYNC_UNKNOWN_SOURCE));
            let key = match (enabled, auto_sync) {
                (true, true) => k::TOOLS_SYNC_STATUS_ENABLED_AUTO_ERROR,
                (true, false) => k::TOOLS_SYNC_STATUS_ENABLED_MANUAL_ERROR,
                (false, true) => k::TOOLS_SYNC_STATUS_DISABLED_AUTO_ERROR,
                (false, false) => k::TOOLS_SYNC_STATUS_DISABLED_MANUAL_ERROR,
            };
            tf!(
                key,
                provider = provider,
                last_sync = last_sync,
                source = source,
                message = message
            )
        }
        None => {
            let key = match (enabled, auto_sync) {
                (true, true) => k::TOOLS_SYNC_STATUS_ENABLED_AUTO,
                (true, false) => k::TOOLS_SYNC_STATUS_ENABLED_MANUAL,
                (false, true) => k::TOOLS_SYNC_STATUS_DISABLED_AUTO,
                (false, false) => k::TOOLS_SYNC_STATUS_DISABLED_MANUAL,
            };
            tf!(key, provider = provider, last_sync = last_sync)
        }
    }
}

/// A write that carried health warnings still succeeded, but the toast should
/// say so; the warning count is part of the message either way.
fn openclaw_outcome_level(outcome: &openclaw::OpenClawWriteOutcome) -> NotificationLevel {
    if outcome.warnings.is_empty() {
        NotificationLevel::Success
    } else {
        NotificationLevel::Warning
    }
}

/// The four sentences one OpenClaw write can report: the bare confirmation,
/// plus the variants that name a backup path, a warning count, or both. A
/// translation cannot be built by appending clauses to the plain one, so each
/// caller hands over its own full set.
struct OutcomeKeys {
    plain: Key,
    backup: Key,
    warnings: Key,
    backup_warnings: Key,
}

fn openclaw_outcome_message(keys: OutcomeKeys, outcome: openclaw::OpenClawWriteOutcome) -> String {
    match (outcome.backup_path.as_deref(), outcome.warnings.len()) {
        (None, 0) => raw(keys.plain).to_string(),
        (Some(path), 0) => tf!(keys.backup, path = path),
        (None, count) => tf!(keys.warnings, count = count),
        (Some(path), count) => tf!(keys.backup_warnings, path = path, count = count),
    }
}

fn hermes_outcome_message(
    plain: Key,
    with_backup: Key,
    outcome: hermes::HermesWriteOutcome,
) -> String {
    match outcome.backup_path.as_deref() {
        None => raw(plain).to_string(),
        Some(path) => tf!(with_backup, path = path),
    }
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
        .map_err(|e| AppError::Message(tf!(k::TOOLS_DB_SERIALIZE_FAILED, error = e)))?;
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
        .map_err(|e| AppError::Message(tf!(k::TOOLS_PATH_OPEN_PATH_FAILED, error = e)))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(tf!(
            k::TOOLS_PATH_OPEN_PATH_FAILED,
            error = status
        )))
    }
}

crate::notifications::impl_status_toasts_leveled!(ToolsView);
