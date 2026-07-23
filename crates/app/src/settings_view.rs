//! Device-level settings panel. Reads `ochub_core::settings::get_settings()` and
//! writes changes back via `ochub_core::settings::update_settings`.

use std::process::Command;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Context, Entity, Focusable, ListAlignment, ListState, SharedString,
    Window,
};
use ochub_core::app_store;
use ochub_core::services::UpdateCheckResult;
use ochub_core::settings::{self, AppSettings, S3SyncSettings, WebDavSyncSettings};
use ochub_core::AppState;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::layout;
use crate::shell_menu;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Copy)]
enum SyncOperation {
    Test,
    Upload,
    Download,
}

/// Which remote-sync provider a pending download-and-restore confirmation
/// targets. Downloading overwrites the local database, so it goes through a
/// confirm modal before [`SettingsView::run_webdav_sync`] /
/// [`SettingsView::run_s3_sync`] fires.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncDownloadTarget {
    WebDav,
    S3,
}

/// Events the settings view emits for the app shell.
pub enum SettingsEvent {
    /// The set of enabled apps changed (sidebar/views must re-derive).
    AppsChanged,
}

impl gpui::EventEmitter<SettingsEvent> for SettingsView {}

pub struct SettingsView {
    app: Arc<AppState>,
    settings: AppSettings,
    status: Option<SharedString>,
    update_checking: bool,
    update_info: Option<UpdateCheckResult>,
    sync_busy: bool,
    /// Pending download-and-restore confirmation (overwrites local data).
    confirm_download: Option<SyncDownloadTarget>,
    webdav_url: Entity<TextInput>,
    webdav_username: Entity<TextInput>,
    webdav_password: Entity<TextInput>,
    webdav_remote_root: Entity<TextInput>,
    webdav_profile: Entity<TextInput>,
    s3_region: Entity<TextInput>,
    s3_bucket: Entity<TextInput>,
    s3_access_key: Entity<TextInput>,
    s3_secret_key: Entity<TextInput>,
    s3_endpoint: Entity<TextInput>,
    s3_remote_root: Entity<TextInput>,
    s3_profile: Entity<TextInput>,
    app_config_dir: Entity<TextInput>,
    preferred_terminal: Entity<TextInput>,
    backup_interval_hours: Entity<TextInput>,
    backup_retain_count: Entity<TextInput>,
    /// Drives the virtualized settings list (one item per section block).
    list_state: ListState,
}

/// Number of top-level section blocks rendered by [`SettingsView::render_block`].
const SETTINGS_BLOCK_COUNT: usize = 6;

impl SettingsView {
    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_download.is_some() {
            window.play_system_bell();
            return;
        }
        let focused = |input: &Entity<TextInput>, cx: &App| {
            input.read(cx).focus_handle(cx).is_focused(window)
        };
        if [
            &self.webdav_url,
            &self.webdav_username,
            &self.webdav_password,
            &self.webdav_remote_root,
            &self.webdav_profile,
        ]
        .into_iter()
        .any(|input| focused(input, cx))
        {
            self.save_webdav(cx);
        } else if [
            &self.s3_region,
            &self.s3_bucket,
            &self.s3_access_key,
            &self.s3_secret_key,
            &self.s3_endpoint,
            &self.s3_remote_root,
            &self.s3_profile,
        ]
        .into_iter()
        .any(|input| focused(input, cx))
        {
            self.save_s3(cx);
        } else if focused(&self.app_config_dir, cx) {
            self.save_paths(cx);
        } else if focused(&self.preferred_terminal, cx)
            || focused(&self.backup_interval_hours, cx)
            || focused(&self.backup_retain_count, cx)
        {
            self.save_terminal_and_backup(cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_download.take().is_some() {
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let webdav = settings.webdav_sync.clone().unwrap_or_default();
        let s3 = settings.s3_sync.clone().unwrap_or_default();
        let webdav_url = cx.new(|cx| text_input(cx, "https://dav.example.com", &webdav.base_url));
        let webdav_username = cx.new(|cx| text_input(cx, "用户名", &webdav.username));
        let webdav_password = cx.new(|cx| {
            let mut input = TextInput::new(cx, "密码").masked(true);
            input.set_content(webdav.password.clone(), cx);
            input
        });
        let webdav_remote_root = cx.new(|cx| text_input(cx, "ochub-sync", &webdav.remote_root));
        let webdav_profile = cx.new(|cx| text_input(cx, "default", &webdav.profile));

        let s3_region = cx.new(|cx| text_input(cx, "auto", &s3.region));
        let s3_bucket = cx.new(|cx| text_input(cx, "bucket", &s3.bucket));
        let s3_access_key = cx.new(|cx| text_input(cx, "Access Key ID", &s3.access_key_id));
        let s3_secret_key = cx.new(|cx| {
            let mut input = TextInput::new(cx, "Secret Access Key").masked(true);
            input.set_content(s3.secret_access_key.clone(), cx);
            input
        });
        let s3_endpoint = cx.new(|cx| {
            text_input(
                cx,
                "https://<account>.r2.cloudflarestorage.com",
                &s3.endpoint,
            )
        });
        let s3_remote_root = cx.new(|cx| text_input(cx, "ochub-sync", &s3.remote_root));
        let s3_profile = cx.new(|cx| text_input(cx, "default", &s3.profile));
        let app_config_dir = cx.new(|cx| {
            text_input(
                cx,
                "~/.ochub",
                &app_store::get_app_config_dir_override()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
        });
        let preferred_terminal = cx.new(|cx| {
            option_text_input(
                cx,
                "Terminal / iTerm / WezTerm / Ghostty",
                &settings.preferred_terminal,
            )
        });
        let backup_interval_hours = cx.new(|cx| {
            text_input(
                cx,
                "24",
                &settings
                    .backup_interval_hours
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            )
        });
        let backup_retain_count = cx.new(|cx| {
            text_input(
                cx,
                "10",
                &settings
                    .backup_retain_count
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            )
        });

        Self {
            app,
            settings,
            status: None,
            update_checking: false,
            update_info: None,
            sync_busy: false,
            confirm_download: None,
            webdav_url,
            webdav_username,
            webdav_password,
            webdav_remote_root,
            webdav_profile,
            s3_region,
            s3_bucket,
            s3_access_key,
            s3_secret_key,
            s3_endpoint,
            s3_remote_root,
            s3_profile,
            app_config_dir,
            preferred_terminal,
            backup_interval_hours,
            backup_retain_count,
            list_state: ListState::new(SETTINGS_BLOCK_COUNT, ListAlignment::Top, px(600.)),
        }
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let saved = match settings::update_settings(self.settings.clone()) {
            Ok(()) => {
                self.status = Some(SharedString::from("已保存"));
                true
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("保存失败: {err}")));
                false
            }
        };
        // Re-read so we reflect any normalization.
        self.settings = settings::get_settings();
        if saved {
            shell_menu::refresh(&self.app, cx);
        }
        // A toggle/save may change a block's height; re-measure (keeps scroll pos).
        self.list_state.remeasure();
        cx.notify();
    }

    fn webdav_settings_from_inputs(&self, cx: &mut Context<Self>) -> WebDavSyncSettings {
        let mut sync = self.settings.webdav_sync.clone().unwrap_or_default();
        sync.base_url = input_value(&self.webdav_url, cx);
        sync.username = input_value(&self.webdav_username, cx);
        sync.password = input_value(&self.webdav_password, cx);
        sync.remote_root = input_value(&self.webdav_remote_root, cx);
        sync.profile = input_value(&self.webdav_profile, cx);
        sync.normalize();
        sync
    }

    fn s3_settings_from_inputs(&self, cx: &mut Context<Self>) -> S3SyncSettings {
        let mut sync = self.settings.s3_sync.clone().unwrap_or_default();
        sync.region = input_value(&self.s3_region, cx);
        sync.bucket = input_value(&self.s3_bucket, cx);
        sync.access_key_id = input_value(&self.s3_access_key, cx);
        sync.secret_access_key = input_value(&self.s3_secret_key, cx);
        sync.endpoint = input_value(&self.s3_endpoint, cx);
        sync.remote_root = input_value(&self.s3_remote_root, cx);
        sync.profile = input_value(&self.s3_profile, cx);
        sync.normalize();
        sync
    }

    fn save_webdav(&mut self, cx: &mut Context<Self>) {
        let sync = self.webdav_settings_from_inputs(cx);
        self.settings.webdav_sync = Some(sync);
        self.persist(cx);
    }

    fn save_s3(&mut self, cx: &mut Context<Self>) {
        let sync = self.s3_settings_from_inputs(cx);
        self.settings.s3_sync = Some(sync);
        self.persist(cx);
    }

    fn toggle_webdav_enabled(&mut self, cx: &mut Context<Self>) {
        let mut sync = self.webdav_settings_from_inputs(cx);
        sync.enabled = !sync.enabled;
        self.settings.webdav_sync = Some(sync);
        self.persist(cx);
    }

    fn toggle_webdav_auto(&mut self, cx: &mut Context<Self>) {
        let mut sync = self.webdav_settings_from_inputs(cx);
        sync.auto_sync = !sync.auto_sync;
        self.settings.webdav_sync = Some(sync);
        self.persist(cx);
    }

    fn toggle_s3_enabled(&mut self, cx: &mut Context<Self>) {
        let mut sync = self.s3_settings_from_inputs(cx);
        sync.enabled = !sync.enabled;
        self.settings.s3_sync = Some(sync);
        self.persist(cx);
    }

    fn toggle_s3_auto(&mut self, cx: &mut Context<Self>) {
        let mut sync = self.s3_settings_from_inputs(cx);
        sync.auto_sync = !sync.auto_sync;
        self.settings.s3_sync = Some(sync);
        self.persist(cx);
    }

    fn run_webdav_sync(&mut self, operation: SyncOperation, cx: &mut Context<Self>) {
        if self.sync_busy {
            return;
        }
        let mut sync = self.webdav_settings_from_inputs(cx);
        if let Err(err) = settings::set_webdav_sync_settings(Some(sync.clone())) {
            self.status = Some(SharedString::from(format!("保存 WebDAV 设置失败: {err}")));
            cx.notify();
            return;
        }
        self.settings = settings::get_settings();
        self.sync_busy = true;
        self.status = Some(SharedString::from(sync_start_message("WebDAV", operation)));
        cx.notify();

        let db = self.app.db.clone();
        cx.spawn(async move |this, cx| {
            let result = match operation {
                SyncOperation::Test => ochub_core::services::webdav_sync::check_connection(&sync)
                    .await
                    .map(|_| "WebDAV 连接成功".to_string()),
                SyncOperation::Upload => ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::upload(&db, &mut sync),
                )
                .await
                .map(|_| "WebDAV 上传完成".to_string()),
                SyncOperation::Download => ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::download(&db, &mut sync),
                )
                .await
                .map(|_| "WebDAV 下载并还原完成".to_string()),
            };
            this.update(cx, |this, cx| {
                this.sync_busy = false;
                this.settings = settings::get_settings();
                this.status = Some(SharedString::from(match result {
                    Ok(msg) => msg,
                    Err(err) => format!("WebDAV 操作失败: {err}"),
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_s3_sync(&mut self, operation: SyncOperation, cx: &mut Context<Self>) {
        if self.sync_busy {
            return;
        }
        let mut sync = self.s3_settings_from_inputs(cx);
        if let Err(err) = settings::set_s3_sync_settings(Some(sync.clone())) {
            self.status = Some(SharedString::from(format!("保存 S3 设置失败: {err}")));
            cx.notify();
            return;
        }
        self.settings = settings::get_settings();
        self.sync_busy = true;
        self.status = Some(SharedString::from(sync_start_message("S3", operation)));
        cx.notify();

        let db = self.app.db.clone();
        cx.spawn(async move |this, cx| {
            let result = match operation {
                SyncOperation::Test => ochub_core::services::s3_sync::check_connection(&sync)
                    .await
                    .map(|_| "S3 连接成功".to_string()),
                SyncOperation::Upload => ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::upload(&db, &mut sync),
                )
                .await
                .map(|_| "S3 上传完成".to_string()),
                SyncOperation::Download => ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::download(&db, &mut sync),
                )
                .await
                .map(|_| "S3 下载并还原完成".to_string()),
            };
            this.update(cx, |this, cx| {
                this.sync_busy = false;
                this.settings = settings::get_settings();
                this.status = Some(SharedString::from(match result {
                    Ok(msg) => msg,
                    Err(err) => format!("S3 操作失败: {err}"),
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_show_in_tray(&mut self, cx: &mut Context<Self>) {
        self.settings.show_in_tray = !self.settings.show_in_tray;
        self.persist(cx);
    }

    fn toggle_minimize_to_tray(&mut self, cx: &mut Context<Self>) {
        self.settings.minimize_to_tray_on_close = !self.settings.minimize_to_tray_on_close;
        self.persist(cx);
    }

    fn toggle_app_window_controls(&mut self, cx: &mut Context<Self>) {
        self.settings.use_app_window_controls = !self.settings.use_app_window_controls;
        self.persist(cx);
    }

    fn toggle_launch_on_startup(&mut self, cx: &mut Context<Self>) {
        self.settings.launch_on_startup = !self.settings.launch_on_startup;
        self.persist(cx);
    }

    fn toggle_silent_startup(&mut self, cx: &mut Context<Self>) {
        self.settings.silent_startup = !self.settings.silent_startup;
        self.persist(cx);
    }

    fn cycle_language(&mut self, cx: &mut Context<Self>) {
        let next = match self.settings.language.as_deref() {
            Some("en") => "zh",
            Some("zh") => "en",
            _ => "en",
        };
        self.settings.language = Some(next.to_string());
        self.persist(cx);
    }

    fn reload_user_plugins(&mut self, cx: &mut Context<Self>) {
        let errors = ochub_core::plugin::reload_user_plugins();
        let plugin_count = ochub_core::plugin::all_plugins()
            .iter()
            .filter(|p| p.is_user_manifest())
            .count();
        self.status = Some(SharedString::from(if errors.is_empty() {
            format!("插件已重新加载（{plugin_count} 个用户插件）")
        } else {
            format!(
                "插件已重新加载（{plugin_count} 个成功，{} 个失败）",
                errors.len()
            )
        }));
        shell_menu::refresh(&self.app, cx);
        cx.emit(SettingsEvent::AppsChanged);
        self.list_state.remeasure();
        cx.notify();
    }

    fn open_user_plugins_dir(&mut self, cx: &mut Context<Self>) {
        let dir = ochub_core::plugin::user_plugins_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.status = Some(SharedString::from(format!("创建插件目录失败: {err}")));
            cx.notify();
            return;
        }
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg(&dir).status();
        #[cfg(target_os = "windows")]
        let result = Command::new("explorer").arg(&dir).status();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = Command::new("xdg-open").arg(&dir).status();
        match result {
            Ok(status) if status.success() => {
                self.status = Some(SharedString::from("已打开插件目录"));
            }
            Ok(status) => {
                self.status = Some(SharedString::from(format!("打开失败，状态: {status}")));
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("打开失败: {err}")));
            }
        }
        cx.notify();
    }

    fn app_is_enabled(&self, plugin: &dyn ochub_core::plugin::AppPlugin) -> bool {
        self.settings
            .app_enabled(plugin.id().as_str())
            .unwrap_or_else(|| plugin.enabled_by_default())
    }

    fn toggle_app_enabled(&mut self, id: &str, cx: &mut Context<Self>) {
        let plugins = ochub_core::plugin::all_plugins();
        let Some(plugin) = plugins.iter().find(|p| p.id().as_str() == id) else {
            return;
        };
        let currently = self.app_is_enabled(plugin.as_ref());
        let enabled_count = plugins
            .iter()
            .filter(|p| self.app_is_enabled(p.as_ref()))
            .count();
        if currently && enabled_count <= 1 {
            self.status = Some(SharedString::from("至少保留一个启用的应用"));
            cx.notify();
            return;
        }

        // The core service persists the flag and refreshes the enabled-app registry.
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            self.status = Some(SharedString::from("操作失败: runtime 初始化失败"));
            cx.notify();
            return;
        };
        let result = runtime.block_on(ochub_core::services::apps::set_app_enabled(
            &self.app,
            plugin.id(),
            !currently,
        ));
        match result {
            Ok(()) => {
                self.settings = settings::get_settings();
                self.status = Some(SharedString::from(if currently {
                    "已停用"
                } else {
                    "已启用"
                }));
                shell_menu::refresh(&self.app, cx);
                cx.emit(SettingsEvent::AppsChanged);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("操作失败: {err}")));
            }
        }
        cx.notify();
    }

    fn save_paths(&mut self, cx: &mut Context<Self>) {
        let app_dir = input_value(&self.app_config_dir, cx);
        match app_store::set_app_config_dir_to_store(empty_as_none(&app_dir)) {
            Ok(()) => {
                self.persist(cx);
                self.status = Some(SharedString::from(
                    "目录设置已保存；数据目录切换后建议重启应用。",
                ));
                cx.notify();
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("保存数据目录失败: {err}")));
                cx.notify();
            }
        }
    }

    fn save_terminal_and_backup(&mut self, cx: &mut Context<Self>) {
        let interval_raw = input_value(&self.backup_interval_hours, cx);
        let retain_raw = input_value(&self.backup_retain_count, cx);
        let Ok(interval) = parse_optional_u32(&interval_raw) else {
            self.status = Some(SharedString::from("备份间隔必须是正整数，或留空使用默认值"));
            cx.notify();
            return;
        };
        let Ok(retain) = parse_optional_u32(&retain_raw) else {
            self.status = Some(SharedString::from(
                "备份保留数量必须是正整数，或留空使用默认值",
            ));
            cx.notify();
            return;
        };
        if interval == Some(0) || retain == Some(0) {
            self.status = Some(SharedString::from("备份参数必须大于 0，或留空使用默认值"));
            cx.notify();
            return;
        }

        self.settings.preferred_terminal =
            empty_string_as_none(input_value(&self.preferred_terminal, cx));
        self.settings.backup_interval_hours = interval;
        self.settings.backup_retain_count = retain;
        self.persist(cx);
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.status = Some(SharedString::from("正在检查更新..."));
        cx.notify();

        let task = cx.background_spawn(async move {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    runtime.block_on(ochub_core::services::update::check_for_updates(None))
                }
                Err(err) => Err(ochub_core::AppError::Config(format!(
                    "构建异步运行时失败: {err}"
                ))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.update_checking = false;
                match result {
                    Ok(info) => {
                        this.status = Some(SharedString::from(if info.has_update {
                            format!(
                                "发现新版本 {}，当前版本 {}。",
                                info.latest_version.as_deref().unwrap_or("未知"),
                                info.current_version
                            )
                        } else {
                            format!("已是最新版本 {}。", info.current_version)
                        }));
                        this.update_info = Some(info);
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("检查更新失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_release_page(&mut self, cx: &mut Context<Self>) {
        let url = self
            .update_info
            .as_ref()
            .map(|info| info.release_url.clone())
            .unwrap_or_else(|| ochub_core::services::latest_release_url(None));
        match open_url(&url) {
            Ok(()) => {
                self.status = Some(SharedString::from("已打开发布页"));
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("打开发布页失败: {err}")));
            }
        }
        cx.notify();
    }

    fn update_row_value(&self) -> String {
        if self.update_checking {
            return "检查中".to_string();
        }
        if let Some(info) = &self.update_info {
            return match (info.has_update, info.latest_version.as_deref()) {
                (true, Some(latest)) => format!("可更新到 {latest}"),
                (true, None) => "发现新版本".to_string(),
                (false, _) => format!("当前 {}", info.current_version),
            };
        }
        format!("当前 {}", env!("CARGO_PKG_VERSION"))
    }

    fn render_toggle_row(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &str,
        description: &str,
        value: bool,
        on_toggle: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        layout::row()
            .id(id)
            .role(gpui::Role::Switch)
            .aria_label(SharedString::from(label.to_string()))
            .aria_toggled(if value {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme::inset()))
            .child(layout::row_label(
                label.to_string(),
                description.to_string(),
            ))
            .child(layout::toggle(value))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                on_toggle(this, cx);
            }))
    }

    fn render_value_row(
        &self,
        id: &'static str,
        label: &str,
        description: &str,
        value: String,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        layout::row()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(label.to_string()))
            .cursor_pointer()
            .hover(|s| s.bg(theme::inset()))
            .child(layout::row_label(
                label.to_string(),
                description.to_string(),
            ))
            .when(!value.is_empty(), |s| {
                s.child(components::badge(BadgeTone::Neutral, value))
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                on_click(this, cx);
            }))
    }

    fn render_input_row(
        label: &'static str,
        description: &'static str,
        input: Entity<TextInput>,
    ) -> impl IntoElement {
        layout::row()
            .child(layout::row_label(label, description))
            .child(div().w(px(320.)).flex_shrink_0().child(input))
    }

    fn render_action_row(
        provider: &'static str,
        status: String,
        save: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        test: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        upload: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        download_target: SyncDownloadTarget,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        layout::row()
            .child(layout::row_label(
                format!("{provider} 状态"),
                SharedString::from(status),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            format!("{provider}-save"),
                            "保存",
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| save(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-test"),
                            "测试",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| test(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-upload"),
                            "上传",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| upload(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-download"),
                            "下载",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_download = Some(download_target);
                                cx.notify();
                            },
                        )),
                    ),
            )
    }
    /// Render one top-level settings section as a list item. Driven by the
    /// virtualized `ListState`, so only on-screen sections (and their text inputs)
    /// are built — see [`crate::layout::virtual_body`].
    fn render_block(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match ix {
            0 => {
                let language = self
                    .settings
                    .language
                    .clone()
                    .unwrap_or_else(|| "auto".to_string());
                let mut rows: Vec<gpui::AnyElement> = vec![
                    self.render_toggle_row(
                        "set-tray",
                        "系统菜单快捷切换",
                        "在 macOS 顶栏和 Windows 任务栏菜单里显示供应商快捷入口。",
                        self.settings.show_in_tray,
                        Self::toggle_show_in_tray,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-minimize",
                        "关闭窗口时后台保留",
                        "点击关闭按钮时保留后台进程，方便从系统菜单重新打开。",
                        self.settings.minimize_to_tray_on_close,
                        Self::toggle_minimize_to_tray,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-window-controls",
                        "使用应用内窗口控制",
                        "启用 GPUI 自绘窗口控制区，适合无标题栏窗口。",
                        self.settings.use_app_window_controls,
                        Self::toggle_app_window_controls,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-launch-startup",
                        "开机启动",
                        "记录开机启动偏好，供启动项集成读取。",
                        self.settings.launch_on_startup,
                        Self::toggle_launch_on_startup,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-silent-startup",
                        "静默启动",
                        "开机启动时默认隐藏主窗口。",
                        self.settings.silent_startup,
                        Self::toggle_silent_startup,
                        cx,
                    )
                    .into_any_element(),
                ];
                rows.push(
                    self.render_value_row(
                        "set-language",
                        "语言",
                        "界面语言，点击在 en / zh 间切换。",
                        language,
                        Self::cycle_language,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "set-update-check",
                        "检查更新",
                        "查询 GitHub 最新发布版本，并和当前应用版本比较。",
                        self.update_row_value(),
                        Self::check_updates,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "set-update-release",
                        "发布页",
                        "打开最新版本发布页；当前 GPUI 版本需要手动下载安装。",
                        "打开".to_string(),
                        Self::open_release_page,
                        cx,
                    )
                    .into_any_element(),
                );
                section_block("基础行为", "窗口、启动与语言偏好。", rows)
            }
            1 => {
                let mut rows: Vec<gpui::AnyElement> = ochub_core::plugin::all_plugins()
                    .into_iter()
                    .map(|plugin| {
                        let id = plugin.id().as_str().to_string();
                        let label = plugin.display_name().to_string();
                        let description = if plugin.is_user_manifest() {
                            format!("启用 {label} 供应商管理（用户插件）。")
                        } else {
                            format!("启用 {label} 供应商管理与配置写入。")
                        };
                        let enabled = self.app_is_enabled(plugin.as_ref());
                        self.render_toggle_row(
                            SharedString::from(format!("app-enabled-{id}")),
                            &label,
                            &description,
                            enabled,
                            move |this, cx| this.toggle_app_enabled(&id, cx),
                            cx,
                        )
                        .into_any_element()
                    })
                    .collect();
                for (index, err) in ochub_core::plugin::manifest_load_errors()
                    .into_iter()
                    .enumerate()
                {
                    rows.push(
                        div()
                            .id(SharedString::from(format!("plugin-load-error-{index}")))
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(theme::red())
                            .child(SharedString::from(format!(
                                "插件加载失败 {}: {}",
                                err.path, err.message
                            )))
                            .into_any_element(),
                    );
                }
                rows.push(
                    self.render_value_row(
                        "reload-user-plugins",
                        "重新加载插件",
                        "重新扫描 ~/.ochub/apps/*.toml 并注册用户插件。",
                        String::new(),
                        |this, cx| this.reload_user_plugins(cx),
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "open-user-plugins-dir",
                        "打开插件目录",
                        "在文件管理器中打开用户插件目录（不存在时自动创建）。",
                        String::new(),
                        |this, cx| this.open_user_plugins_dir(cx),
                        cx,
                    )
                    .into_any_element(),
                );
                section_block(
                    "应用管理",
                    "启用或停用应用。停用后不再显示、不再写入其配置文件；数据保留，可随时重新启用。用户可在插件目录放置 TOML manifest 适配新应用。",
                    rows,
                )
            }
            2 => section_block(
                "配置目录",
                "覆盖 OcHub 数据目录；各 CLI 的配置目录已移至对应应用的「应用设置」。",
                vec![
                    Self::render_input_row(
                        "OcHub 数据目录",
                        "数据库、备份和托管技能目录；保存后建议重启应用。",
                        self.app_config_dir.clone(),
                    )
                    .into_any_element(),
                    self.render_value_row(
                        "set-save-paths",
                        "保存目录设置",
                        "写入路径覆盖；数据目录切换后重启才能完整重新加载数据库。",
                        "保存".to_string(),
                        Self::save_paths,
                        cx,
                    )
                    .into_any_element(),
                ],
            ),
            3 => section_block(
                "终端与备份",
                "配置会话启动终端和本地自动备份策略。",
                vec![
                    Self::render_input_row(
                        "首选终端",
                        "留空自动探测；可填写 Terminal、iTerm、WezTerm、Ghostty 等名称。",
                        self.preferred_terminal.clone(),
                    )
                    .into_any_element(),
                    Self::render_input_row(
                        "备份间隔（小时）",
                        "留空使用默认 24；用于数据库自动备份。",
                        self.backup_interval_hours.clone(),
                    )
                    .into_any_element(),
                    Self::render_input_row(
                        "备份保留数量",
                        "留空使用默认 10；最小为 1。",
                        self.backup_retain_count.clone(),
                    )
                    .into_any_element(),
                    self.render_value_row(
                        "set-save-terminal-backup",
                        "保存终端与备份",
                        "写入终端偏好、备份间隔和保留数量。",
                        "保存".to_string(),
                        Self::save_terminal_and_backup,
                        cx,
                    )
                    .into_any_element(),
                ],
            ),
            4 => {
                let webdav = self.settings.webdav_sync.clone().unwrap_or_default();
                let webdav_status =
                    sync_status_text(webdav.enabled, webdav.auto_sync, &webdav.status);
                section_block(
                    "WebDAV 同步",
                    "配置远端 WebDAV 目录，手动上传/下载本地数据库和技能快照。",
                    vec![
                        self.render_toggle_row(
                            "webdav-enabled",
                            "启用 WebDAV",
                            "启用后可手动同步；自动同步需要另行打开。",
                            webdav.enabled,
                            Self::toggle_webdav_enabled,
                            cx,
                        )
                        .into_any_element(),
                        self.render_toggle_row(
                            "webdav-auto",
                            "WebDAV 自动同步",
                            "数据库变更后自动排队上传快照。",
                            webdav.auto_sync,
                            Self::toggle_webdav_auto,
                            cx,
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "地址",
                            "WebDAV 服务根地址。",
                            self.webdav_url.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "用户名",
                            "WebDAV 登录用户名。",
                            self.webdav_username.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "密码",
                            "WebDAV 登录密码；本地 settings 文件会以 0600 权限保存。",
                            self.webdav_password.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "远端目录",
                            "同步快照保存的根目录，默认 ochub-sync。",
                            self.webdav_remote_root.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "配置档",
                            "同一远端目录下的配置档名称，默认 default。",
                            self.webdav_profile.clone(),
                        )
                        .into_any_element(),
                        Self::render_action_row(
                            "WebDAV",
                            webdav_status,
                            Self::save_webdav,
                            |this, cx| this.run_webdav_sync(SyncOperation::Test, cx),
                            |this, cx| this.run_webdav_sync(SyncOperation::Upload, cx),
                            SyncDownloadTarget::WebDav,
                            cx,
                        )
                        .into_any_element(),
                    ],
                )
            }
            5 => {
                let s3 = self.settings.s3_sync.clone().unwrap_or_default();
                let s3_status = sync_status_text(s3.enabled, s3.auto_sync, &s3.status);
                section_block(
                    "S3 同步",
                    "配置 S3/R2 兼容存储，手动上传/下载本地数据库和技能快照。",
                    vec![
                        self.render_toggle_row(
                            "s3-enabled",
                            "启用 S3",
                            "启用后可手动同步；自动同步需要另行打开。",
                            s3.enabled,
                            Self::toggle_s3_enabled,
                            cx,
                        )
                        .into_any_element(),
                        self.render_toggle_row(
                            "s3-auto",
                            "S3 自动同步",
                            "数据库变更后自动排队上传快照。",
                            s3.auto_sync,
                            Self::toggle_s3_auto,
                            cx,
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Region",
                            "S3 区域；Cloudflare R2 常用 auto。",
                            self.s3_region.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Bucket",
                            "保存同步快照的存储桶。",
                            self.s3_bucket.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Access Key ID",
                            "S3/R2 访问密钥 ID。",
                            self.s3_access_key.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Secret Access Key",
                            "S3/R2 访问密钥 Secret；本地 settings 文件会以 0600 权限保存。",
                            self.s3_secret_key.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Endpoint",
                            "兼容 S3 的自定义 Endpoint；AWS S3 可留空。",
                            self.s3_endpoint.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "远端目录",
                            "同步快照保存的根目录，默认 ochub-sync。",
                            self.s3_remote_root.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "配置档",
                            "同一远端目录下的配置档名称，默认 default。",
                            self.s3_profile.clone(),
                        )
                        .into_any_element(),
                        Self::render_action_row(
                            "S3",
                            s3_status,
                            Self::save_s3,
                            |this, cx| this.run_s3_sync(SyncOperation::Test, cx),
                            |this, cx| this.run_s3_sync(SyncOperation::Upload, cx),
                            SyncDownloadTarget::S3,
                            cx,
                        )
                        .into_any_element(),
                    ],
                )
            }
            _ => gpui::Empty.into_any_element(),
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        layout::page()
            .relative()
            .child(layout::page_header("设置", None))
            .child(layout::virtual_body(gpui::list(
                self.list_state.clone(),
                cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
            )))
            .when_some(self.confirm_download, |root, target| {
                let provider = match target {
                    SyncDownloadTarget::WebDav => "WebDAV",
                    SyncDownloadTarget::S3 => "S3",
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(SharedString::from(format!(
                            "下载并还原 {provider} 快照"
                        ))))
                        .child(
                            components::modal_body().child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(format!(
                                        "确定从 {provider} 下载快照并还原到本地吗？当前本地数据将被覆盖，此操作不可撤销。"
                                    ))),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "confirm-download-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_download = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "confirm-download-ok",
                                "下载并还原",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_download = None;
                                match target {
                                    SyncDownloadTarget::WebDav => {
                                        this.run_webdav_sync(SyncOperation::Download, cx);
                                    }
                                    SyncDownloadTarget::S3 => {
                                        this.run_s3_sync(SyncOperation::Download, cx);
                                    }
                                }
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

/// One settings section as a list item: section header above a grouped card, with
/// its own bottom spacing (the virtualized list draws no inter-item gap).
fn section_block(
    title: &'static str,
    description: &'static str,
    rows: Vec<gpui::AnyElement>,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pb_3()
        .w_full()
        .child(layout::section_header(title, description))
        .child(layout::group(rows))
        .into_any_element()
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    cmd.status()
        .map_err(|err| err.to_string())
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("退出状态 {status}"))
            }
        })
}

fn text_input(cx: &mut Context<TextInput>, placeholder: &str, value: &str) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn option_text_input(
    cx: &mut Context<TextInput>,
    placeholder: &str,
    value: &Option<String>,
) -> TextInput {
    text_input(cx, placeholder, value.as_deref().unwrap_or_default())
}

fn input_value(input: &Entity<TextInput>, cx: &mut Context<SettingsView>) -> String {
    input.read(cx).content().trim().to_string()
}

fn empty_as_none(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn empty_string_as_none(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_optional_u32(value: &str) -> Result<Option<u32>, ()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u32>().map(Some).map_err(|_| ())
}

fn sync_start_message(provider: &str, operation: SyncOperation) -> String {
    match operation {
        SyncOperation::Test => format!("正在测试 {provider} 连接..."),
        SyncOperation::Upload => format!("正在上传 {provider} 快照..."),
        SyncOperation::Download => format!("正在从 {provider} 下载并还原..."),
    }
}

fn sync_status_text(enabled: bool, auto_sync: bool, status: &settings::WebDavSyncStatus) -> String {
    let mode = match (enabled, auto_sync) {
        (true, true) => "已启用，自动同步开启",
        (true, false) => "已启用，自动同步关闭",
        (false, _) => "未启用",
    };
    if let Some(err) = &status.last_error {
        return format!("{mode}；上次错误：{err}");
    }
    if let Some(ts) = status.last_sync_at {
        return format!("{mode}；上次同步 Unix 时间 {ts}");
    }
    mode.to_string()
}

crate::notifications::impl_status_toasts!(SettingsView);
