//! The OCHUB root view: an app switcher sidebar plus a main panel that
//! can show the provider list, a provider editor, the settings panel, or the
//! proxy panel, all wired to live `ochub-core` data via an in-process `AppState`.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Context, Entity, FontWeight, MouseButton, SharedString, Window,
};
use ochub_core::services::provider::{self, ProviderService};
use ochub_core::{AppState, AppType, Provider};

use crate::app_settings_view::{app_has_settings, AppSettingsEvent, AppSettingsView};
use crate::auth_view::AuthView;
use crate::components::{self, ButtonTone};
use crate::gallery_view::GalleryView;
use crate::gateway_view::GatewayView;
use crate::icons::{icon, IconName};
use crate::mcp_view::McpView;
use crate::notifications::{NotificationHost, NotificationLevel};
use crate::prompts_view::PromptsView;
use crate::provider_editor::{EditorEvent, ProviderEditor};
use crate::proxy_view::ProxyView;
use crate::sessions_view::SessionsView;
use crate::settings_view::SettingsView;
use crate::shell_menu;
use crate::skills_view::SkillsView;
use crate::theme;
use crate::tools_view::ToolsView;
use crate::usage_view::UsageView;
use crate::workspace_view::WorkspaceView;

pub(crate) fn notify_open_roots(
    cx: &mut App,
    app_type: Option<AppType>,
    level: NotificationLevel,
    title: String,
    message: Option<String>,
) -> bool {
    let mut delivered = false;
    for window in cx.windows() {
        if let Some(root) = window.downcast::<AppRoot>() {
            let title = title.clone();
            let message = message.clone();
            if root
                .update(cx, |root, _window, cx| {
                    root.report_shell_notice(app_type, level, title, message, cx);
                })
                .is_ok()
            {
                delivered = true;
            }
        }
    }
    delivered
}

/// Which top-level section the main panel renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Providers,
    Mcp,
    Prompts,
    Skills,
    Auth,
    Usage,
    Sessions,
    Workspace,
    Tools,
    Settings,
    Proxy,
    Gateway,
    /// Dev-only component gallery (visible with MS_GALLERY=1).
    Gallery,
}

impl Section {
    fn from_env() -> Self {
        match std::env::var("MS_START_SECTION")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "mcp" => Self::Mcp,
            "prompts" | "prompt" => Self::Prompts,
            "skills" | "skill" => Self::Skills,
            "auth" | "oauth" | "accounts" => Self::Auth,
            "usage" => Self::Usage,
            "sessions" | "session" => Self::Sessions,
            "workspace" | "workspaces" => Self::Workspace,
            "tools" | "tool" => Self::Tools,
            "settings" | "setting" => Self::Settings,
            "proxy" => Self::Proxy,
            "gateway" => Self::Gateway,
            "gallery" => Self::Gallery,
            _ => Self::Providers,
        }
    }
}

pub struct AppRoot {
    app: Arc<AppState>,
    selected_app: AppType,
    section: Section,
    providers: Vec<Provider>,
    current: String,
    status: Option<SharedString>,
    notifications: Entity<NotificationHost>,
    /// Active provider editor (add or edit); when `Some`, replaces the list.
    editor: Option<Entity<ProviderEditor>>,
    settings_view: Entity<SettingsView>,
    proxy_view: Entity<ProxyView>,
    gateway_view: Entity<GatewayView>,
    mcp_view: Entity<McpView>,
    prompts_view: Entity<PromptsView>,
    skills_view: Entity<SkillsView>,
    auth_view: Entity<AuthView>,
    usage_view: Entity<UsageView>,
    sessions_view: Entity<SessionsView>,
    workspace_view: Entity<WorkspaceView>,
    tools_view: Entity<ToolsView>,
    gallery_view: Entity<GalleryView>,
    /// Per-app settings panel (app-scoped toggles + config dir), shown over the
    /// provider list when `showing_app_settings` is set.
    app_settings_view: Entity<AppSettingsView>,
    showing_app_settings: bool,
}

impl AppRoot {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let settings_view = cx.new(|cx| SettingsView::new(app.clone(), cx));
        let proxy_view = cx.new(|cx| ProxyView::new(app.clone(), cx));
        let gateway_view = cx.new(|cx| GatewayView::new(app.clone(), cx));
        let mcp_view = cx.new(|cx| McpView::new(app.clone(), cx));
        let notifications = cx.new(|_| NotificationHost::new());
        let prompts_view = cx.new(|cx| PromptsView::new(app.clone(), cx));
        let skills_view = cx.new(|cx| SkillsView::new(app.clone(), cx));
        let auth_view = cx.new(|cx| AuthView::new(app.clone(), cx));
        let usage_view = cx.new(|cx| UsageView::new(app.clone(), cx));
        let sessions_view = cx.new(|cx| SessionsView::new(app.clone(), cx));
        let workspace_view = cx.new(|cx| WorkspaceView::new(cx));
        let tools_view = cx.new(|cx| ToolsView::new(app.clone(), cx));
        let gallery_view = cx.new(|cx| GalleryView::new(cx));
        let initial_section = Section::from_env();
        let enabled = Self::visible_apps();
        let initial_app = std::env::var("MS_START_APP")
            .ok()
            .and_then(|value| value.parse::<AppType>().ok())
            .filter(|app| enabled.contains(app))
            .or_else(|| enabled.first().copied())
            .unwrap_or(AppType::Claude);
        let app_settings_view = cx.new(|cx| AppSettingsView::new(initial_app, cx));
        let mut this = Self {
            app,
            selected_app: initial_app,
            section: initial_section,
            providers: Vec::new(),
            current: String::new(),
            status: None,
            notifications,
            editor: None,
            settings_view,
            proxy_view,
            gateway_view,
            mcp_view,
            prompts_view,
            skills_view,
            auth_view,
            usage_view,
            sessions_view,
            workspace_view,
            tools_view,
            gallery_view,
            app_settings_view,
            showing_app_settings: false,
        };
        cx.subscribe(
            &this.app_settings_view,
            |this, _view, event, cx| match event {
                AppSettingsEvent::Close => {
                    this.showing_app_settings = false;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(&this.settings_view, |this, _view, event, cx| match event {
            crate::settings_view::SettingsEvent::AppsChanged => {
                this.ensure_valid_selection(cx);
                cx.notify();
            }
        })
        .detach();
        this.reload();
        if initial_section == Section::Providers
            && std::env::var("MS_START_EDITOR")
                .map(|value| value.eq_ignore_ascii_case("add"))
                .unwrap_or(false)
        {
            this.open_add_editor(cx);
        }
        if initial_section == Section::Providers
            && std::env::var("MS_START_APP_SETTINGS")
                .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false)
        {
            this.open_app_settings(cx);
        }
        match initial_section {
            Section::Mcp => this.mcp_view.update(cx, |v, _| v.reload()),
            Section::Prompts => this.prompts_view.update(cx, |v, _| v.reload()),
            Section::Skills => this.skills_view.update(cx, |v, _| v.reload()),
            Section::Auth => this.auth_view.update(cx, |v, cx| v.reload(cx)),
            Section::Gateway => this.gateway_view.update(cx, |v, cx| v.reload(cx)),
            Section::Usage => this.usage_view.update(cx, |v, _| v.reload()),
            Section::Sessions => this.sessions_view.update(cx, |v, _| v.reload()),
            Section::Workspace => this.workspace_view.update(cx, |v, _| v.reload()),
            Section::Tools => this.tools_view.update(cx, |v, _| v.reload()),
            _ => {}
        }
        this
    }

    fn visible_apps() -> Vec<AppType> {
        ochub_core::plugin::enabled_plugins()
            .iter()
            .filter_map(|plugin| AppType::from_app_id(plugin.id()))
            .collect()
    }

    fn app_label(app: AppType) -> SharedString {
        crate::app_meta::label(app)
    }

    fn app_accent(app: AppType) -> u32 {
        crate::app_meta::accent(app)
    }

    fn app_icon(app: AppType) -> IconName {
        crate::app_meta::icon(app).unwrap_or(IconName::AgentClaudeCode)
    }

    fn notify_success(&self, title: impl Into<SharedString>, cx: &mut Context<Self>) {
        let title = title.into();
        self.notifications.update(cx, move |host, cx| {
            host.success(title.clone(), cx);
        });
    }

    fn notify_info(&self, title: impl Into<SharedString>, cx: &mut Context<Self>) {
        let title = title.into();
        self.notifications.update(cx, move |host, cx| {
            host.info(title.clone(), cx);
        });
    }

    fn notify_warning(
        &self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let title = title.into();
        let message = message.into();
        self.notifications.update(cx, move |host, cx| {
            host.warning(title.clone(), message.clone(), cx);
        });
    }

    fn notify_error(
        &self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let title = title.into();
        let message = message.into();
        self.notifications.update(cx, move |host, cx| {
            host.error(title.clone(), message.clone(), cx);
        });
    }

    pub(crate) fn report_shell_notice(
        &mut self,
        app_type: Option<AppType>,
        level: NotificationLevel,
        title: String,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if app_type == Some(self.selected_app) {
            self.reload();
        }
        self.status = None;
        match level {
            NotificationLevel::Info => self.notify_info(title, cx),
            NotificationLevel::Success => self.notify_success(title, cx),
            NotificationLevel::Warning => self.notify_warning(
                title,
                message.unwrap_or_else(|| "操作完成，但需要检查结果。".to_string()),
                cx,
            ),
            NotificationLevel::Error => {
                self.notify_error(title, message.unwrap_or_else(|| "未知错误".to_string()), cx)
            }
        }
        cx.notify();
    }

    fn section_icon(section: Section) -> IconName {
        match section {
            Section::Mcp => IconName::Blocks,
            Section::Prompts => IconName::Message,
            Section::Skills => IconName::Wrench,
            Section::Auth => IconName::Key,
            Section::Usage => IconName::Chart,
            Section::Sessions => IconName::Clock,
            Section::Workspace => IconName::Folder,
            Section::Tools => IconName::Tools,
            Section::Settings => IconName::Settings,
            Section::Proxy => IconName::Proxy,
            Section::Gateway => IconName::Cloud,
            Section::Providers => IconName::Cloud,
            Section::Gallery => IconName::Layers,
        }
    }

    /// (Re)load providers + current id for the selected app from the store.
    fn reload(&mut self) {
        match ProviderService::list(&self.app, self.selected_app) {
            Ok(map) => self.providers = map.into_values().collect(),
            Err(err) => {
                self.providers = Vec::new();
                self.status = Some(SharedString::from(format!("加载供应商失败: {err}")));
            }
        }
        self.current = ProviderService::current(&self.app, self.selected_app).unwrap_or_default();
    }

    /// If the currently selected app got disabled (or unregistered), move the
    /// selection to the first enabled app and close any per-app panels.
    fn ensure_valid_selection(&mut self, cx: &mut Context<Self>) {
        let enabled = Self::visible_apps();
        if enabled.contains(&self.selected_app) {
            return;
        }
        let Some(first) = enabled.first().copied() else {
            return;
        };
        self.selected_app = first;
        self.editor = None;
        self.showing_app_settings = false;
        self.status = None;
        self.reload();
        cx.notify();
    }

    fn select_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        if !Self::visible_apps().contains(&app) {
            return;
        }
        let changed = self.selected_app != app || self.section != Section::Providers;
        if changed || self.showing_app_settings {
            self.selected_app = app;
            self.section = Section::Providers;
            self.editor = None;
            self.showing_app_settings = false;
            self.status = None;
            self.reload();
            cx.notify();
        }
    }

    fn open_app_settings(&mut self, cx: &mut Context<Self>) {
        let app = self.selected_app;
        self.app_settings_view
            .update(cx, |view, cx| view.reload_for(app, cx));
        self.editor = None;
        self.showing_app_settings = true;
        cx.notify();
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section || self.showing_app_settings {
            self.section = section;
            self.editor = None;
            self.showing_app_settings = false;
            // Reload the destination view's data so it reflects current state.
            match section {
                Section::Mcp => self.mcp_view.update(cx, |v, _| v.reload()),
                Section::Prompts => self.prompts_view.update(cx, |v, _| v.reload()),
                Section::Skills => self.skills_view.update(cx, |v, _| v.reload()),
                Section::Auth => self.auth_view.update(cx, |v, cx| v.reload(cx)),
                Section::Gateway => self.gateway_view.update(cx, |v, cx| v.reload(cx)),
                Section::Usage => self.usage_view.update(cx, |v, _| v.reload()),
                Section::Sessions => self.sessions_view.update(cx, |v, _| v.reload()),
                Section::Workspace => self.workspace_view.update(cx, |v, _| v.reload()),
                Section::Tools => self.tools_view.update(cx, |v, _| v.reload()),
                _ => {}
            }
            cx.notify();
        }
    }

    fn do_switch(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::switch(&self.app, self.selected_app, &id) {
            Ok(result) => {
                self.status = None;
                if result.warnings.is_empty() {
                    self.notify_success(format!("已切换到 {id}"), cx);
                } else {
                    self.notify_warning(
                        format!("已切换到 {id}"),
                        format!("应用工具配置时返回 {} 个警告", result.warnings.len()),
                        cx,
                    );
                }
            }
            Err(err) => {
                self.notify_error("切换供应商失败", err.to_string(), cx);
            }
        }
        self.reload();
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_import_default(&mut self, cx: &mut Context<Self>) {
        match ProviderService::import_default_config(&self.app, self.selected_app) {
            Ok(true) => {
                self.status = None;
                self.notify_success("已从工具配置导入供应商", cx);
            }
            Ok(false) => {
                self.status = None;
                self.notify_info("没有可导入的工具配置", cx);
            }
            Err(err) => self.notify_error("导入工具配置失败", err.to_string(), cx),
        }
        self.reload();
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_import_live(&mut self, cx: &mut Context<Self>) {
        let imported = match self.selected_app {
            AppType::OpenCode => provider::import_opencode_providers_from_live(&self.app),
            AppType::OpenClaw => provider::import_openclaw_providers_from_live(&self.app),
            AppType::Hermes => provider::import_hermes_providers_from_live(&self.app),
            other => {
                self.notify_warning(
                    "暂不支持导入",
                    format!("{} 暂不支持从工具配置批量导入供应商", other.as_str()),
                    cx,
                );
                cx.notify();
                return;
            }
        };

        match imported {
            Ok(count) => {
                self.status = None;
                self.notify_success(format!("已从工具配置导入 {count} 个供应商"), cx);
            }
            Err(err) => self.notify_error("导入工具配置失败", err.to_string(), cx),
        }
        self.reload();
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_remove_from_live(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::remove_from_live_config(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.status = None;
                self.notify_success("已从工具配置移除", cx);
            }
            Err(err) => self.notify_error("从工具配置移除失败", err.to_string(), cx),
        }
        self.reload();
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::delete(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.status = None;
                self.notify_success("供应商已删除", cx);
            }
            Err(err) => self.notify_error("删除供应商失败", err.to_string(), cx),
        }
        self.reload();
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn open_add_editor(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let editor = cx.new(|cx| ProviderEditor::new_add(app, app_type, cx));
        self.subscribe_editor(&editor, cx);
        self.editor = Some(editor);
        self.status = None;
        cx.notify();
    }

    fn open_edit_editor(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let editor = cx.new(|cx| ProviderEditor::new_edit(app, app_type, &provider, cx));
        self.subscribe_editor(&editor, cx);
        self.editor = Some(editor);
        self.status = None;
        cx.notify();
    }

    fn subscribe_editor(&self, editor: &Entity<ProviderEditor>, cx: &mut Context<Self>) {
        cx.subscribe(editor, |this, _editor, event, cx| match event {
            EditorEvent::Saved => {
                this.editor = None;
                this.status = None;
                this.notify_success("供应商已保存", cx);
                this.reload();
                shell_menu::refresh(&this.app, cx);
                cx.notify();
            }
            EditorEvent::Cancelled => {
                this.editor = None;
                cx.notify();
            }
        })
        .detach();
    }

    fn provider_base_url(&self, provider: &Provider) -> String {
        let (base_url, _) = provider.resolve_usage_credentials(&self.selected_app);
        if base_url.is_empty() {
            "—".to_string()
        } else {
            base_url
        }
    }

    fn render_sidebar_item(&self, app: AppType, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_app == app && self.section == Section::Providers;
        let accent = Self::app_accent(app);
        div()
            .id(SharedString::from(format!("app-{}", app.as_str())))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("打开 {}", Self::app_label(app))))
            .aria_selected(selected)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .pl_2()
            .pr_2()
            .py_1()
            .rounded_lg()
            .cursor_pointer()
            .text_color(if selected {
                theme::sidebar_text()
            } else {
                theme::sidebar_muted()
            })
            .when(selected, |s| {
                s.bg(theme::accent_soft()).font_weight(FontWeight::MEDIUM)
            })
            .when(!selected, |s| {
                s.hover(|h| {
                    h.bg(theme::surface_hover())
                        .text_color(theme::sidebar_text())
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(24.))
                    .h(px(24.))
                    .rounded_md()
                    .bg(theme::c(accent))
                    .shadow_xs()
                    .child(icon(Self::app_icon(app), theme::accent_text(), 15.)),
            )
            .child(div().text_sm().child(Self::app_label(app)))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_app(app, cx);
            }))
    }

    fn render_nav_item(
        &self,
        id: &'static str,
        label: &'static str,
        section: Section,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.section == section;
        let fg = if selected {
            theme::accent()
        } else {
            theme::sidebar_muted()
        };
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("打开 {label}")))
            .aria_selected(selected)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .pl_2()
            .pr_2()
            .py_1p5()
            .rounded_lg()
            .cursor_pointer()
            .text_sm()
            .text_color(fg)
            .when(selected, |s| {
                s.bg(theme::accent_soft()).font_weight(FontWeight::MEDIUM)
            })
            .when(!selected, |s| {
                s.hover(|h| {
                    h.bg(theme::surface_hover())
                        .text_color(theme::sidebar_text())
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.))
                    .h(px(20.))
                    .child(icon(Self::section_icon(section), fg, 15.)),
            )
            .child(label)
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_section(section, cx);
            }))
    }

    fn render_sidebar_group(label: &'static str) -> impl IntoElement {
        div()
            .mt_4()
            .mb_1()
            .px_3()
            .text_color(theme::sidebar_muted())
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .child(label)
    }

    fn section_title(&self) -> SharedString {
        match self.section {
            Section::Providers => Self::app_label(self.selected_app),
            Section::Mcp => "MCP 服务器".into(),
            Section::Prompts => "提示词".into(),
            Section::Skills => "技能".into(),
            Section::Auth => "认证中心".into(),
            Section::Usage => "用量".into(),
            Section::Sessions => "会话".into(),
            Section::Workspace => "工作区".into(),
            Section::Tools => "高级工具".into(),
            Section::Settings => "设置".into(),
            Section::Proxy => "代理".into(),
            Section::Gateway => "中转网关".into(),
            Section::Gallery => "组件画廊".into(),
        }
    }

    /// Custom, draggable unified titlebar (the system titlebar is transparent).
    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("titlebar")
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(44.))
            .flex_shrink_0()
            .pl(px(88.))
            .pr_4()
            .gap_3()
            .bg(theme::header())
            .border_b_1()
            .border_color(theme::border())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, window, _cx| window.start_window_move()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(20.))
                            .h(px(20.))
                            .rounded_md()
                            .bg(theme::accent())
                            .child(icon(IconName::Cloud, theme::accent_text(), 13.)),
                    )
                    .child(
                        div()
                            .text_color(theme::text())
                            .font_weight(FontWeight::BOLD)
                            .text_sm()
                            .child("OCHUB"),
                    ),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(theme::border()))
            .child(
                div()
                    .text_color(theme::subtext())
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(self.section_title())),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .h_full()
            .w(px(252.))
            .flex_shrink_0()
            .bg(theme::mantle().alpha(0.96))
            .border_r_1()
            .border_color(theme::border())
            .shadow_xs()
            .overflow_y_scroll()
            .child(div().h(px(10.)))
            .child(Self::render_sidebar_group("应用"))
            .child(
                div().flex().flex_col().gap_1().px_2().children(
                    Self::visible_apps()
                        .into_iter()
                        .map(|app| self.render_sidebar_item(app, cx)),
                ),
            )
            .child(Self::render_sidebar_group("工具"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item("nav-mcp", "MCP 服务器", Section::Mcp, cx))
                    .child(self.render_nav_item("nav-prompts", "提示词", Section::Prompts, cx))
                    .child(self.render_nav_item("nav-skills", "技能", Section::Skills, cx))
                    .child(self.render_nav_item("nav-auth", "认证中心", Section::Auth, cx))
                    .child(self.render_nav_item("nav-usage", "用量", Section::Usage, cx))
                    .child(self.render_nav_item("nav-sessions", "会话", Section::Sessions, cx))
                    .child(self.render_nav_item("nav-workspace", "工作区", Section::Workspace, cx))
                    .child(self.render_nav_item("nav-tools", "高级工具", Section::Tools, cx)),
            )
            .child(Self::render_sidebar_group("系统"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item("nav-settings", "设置", Section::Settings, cx))
                    .child(self.render_nav_item("nav-proxy", "代理", Section::Proxy, cx))
                    .child(self.render_nav_item("nav-gateway", "中转网关", Section::Gateway, cx))
                    .when(std::env::var_os("MS_GALLERY").is_some(), |col| {
                        col.child(self.render_nav_item(
                            "nav-gallery",
                            "组件画廊",
                            Section::Gallery,
                            cx,
                        ))
                    }),
            )
    }

    fn render_provider_card(
        &self,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_current = !self.selected_app.is_additive_mode() && provider.id == self.current;
        let id = provider.id.clone();
        let edit_provider = provider.clone();
        let delete_id = provider.id.clone();
        let live_id = provider.id.clone();
        let base_url = self.provider_base_url(provider);
        let is_additive = self.selected_app.is_additive_mode();
        let is_in_live = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.live_config_managed)
            .unwrap_or(!is_additive);
        let main_label = if is_additive {
            if is_in_live {
                "从工具移除"
            } else {
                "添加到工具"
            }
        } else if is_current {
            "已启用"
        } else {
            "切换"
        };
        components::panel()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_4()
            .py_3()
            .border_color(if is_current {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|s| {
                s.border_color(theme::border_strong())
                    .shadow(theme::shadow_hover())
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(30.))
                            .h(px(30.))
                            .rounded_md()
                            .bg(if is_current {
                                theme::sidebar_selected()
                            } else {
                                theme::surface_hover()
                            })
                            .child(icon(
                                if is_current {
                                    IconName::Check
                                } else {
                                    Self::app_icon(self.selected_app)
                                },
                                if is_current {
                                    theme::accent()
                                } else {
                                    theme::subtext()
                                },
                                16.,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(theme::text())
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(SharedString::from(provider.name.clone())),
                                    )
                                    .when(is_current, |s| {
                                        s.child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .px_2()
                                                .py_0p5()
                                                .rounded_full()
                                                .bg(theme::accent_soft())
                                                .text_color(theme::accent())
                                                .text_xs()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(
                                                    div()
                                                        .w(px(5.))
                                                        .h(px(5.))
                                                        .rounded_full()
                                                        .bg(theme::accent()),
                                                )
                                                .child("当前"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(SharedString::from(base_url)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        components::action_button(
                            SharedString::from(format!("edit-{}", provider.id)),
                            "编辑",
                            false,
                        )
                        .aria_label(SharedString::from(format!("编辑 {}", provider.name)))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_edit_editor(edit_provider.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::action_button_tone(
                            SharedString::from(format!("delete-{}", provider.id)),
                            "删除",
                            ButtonTone::Danger,
                        )
                        .aria_label(SharedString::from(format!("删除 {}", provider.name)))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.do_delete(delete_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::action_button(
                            SharedString::from(format!("switch-{}", provider.id)),
                            main_label,
                            !(is_current || (is_additive && is_in_live)),
                        )
                        .aria_label(SharedString::from(format!(
                            "{main_label} {}",
                            provider.name
                        )))
                        .aria_selected(is_current || (is_additive && is_in_live))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                if is_additive && is_in_live {
                                    this.do_remove_from_live(live_id.clone(), cx);
                                } else {
                                    this.do_switch(id.clone(), cx);
                                }
                            },
                        )),
                    ),
            )
    }

    /// The "console" hero: a single prominent card that answers *which provider is
    /// live right now* for the selected app. Shown only in switch (non-additive) mode,
    /// above the list of switchable alternatives.
    fn render_active_hero(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.selected_app;
        let accent = Self::app_accent(app);
        let current = self
            .providers
            .iter()
            .find(|p| p.id == self.current)
            .cloned();
        let has_current = current.is_some();

        let icon_tile = div()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .w(px(46.))
            .h(px(46.))
            .rounded_lg()
            .bg(if has_current {
                theme::c(accent)
            } else {
                theme::surface_hover()
            })
            .when(has_current, |s| s.shadow_xs())
            .child(icon(
                Self::app_icon(app),
                if has_current {
                    theme::accent_text()
                } else {
                    theme::muted()
                },
                23.,
            ));

        let info = match &current {
            Some(provider) => {
                let base_url = self.provider_base_url(provider);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::accent())
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("当前生效"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(theme::green_soft())
                                    .child(
                                        div().w(px(5.)).h(px(5.)).rounded_full().bg(theme::green()),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme::green())
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("已启用"),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(SharedString::from(provider.name.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(icon(IconName::Cloud, theme::muted(), 12.))
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .truncate()
                                    .child(SharedString::from(base_url)),
                            ),
                    )
            }
            None => div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("当前生效"),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .child("未选择供应商"),
                )
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child("从下方列表选择一个供应商以启用。"),
                ),
        };

        let actions = current.map(|provider| {
            let edit_provider = provider.clone();
            div().flex().flex_row().items_center().gap_2().child(
                components::action_button(
                    SharedString::from(format!("hero-edit-{}", provider.id)),
                    "编辑",
                    false,
                )
                .aria_label(SharedString::from(format!("编辑 {}", provider.name)))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_edit_editor(edit_provider.clone(), cx);
                })),
            )
        });

        let mut card = components::panel()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .w_full()
            .px_5()
            .py_4()
            .border_color(if has_current {
                theme::accent()
            } else {
                theme::border()
            })
            .when(has_current, |s| s.shadow(theme::shadow_panel()))
            .child(icon_tile)
            .child(info);
        if let Some(actions) = actions {
            card = card.child(actions);
        }
        card
    }

    fn render_provider_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.selected_app;
        let is_switch = !app.is_additive_mode();
        let can_import_live =
            matches!(app, AppType::OpenCode | AppType::OpenClaw | AppType::Hermes);

        // In switch mode the live provider is surfaced in the hero, so the list below
        // shows only the switchable alternatives. Additive apps list everything.
        let cards: Vec<_> = self
            .providers
            .iter()
            .filter(|p| !is_switch || p.id != self.current)
            .map(|p| self.render_provider_card(p, cx))
            .collect();
        let no_providers = self.providers.is_empty();
        let others_empty = cards.is_empty();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(theme::bg())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .px_6()
                    .py_3()
                    .bg(theme::header().alpha(0.95))
                    .border_b_1()
                    .border_color(theme::border())
                    .shadow_xs()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .text_color(theme::text())
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child(icon(
                                        Self::app_icon(app),
                                        theme::c(Self::app_accent(app)),
                                        18.,
                                    ))
                                    .child(Self::app_label(app)),
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
                                            .py_0p5()
                                            .rounded_full()
                                            .bg(theme::inset())
                                            .text_color(theme::subtext())
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(if app.is_additive_mode() {
                                                "累加模式"
                                            } else {
                                                "切换模式"
                                            }),
                                    )
                                    .child(div().text_color(theme::muted()).text_xs().child(
                                        SharedString::from(format!(
                                            "{} 个供应商",
                                            self.providers.len()
                                        )),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                components::icon_button(
                                    "add-provider",
                                    "新增",
                                    IconName::Add,
                                    true,
                                )
                                .aria_label("新增供应商")
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.open_add_editor(cx);
                                    },
                                )),
                            )
                            .child(
                                components::icon_button(
                                    "import-default",
                                    "导入工具配置",
                                    IconName::Archive,
                                    false,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.do_import_default(cx);
                                    },
                                )),
                            )
                            .when(can_import_live, |s| {
                                s.child(
                                    components::icon_button(
                                        "import-live",
                                        "批量导入",
                                        IconName::Cloud,
                                        false,
                                    )
                                    .aria_label("从工具配置批量导入供应商")
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.do_import_live(cx);
                                        },
                                    )),
                                )
                            })
                            .when(app_has_settings(app), |s| {
                                s.child(
                                    components::icon_button(
                                        "app-settings-gear",
                                        "应用设置",
                                        IconName::Settings,
                                        false,
                                    )
                                    .aria_label("打开应用设置")
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.open_app_settings(cx);
                                        },
                                    )),
                                )
                            }),
                    ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(div().px_6().py_2().child(components::status_banner(status)))
            })
            .child(
                div()
                    .id("provider-list")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_6()
                    .w_full()
                    .overflow_y_scroll()
                    .when(is_switch, |s| s.child(self.render_active_hero(cx)))
                    .when(is_switch && !others_empty, |s| {
                        s.child(
                            div()
                                .pt_1()
                                .text_color(theme::subtext())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("切换到其他供应商"),
                        )
                    })
                    .when(no_providers, |s| {
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .child("还没有供应商。点击“新增”或“导入工具配置”创建一个。"),
                        )
                    })
                    .when(is_switch && !no_providers && others_empty, |s| {
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child("暂无其他供应商，点击“新增”可添加更多。"),
                        )
                    })
                    .children(cards),
            )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.section {
            Section::Settings => self.settings_view.clone().into_any_element(),
            Section::Proxy => self.proxy_view.clone().into_any_element(),
            Section::Gateway => self.gateway_view.clone().into_any_element(),
            Section::Mcp => self.mcp_view.clone().into_any_element(),
            Section::Prompts => self.prompts_view.clone().into_any_element(),
            Section::Skills => self.skills_view.clone().into_any_element(),
            Section::Auth => self.auth_view.clone().into_any_element(),
            Section::Usage => self.usage_view.clone().into_any_element(),
            Section::Sessions => self.sessions_view.clone().into_any_element(),
            Section::Workspace => self.workspace_view.clone().into_any_element(),
            Section::Tools => self.tools_view.clone().into_any_element(),
            Section::Gallery => self.gallery_view.clone().into_any_element(),
            Section::Providers => {
                if self.showing_app_settings {
                    self.app_settings_view.clone().into_any_element()
                } else if let Some(editor) = self.editor.as_ref() {
                    editor.clone().into_any_element()
                } else {
                    self.render_provider_list(cx).into_any_element()
                }
            }
        }
    }
}

impl Render for AppRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family("Helvetica Neue")
            .relative()
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(cx)),
            )
            .child(self.notifications.clone())
    }
}
