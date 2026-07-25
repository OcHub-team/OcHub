//! The root page: six groups, fifteen-odd rows, no text input and no Save.
//!
//! Every writer in here goes straight to [`settings::mutate_settings`] and then
//! re-reads, so the control on screen always shows what is on disk. Side
//! effects fire only where they matter — the tray refresh on the tray toggle,
//! the quit mode on keep-running, the locale install on language — rather than
//! on every save the way the old `persist()` did.

use std::process::Command;

use gpui::{div, prelude::*, AnyElement, Context, PathPromptOptions, SharedString, Window};
use ochub_core::app_store;
use ochub_core::i18n::Locale;
use ochub_core::services::UpdateCheckResult;
use ochub_core::settings;

use crate::components::{self, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::shell_menu;
use crate::tf;

use super::options;
use super::rows;
use super::search::RowId;
use super::sync::SyncTarget;
use super::{Confirm, Page, SettingsEvent, SettingsView};

/// What the 检查更新 row knows. `info` is `None` until a check has run, which
/// is why the row reads "当前 x.y.z" rather than claiming to be up to date.
#[derive(Default)]
pub(super) struct UpdateState {
    pub checking: bool,
    pub info: Option<UpdateCheckResult>,
    /// Set once the user starts an install. Blocks a second one and switches
    /// the row's description to download progress.
    pub installing: bool,
    /// Bytes downloaded and, when the server sent a length, the total.
    pub progress: Option<(u64, Option<u64>)>,
}

impl UpdateState {
    /// Whether the row's button should install rather than check.
    fn can_install(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|info| info.has_update && info.can_self_install)
    }

    /// Download progress as a whole percentage, when the total is known.
    fn percent(&self) -> Option<u64> {
        match self.progress {
            Some((done, Some(total))) if total > 0 => Some(done.saturating_mul(100) / total),
            _ => None,
        }
    }
}

impl SettingsView {
    pub(super) fn render_root(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        layout::page()
            .relative()
            .child(
                layout::page_header(t(k::SETTINGS_PAGE_TITLE), Some(t(k::SETTINGS_PAGE_DESC)))
                    .child(
                        div()
                            .flex_none()
                            .w(gpui::px(220.))
                            .child(self.search.clone()),
                    ),
            )
            .child(layout::virtual_body(
                "settings-body",
                gpui::list(
                    self.root_list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_root_block(ix, window, cx)),
                ),
                &self.root_list,
            ))
    }

    fn render_root_block(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.query.trim().is_empty() {
            return self.render_search_results(cx);
        }
        match ix {
            0 => {
                let language = self.render_row(RowId::Language, cx);
                let terminal = self.render_row(RowId::Terminal, cx);
                rows::group_block(
                    t(k::SETTINGS_GENERAL_TITLE),
                    t(k::SETTINGS_GENERAL_DESC),
                    vec![language, terminal],
                )
            }
            1 => {
                let mut group = vec![
                    self.render_row(RowId::StartupLogin, cx),
                    self.render_row(RowId::StartupHidden, cx),
                    self.render_row(RowId::WindowKeepRunning, cx),
                    self.render_row(RowId::WindowTray, cx),
                ];
                // The login-item failure belongs to the two rows above it, so
                // it renders here rather than as a toast that outlives the
                // context it came from.
                if let Some(error) = self.startup_error.clone() {
                    group.push(
                        layout::row()
                            .child(components::field_error(SharedString::from(tf!(
                                k::SETTINGS_STARTUP_REGISTER_FAILED,
                                error = error
                            ))))
                            .into_any_element(),
                    );
                }
                rows::group_block(
                    t(k::SETTINGS_STARTUP_TITLE),
                    t(k::SETTINGS_STARTUP_DESC),
                    group,
                )
            }
            2 => {
                let open = self.render_row(RowId::AppsOpen, cx);
                rows::group_block(
                    t(k::SETTINGS_APPS_TITLE),
                    t(k::SETTINGS_APPS_DESC),
                    vec![open],
                )
            }
            3 => {
                let mut group = vec![self.render_row(RowId::DataDir, cx)];
                if app_store::get_app_config_dir_override().is_some() {
                    group.push(self.render_row(RowId::DataDirReset, cx));
                }
                if self.ccswitch_source.is_some() {
                    group.push(self.render_row(RowId::DataCcswitchImport, cx));
                }
                group.push(self.render_row(RowId::BackupInterval, cx));
                group.push(self.render_row(RowId::BackupRetain, cx));
                rows::group_block(t(k::SETTINGS_DATA_TITLE), t(k::SETTINGS_DATA_DESC), group)
            }
            4 => {
                let group = vec![
                    self.render_row(RowId::SyncTarget, cx),
                    self.render_row(RowId::SyncAuto, cx),
                    self.render_row(RowId::SyncOpen, cx),
                ];
                rows::group_block(t(k::SETTINGS_SYNC_TITLE), t(k::SETTINGS_SYNC_DESC), group)
            }
            5 => {
                let group = vec![
                    self.render_row(RowId::AboutAutoUpdate, cx),
                    self.render_row(RowId::AboutUpdate, cx),
                    self.render_row(RowId::AboutRelease, cx),
                ];
                rows::group_block(t(k::SETTINGS_ABOUT_TITLE), t(k::SETTINGS_ABOUT_DESC), group)
            }
            _ => gpui::Empty.into_any_element(),
        }
    }

    /// Every root row that currently exists, in page order. Search ranks
    /// against this, so a row the page is not drawing is not findable either.
    pub(super) fn visible_rows(&self) -> Vec<RowId> {
        let mut rows = vec![
            RowId::Language,
            RowId::Terminal,
            RowId::StartupLogin,
            RowId::StartupHidden,
            RowId::WindowKeepRunning,
            RowId::WindowTray,
            RowId::AppsOpen,
            RowId::DataDir,
        ];
        if app_store::get_app_config_dir_override().is_some() {
            rows.push(RowId::DataDirReset);
        }
        if self.ccswitch_source.is_some() {
            rows.push(RowId::DataCcswitchImport);
        }
        rows.extend([
            RowId::BackupInterval,
            RowId::BackupRetain,
            RowId::SyncTarget,
            RowId::SyncAuto,
            RowId::SyncOpen,
            RowId::AboutAutoUpdate,
            RowId::AboutUpdate,
            RowId::AboutRelease,
        ]);
        rows
    }

    /// The one place a root row is built. Search calls it too, so a hit is the
    /// real row — operable in place, not a link back to where it lives.
    pub(super) fn render_row(&mut self, row: RowId, cx: &mut Context<Self>) -> AnyElement {
        match row {
            RowId::Language => {
                let options: Vec<&str> = std::iter::once(raw(k::SETTINGS_BASIC_LANGUAGE_SYSTEM))
                    .chain(Locale::ALL.iter().map(|locale| locale.endonym()))
                    .collect();
                rows::select(
                    cx,
                    row,
                    &options,
                    self.selected_language(),
                    layout::SelectRowState::new(false, self.open_select_row == Some(row)),
                    None,
                    |this, index, cx| this.set_language(index, cx),
                )
            }
            RowId::Terminal => {
                let (values, labels, selected) =
                    options::terminal_choices(self.settings.preferred_terminal.clone());
                let options: Vec<&str> = labels.iter().map(String::as_str).collect();
                rows::select(
                    cx,
                    row,
                    &options,
                    selected,
                    layout::SelectRowState::new(false, self.open_select_row == Some(row)),
                    None,
                    move |this, index, cx| {
                        let Some(value) = values.get(index).cloned() else {
                            return;
                        };
                        this.write(move |settings| settings.preferred_terminal = value, cx);
                    },
                )
            }
            RowId::StartupLogin => rows::switch(
                cx,
                row,
                self.settings.launch_on_startup,
                false,
                None,
                |this, cx| this.toggle_launch_on_startup(cx),
            ),
            RowId::StartupHidden => {
                // Refused by disabling rather than by erroring after the click:
                // the flag only travels on a registered login item's command
                // line, so without one there is nothing for it to mean.
                let blocked = !self.settings.launch_on_startup;
                rows::switch(
                    cx,
                    row,
                    self.settings.silent_startup,
                    blocked,
                    blocked.then(|| t(k::SETTINGS_STARTUP_HIDDEN_REQUIRES_LOGIN)),
                    |this, cx| this.toggle_silent_startup(cx),
                )
            }
            RowId::WindowKeepRunning => rows::switch(
                cx,
                row,
                self.settings.minimize_to_tray_on_close,
                false,
                None,
                |this, cx| this.toggle_keep_running(cx),
            ),
            RowId::WindowTray => rows::switch(
                cx,
                row,
                self.settings.show_in_tray,
                false,
                None,
                |this, cx| this.toggle_tray(cx),
            ),
            RowId::AppsOpen => {
                let total = self.plugins.len();
                let enabled = self
                    .plugins
                    .iter()
                    .filter(|plugin| self.app_is_enabled(plugin.as_ref()))
                    .count();
                rows::nav(
                    cx,
                    row,
                    Some(SharedString::from(tf!(
                        k::SETTINGS_APPS_COUNT,
                        enabled = enabled,
                        total = total
                    ))),
                    false,
                    None,
                    |this, cx| this.go(Page::Apps, cx),
                )
            }
            RowId::DataDir => rows::act(
                cx,
                row,
                t(k::SETTINGS_DATA_DIR_CHANGE),
                ButtonTone::Neutral,
                false,
                Some(self.data_dir_description()),
                |this, cx| this.pick_data_dir(cx),
            ),
            RowId::DataDirReset => rows::act(
                cx,
                row,
                t(k::SETTINGS_DATA_DIR_RESET_ACTION),
                ButtonTone::Neutral,
                false,
                None,
                |this, cx| this.reset_data_dir(cx),
            ),
            RowId::DataCcswitchImport => rows::act(
                cx,
                row,
                if self.ccswitch_busy {
                    t(k::SETTINGS_DATA_CCSWITCH_BUSY)
                } else {
                    t(k::SETTINGS_DATA_CCSWITCH_ACTION)
                },
                ButtonTone::Neutral,
                self.ccswitch_busy,
                self.ccswitch_description(),
                |this, cx| {
                    this.confirm = Some(Confirm::CcswitchImport);
                    cx.notify();
                },
            ),
            RowId::BackupInterval => {
                let (values, labels, selected) =
                    options::backup_interval_choices(self.settings.backup_interval_hours);
                let options: Vec<&str> = labels.iter().map(String::as_str).collect();
                rows::select(
                    cx,
                    row,
                    &options,
                    selected,
                    layout::SelectRowState::new(false, self.open_select_row == Some(row)),
                    None,
                    move |this, index, cx| {
                        let Some(hours) = values.get(index).copied() else {
                            return;
                        };
                        this.write(
                            move |settings| settings.backup_interval_hours = Some(hours),
                            cx,
                        );
                    },
                )
            }
            RowId::BackupRetain => {
                let (values, labels, selected) =
                    options::backup_retain_choices(self.settings.backup_retain_count);
                let options: Vec<&str> = labels.iter().map(String::as_str).collect();
                rows::select(
                    cx,
                    row,
                    &options,
                    selected,
                    layout::SelectRowState::new(false, self.open_select_row == Some(row)),
                    None,
                    move |this, index, cx| {
                        let Some(count) = values.get(index).copied() else {
                            return;
                        };
                        this.write(
                            move |settings| settings.backup_retain_count = Some(count),
                            cx,
                        );
                    },
                )
            }
            RowId::SyncTarget => {
                let options = [raw(k::SETTINGS_SYNC_TARGET_OFF), "WebDAV", "S3"];
                let selected = match self.sync_target() {
                    None => 0,
                    Some(SyncTarget::WebDav) => 1,
                    Some(SyncTarget::S3) => 2,
                };
                rows::select(
                    cx,
                    row,
                    &options,
                    selected,
                    layout::SelectRowState::new(self.sync_busy, self.open_select_row == Some(row)),
                    None,
                    |this, index, cx| this.set_sync_target(index, cx),
                )
            }
            RowId::SyncAuto => {
                let target = self.sync_target();
                rows::switch(
                    cx,
                    row,
                    self.sync_auto(),
                    target.is_none() || self.sync_busy,
                    None,
                    |this, cx| this.toggle_sync_auto(cx),
                )
            }
            RowId::SyncOpen => {
                let target = self.sync_target();
                rows::nav(
                    cx,
                    row,
                    Some(self.sync_summary()),
                    target.is_none(),
                    target
                        .is_none()
                        .then(|| t(k::SETTINGS_SYNC_OPEN_REQUIRES_TARGET)),
                    |this, cx| this.go(Page::Sync, cx),
                )
            }
            RowId::AboutAutoUpdate => rows::switch(
                cx,
                row,
                self.settings.auto_update_check,
                false,
                None,
                |this, cx| this.toggle_auto_update_check(cx),
            ),
            // One button, two jobs: check until something is found, then
            // install it. Matches the reference cc-switch behaviour, where the
            // same control changes meaning once an update is known.
            RowId::AboutUpdate => {
                let installable = self.update.can_install();
                rows::act(
                    cx,
                    row,
                    if installable {
                        t(k::SETTINGS_UPDATE_INSTALL)
                    } else {
                        t(k::SETTINGS_ABOUT_UPDATE_ACTION)
                    },
                    if installable {
                        ButtonTone::Primary
                    } else {
                        ButtonTone::Neutral
                    },
                    self.update.checking || self.update.installing,
                    Some(self.update_row_description()),
                    move |this, cx| {
                        if installable {
                            this.install_update(cx)
                        } else {
                            this.check_updates(cx)
                        }
                    },
                )
            }
            RowId::AboutRelease => {
                // Primary only once there is something to go and get.
                let tone = if self
                    .update
                    .info
                    .as_ref()
                    .is_some_and(|info| info.has_update)
                {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Neutral
                };
                rows::act(
                    cx,
                    row,
                    t(k::SETTINGS_ACTION_OPEN),
                    tone,
                    false,
                    None,
                    |this, cx| this.open_release_page(cx),
                )
            }
        }
    }

    // ── Language ────────────────────────────────────────────────────────────

    fn selected_language(&self) -> usize {
        let current = self.settings.language.as_deref().and_then(Locale::from_tag);
        options::language_choices()
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0)
    }

    fn set_language(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(choice) = options::language_choices().get(index).copied() else {
            return;
        };
        let tag = choice.map(|locale| locale.tag().to_string());
        self.write_then(
            move |settings| settings.language = tag,
            |this, cx| {
                // Apply, then repaint: `refresh_windows` is what defeats gpui's
                // element-state reuse and re-runs `render` across the whole tree.
                ochub_core::i18n::install(ochub_core::i18n::resolve(
                    this.settings.language.as_deref(),
                ));
                cx.emit(SettingsEvent::LocaleChanged);
                cx.refresh_windows();
            },
            cx,
        );
    }

    // ── Startup and window ──────────────────────────────────────────────────

    fn toggle_launch_on_startup(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.launch_on_startup;
        self.apply_autostart(target, self.settings.silent_startup, true, cx);
    }

    fn toggle_silent_startup(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.silent_startup;
        self.apply_autostart(true, target, false, cx);
    }

    fn apply_autostart(
        &mut self,
        launch_enabled: bool,
        silent_startup: bool,
        update_launch_flag: bool,
        cx: &mut Context<Self>,
    ) {
        if self.settings_busy {
            return;
        }
        self.settings_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    // Register with the OS first: persisted settings must never
                    // claim a login item that does not exist.
                    ochub_core::autostart::set_enabled(launch_enabled, silent_startup)
                        .map_err(|error| error.to_string())?;
                    settings::mutate_settings(move |settings| {
                        if update_launch_flag {
                            settings.launch_on_startup = launch_enabled;
                        } else {
                            settings.silent_startup = silent_startup;
                        }
                    })
                    .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.settings_busy = false;
                match result {
                    Ok(()) => {
                        this.startup_error = None;
                        this.reload(cx);
                    }
                    Err(error) => {
                        this.startup_error = Some(SharedString::from(error));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_keep_running(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.minimize_to_tray_on_close;
        self.write_then(
            move |settings| settings.minimize_to_tray_on_close = target,
            |_this, cx| {
                // This toggle changes when the process may exit.
                crate::apply_quit_mode(cx);
            },
            cx,
        );
    }

    fn toggle_auto_update_check(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.auto_update_check;
        self.write(move |settings| settings.auto_update_check = target, cx);
    }

    fn toggle_tray(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.show_in_tray;
        self.write_then(
            move |settings| settings.show_in_tray = target,
            |this, cx| shell_menu::refresh(&this.app, cx),
            cx,
        );
    }

    // ── Data directory ──────────────────────────────────────────────────────

    fn data_dir_description(&self) -> SharedString {
        match app_store::get_app_config_dir_override() {
            Some(path) => SharedString::from(path.to_string_lossy().to_string()),
            None => SharedString::from(tf!(
                k::SETTINGS_DATA_DIR_DEFAULT,
                path = ochub_core::paths::get_app_config_dir().display()
            )),
        }
    }

    /// The native directory picker *is* the commit: it cannot return anything
    /// that is not a directory, so the "is this a valid path?" rule and the
    /// form that used to host it both disappear.
    fn pick_data_dir(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t(k::SETTINGS_DATA_DIR_PROMPT)),
        });
        cx.spawn(async move |this, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = path else {
                return;
            };
            this.update(cx, |this, cx| {
                this.apply_data_dir(Some(path.to_string_lossy().to_string()), cx);
            })
            .ok();
        })
        .detach();
    }

    fn reset_data_dir(&mut self, cx: &mut Context<Self>) {
        self.apply_data_dir(None, cx);
    }

    // ── cc-switch import ────────────────────────────────────────────────────

    /// What the detected cc-switch install holds. The row is only drawn when a
    /// source exists, so falling back to the generic description is a formality.
    fn ccswitch_description(&self) -> Option<SharedString> {
        let source = self.ccswitch_source.as_ref()?;
        Some(SharedString::from(tf!(
            k::SETTINGS_DATA_CCSWITCH_FOUND,
            path = ochub_core::paths::abbreviate_home(&source.path),
            providers = source.providers,
            mcp = source.mcp_servers
        )))
    }

    /// Import off the main thread — a cc-switch database can carry months of
    /// usage history, and the settings page has to stay responsive.
    pub(super) fn start_ccswitch_import(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.ccswitch_source.clone() else {
            return;
        };
        if self.ccswitch_busy {
            return;
        }
        self.ccswitch_busy = true;
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = app.db.import_from_ccswitch_source(&source);
                    if result.is_ok() {
                        if let Err(error) = app.db.set_ccswitch_import_decision("imported") {
                            log::warn!("保存 cc-switch 导入选择失败: {error}");
                        }
                    }
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                this.ccswitch_busy = false;
                match result {
                    Ok(report) => {
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(
                                k::SETTINGS_DATA_CCSWITCH_SUCCEEDED,
                                rows = report.total_rows()
                            ),
                            cx,
                        );
                        cx.emit(SettingsEvent::DataImported);
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_DATA_CCSWITCH_FAILED, error = err),
                        cx,
                    ),
                }
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_data_dir(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.settings_busy {
            return;
        }
        self.settings_busy = true;
        cx.notify();
        // The override lives in its own bootstrap file, not in settings.json,
        // so this failure has no field to attach to — it is plain I/O.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    app_store::set_app_config_dir_to_store(path.as_deref())
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.settings_busy = false;
                match result {
                    Ok(()) => {
                        this.reload(cx);
                        this.set_status(
                            NotificationLevel::Success,
                            t(k::SETTINGS_CONFIG_DIR_SAVED),
                            cx,
                        );
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_CONFIG_DIR_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    // ── Updates ─────────────────────────────────────────────────────────────

    fn update_row_description(&self) -> SharedString {
        if self.update.installing {
            return match self.update.percent() {
                // The last progress callback fires before the signature check,
                // so a stuck "100%" would look like a hang. Name what is
                // actually happening instead.
                Some(100) => t(k::SETTINGS_UPDATE_VERIFYING),
                Some(percent) => SharedString::from(tf!(
                    k::SETTINGS_UPDATE_DOWNLOADING,
                    percent = percent.to_string()
                )),
                // Nothing downloaded yet, or the server sent no content-length.
                None => t(k::SETTINGS_UPDATE_INSTALLING),
            };
        }
        if self.update.checking {
            return t(k::SETTINGS_UPDATE_CHECKING);
        }
        // An update exists but this install cannot apply it itself: say why
        // here rather than offering a button that opens a browser instead.
        if let Some(info) = &self.update.info {
            if info.has_update && !info.can_self_install {
                return SharedString::from(tf!(
                    k::SETTINGS_UPDATE_MANUAL_ONLY,
                    channel = info.install_channel.clone()
                ));
            }
        }
        match &self.update.info {
            Some(info) if info.has_update => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_UPGRADABLE,
                latest = info
                    .latest_version
                    .as_deref()
                    .unwrap_or(raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION)),
                current = info.current_version
            )),
            Some(info) => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_UP_TO_DATE,
                version = info.current_version
            )),
            None => SharedString::from(tf!(
                k::SETTINGS_ABOUT_UPDATE_CURRENT,
                version = env!("CARGO_PKG_VERSION")
            )),
        }
    }

    fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.update.checking {
            return;
        }
        self.update.checking = true;
        self.set_status(NotificationLevel::Info, t(k::SETTINGS_UPDATE_CHECKING), cx);

        let task = cx.background_spawn(crate::core_async::run(
            ochub_core::services::update::check_for_updates(None),
        ));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.update.checking = false;
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
                        this.update.info = Some(info);
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_CHECK_FAILED, error = err),
                        cx,
                    ),
                }
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Download, verify, install, and restart.
    ///
    /// The download and signature check run on a background thread; only when
    /// they succeed does anything get replaced. Quitting is left to
    /// [`gpui::App::quit`] rather than `process::exit` so GPUI's own shutdown
    /// handlers still run — the window bounds saved in `main.rs` would
    /// otherwise be lost on every update, and the single-instance lock and
    /// gateway port would be released late enough to race the relaunch.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        if self.update.installing {
            return;
        }
        self.update.installing = true;
        self.update.progress = None;
        self.set_status(
            NotificationLevel::Info,
            t(k::SETTINGS_UPDATE_INSTALLING),
            cx,
        );

        // Progress arrives from the download thread; `report` hops back to the
        // UI thread to touch view state.
        let (progress_tx, mut progress_rx) =
            futures::channel::mpsc::unbounded::<(u64, Option<u64>)>();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(update) = progress_rx.next().await {
                this.update(cx, |this, cx| {
                    this.update.progress = Some(update);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();

        let task = cx.background_spawn(crate::core_async::run(async move {
            let report = Box::new(move |done: u64, total: Option<u64>| {
                let _ = progress_tx.unbounded_send((done, total));
            });
            ochub_core::services::update::install::prepare(None, Some(report)).await
        }));

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            this.update(cx, |this, cx| {
                this.update.installing = false;
                this.update.progress = None;
                match prepared {
                    Ok(Some(prepared)) => {
                        let version = prepared.version.clone();
                        match ochub_core::services::update::apply_and_arm_restart(prepared) {
                            Ok(()) => {
                                this.set_status(
                                    NotificationLevel::Success,
                                    tf!(k::SETTINGS_UPDATE_INSTALLED, version = version),
                                    cx,
                                );
                                // The relaunch watcher is already waiting on
                                // this PID, so a clean quit is all that is
                                // left. Give the status line a moment first.
                                cx.spawn(async move |_this, cx| {
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(600))
                                        .await;
                                    cx.update(|cx| cx.quit());
                                })
                                .detach();
                            }
                            Err(err) => this.set_status(
                                NotificationLevel::Error,
                                tf!(k::SETTINGS_UPDATE_INSTALL_FAILED, error = err),
                                cx,
                            ),
                        }
                    }
                    // The manifest turned out not to be newer after all.
                    Ok(None) => this.set_status(
                        NotificationLevel::Success,
                        tf!(
                            k::SETTINGS_UPDATE_UP_TO_DATE,
                            current = ochub_core::services::update::current_version()
                        ),
                        cx,
                    ),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_INSTALL_FAILED, error = err),
                        cx,
                    ),
                }
                this.root_list.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_release_page(&mut self, cx: &mut Context<Self>) {
        if self.settings_busy {
            return;
        }
        let url = self
            .update
            .info
            .as_ref()
            .map(|info| info.release_url.clone())
            .unwrap_or_else(|| ochub_core::services::latest_release_url(None));
        self.settings_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { open_url(&url) }).await;
            this.update(cx, |this, cx| {
                this.settings_busy = false;
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        t(k::SETTINGS_UPDATE_RELEASE_OPENED),
                        cx,
                    ),
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::SETTINGS_UPDATE_RELEASE_OPEN_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    // ── Sync destination (the two rows that stay on the root page) ──────────

    /// The active destination. Exclusive by construction: two independent auto
    /// loops pushing snapshots of one database to two remotes is a race, not
    /// redundancy, so when a legacy config has both enabled the more recently
    /// synced one wins and nothing is written until the select is touched.
    pub(super) fn sync_target(&self) -> Option<SyncTarget> {
        let webdav = self
            .settings
            .webdav_sync
            .as_ref()
            .filter(|sync| sync.enabled);
        let s3 = self.settings.s3_sync.as_ref().filter(|sync| sync.enabled);
        match (webdav, s3) {
            (Some(_), None) => Some(SyncTarget::WebDav),
            (None, Some(_)) => Some(SyncTarget::S3),
            (Some(webdav), Some(s3)) => {
                if s3.status.last_sync_at.unwrap_or(i64::MIN)
                    > webdav.status.last_sync_at.unwrap_or(i64::MIN)
                {
                    Some(SyncTarget::S3)
                } else {
                    Some(SyncTarget::WebDav)
                }
            }
            (None, None) => None,
        }
    }

    pub(super) fn warn_if_dual_target(&mut self, cx: &mut Context<Self>) {
        if self.warned_dual_target {
            return;
        }
        let both = self
            .settings
            .webdav_sync
            .as_ref()
            .is_some_and(|sync| sync.enabled)
            && self
                .settings
                .s3_sync
                .as_ref()
                .is_some_and(|sync| sync.enabled);
        if !both {
            return;
        }
        self.warned_dual_target = true;
        let provider = self
            .sync_target()
            .map(SyncTarget::provider)
            .unwrap_or_default();
        self.set_status(
            NotificationLevel::Warning,
            tf!(k::SETTINGS_SYNC_BOTH_ENABLED, provider = provider),
            cx,
        );
    }

    pub(super) fn sync_auto(&self) -> bool {
        match self.sync_target() {
            Some(SyncTarget::WebDav) => self
                .settings
                .webdav_sync
                .as_ref()
                .is_some_and(|sync| sync.auto_sync),
            Some(SyncTarget::S3) => self
                .settings
                .s3_sync
                .as_ref()
                .is_some_and(|sync| sync.auto_sync),
            None => false,
        }
    }

    /// One line of "what is configured in there": the host or the bucket, or
    /// an explicit 未配置 when the destination is picked but empty.
    fn sync_summary(&self) -> SharedString {
        let configured = match self.sync_target() {
            Some(SyncTarget::WebDav) => self
                .settings
                .webdav_sync
                .as_ref()
                .map(|sync| sync.base_url.clone()),
            Some(SyncTarget::S3) => self
                .settings
                .s3_sync
                .as_ref()
                .map(|sync| sync.bucket.clone()),
            None => None,
        };
        match configured {
            Some(value) if !value.trim().is_empty() => SharedString::from(value),
            _ => t(k::SETTINGS_SYNC_OPEN_UNSET),
        }
    }

    fn set_sync_target(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.sync_busy {
            return;
        }
        let target = match index {
            1 => Some(SyncTarget::WebDav),
            2 => Some(SyncTarget::S3),
            _ => None,
        };
        // Touching the select resolves a legacy both-enabled config, so the
        // warning has nothing left to say.
        self.warned_dual_target = true;
        self.write(
            move |settings| match target {
                Some(SyncTarget::WebDav) => {
                    settings
                        .webdav_sync
                        .get_or_insert_with(Default::default)
                        .enabled = true;
                    if let Some(s3) = settings.s3_sync.as_mut() {
                        s3.enabled = false;
                    }
                }
                Some(SyncTarget::S3) => {
                    settings
                        .s3_sync
                        .get_or_insert_with(Default::default)
                        .enabled = true;
                    if let Some(webdav) = settings.webdav_sync.as_mut() {
                        webdav.enabled = false;
                    }
                }
                None => {
                    if let Some(webdav) = settings.webdav_sync.as_mut() {
                        webdav.enabled = false;
                    }
                    if let Some(s3) = settings.s3_sync.as_mut() {
                        s3.enabled = false;
                    }
                }
            },
            cx,
        );
    }

    fn toggle_sync_auto(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.sync_target() else {
            return;
        };
        let next = !self.sync_auto();
        self.write(
            move |settings| match target {
                SyncTarget::WebDav => {
                    if let Some(sync) = settings.webdav_sync.as_mut() {
                        sync.auto_sync = next;
                    }
                }
                SyncTarget::S3 => {
                    if let Some(sync) = settings.s3_sync.as_mut() {
                        sync.auto_sync = next;
                    }
                }
            },
            cx,
        );
    }
}

pub(super) fn open_url(url: &str) -> Result<(), String> {
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
