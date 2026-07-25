//! Device-level settings panel. Reads `ochub_core::settings::get_settings()` and
//! writes changes back via `ochub_core::settings::update_settings`.

use std::process::Command;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Context, Entity, Focusable, ListAlignment, ListState, SharedString,
    Window,
};
use ochub_core::app_store;
use ochub_core::i18n::Locale;
use ochub_core::services::UpdateCheckResult;
use ochub_core::settings::{self, AppSettings, S3SyncSettings, WebDavSyncSettings};
use ochub_core::AppState;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::shell_menu;
use crate::text_input::TextInput;
use crate::tf;
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
    /// The interface language changed. Repainting covers rendered text, but
    /// strings captured at construction time (text-input placeholders) and
    /// memoized list-item heights have to be refreshed explicitly.
    LocaleChanged,
}

impl gpui::EventEmitter<SettingsEvent> for SettingsView {}

pub struct SettingsView {
    app: Arc<AppState>,
    settings: AppSettings,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
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
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // Placeholders are captured when the input is constructed, and this
        // view is built once at startup, so they need pushing in by hand.
        self.webdav_username.update(cx, |input, cx| {
            input.set_placeholder(t(k::SETTINGS_SYNC_USERNAME_PLACEHOLDER), cx)
        });
        self.webdav_password.update(cx, |input, cx| {
            input.set_placeholder(t(k::SETTINGS_SYNC_PASSWORD_PLACEHOLDER), cx)
        });
        self.list_state.remeasure();
        cx.notify();
    }

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
        let webdav_username = cx.new(|cx| {
            text_input(
                cx,
                t(k::SETTINGS_SYNC_USERNAME_PLACEHOLDER),
                &webdav.username,
            )
        });
        let webdav_password = cx.new(|cx| {
            let mut input =
                TextInput::new(cx, t(k::SETTINGS_SYNC_PASSWORD_PLACEHOLDER)).masked(true);
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
            status_level: None,
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

    /// Every status toast carries its severity explicitly. Inferring it from
    /// the wording mis-reads several of these messages (a saved directory that
    /// merely *suggests* a restart is not a warning) and stops working
    /// entirely once the copy is translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(text.into());
        self.status_level = Some(level);
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let saved = match settings::update_settings(self.settings.clone()) {
            Ok(()) => {
                self.set_status(NotificationLevel::Success, t(k::SETTINGS_STATUS_SAVED), cx);
                true
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_STATUS_SAVE_FAILED, error = err),
                    cx,
                );
                false
            }
        };
        // Re-read so we reflect any normalization.
        self.settings = settings::get_settings();
        if saved {
            shell_menu::refresh(&self.app, cx);
            // The close-behaviour toggle changes when the process may exit.
            crate::apply_quit_mode(cx);
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
            self.set_status(
                NotificationLevel::Error,
                tf!(
                    k::SETTINGS_SYNC_SETTINGS_SAVE_FAILED,
                    provider = "WebDAV",
                    error = err
                ),
                cx,
            );
            return;
        }
        self.settings = settings::get_settings();
        self.sync_busy = true;
        self.set_status(
            NotificationLevel::Info,
            sync_start_message("WebDAV", operation),
            cx,
        );

        let db = self.app.db.clone();
        cx.spawn(async move |this, cx| {
            let result = match operation {
                SyncOperation::Test => ochub_core::services::webdav_sync::check_connection(&sync)
                    .await
                    .map(|_| tf!(k::SETTINGS_SYNC_TEST_OK, provider = "WebDAV")),
                SyncOperation::Upload => ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::upload(&db, &mut sync),
                )
                .await
                .map(|_| tf!(k::SETTINGS_SYNC_UPLOAD_OK, provider = "WebDAV")),
                SyncOperation::Download => ochub_core::services::webdav_sync::run_with_sync_lock(
                    ochub_core::services::webdav_sync::download(&db, &mut sync),
                )
                .await
                .map(|_| tf!(k::SETTINGS_SYNC_DOWNLOAD_OK, provider = "WebDAV")),
            };
            this.update(cx, |this, cx| {
                this.sync_busy = false;
                this.settings = settings::get_settings();
                match result {
                    Ok(msg) => this.set_status(NotificationLevel::Success, msg, cx),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_SYNC_FAILED, provider = "WebDAV", error = err),
                        cx,
                    ),
                }
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
            self.set_status(
                NotificationLevel::Error,
                tf!(
                    k::SETTINGS_SYNC_SETTINGS_SAVE_FAILED,
                    provider = "S3",
                    error = err
                ),
                cx,
            );
            return;
        }
        self.settings = settings::get_settings();
        self.sync_busy = true;
        self.set_status(
            NotificationLevel::Info,
            sync_start_message("S3", operation),
            cx,
        );

        let db = self.app.db.clone();
        cx.spawn(async move |this, cx| {
            let result = match operation {
                SyncOperation::Test => ochub_core::services::s3_sync::check_connection(&sync)
                    .await
                    .map(|_| tf!(k::SETTINGS_SYNC_TEST_OK, provider = "S3")),
                SyncOperation::Upload => ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::upload(&db, &mut sync),
                )
                .await
                .map(|_| tf!(k::SETTINGS_SYNC_UPLOAD_OK, provider = "S3")),
                SyncOperation::Download => ochub_core::services::s3_sync::run_with_sync_lock(
                    ochub_core::services::s3_sync::download(&db, &mut sync),
                )
                .await
                .map(|_| tf!(k::SETTINGS_SYNC_DOWNLOAD_OK, provider = "S3")),
            };
            this.update(cx, |this, cx| {
                this.sync_busy = false;
                this.settings = settings::get_settings();
                match result {
                    Ok(msg) => this.set_status(NotificationLevel::Success, msg, cx),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_SYNC_FAILED, provider = "S3", error = err),
                        cx,
                    ),
                }
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

    fn toggle_launch_on_startup(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.launch_on_startup;
        // Register with the OS first: the stored flag must never claim a login
        // item that does not exist.
        if let Err(err) = ochub_core::autostart::set_enabled(target, self.settings.silent_startup) {
            self.set_status(NotificationLevel::Error, err.to_string(), cx);
            return;
        }
        self.settings.launch_on_startup = target;
        self.persist(cx);
    }

    fn toggle_silent_startup(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.silent_startup;
        // The flag is carried on the registered command line, so an existing
        // login item has to be rewritten for the change to mean anything.
        if self.settings.launch_on_startup {
            if let Err(err) = ochub_core::autostart::set_enabled(true, target) {
                self.set_status(NotificationLevel::Error, err.to_string(), cx);
                return;
            }
        }
        self.settings.silent_startup = target;
        self.persist(cx);
    }

    /// `None` = follow the OS, then one entry per shipped locale.
    fn language_choices() -> Vec<Option<Locale>> {
        std::iter::once(None).chain(Locale::ALL.map(Some)).collect()
    }

    fn selected_language(&self) -> usize {
        let current = self.settings.language.as_deref().and_then(Locale::from_tag);
        Self::language_choices()
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0)
    }

    fn set_language(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(choice) = Self::language_choices().get(index).copied() else {
            return;
        };
        self.settings.language = choice.map(|locale| locale.tag().to_string());
        self.persist(cx);
        // Apply, then repaint: `refresh_windows` is what defeats gpui's
        // element-state reuse and re-runs `render` across the whole tree.
        ochub_core::i18n::install(ochub_core::i18n::resolve(self.settings.language.as_deref()));
        cx.emit(SettingsEvent::LocaleChanged);
        cx.refresh_windows();
    }

    fn reload_user_plugins(&mut self, cx: &mut Context<Self>) {
        let errors = ochub_core::plugin::reload_user_plugins();
        let plugin_count = ochub_core::plugin::all_plugins()
            .iter()
            .filter(|p| p.is_user_manifest())
            .count();
        // A partial reload still leaves broken manifests behind, so it is a
        // warning rather than a clean success.
        let (level, message) = if errors.is_empty() {
            (
                NotificationLevel::Success,
                tf!(k::SETTINGS_APPS_PLUGINS_RELOADED, count = plugin_count),
            )
        } else {
            (
                NotificationLevel::Warning,
                tf!(
                    k::SETTINGS_APPS_PLUGINS_RELOADED_PARTIAL,
                    loaded = plugin_count,
                    failed = errors.len()
                ),
            )
        };
        self.set_status(level, message, cx);
        shell_menu::refresh(&self.app, cx);
        cx.emit(SettingsEvent::AppsChanged);
        self.list_state.remeasure();
        cx.notify();
    }

    fn open_user_plugins_dir(&mut self, cx: &mut Context<Self>) {
        let dir = ochub_core::plugin::user_plugins_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::SETTINGS_APPS_PLUGINS_DIR_CREATE_FAILED, error = err),
                cx,
            );
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
                self.set_status(
                    NotificationLevel::Success,
                    t(k::SETTINGS_APPS_PLUGINS_DIR_OPENED),
                    cx,
                );
            }
            Ok(status) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_APPS_OPEN_FAILED_STATUS, status = status),
                    cx,
                );
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_APPS_OPEN_FAILED, error = err),
                    cx,
                );
            }
        }
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
            // Refused, not failed: the toggle simply does not apply here.
            self.set_status(
                NotificationLevel::Warning,
                t(k::SETTINGS_APPS_KEEP_ONE_ENABLED),
                cx,
            );
            return;
        }

        // The core service persists the flag and refreshes the enabled-app registry.
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            self.set_status(
                NotificationLevel::Error,
                t(k::SETTINGS_APPS_RUNTIME_INIT_FAILED),
                cx,
            );
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
                self.set_status(
                    NotificationLevel::Success,
                    if currently {
                        t(k::SETTINGS_APPS_DISABLED)
                    } else {
                        t(k::SETTINGS_APPS_ENABLED)
                    },
                    cx,
                );
                shell_menu::refresh(&self.app, cx);
                cx.emit(SettingsEvent::AppsChanged);
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_APPS_ACTION_FAILED, error = err),
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn save_paths(&mut self, cx: &mut Context<Self>) {
        let app_dir = input_value(&self.app_config_dir, cx);
        match app_store::set_app_config_dir_to_store(empty_as_none(&app_dir)) {
            Ok(()) => {
                self.persist(cx);
                // The restart is advice, not a caveat on the save itself.
                self.set_status(
                    NotificationLevel::Success,
                    t(k::SETTINGS_CONFIG_DIR_SAVED),
                    cx,
                );
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_CONFIG_DIR_SAVE_FAILED, error = err),
                    cx,
                );
            }
        }
    }

    fn save_terminal_and_backup(&mut self, cx: &mut Context<Self>) {
        let interval_raw = input_value(&self.backup_interval_hours, cx);
        let retain_raw = input_value(&self.backup_retain_count, cx);
        let Ok(interval) = parse_optional_u32(&interval_raw) else {
            self.set_status(
                NotificationLevel::Error,
                t(k::SETTINGS_BACKUP_INTERVAL_INVALID),
                cx,
            );
            return;
        };
        let Ok(retain) = parse_optional_u32(&retain_raw) else {
            self.set_status(
                NotificationLevel::Error,
                t(k::SETTINGS_BACKUP_RETAIN_INVALID),
                cx,
            );
            return;
        };
        if interval == Some(0) || retain == Some(0) {
            self.set_status(
                NotificationLevel::Error,
                t(k::SETTINGS_BACKUP_VALUE_TOO_SMALL),
                cx,
            );
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
        self.set_status(
            NotificationLevel::Info,
            t(k::SETTINGS_UPDATE_CHECKING_STATUS),
            cx,
        );

        let task = cx.background_spawn(async move {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    runtime.block_on(ochub_core::services::update::check_for_updates(None))
                }
                Err(err) => Err(ochub_core::AppError::Config(tf!(
                    k::SETTINGS_UPDATE_RUNTIME_FAILED,
                    error = err
                ))),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.update_checking = false;
                match result {
                    Ok(info) => {
                        // An available update is a neutral finding; being up to
                        // date is the check completing with nothing to do.
                        let (level, message) = if info.has_update {
                            (
                                NotificationLevel::Info,
                                tf!(
                                    k::SETTINGS_UPDATE_AVAILABLE,
                                    latest = info
                                        .latest_version
                                        .as_deref()
                                        .unwrap_or(raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION)),
                                    current = info.current_version
                                ),
                            )
                        } else {
                            (
                                NotificationLevel::Success,
                                tf!(
                                    k::SETTINGS_UPDATE_UP_TO_DATE,
                                    current = info.current_version
                                ),
                            )
                        };
                        this.set_status(level, message, cx);
                        this.update_info = Some(info);
                    }
                    Err(err) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::SETTINGS_UPDATE_CHECK_FAILED, error = err),
                            cx,
                        );
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
                self.set_status(
                    NotificationLevel::Success,
                    t(k::SETTINGS_UPDATE_RELEASE_OPENED),
                    cx,
                );
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::SETTINGS_UPDATE_RELEASE_OPEN_FAILED, error = err),
                    cx,
                );
            }
        }
    }

    fn update_row_value(&self) -> String {
        if self.update_checking {
            return raw(k::SETTINGS_UPDATE_CHECKING).to_string();
        }
        if let Some(info) = &self.update_info {
            return match (info.has_update, info.latest_version.as_deref()) {
                (true, Some(latest)) => tf!(k::SETTINGS_UPDATE_ROW_UPGRADABLE, latest = latest),
                (true, None) => raw(k::SETTINGS_UPDATE_ROW_NEW_VERSION).to_string(),
                (false, _) => tf!(
                    k::SETTINGS_UPDATE_ROW_CURRENT,
                    version = info.current_version
                ),
            };
        }
        tf!(
            k::SETTINGS_UPDATE_ROW_CURRENT,
            version = env!("CARGO_PKG_VERSION")
        )
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

    /// A row whose control is a single-select pill row. Unlike a click-to-cycle
    /// row, every option and the current one are both visible, and any option
    /// is reachable in one action.
    fn render_choice_row(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
        options: &[&str],
        selected: usize,
        on_select: impl Fn(&mut Self, usize, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let listener = cx.listener(move |this, index: &usize, _window, cx| {
            on_select(this, *index, cx);
        });
        layout::row()
            .child(layout::row_label(label, description))
            .child(div().flex_shrink_0().child(components::segmented(
                id,
                options,
                selected,
                move |index, window, cx| listener(&index, window, cx),
            )))
    }

    fn render_input_row(
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
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
                tf!(k::SETTINGS_SYNC_STATUS_LABEL, provider = provider),
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
                            t(k::SETTINGS_ACTION_SAVE),
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| save(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-test"),
                            t(k::SETTINGS_ACTION_TEST),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| test(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-upload"),
                            t(k::SETTINGS_ACTION_UPLOAD),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| upload(this, cx))),
                    )
                    .child(
                        components::button(
                            format!("{provider}-download"),
                            t(k::SETTINGS_ACTION_DOWNLOAD),
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
                let mut rows: Vec<gpui::AnyElement> = vec![
                    self.render_toggle_row(
                        "set-tray",
                        &t(k::SETTINGS_BASIC_TRAY_LABEL),
                        &t(k::SETTINGS_BASIC_TRAY_DESC),
                        self.settings.show_in_tray,
                        Self::toggle_show_in_tray,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-minimize",
                        &t(k::SETTINGS_BASIC_MINIMIZE_LABEL),
                        &t(k::SETTINGS_BASIC_MINIMIZE_DESC),
                        self.settings.minimize_to_tray_on_close,
                        Self::toggle_minimize_to_tray,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-launch-startup",
                        &t(k::SETTINGS_BASIC_LAUNCH_STARTUP_LABEL),
                        &t(k::SETTINGS_BASIC_LAUNCH_STARTUP_DESC),
                        self.settings.launch_on_startup,
                        Self::toggle_launch_on_startup,
                        cx,
                    )
                    .into_any_element(),
                    self.render_toggle_row(
                        "set-silent-startup",
                        &t(k::SETTINGS_BASIC_SILENT_STARTUP_LABEL),
                        &t(k::SETTINGS_BASIC_SILENT_STARTUP_DESC),
                        self.settings.silent_startup,
                        Self::toggle_silent_startup,
                        cx,
                    )
                    .into_any_element(),
                ];
                let language_options: Vec<&str> =
                    std::iter::once(raw(k::SETTINGS_BASIC_LANGUAGE_SYSTEM))
                        .chain(Locale::ALL.iter().map(|locale| locale.endonym()))
                        .collect();
                rows.push(
                    self.render_choice_row(
                        "set-language",
                        t(k::SETTINGS_BASIC_LANGUAGE_LABEL),
                        t(k::SETTINGS_BASIC_LANGUAGE_DESC),
                        &language_options,
                        self.selected_language(),
                        |this, index, cx| this.set_language(index, cx),
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "set-update-check",
                        &t(k::SETTINGS_UPDATE_LABEL),
                        &t(k::SETTINGS_UPDATE_DESC),
                        self.update_row_value(),
                        Self::check_updates,
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "set-update-release",
                        &t(k::SETTINGS_UPDATE_RELEASE_LABEL),
                        &t(k::SETTINGS_UPDATE_RELEASE_DESC),
                        raw(k::SETTINGS_ACTION_OPEN).to_string(),
                        Self::open_release_page,
                        cx,
                    )
                    .into_any_element(),
                );
                section_block(t(k::SETTINGS_BASIC_TITLE), t(k::SETTINGS_BASIC_DESC), rows)
            }
            1 => {
                let mut rows: Vec<gpui::AnyElement> = ochub_core::plugin::all_plugins()
                    .into_iter()
                    .map(|plugin| {
                        let id = plugin.id().as_str().to_string();
                        let label = plugin.display_name().to_string();
                        let description = if plugin.is_user_manifest() {
                            tf!(k::SETTINGS_APPS_PLUGIN_DESC_USER, app = label)
                        } else {
                            tf!(k::SETTINGS_APPS_PLUGIN_DESC, app = label)
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
                            .child(SharedString::from(tf!(
                                k::SETTINGS_APPS_PLUGIN_LOAD_FAILED,
                                path = err.path,
                                message = err.message
                            )))
                            .into_any_element(),
                    );
                }
                rows.push(
                    self.render_value_row(
                        "reload-user-plugins",
                        &t(k::SETTINGS_APPS_RELOAD_LABEL),
                        &t(k::SETTINGS_APPS_RELOAD_DESC),
                        String::new(),
                        |this, cx| this.reload_user_plugins(cx),
                        cx,
                    )
                    .into_any_element(),
                );
                rows.push(
                    self.render_value_row(
                        "open-user-plugins-dir",
                        &t(k::SETTINGS_APPS_PLUGINS_DIR_LABEL),
                        &t(k::SETTINGS_APPS_PLUGINS_DIR_DESC),
                        String::new(),
                        |this, cx| this.open_user_plugins_dir(cx),
                        cx,
                    )
                    .into_any_element(),
                );
                section_block(t(k::SETTINGS_APPS_TITLE), t(k::SETTINGS_APPS_DESC), rows)
            }
            2 => section_block(
                t(k::SETTINGS_CONFIG_DIR_TITLE),
                t(k::SETTINGS_CONFIG_DIR_DESC),
                vec![
                    Self::render_input_row(
                        t(k::SETTINGS_CONFIG_DIR_DATA_LABEL),
                        t(k::SETTINGS_CONFIG_DIR_DATA_DESC),
                        self.app_config_dir.clone(),
                    )
                    .into_any_element(),
                    self.render_value_row(
                        "set-save-paths",
                        &t(k::SETTINGS_CONFIG_DIR_SAVE_LABEL),
                        &t(k::SETTINGS_CONFIG_DIR_SAVE_DESC),
                        raw(k::SETTINGS_ACTION_SAVE).to_string(),
                        Self::save_paths,
                        cx,
                    )
                    .into_any_element(),
                ],
            ),
            3 => section_block(
                t(k::SETTINGS_TERMINAL_TITLE),
                t(k::SETTINGS_TERMINAL_DESC),
                vec![
                    Self::render_input_row(
                        t(k::SETTINGS_TERMINAL_PREFERRED_LABEL),
                        t(k::SETTINGS_TERMINAL_PREFERRED_DESC),
                        self.preferred_terminal.clone(),
                    )
                    .into_any_element(),
                    Self::render_input_row(
                        t(k::SETTINGS_BACKUP_INTERVAL_LABEL),
                        t(k::SETTINGS_BACKUP_INTERVAL_DESC),
                        self.backup_interval_hours.clone(),
                    )
                    .into_any_element(),
                    Self::render_input_row(
                        t(k::SETTINGS_BACKUP_RETAIN_LABEL),
                        t(k::SETTINGS_BACKUP_RETAIN_DESC),
                        self.backup_retain_count.clone(),
                    )
                    .into_any_element(),
                    self.render_value_row(
                        "set-save-terminal-backup",
                        &t(k::SETTINGS_TERMINAL_SAVE_LABEL),
                        &t(k::SETTINGS_TERMINAL_SAVE_DESC),
                        raw(k::SETTINGS_ACTION_SAVE).to_string(),
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
                    t(k::SETTINGS_WEBDAV_TITLE),
                    t(k::SETTINGS_WEBDAV_DESC),
                    vec![
                        self.render_toggle_row(
                            "webdav-enabled",
                            &t(k::SETTINGS_WEBDAV_ENABLED_LABEL),
                            &t(k::SETTINGS_SYNC_ENABLED_DESC),
                            webdav.enabled,
                            Self::toggle_webdav_enabled,
                            cx,
                        )
                        .into_any_element(),
                        self.render_toggle_row(
                            "webdav-auto",
                            &t(k::SETTINGS_WEBDAV_AUTO_LABEL),
                            &t(k::SETTINGS_SYNC_AUTO_DESC),
                            webdav.auto_sync,
                            Self::toggle_webdav_auto,
                            cx,
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_WEBDAV_URL_LABEL),
                            t(k::SETTINGS_WEBDAV_URL_DESC),
                            self.webdav_url.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_WEBDAV_USERNAME_LABEL),
                            t(k::SETTINGS_WEBDAV_USERNAME_DESC),
                            self.webdav_username.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_WEBDAV_PASSWORD_LABEL),
                            t(k::SETTINGS_WEBDAV_PASSWORD_DESC),
                            self.webdav_password.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_SYNC_REMOTE_ROOT_LABEL),
                            t(k::SETTINGS_SYNC_REMOTE_ROOT_DESC),
                            self.webdav_remote_root.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_SYNC_PROFILE_LABEL),
                            t(k::SETTINGS_SYNC_PROFILE_DESC),
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
                    t(k::SETTINGS_S3_TITLE),
                    t(k::SETTINGS_S3_DESC),
                    vec![
                        self.render_toggle_row(
                            "s3-enabled",
                            &t(k::SETTINGS_S3_ENABLED_LABEL),
                            &t(k::SETTINGS_SYNC_ENABLED_DESC),
                            s3.enabled,
                            Self::toggle_s3_enabled,
                            cx,
                        )
                        .into_any_element(),
                        self.render_toggle_row(
                            "s3-auto",
                            &t(k::SETTINGS_S3_AUTO_LABEL),
                            &t(k::SETTINGS_SYNC_AUTO_DESC),
                            s3.auto_sync,
                            Self::toggle_s3_auto,
                            cx,
                        )
                        .into_any_element(),
                        // The S3 field names stay in Latin: they are the
                        // canonical names in every S3 console and SDK.
                        Self::render_input_row(
                            "Region",
                            t(k::SETTINGS_S3_REGION_DESC),
                            self.s3_region.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Bucket",
                            t(k::SETTINGS_S3_BUCKET_DESC),
                            self.s3_bucket.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Access Key ID",
                            t(k::SETTINGS_S3_ACCESS_KEY_DESC),
                            self.s3_access_key.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Secret Access Key",
                            t(k::SETTINGS_S3_SECRET_KEY_DESC),
                            self.s3_secret_key.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            "Endpoint",
                            t(k::SETTINGS_S3_ENDPOINT_DESC),
                            self.s3_endpoint.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_SYNC_REMOTE_ROOT_LABEL),
                            t(k::SETTINGS_SYNC_REMOTE_ROOT_DESC),
                            self.s3_remote_root.clone(),
                        )
                        .into_any_element(),
                        Self::render_input_row(
                            t(k::SETTINGS_SYNC_PROFILE_LABEL),
                            t(k::SETTINGS_SYNC_PROFILE_DESC),
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
            .child(layout::page_header(t(k::SETTINGS_PAGE_TITLE), None))
            .child(layout::virtual_body(
                "settings-body",
                gpui::list(
                    self.list_state.clone(),
                    cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
                ),
                &self.list_state,
            ))
            .when_some(self.confirm_download, |root, target| {
                let provider = match target {
                    SyncDownloadTarget::WebDav => "WebDAV",
                    SyncDownloadTarget::S3 => "S3",
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(SharedString::from(tf!(
                            k::SETTINGS_CONFIRM_DOWNLOAD_TITLE,
                            provider = provider
                        ))))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(tf!(
                                        k::SETTINGS_CONFIRM_DOWNLOAD_BODY,
                                        provider = provider
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "confirm-download-cancel",
                                t(k::SETTINGS_ACTION_CANCEL),
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
                                t(k::SETTINGS_CONFIRM_DOWNLOAD_CONFIRM),
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
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
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
                Err(tf!(k::SETTINGS_ERROR_EXIT_STATUS, status = status))
            }
        })
}

fn text_input(
    cx: &mut Context<TextInput>,
    placeholder: impl Into<SharedString>,
    value: &str,
) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn option_text_input(
    cx: &mut Context<TextInput>,
    placeholder: impl Into<SharedString>,
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
        SyncOperation::Test => tf!(k::SETTINGS_SYNC_START_TEST, provider = provider),
        SyncOperation::Upload => tf!(k::SETTINGS_SYNC_START_UPLOAD, provider = provider),
        SyncOperation::Download => tf!(k::SETTINGS_SYNC_START_DOWNLOAD, provider = provider),
    }
}

/// The mode is a whole clause rather than a fragment, so every locale is free to
/// pick its own separator and word order when a last-error or last-sync detail
/// is appended to it.
fn sync_status_text(enabled: bool, auto_sync: bool, status: &settings::WebDavSyncStatus) -> String {
    let mode = match (enabled, auto_sync) {
        (true, true) => raw(k::SETTINGS_SYNC_STATUS_ON_AUTO),
        (true, false) => raw(k::SETTINGS_SYNC_STATUS_ON),
        (false, _) => raw(k::SETTINGS_SYNC_STATUS_OFF),
    };
    if let Some(err) = &status.last_error {
        return tf!(k::SETTINGS_SYNC_STATUS_WITH_ERROR, mode = mode, error = err);
    }
    if let Some(ts) = status.last_sync_at {
        return tf!(k::SETTINGS_SYNC_STATUS_WITH_TIME, mode = mode, time = ts);
    }
    mode.to_string()
}

crate::notifications::impl_status_toasts_leveled!(SettingsView);
