//! The root page: six groups, fifteen-odd rows, no text input and no Save.
//!
//! Every writer in here goes straight to [`settings::mutate_settings`] and then
//! re-reads, so the control on screen always shows what is on disk. Side
//! effects fire only where they matter — the tray refresh on the tray toggle,
//! the quit mode on keep-running, the locale install on language — rather than
//! on every save the way the old `persist()` did.

use gpui::{AnyElement, Context, PathPromptOptions, SharedString, Window, div, prelude::*};
use ochub_core::app_store;
use ochub_core::i18n::Locale;
use ochub_core::settings;

use crate::components::{self, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::product_ui::common as common_screen;
use crate::shell_menu;
use crate::tf;

use super::options;
use super::rows;
use super::search::RowId;
use super::sync::SyncTarget;
use super::{Confirm, Page, SettingsEvent, SettingsView};

impl SettingsView {
    pub(super) fn render_root(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        common_screen::page(
            t(k::SETTINGS_PAGE_TITLE),
            div()
                .flex_none()
                .w(gpui::px(220.))
                .child(self.search.clone()),
            layout::virtual_body(
                "settings-body",
                gpui::list(
                    self.root_list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_root_block(ix, window, cx)),
                ),
                &self.root_list,
            ),
        )
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
                rows::group_block(t(k::SETTINGS_GENERAL_TITLE), vec![language, terminal])
            }
            1 => {
                let mut group = vec![
                    self.render_row(RowId::StartupLogin, cx),
                    self.render_row(RowId::StartupHidden, cx),
                    self.render_row(RowId::WindowKeepRunning, cx),
                    self.render_row(RowId::WindowTrayResident, cx),
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
                rows::group_block(t(k::SETTINGS_STARTUP_TITLE), group)
            }
            2 => {
                let open = self.render_row(RowId::AppsOpen, cx);
                rows::group_block(t(k::SETTINGS_APPS_TITLE), vec![open])
            }
            3 => {
                let mut group = vec![self.render_row(RowId::DataDir, cx)];
                if self.data_dir_has_override {
                    group.push(self.render_row(RowId::DataDirReset, cx));
                }
                if self.ccswitch_source.is_some() {
                    group.push(self.render_row(RowId::DataCcswitchImport, cx));
                }
                group.push(self.render_row(RowId::BackupInterval, cx));
                group.push(self.render_row(RowId::BackupRetain, cx));
                group.push(self.render_row(RowId::SessionIndex, cx));
                if self.settings.session_index_enabled {
                    group.push(self.render_row(RowId::SessionIndexReclaim, cx));
                }
                // Both act on a file, so neither exists without one.
                if self.session_index_stats.is_some() {
                    group.push(self.render_row(RowId::SessionIndexReclaimNow, cx));
                    group.push(self.render_row(RowId::SessionIndexDelete, cx));
                }
                rows::group_block(t(k::SETTINGS_DATA_TITLE), group)
            }
            4 => {
                let group = vec![
                    self.render_row(RowId::SyncTarget, cx),
                    self.render_row(RowId::SyncAuto, cx),
                    self.render_row(RowId::SyncOpen, cx),
                ];
                rows::group_block(t(k::SETTINGS_SYNC_TITLE), group)
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
            RowId::WindowTrayResident,
            RowId::WindowTray,
            RowId::AppsOpen,
            RowId::DataDir,
        ];
        if self.data_dir_has_override {
            rows.push(RowId::DataDirReset);
        }
        if self.ccswitch_source.is_some() {
            rows.push(RowId::DataCcswitchImport);
        }
        rows.extend([
            RowId::BackupInterval,
            RowId::BackupRetain,
            RowId::SessionIndex,
        ]);
        if self.settings.session_index_enabled {
            rows.push(RowId::SessionIndexReclaim);
        }
        if self.session_index_stats.is_some() {
            rows.push(RowId::SessionIndexReclaimNow);
            rows.push(RowId::SessionIndexDelete);
        }
        rows.extend([RowId::SyncTarget, RowId::SyncAuto, RowId::SyncOpen]);
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
            RowId::WindowTrayResident => {
                let blocked = !self.settings.minimize_to_tray_on_close;
                rows::switch(
                    cx,
                    row,
                    self.settings.tray_resident_mode,
                    blocked,
                    blocked.then(|| t(k::SETTINGS_BASIC_TRAY_RESIDENT_REQUIRES_KEEP_RUNNING)),
                    |this, cx| this.toggle_tray_resident(cx),
                )
            }
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
                        this.write_workspace(
                            vec![("backupIntervalHours", serde_json::json!(hours))],
                            |_this, _cx| {},
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
                        this.write_workspace(
                            vec![("backupRetainCount", serde_json::json!(count))],
                            |_this, _cx| {},
                            cx,
                        );
                    },
                )
            }
            RowId::SessionIndex => {
                // The static description explains the feature; once an index
                // exists, what the user actually needs to see is its cost.
                let readout = self.session_index_stats.as_ref().map(|stats| {
                    SharedString::from(tf!(
                        k::SETTINGS_SESSION_INDEX_USAGE,
                        size = components::format_bytes(stats.bytes),
                        sessions = stats.sessions.to_string()
                    ))
                });
                rows::switch(
                    cx,
                    row,
                    self.settings.session_index_enabled,
                    self.settings_busy,
                    readout,
                    |this, cx| this.toggle_session_index(cx),
                )
            }
            RowId::SessionIndexReclaim => rows::switch(
                cx,
                row,
                self.settings.session_index_auto_reclaim,
                self.settings_busy,
                None,
                |this, cx| this.toggle_session_index_auto_reclaim(cx),
            ),
            RowId::SessionIndexReclaimNow => {
                let reclaimable = self
                    .session_index_stats
                    .as_ref()
                    .map(|stats| stats.reclaimable_bytes)
                    .unwrap_or(0);
                rows::act(
                    cx,
                    row,
                    t(k::SETTINGS_SESSION_INDEX_RECLAIM_NOW_ACTION),
                    ButtonTone::Neutral,
                    self.session_index_busy || reclaimable == 0,
                    Some(SharedString::from(tf!(
                        k::SETTINGS_SESSION_INDEX_RECLAIMABLE,
                        size = components::format_bytes(reclaimable)
                    ))),
                    |this, cx| this.reclaim_session_index(cx),
                )
            }
            RowId::SessionIndexDelete => rows::act(
                cx,
                row,
                t(k::SETTINGS_SESSION_INDEX_DELETE_ACTION),
                ButtonTone::Danger,
                self.session_index_busy,
                None,
                |this, cx| this.delete_session_index(cx),
            ),
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
                crate::install_locale(ochub_core::i18n::resolve(this.settings.language.as_deref()));
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
            move |settings| {
                settings.minimize_to_tray_on_close = target;
                if !target {
                    settings.tray_resident_mode = false;
                }
            },
            |this, cx| {
                // This toggle changes when the process may exit.
                crate::apply_quit_mode(cx);
                shell_menu::refresh(&this.app, cx);
            },
            cx,
        );
    }

    fn toggle_tray_resident(&mut self, cx: &mut Context<Self>) {
        let target = !self.settings.tray_resident_mode;
        self.write_then(
            move |settings| settings.tray_resident_mode = target,
            |this, cx| shell_menu::refresh(&this.app, cx),
            cx,
        );
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
        if !self.data_dir_value.is_empty() {
            return self.data_dir_value.clone();
        }
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
        if self.workspace_remote {
            let current = self.data_dir_value.clone();
            self.data_dir_input
                .update(cx, |input, cx| input.set_content(current, cx));
            self.data_dir_editing = true;
            cx.notify();
            return;
        }
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
        if self.ccswitch_source.is_none() || !self.workspace_available {
            return;
        }
        if self.ccswitch_busy {
            return;
        }
        self.ccswitch_busy = true;
        cx.notify();

        let backend = self.backend.clone();
        let generation = self.workspace_generation;
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(async move { backend.import_ccswitch().await }).await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
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

    pub(super) fn apply_data_dir(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.settings_busy || !self.workspace_available {
            return;
        }
        self.settings_busy = true;
        let backend = self.backend.clone();
        let generation = self.workspace_generation;
        cx.notify();
        // The override lives in its own bootstrap file, not in settings.json,
        // so this failure has no field to attach to — it is plain I/O.
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                match path {
                    Some(path) => backend.set_data_dir(&path).await,
                    None => backend.reset_data_dir().await,
                }
            })
            .await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.settings_busy = false;
                match result {
                    Ok(_) => {
                        this.data_dir_editing = false;
                        this.load_workspace_metadata(cx);
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
        let mut values = Vec::new();
        match target {
            Some(SyncTarget::WebDav) => {
                if self.settings.webdav_sync.is_some() {
                    values.push(("webdavSync.enabled", serde_json::json!(true)));
                } else {
                    let sync = ochub_core::settings::WebDavSyncSettings {
                        enabled: true,
                        ..Default::default()
                    };
                    values.push(("webdavSync", serde_json::json!(sync)));
                }
                if self.settings.s3_sync.is_some() {
                    values.push(("s3Sync.enabled", serde_json::json!(false)));
                }
            }
            Some(SyncTarget::S3) => {
                if self.settings.s3_sync.is_some() {
                    values.push(("s3Sync.enabled", serde_json::json!(true)));
                } else {
                    let sync = ochub_core::settings::S3SyncSettings {
                        enabled: true,
                        ..Default::default()
                    };
                    values.push(("s3Sync", serde_json::json!(sync)));
                }
                if self.settings.webdav_sync.is_some() {
                    values.push(("webdavSync.enabled", serde_json::json!(false)));
                }
            }
            None => {
                if self.settings.webdav_sync.is_some() {
                    values.push(("webdavSync.enabled", serde_json::json!(false)));
                }
                if self.settings.s3_sync.is_some() {
                    values.push(("s3Sync.enabled", serde_json::json!(false)));
                }
            }
        }
        self.write_workspace(values, |_this, _cx| {}, cx);
    }

    fn toggle_sync_auto(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.sync_target() else {
            return;
        };
        let next = !self.sync_auto();
        self.write_workspace(
            vec![(
                match target {
                    SyncTarget::WebDav => "webdavSync.autoSync",
                    SyncTarget::S3 => "s3Sync.autoSync",
                },
                serde_json::json!(next),
            )],
            |_this, _cx| {},
            cx,
        );
    }
}
