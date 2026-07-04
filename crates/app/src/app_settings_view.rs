//! Per-app settings panel.
//!
//! Settings that affect exactly one managed tool — Claude Code's plugin
//! integration / onboarding skip, Codex's official-OAuth preservation and
//! session-history unification, and each CLI's config directory — used to be
//! dumped into the single global Settings page. They belong with the app they
//! configure, so this panel renders just the selected app's settings and is
//! opened from a gear in that app's provider-list header. The values still live
//! in the global [`AppSettings`] (persisted via `settings::update_settings`);
//! only their *placement* is app-scoped.

use gpui::{div, prelude::*, Context, Entity, SharedString, Window};
use routedeck_core::settings::{self, AppSettings};
use routedeck_core::AppType;

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
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
}

/// Whether an app has any app-scoped settings worth a gear button.
pub fn app_has_settings(app: AppType) -> bool {
    config_dir_meta(app).is_some() || !app_toggles(app).is_empty()
}

impl AppSettingsView {
    pub fn new(app_type: AppType, cx: &mut Context<Self>) -> Self {
        let settings = settings::get_settings();
        let config_dir = Self::make_config_dir_input(app_type, &settings, cx);
        Self {
            app_type,
            settings,
            config_dir,
            status: None,
        }
    }

    /// Re-point the panel at a different app (called when the gear is opened).
    pub fn reload_for(&mut self, app_type: AppType, cx: &mut Context<Self>) {
        self.app_type = app_type;
        self.settings = settings::get_settings();
        self.config_dir = Self::make_config_dir_input(app_type, &self.settings, cx);
        self.status = None;
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

    fn persist(&mut self, cx: &mut Context<Self>) {
        match settings::update_settings(self.settings.clone()) {
            Ok(()) => self.status = Some(SharedString::from("已保存")),
            Err(err) => self.status = Some(SharedString::from(format!("保存失败: {err}"))),
        }
        self.settings = settings::get_settings();
        cx.notify();
    }

    fn toggle(&mut self, toggle: AppToggle, cx: &mut Context<Self>) {
        let current = (toggle.get)(&self.settings);
        (toggle.set)(&mut self.settings, !current);
        self.persist(cx);
    }

    fn save_config_dir(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.config_dir.as_ref() else {
            return;
        };
        let raw = input.read(cx).content().trim().to_string();
        let value = if raw.is_empty() { None } else { Some(raw) };
        write_config_dir(&mut self.settings, self.app_type, value);
        self.persist(cx);
        self.status = Some(SharedString::from("目录已保存；建议重启应用以完整生效。"));
        cx.notify();
    }

    fn render_toggle_row(&self, toggle: AppToggle, cx: &mut Context<Self>) -> impl IntoElement {
        let on = (toggle.get)(&self.settings);
        layout::row()
            .id(toggle.id)
            .cursor_pointer()
            .hover(|s| s.bg(theme::c(theme::SURFACE_HOVER)))
            .child(layout::row_label(toggle.label, toggle.description))
            .child(layout::toggle(on))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.toggle(toggle, cx);
            }))
    }

    fn render_config_dir(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let input = self.config_dir.as_ref()?;
        let (_placeholder, desc) = config_dir_meta(self.app_type)?;
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .child(layout::section_header("配置目录", desc))
                .child(
                    div()
                        .w_full()
                        .rounded_lg()
                        .bg(theme::c(theme::SURFACE))
                        .border_1()
                        .border_color(theme::c(theme::BORDER))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(input.clone())
                        .child(
                            div().flex().flex_row().justify_end().child(
                                components::action_button(
                                    "app-settings-save-dir",
                                    "保存目录",
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.save_config_dir(cx);
                                    },
                                )),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for AppSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app_type = self.app_type;
        let toggles = app_toggles(app_type);

        let header = layout::page_header(
            SharedString::from(format!("{} 设置", app_label(app_type))),
            Some("仅作用于该应用的行为与目录。".into()),
        )
        .child(
            components::action_button("app-settings-back", "← 返回", false).on_click(cx.listener(
                |_this, _event, _window, cx| {
                    cx.emit(AppSettingsEvent::Close);
                },
            )),
        );

        let mut column = layout::content_column();
        if !toggles.is_empty() {
            column = column.child(layout::section_header(
                "行为",
                "该应用切换/写入时的行为开关。",
            ));
            let rows: Vec<gpui::AnyElement> = toggles
                .into_iter()
                .map(|t| self.render_toggle_row(t, cx).into_any_element())
                .collect();
            column = column.child(layout::group(rows));
        }
        if let Some(dir) = self.render_config_dir(cx) {
            column = column.child(dir);
        }
        if let Some(status) = self.status.clone() {
            column = column.child(
                div()
                    .text_color(theme::c(theme::TEAL))
                    .text_xs()
                    .child(status),
            );
        }

        layout::page()
            .child(header)
            .child(layout::scroll_body("app-settings-body", column))
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

fn app_toggles(app: AppType) -> Vec<AppToggle> {
    match app {
        AppType::Claude => vec![
            AppToggle {
                id: "app-set-claude-plugin",
                label: "Claude 插件集成",
                description: "允许技能和 MCP 功能写入 Claude Code 插件相关配置。",
                get: |s| s.enable_claude_plugin_integration,
                set: |s, v| s.enable_claude_plugin_integration = v,
            },
            AppToggle {
                id: "app-set-claude-onboarding",
                label: "跳过 Claude 引导",
                description: "自动标记 Claude Code MCP 引导已完成。",
                get: |s| s.skip_claude_onboarding,
                set: |s, v| s.skip_claude_onboarding = v,
            },
        ],
        AppType::Codex => {
            let mut toggles = vec![
                AppToggle {
                    id: "app-set-codex-preserve-auth",
                    label: "保留 Codex 官方 OAuth",
                    description: "切换 Codex 官方供应商时保留现有 OAuth 认证信息。",
                    get: |s| s.preserve_codex_official_auth_on_switch,
                    set: |s, v| s.preserve_codex_official_auth_on_switch = v,
                },
                AppToggle {
                    id: "app-set-codex-unify-history",
                    label: "统一 Codex 会话历史",
                    description: "将官方和第三方 Codex 会话写入统一历史位置。",
                    get: |s| s.unify_codex_session_history,
                    set: |s, v| s.unify_codex_session_history = v,
                },
            ];
            if settings::get_settings().unify_codex_session_history {
                toggles.push(AppToggle {
                    id: "app-set-codex-migrate-history",
                    label: "迁入既有 Codex 会话",
                    description: "开启后在下一次迁移流程中导入已有官方会话历史。",
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
        AppType::Claude => Some((
            "~/.claude",
            "默认 ~/.claude；MCP 配置会按目录名推导到相邻 JSON 文件。",
        )),
        AppType::Codex => Some((
            "~/.codex",
            "默认 ~/.codex；影响 auth.json、config.toml 和会话历史。",
        )),
        AppType::Gemini => Some(("~/.gemini", "默认 ~/.gemini；影响 settings.json 和 .env。")),
        AppType::OpenCode => Some((
            "~/.config/opencode",
            "默认 ~/.config/opencode；影响 opencode.json。",
        )),
        AppType::OpenClaw => Some(("~/.openclaw", "默认 ~/.openclaw；影响 openclaw.json。")),
        AppType::Hermes => Some(("~/.hermes", "默认 ~/.hermes；影响 config.yaml。")),
        AppType::ClaudeDesktop => None,
    }
}

fn read_config_dir(settings: &AppSettings, app: AppType) -> Option<String> {
    match app {
        AppType::Claude => settings.claude_config_dir.clone(),
        AppType::Codex => settings.codex_config_dir.clone(),
        AppType::Gemini => settings.gemini_config_dir.clone(),
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
        AppType::Gemini => settings.gemini_config_dir = value,
        AppType::OpenCode => settings.opencode_config_dir = value,
        AppType::OpenClaw => settings.openclaw_config_dir = value,
        AppType::Hermes => settings.hermes_config_dir = value,
        AppType::ClaudeDesktop => {}
    }
}

fn app_label(app: AppType) -> &'static str {
    match app {
        AppType::Claude => "Claude Code",
        AppType::ClaudeDesktop => "Claude Desktop",
        AppType::Codex => "Codex",
        AppType::Gemini => "Gemini CLI",
        AppType::OpenCode => "OpenCode",
        AppType::OpenClaw => "OpenClaw",
        AppType::Hermes => "Hermes",
    }
}
