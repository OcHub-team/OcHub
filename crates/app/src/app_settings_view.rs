//! Per-app settings panel.
//!
//! Settings that affect exactly one managed tool — Codex's session-history
//! unification and each CLI's config directory — used to be
//! dumped into the single global Settings page. They belong with the app they
//! configure, so this panel renders just the selected app's settings and is
//! opened from a gear in that app's provider-list header. The values still live
//! in the global [`AppSettings`];
//! only their *placement* is app-scoped.

use std::sync::Arc;

use gpui::{Context, Entity, ScrollHandle, SharedString, Window, div, prelude::*};
use ochub_core::settings::{self, AppSettings};
use ochub_core::{AppId, AppState, AppType};
use serde_json::Value;

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::remote::{WorkspaceBackend, WorkspaceBackendError};
use crate::text_input::TextInput;
use crate::tf;

/// Emitted when the user dismisses the panel (back to the provider list).
pub enum AppSettingsEvent {
    Close,
}

impl gpui::EventEmitter<AppSettingsEvent> for AppSettingsView {}

pub struct AppSettingsView {
    backend: WorkspaceBackend,
    workspace_available: bool,
    app_id: AppId,
    app_type: Option<AppType>,
    settings: AppSettings,
    /// The app's config-dir override input (None for apps without one).
    config_dir: Option<Entity<TextInput>>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    scroll_handle: ScrollHandle,
    saving: bool,
    workspace_generation: u64,
}

/// Whether an app has any app-scoped settings worth a gear button.
pub fn app_has_settings(app: &AppId) -> bool {
    match AppType::from_app_id(app) {
        Some(builtin) => builtin_config_dir_placeholder(builtin).is_some(),
        None => ochub_core::plugin::get_plugin(app)
            .is_some_and(|plugin| plugin.provider_config().is_some()),
    }
}

enum SettingsMutation {
    Setting(&'static str, Value),
    ConfigDirectory(AppId, Option<String>),
}

impl AppSettingsView {
    pub(crate) fn leave_state(&self, cx: &gpui::App) -> crate::interaction::LeaveState {
        crate::interaction::LeaveState::new(
            self.config_dir.as_ref().is_some_and(|input| {
                input.read(cx).content().trim()
                    != read_config_dir(&self.settings, &self.app_id)
                        .unwrap_or_default()
                        .trim()
            }),
            self.saving,
        )
    }

    pub(crate) fn discard_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(input) = self.config_dir.as_ref() {
            input.update(cx, |input, cx| {
                input.set_content(
                    read_config_dir(&self.settings, &self.app_id).unwrap_or_default(),
                    cx,
                )
            });
        }
        cx.notify();
    }

    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.config_dir.is_some() && self.workspace_available {
            self.save_config_dir(cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AppSettingsEvent::Close);
    }

    pub fn new(app: Arc<AppState>, app_id: AppId, cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let config_dir = Self::make_config_dir_input(&app_id, &settings, false, cx);
        let app_type = AppType::from_app_id(&app_id);
        Self {
            backend: WorkspaceBackend::local(app),
            workspace_available: true,
            app_id,
            app_type,
            settings,
            config_dir,
            status: None,
            status_level: None,
            scroll_handle: ScrollHandle::new(),
            saving: false,
            workspace_generation: 0,
        }
    }

    /// Re-point the panel at a different app (called when the gear is opened).
    pub fn reload_for(&mut self, app_id: AppId, cx: &mut Context<Self>) {
        if app_id == self.app_id {
            self.load_settings(cx);
            return;
        }
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.app_type = AppType::from_app_id(&app_id);
        self.app_id = app_id;
        self.saving = false;
        self.config_dir =
            Self::make_config_dir_input(&self.app_id, &self.settings, self.backend.is_remote(), cx);
        self.status = None;
        self.status_level = None;
        self.load_settings(cx);
    }

    pub fn set_workspace(&mut self, backend: WorkspaceBackend, cx: &mut Context<Self>) {
        if self.backend.same_workspace(&backend) {
            self.workspace_generation = self.workspace_generation.wrapping_add(1);
            self.backend = backend;
            self.workspace_available = true;
            self.load_settings(cx);
            return;
        }
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.backend = backend;
        self.workspace_available = true;
        self.settings = AppSettings::default();
        self.config_dir =
            Self::make_config_dir_input(&self.app_id, &self.settings, self.backend.is_remote(), cx);
        self.saving = false;
        cx.notify();
    }

    pub fn set_workspace_unavailable(&mut self, cx: &mut Context<Self>) {
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.workspace_available = false;
        self.saving = false;
        cx.notify();
    }

    fn load_settings(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            return;
        }
        let backend = self.backend.clone();
        let generation = self.workspace_generation;
        let initial_dir = self
            .config_dir
            .as_ref()
            .map(|input| input.read(cx).content());
        let was_dirty = self.leave_state(cx) == crate::interaction::LeaveState::Dirty;
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let settings = backend.settings().await?;
                serde_json::from_value::<AppSettings>(settings).map_err(WorkspaceBackendError::from)
            })
            .await
            .map_err(|error| error.to_string());
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                match result {
                    Ok(settings) => {
                        this.settings = settings;
                        // A delayed initial read must not replace text typed
                        // while it was in flight or recreate the focused input.
                        if !was_dirty
                            && let Some(input) = this.config_dir.as_ref()
                            && initial_dir.as_ref() == Some(&input.read(cx).content())
                        {
                            let stored =
                                read_config_dir(&this.settings, &this.app_id).unwrap_or_default();
                            input.update(cx, |input, cx| input.set_content(stored, cx));
                        }
                    }
                    Err(error) => {
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::APP_SETTINGS_STATUS_SAVE_FAILED, error = error),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn make_config_dir_input(
        app_id: &AppId,
        settings: &AppSettings,
        remote: bool,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextInput>> {
        let placeholder = config_dir_placeholder(app_id, remote)?;
        let current = read_config_dir(settings, app_id).unwrap_or_default();
        Some(cx.new(|cx| {
            let mut input = TextInput::new(cx, placeholder);
            input.set_content(current, cx);
            input
        }))
    }

    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        if let Some(input) = self.config_dir.as_ref()
            && let Some(placeholder) =
                config_dir_placeholder(&self.app_id, self.backend.is_remote())
        {
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        cx.notify();
    }

    /// Every toast carries its severity explicitly; leaving the level unset
    /// would make the toast host guess it from the wording, which breaks as
    /// soon as the wording is translated. Callers redraw themselves.
    fn set_status(&mut self, level: NotificationLevel, text: impl Into<SharedString>) {
        self.status = Some(text.into());
        self.status_level = Some(level);
    }

    fn persist_mutation(
        &mut self,
        mutation: SettingsMutation,
        success: SharedString,
        cx: &mut Context<Self>,
    ) {
        if self.saving || !self.workspace_available {
            return;
        }
        let backend = self.backend.clone();
        // Saving supersedes an initial settings read for the same panel.
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        let generation = self.workspace_generation;
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                match mutation {
                    SettingsMutation::Setting(path, value) => {
                        backend.set_setting(path, value).await?;
                    }
                    SettingsMutation::ConfigDirectory(app, path) => {
                        backend.set_app_config_dir(&app, path).await?;
                    }
                }
                let settings = backend.settings().await?;
                serde_json::from_value::<AppSettings>(settings).map_err(WorkspaceBackendError::from)
            })
            .await
            .map_err(|error| error.to_string());
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
                this.saving = false;
                match result {
                    Ok(stored) => {
                        this.settings = stored;
                        // The input is the user's draft. This applies both to
                        // unrelated toggle saves and typing after a directory
                        // save was submitted: neither may overwrite it.
                        this.set_status(NotificationLevel::Success, success);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::APP_SETTINGS_STATUS_SAVE_FAILED, error = error),
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle(&mut self, toggle: AppToggle, cx: &mut Context<Self>) {
        if self.saving || !self.workspace_available {
            return;
        }
        let current = (toggle.get)(&self.settings);
        self.persist_mutation(
            SettingsMutation::Setting(toggle.path, Value::Bool(!current)),
            t(k::APP_SETTINGS_STATUS_SAVED),
            cx,
        );
    }

    fn save_config_dir(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.config_dir.as_ref() else {
            return;
        };
        let entered = input.read(cx).content().trim().to_string();
        let value = if entered.is_empty() {
            None
        } else {
            Some(entered)
        };
        let mutation = match self.app_type {
            Some(app) => SettingsMutation::Setting(
                config_dir_path(app),
                serde_json::to_value(value).unwrap_or(Value::Null),
            ),
            None => SettingsMutation::ConfigDirectory(self.app_id.clone(), value),
        };
        self.persist_mutation(
            mutation,
            // Restart is a recommendation, not a caveat that makes this a
            // warning.
            t(k::APP_SETTINGS_STATUS_DIR_SAVED),
            cx,
        );
    }

    fn render_toggle_row(
        &self,
        toggle: AppToggle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let on = (toggle.get)(&self.settings);
        let activate = cx.listener(move |this, _: &(), _window, cx| this.toggle(toggle, cx));
        components::checkbox(
            toggle.id,
            toggle.label,
            on,
            self.saving || !self.workspace_available,
            move |window, cx| activate(&(), window, cx),
        )
    }

    fn render_config_dir(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let input = self.config_dir.as_ref()?;
        let save_button = if self.saving {
            components::busy_button(
                "app-settings-save-dir",
                t(k::APP_SETTINGS_CONFIG_DIR_SAVE),
                ButtonTone::Primary,
                ButtonSize::Sm,
                true,
            )
            .into_any_element()
        } else if !self.workspace_available {
            components::disabled_button(
                "app-settings-save-dir",
                t(k::APP_SETTINGS_CONFIG_DIR_SAVE),
                ButtonTone::Primary,
                ButtonSize::Sm,
                true,
            )
            .into_any_element()
        } else {
            components::button(
                "app-settings-save-dir",
                t(k::APP_SETTINGS_CONFIG_DIR_SAVE),
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.save_config_dir(cx);
            }))
            .into_any_element()
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .child(layout::section_header(
                    t(k::APP_SETTINGS_CONFIG_DIR_TITLE),
                    None,
                ))
                .child(
                    components::card()
                        .gap_3()
                        .child(input.clone())
                        .child(div().flex().flex_row().justify_end().child(save_button)),
                )
                .into_any_element(),
        )
    }
}

impl Render for AppSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toggles = self
            .app_type
            .map(|app| app_toggles(app, &self.settings))
            .unwrap_or_default();

        let header = layout::page_header(
            SharedString::from(tf!(
                k::APP_SETTINGS_HEADER_TITLE,
                app = app_label(&self.app_id)
            )),
            None,
        )
        .child(
            components::button(
                "app-settings-back",
                t(k::APP_SETTINGS_HEADER_BACK),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|_this, _event, _window, cx| {
                cx.emit(AppSettingsEvent::Close);
            })),
        );

        let mut column = layout::content_column();
        if !toggles.is_empty() {
            column = column.child(layout::section_header(
                t(k::APP_SETTINGS_BEHAVIOR_TITLE),
                None,
            ));
            let rows: Vec<gpui::AnyElement> = toggles
                .into_iter()
                .map(|toggle| self.render_toggle_row(toggle, cx).into_any_element())
                .collect();
            column = column.child(layout::group(rows));
        }
        if let Some(dir) = self.render_config_dir(cx) {
            column = column.child(dir);
        } else if self.app_type.is_none() && self.backend.is_remote() {
            column = column.child(components::field_error(t(
                k::APP_SETTINGS_CONFIG_DIR_REMOTE_UNSUPPORTED,
            )));
        }

        layout::page().child(header).child(layout::scroll_body(
            "app-settings-body",
            &self.scroll_handle,
            column,
        ))
    }
}

// ---- per-app setting definitions -------------------------------------------

/// A single app-scoped boolean setting, with accessors into [`AppSettings`].
#[derive(Clone, Copy)]
struct AppToggle {
    id: &'static str,
    path: &'static str,
    label: &'static str,
    get: fn(&AppSettings) -> bool,
}

fn app_toggles(app: AppType, settings: &AppSettings) -> Vec<AppToggle> {
    match app {
        // Legacy Claude booleans remain readable in AppSettings for config
        // compatibility. No runtime behavior consumes them, so they are not
        // offered as controls that would appear to perform an action.
        AppType::Codex => {
            let mut toggles = vec![AppToggle {
                id: "app-set-codex-unify-history",
                path: "unifyCodexSessionHistory",
                label: raw(k::APP_SETTINGS_CODEX_UNIFY_HISTORY_LABEL),
                get: |s| s.unify_codex_session_history,
            }];
            if settings.unify_codex_session_history {
                toggles.push(AppToggle {
                    id: "app-set-codex-migrate-history",
                    path: "unifyCodexMigrateExisting",
                    label: raw(k::APP_SETTINGS_CODEX_MIGRATE_HISTORY_LABEL),
                    get: |s| s.unify_codex_migrate_existing.unwrap_or(false),
                });
            }
            toggles
        }
        _ => Vec::new(),
    }
}

/// The placeholder + description for an app's config-dir override, or `None`.
fn builtin_config_dir_placeholder(app: AppType) -> Option<&'static str> {
    match app {
        AppType::Claude => Some("~/.claude"),
        AppType::Codex => Some("~/.codex"),
        AppType::GrokBuild => Some("~/.grok"),
        AppType::KimiCode => Some("~/.kimi-code"),
        AppType::OpenCode => Some("~/.config/opencode"),
        AppType::OpenClaw => Some("~/.openclaw"),
        AppType::Hermes => Some("~/.hermes"),
        AppType::ClaudeDesktop | AppType::CherryStudio => None,
    }
}

fn config_dir_placeholder(app: &AppId, remote: bool) -> Option<SharedString> {
    match AppType::from_app_id(app) {
        Some(builtin) => builtin_config_dir_placeholder(builtin).map(SharedString::new_static),
        None if !remote && app_has_settings(app) => {
            Some(t(k::APP_SETTINGS_CONFIG_DIR_PLUGIN_DEFAULT))
        }
        None => None,
    }
}

fn read_config_dir(settings: &AppSettings, app: &AppId) -> Option<String> {
    match AppType::from_app_id(app) {
        Some(AppType::Claude) => settings.claude_config_dir.clone(),
        Some(AppType::Codex) => settings.codex_config_dir.clone(),
        Some(AppType::GrokBuild) => settings.grokbuild_config_dir.clone(),
        Some(AppType::KimiCode) => settings.kimi_code_config_dir.clone(),
        Some(AppType::OpenCode) => settings.opencode_config_dir.clone(),
        Some(AppType::OpenClaw) => settings.openclaw_config_dir.clone(),
        Some(AppType::Hermes) => settings.hermes_config_dir.clone(),
        Some(AppType::ClaudeDesktop | AppType::CherryStudio) => None,
        None => settings
            .app_config_dirs
            .as_ref()
            .and_then(|dirs| dirs.get(app.as_str()))
            .cloned(),
    }
}

fn config_dir_path(app: AppType) -> &'static str {
    match app {
        AppType::Claude => "claudeConfigDir",
        AppType::Codex => "codexConfigDir",
        AppType::GrokBuild => "grokbuildConfigDir",
        AppType::KimiCode => "kimiCodeConfigDir",
        AppType::OpenCode => "opencodeConfigDir",
        AppType::OpenClaw => "openclawConfigDir",
        AppType::Hermes => "hermesConfigDir",
        AppType::ClaudeDesktop | AppType::CherryStudio => {
            unreachable!("this app has no config directory override")
        }
    }
}

fn app_label(app: &AppId) -> gpui::SharedString {
    ochub_core::plugin::get_plugin(app)
        .map(|plugin| SharedString::from(plugin.display_name().to_string()))
        .unwrap_or_else(|| SharedString::from(app.to_string()))
}

crate::notifications::impl_status_toasts_leveled!(AppSettingsView);
