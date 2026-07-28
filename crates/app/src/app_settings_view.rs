//! Per-app settings panel.
//!
//! Settings that affect exactly one managed tool — Claude Code's plugin
//! integration / onboarding skip, Codex's official-OAuth preservation and
//! session-history unification, and each CLI's config directory — used to be
//! dumped into the single global Settings page. They belong with the app they
//! configure, so this panel renders just the selected app's settings and is
//! opened from a gear in that app's provider-list header. The values still live
//! in the global [`AppSettings`] (persisted via `settings::mutate_settings`);
//! only their *placement* is app-scoped.

use gpui::{Context, Entity, ScrollHandle, SharedString, Window, div, prelude::*};
use ochub_core::AppType;
use ochub_core::settings::{self, AppSettings};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

/// Emitted when the user dismisses the panel (back to the provider list).
pub enum AppSettingsEvent {
    Close,
}

impl gpui::EventEmitter<AppSettingsEvent> for AppSettingsView {}

pub struct AppSettingsView {
    app_type: AppType,
    settings: AppSettings,
    /// The app's config-dir override input (None for apps without one).
    config_dir: Option<Entity<TextInput>>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    scroll_handle: ScrollHandle,
    saving: bool,
}

/// Whether an app has any app-scoped settings worth a gear button.
pub fn app_has_settings(app: AppType) -> bool {
    config_dir_meta(app).is_some() || matches!(app, AppType::Claude | AppType::Codex)
}

impl AppSettingsView {
    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.config_dir.is_some() {
            self.save_config_dir(cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AppSettingsEvent::Close);
    }

    pub fn new(app_type: AppType, cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let config_dir = Self::make_config_dir_input(app_type, &settings, cx);
        Self {
            app_type,
            settings,
            config_dir,
            status: None,
            status_level: None,
            scroll_handle: ScrollHandle::new(),
            saving: false,
        }
    }

    /// Re-point the panel at a different app (called when the gear is opened).
    pub fn reload_for(&mut self, app_type: AppType, cx: &mut Context<Self>) {
        self.app_type = app_type;
        self.settings = settings::get_settings();
        self.config_dir = Self::make_config_dir_input(app_type, &self.settings, cx);
        self.status = None;
        self.status_level = None;
        cx.notify();
    }

    fn make_config_dir_input(
        app_type: AppType,
        settings: &AppSettings,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextInput>> {
        let (placeholder, _desc) = config_dir_meta(app_type)?;
        let current = read_config_dir(settings, app_type).unwrap_or_default();
        Some(cx.new(|cx| {
            let mut input = TextInput::new(cx, placeholder);
            input.set_content(current, cx);
            input
        }))
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
        mutator: impl FnOnce(&mut AppSettings) + Send + 'static,
        success: SharedString,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (result, stored) = cx
                .background_spawn(async move {
                    let result =
                        settings::mutate_settings(mutator).map_err(|error| error.to_string());
                    (result, settings::get_settings())
                })
                .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                this.settings = stored;
                match result {
                    Ok(()) => this.set_status(NotificationLevel::Success, success),
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
        if self.saving {
            return;
        }
        let current = (toggle.get)(&self.settings);
        self.persist_mutation(
            move |settings| (toggle.set)(settings, !current),
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
        let app_type = self.app_type;
        self.persist_mutation(
            move |settings| write_config_dir(settings, app_type, value),
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
        layout::row()
            .id(toggle.id)
            .cursor_pointer()
            .hover(|s| s.bg(theme::surface_hover()))
            .child(layout::row_label(toggle.label, toggle.description))
            .child(layout::toggle(on))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.toggle(toggle, cx);
            }))
    }

    fn render_config_dir(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let input = self.config_dir.as_ref()?;
        let (_placeholder, desc) = config_dir_meta(self.app_type)?;
        let save_button = if self.saving {
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
                    desc,
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
        let app_type = self.app_type;
        let toggles = app_toggles(app_type, &self.settings);

        let header = layout::page_header(
            SharedString::from(tf!(k::APP_SETTINGS_HEADER_TITLE, app = app_label(app_type))),
            Some(t(k::APP_SETTINGS_HEADER_SUBTITLE)),
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
                t(k::APP_SETTINGS_BEHAVIOR_DESC),
            ));
            let rows: Vec<gpui::AnyElement> = toggles
                .into_iter()
                .map(|toggle| self.render_toggle_row(toggle, cx).into_any_element())
                .collect();
            column = column.child(layout::group(rows));
        }
        if let Some(dir) = self.render_config_dir(cx) {
            column = column.child(dir);
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
    label: &'static str,
    description: &'static str,
    get: fn(&AppSettings) -> bool,
    set: fn(&mut AppSettings, bool),
}

fn app_toggles(app: AppType, settings: &AppSettings) -> Vec<AppToggle> {
    match app {
        AppType::Claude => vec![
            AppToggle {
                id: "app-set-claude-plugin",
                label: raw(k::APP_SETTINGS_CLAUDE_PLUGIN_LABEL),
                description: raw(k::APP_SETTINGS_CLAUDE_PLUGIN_DESC),
                get: |s| s.enable_claude_plugin_integration,
                set: |s, v| s.enable_claude_plugin_integration = v,
            },
            AppToggle {
                id: "app-set-claude-onboarding",
                label: raw(k::APP_SETTINGS_CLAUDE_ONBOARDING_LABEL),
                description: raw(k::APP_SETTINGS_CLAUDE_ONBOARDING_DESC),
                get: |s| s.skip_claude_onboarding,
                set: |s, v| s.skip_claude_onboarding = v,
            },
        ],
        AppType::Codex => {
            let mut toggles = vec![
                AppToggle {
                    id: "app-set-codex-preserve-auth",
                    label: raw(k::APP_SETTINGS_CODEX_PRESERVE_AUTH_LABEL),
                    description: raw(k::APP_SETTINGS_CODEX_PRESERVE_AUTH_DESC),
                    get: |s| s.preserve_codex_official_auth_on_switch,
                    set: |s, v| s.preserve_codex_official_auth_on_switch = v,
                },
                AppToggle {
                    id: "app-set-codex-unify-history",
                    label: raw(k::APP_SETTINGS_CODEX_UNIFY_HISTORY_LABEL),
                    description: raw(k::APP_SETTINGS_CODEX_UNIFY_HISTORY_DESC),
                    get: |s| s.unify_codex_session_history,
                    set: |s, v| s.unify_codex_session_history = v,
                },
            ];
            if settings.unify_codex_session_history {
                toggles.push(AppToggle {
                    id: "app-set-codex-migrate-history",
                    label: raw(k::APP_SETTINGS_CODEX_MIGRATE_HISTORY_LABEL),
                    description: raw(k::APP_SETTINGS_CODEX_MIGRATE_HISTORY_DESC),
                    get: |s| s.unify_codex_migrate_existing.unwrap_or(false),
                    set: |s, v| s.unify_codex_migrate_existing = Some(v),
                });
            }
            toggles
        }
        _ => Vec::new(),
    }
}

/// The placeholder + description for an app's config-dir override, or `None`.
fn config_dir_meta(app: AppType) -> Option<(&'static str, &'static str)> {
    match app {
        AppType::Claude => Some(("~/.claude", raw(k::APP_SETTINGS_CONFIG_DIR_CLAUDE_DESC))),
        AppType::Codex => Some(("~/.codex", raw(k::APP_SETTINGS_CONFIG_DIR_CODEX_DESC))),
        AppType::GrokBuild => Some(("~/.grok", raw(k::APP_SETTINGS_CONFIG_DIR_GROKBUILD_DESC))),
        AppType::OpenCode => Some((
            "~/.config/opencode",
            raw(k::APP_SETTINGS_CONFIG_DIR_OPENCODE_DESC),
        )),
        AppType::OpenClaw => Some(("~/.openclaw", raw(k::APP_SETTINGS_CONFIG_DIR_OPENCLAW_DESC))),
        AppType::Hermes => Some(("~/.hermes", raw(k::APP_SETTINGS_CONFIG_DIR_HERMES_DESC))),
        AppType::ClaudeDesktop => None,
    }
}

fn read_config_dir(settings: &AppSettings, app: AppType) -> Option<String> {
    match app {
        AppType::Claude => settings.claude_config_dir.clone(),
        AppType::Codex => settings.codex_config_dir.clone(),
        AppType::GrokBuild => settings.grokbuild_config_dir.clone(),
        AppType::OpenCode => settings.opencode_config_dir.clone(),
        AppType::OpenClaw => settings.openclaw_config_dir.clone(),
        AppType::Hermes => settings.hermes_config_dir.clone(),
        AppType::ClaudeDesktop => None,
    }
}

fn write_config_dir(settings: &mut AppSettings, app: AppType, value: Option<String>) {
    match app {
        AppType::Claude => settings.claude_config_dir = value,
        AppType::Codex => settings.codex_config_dir = value,
        AppType::GrokBuild => settings.grokbuild_config_dir = value,
        AppType::OpenCode => settings.opencode_config_dir = value,
        AppType::OpenClaw => settings.openclaw_config_dir = value,
        AppType::Hermes => settings.hermes_config_dir = value,
        AppType::ClaudeDesktop => {}
    }
}

fn app_label(app: AppType) -> gpui::SharedString {
    crate::app_meta::label(app)
}

crate::notifications::impl_status_toasts_leveled!(AppSettingsView);
