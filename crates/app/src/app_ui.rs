//! The OcHub root view: an app switcher sidebar plus a main panel that
//! can show the provider list, a provider editor, the settings panel, or the
//! gateway panel, all wired to live `ochub-core` data via an in-process `AppState`.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Context, Entity, FontWeight, ListAlignment, ListState, MouseButton,
    SharedString, Window,
};
use ochub_core::services::provider::{self, ProviderService};
use ochub_core::{AppState, AppType, Provider};

use crate::app_settings_view::{app_has_settings, AppSettingsEvent, AppSettingsView};
use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::gallery_view::GalleryView;
use crate::gateway_view::GatewayView;
use crate::icons::{icon, IconName};
use crate::layout;
use crate::mcp_view::McpView;
use crate::notifications::{NotificationHost, NotificationLevel, ToastSource};
use crate::provider_editor::{EditorEvent, ProviderEditor};
use crate::sessions_view::SessionsView;
use crate::settings_view::SettingsView;
use crate::shell_menu;
use crate::shortcuts::{Cancel, CloseWindow, Save};
use crate::skills_view::SkillsView;
use crate::theme;
use crate::theme_view::ThemeView;
use crate::tools_view::ToolsView;
use crate::usage_view::UsageView;

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
    Skills,
    Usage,
    Sessions,
    Tools,
    Themes,
    Settings,
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
            "skills" | "skill" => Self::Skills,
            "usage" => Self::Usage,
            "sessions" | "session" => Self::Sessions,
            "tools" | "tool" => Self::Tools,
            "theme" | "themes" | "appearance" => Self::Themes,
            "settings" | "setting" => Self::Settings,
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
    notifications: Entity<NotificationHost>,
    /// Active provider editor (add or edit); when `Some`, replaces the list.
    editor: Option<Entity<ProviderEditor>>,
    /// Provider pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<Provider>,
    /// One-time acknowledgement shown after the first successful launch.
    show_first_run_notice: bool,
    settings_view: Entity<SettingsView>,
    gateway_view: Entity<GatewayView>,
    mcp_view: Entity<McpView>,
    skills_view: Entity<SkillsView>,
    usage_view: Entity<UsageView>,
    sessions_view: Entity<SessionsView>,
    tools_view: Entity<ToolsView>,
    theme_view: Entity<ThemeView>,
    gallery_view: Entity<GalleryView>,
    /// Per-app settings panel (app-scoped toggles + config dir), shown over the
    /// provider list when `showing_app_settings` is set.
    app_settings_view: Entity<AppSettingsView>,
    showing_app_settings: bool,
    /// Drives the virtualized provider list; row count follows the current
    /// app's provider set, so it is `reset` whenever the plan length changes.
    provider_list_state: ListState,
}

/// Row plan for the virtualized provider list. Rebuilt每帧并被 list 的
/// processor 捕获，保证一帧内索引与内容一致。`Card` 存 `providers` 的下标。
#[derive(Clone, Copy)]
enum ProviderRow {
    Hero,
    OthersLabel,
    EmptyState,
    OthersEmptyHint,
    Card(usize),
}

impl AppRoot {
    fn save_active(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() {
            window.play_system_bell();
            return;
        }
        if self.show_first_run_notice {
            window.play_system_bell();
            return;
        }
        match self.section {
            Section::Providers if self.showing_app_settings => {
                self.app_settings_view
                    .update(cx, |view, cx| view.shortcut_save(window, cx));
            }
            Section::Providers => {
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.shortcut_save(cx));
                } else {
                    window.play_system_bell();
                }
            }
            Section::Gateway => self
                .gateway_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Mcp => self
                .mcp_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Tools => self
                .tools_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Themes => self
                .theme_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            Section::Settings => self
                .settings_view
                .update(cx, |view, cx| view.shortcut_save(window, cx)),
            _ => window.play_system_bell(),
        }
    }

    fn cancel_active(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.take().is_some() {
            cx.notify();
            return;
        }
        match self.section {
            Section::Providers if self.showing_app_settings => {
                self.app_settings_view
                    .update(cx, |view, cx| view.shortcut_cancel(cx));
            }
            Section::Providers => {
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.shortcut_cancel(cx));
                } else {
                    window.play_system_bell();
                }
            }
            Section::Gateway => self
                .gateway_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Mcp => self
                .mcp_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Tools => self
                .tools_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Themes => self
                .theme_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            Section::Settings => self
                .settings_view
                .update(cx, |view, cx| view.shortcut_cancel(window, cx)),
            _ => window.play_system_bell(),
        }
    }

    fn close_window(&mut self, _: &CloseWindow, _window: &mut Window, _cx: &mut Context<Self>) {
        // OcHub owns background gateway state, so keep the single root window
        // alive. macOS can restore a hidden app from Dock; Windows/Linux keep
        // an explicit taskbar/dock entry by minimizing instead.
        #[cfg(target_os = "macos")]
        _cx.hide();
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        _window.minimize_window();
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        _window.minimize_window();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let settings_view = cx.new(|cx| SettingsView::new(app.clone(), cx));
        let gateway_view = cx.new(|cx| GatewayView::new(app.clone(), cx));
        let mcp_view = cx.new(|cx| McpView::new(app.clone(), cx));
        let notifications = cx.new(|_| NotificationHost::new());
        let skills_view = cx.new(|cx| SkillsView::new(app.clone(), cx));
        let usage_view = cx.new(|cx| UsageView::new(app.clone(), cx));
        let sessions_view = cx.new(|cx| SessionsView::new(app.clone(), cx));
        let tools_view = cx.new(|cx| ToolsView::new(app.clone(), cx));
        let theme_view = cx.new(ThemeView::new);
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
            notifications,
            editor: None,
            confirm_delete: None,
            show_first_run_notice: crate::shell_support::first_run_notice_pending(),
            settings_view,
            gateway_view,
            mcp_view,
            skills_view,
            usage_view,
            sessions_view,
            tools_view,
            theme_view,
            gallery_view,
            app_settings_view,
            showing_app_settings: false,
            provider_list_state: ListState::new(0, ListAlignment::Top, px(512.)),
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
        this.connect_toast_sources(cx);
        this.reload(cx);
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
            Section::Skills => this.skills_view.update(cx, |v, cx| v.reload(cx)),
            Section::Gateway => this.gateway_view.update(cx, |v, cx| v.reload(cx)),
            Section::Usage => this.usage_view.update(cx, |v, cx| v.reload(cx)),
            Section::Sessions => this.sessions_view.update(cx, |v, cx| v.reload(cx)),
            Section::Tools => this.tools_view.update(cx, |v, _| v.reload()),
            _ => {}
        }
        this.flush_section_toast(initial_section, cx);
        this
    }

    fn observe_toasts<T: ToastSource + 'static>(
        source: &Entity<T>,
        notifications: &Entity<NotificationHost>,
        cx: &mut Context<Self>,
    ) {
        let notifications = notifications.clone();
        cx.observe(source, move |_this, source, cx| {
            let message = source.update(cx, |source, _| source.take_toast());
            if let Some(message) = message {
                notifications.update(cx, |host, cx| {
                    host.status(message, cx);
                });
            }
        })
        .detach();
    }

    fn forward_toast<T: ToastSource + 'static>(
        source: &Entity<T>,
        notifications: &Entity<NotificationHost>,
        cx: &mut Context<Self>,
    ) {
        let message = source.update(cx, |source, _| source.take_toast());
        if let Some(message) = message {
            notifications.update(cx, |host, cx| {
                host.status(message, cx);
            });
        }
    }

    fn connect_toast_sources(&self, cx: &mut Context<Self>) {
        Self::observe_toasts(&self.settings_view, &self.notifications, cx);
        Self::observe_toasts(&self.gateway_view, &self.notifications, cx);
        Self::observe_toasts(&self.mcp_view, &self.notifications, cx);
        Self::observe_toasts(&self.skills_view, &self.notifications, cx);
        Self::observe_toasts(&self.usage_view, &self.notifications, cx);
        Self::observe_toasts(&self.sessions_view, &self.notifications, cx);
        Self::observe_toasts(&self.tools_view, &self.notifications, cx);
        Self::observe_toasts(&self.theme_view, &self.notifications, cx);
        Self::observe_toasts(&self.app_settings_view, &self.notifications, cx);

        Self::forward_toast(&self.settings_view, &self.notifications, cx);
        Self::forward_toast(&self.gateway_view, &self.notifications, cx);
        Self::forward_toast(&self.mcp_view, &self.notifications, cx);
        Self::forward_toast(&self.skills_view, &self.notifications, cx);
        Self::forward_toast(&self.usage_view, &self.notifications, cx);
        Self::forward_toast(&self.sessions_view, &self.notifications, cx);
        Self::forward_toast(&self.tools_view, &self.notifications, cx);
        Self::forward_toast(&self.theme_view, &self.notifications, cx);
        Self::forward_toast(&self.app_settings_view, &self.notifications, cx);
    }

    fn flush_section_toast(&self, section: Section, cx: &mut Context<Self>) {
        match section {
            Section::Settings => Self::forward_toast(&self.settings_view, &self.notifications, cx),
            Section::Gateway => Self::forward_toast(&self.gateway_view, &self.notifications, cx),
            Section::Mcp => Self::forward_toast(&self.mcp_view, &self.notifications, cx),
            Section::Skills => Self::forward_toast(&self.skills_view, &self.notifications, cx),
            Section::Usage => Self::forward_toast(&self.usage_view, &self.notifications, cx),
            Section::Sessions => Self::forward_toast(&self.sessions_view, &self.notifications, cx),
            Section::Tools => Self::forward_toast(&self.tools_view, &self.notifications, cx),
            Section::Themes => Self::forward_toast(&self.theme_view, &self.notifications, cx),
            Section::Providers | Section::Gallery => {}
        }
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
            self.reload(cx);
        }
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
            Section::Skills => IconName::Wrench,
            Section::Usage => IconName::Chart,
            Section::Sessions => IconName::Clock,
            Section::Tools => IconName::Tools,
            Section::Themes => IconName::Palette,
            Section::Settings => IconName::Settings,
            Section::Gateway => IconName::Cloud,
            Section::Providers => IconName::Cloud,
            Section::Gallery => IconName::Layers,
        }
    }

    /// (Re)load providers + current id for the selected app from the store.
    fn reload(&mut self, cx: &mut Context<Self>) {
        match ProviderService::list(&self.app, self.selected_app) {
            Ok(map) => self.providers = map.into_values().collect(),
            Err(err) => {
                self.providers = Vec::new();
                self.notify_error("加载供应商失败", err.to_string(), cx);
            }
        }
        self.current = ProviderService::current(&self.app, self.selected_app).unwrap_or_default();
        // 行数变化由 render 里的 reset 处理；这里只失效高度缓存。
        self.provider_list_state.remeasure();
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
        self.reload(cx);
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
            self.reload(cx);
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
                Section::Skills => self.skills_view.update(cx, |v, cx| v.reload(cx)),
                Section::Gateway => self.gateway_view.update(cx, |v, cx| v.reload(cx)),
                Section::Usage => self.usage_view.update(cx, |v, cx| v.reload(cx)),
                Section::Sessions => self.sessions_view.update(cx, |v, cx| v.reload(cx)),
                Section::Tools => self.tools_view.update(cx, |v, _| v.reload()),
                Section::Themes => self.theme_view.update(cx, |v, cx| v.reload(cx)),
                _ => {}
            }
            self.flush_section_toast(section, cx);
            cx.notify();
        }
    }

    fn do_switch(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::switch(&self.app, self.selected_app, &id) {
            Ok(result) => {
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
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_import_default(&mut self, cx: &mut Context<Self>) {
        match ProviderService::import_default_config(&self.app, self.selected_app) {
            Ok(true) => {
                self.notify_success("已从工具配置导入供应商", cx);
            }
            Ok(false) => {
                self.notify_info("没有可导入的工具配置", cx);
            }
            Err(err) => self.notify_error("导入工具配置失败", err.to_string(), cx),
        }
        self.reload(cx);
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
                self.notify_success(format!("已从工具配置导入 {count} 个供应商"), cx);
            }
            Err(err) => self.notify_error("导入工具配置失败", err.to_string(), cx),
        }
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn do_remove_from_live(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::remove_from_live_config(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.notify_success("已从工具配置移除", cx);
            }
            Err(err) => self.notify_error("从工具配置移除失败", err.to_string(), cx),
        }
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn visible_provider_ids(&self) -> Vec<String> {
        let hide_current = !self.selected_app.is_additive_mode();
        self.providers
            .iter()
            .filter(|provider| !hide_current || provider.id != self.current)
            .map(|provider| provider.id.clone())
            .collect()
    }

    fn move_provider(&mut self, id: String, delta: isize, cx: &mut Context<Self>) {
        let visible = self.visible_provider_ids();
        let Some(position) = visible.iter().position(|provider_id| provider_id == &id) else {
            return;
        };
        let target = position as isize + delta;
        if target < 0 || target as usize >= visible.len() {
            return;
        }
        let target_id = &visible[target as usize];
        let Some(source_index) = self.providers.iter().position(|provider| provider.id == id)
        else {
            return;
        };
        let Some(target_index) = self
            .providers
            .iter()
            .position(|provider| provider.id == *target_id)
        else {
            return;
        };
        self.providers.swap(source_index, target_index);
        let updates = self
            .providers
            .iter()
            .enumerate()
            .map(|(sort_index, provider)| provider::ProviderSortUpdate {
                id: provider.id.clone(),
                sort_index,
            })
            .collect();
        if let Err(err) = ProviderService::update_sort_order(&self.app, self.selected_app, updates)
        {
            self.notify_error("调整供应商顺序失败", err.to_string(), cx);
        }
        self.reload(cx);
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::delete(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.notify_success("供应商已删除", cx);
            }
            Err(err) => self.notify_error("删除供应商失败", err.to_string(), cx),
        }
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn acknowledge_first_run(&mut self, cx: &mut Context<Self>) {
        crate::shell_support::confirm_first_run_notice();
        self.show_first_run_notice = false;
        cx.notify();
    }

    fn open_add_editor(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let editor = cx.new(|cx| ProviderEditor::new_add(app, app_type, cx));
        self.subscribe_editor(&editor, cx);
        self.editor = Some(editor);
        cx.notify();
    }

    fn open_edit_editor(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let editor = cx.new(|cx| ProviderEditor::new_edit(app, app_type, &provider, cx));
        self.subscribe_editor(&editor, cx);
        self.editor = Some(editor);
        cx.notify();
    }

    fn subscribe_editor(&self, editor: &Entity<ProviderEditor>, cx: &mut Context<Self>) {
        Self::observe_toasts(editor, &self.notifications, cx);
        Self::forward_toast(editor, &self.notifications, cx);
        cx.subscribe(editor, |this, _editor, event, cx| match event {
            EditorEvent::Saved => {
                this.editor = None;
                this.notify_success("供应商已保存", cx);
                this.reload(cx);
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

    /// Empty chrome keeps the native traffic lights embedded in the sidebar
    /// while preserving a reliable drag target above the scrolling navigation.
    fn render_sidebar_drag_region(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-window-drag-region")
            .w_full()
            .h(px(44.))
            .flex_shrink_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, window, _cx| window.start_window_move()),
            )
    }

    /// A shallow strip above page-header content makes the wider content pane
    /// draggable without covering its title or trailing actions.
    fn render_content_drag_region(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("content-window-drag-region")
            .absolute()
            .top_0()
            .left(px(252.))
            .right_0()
            .h(px(10.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, window, _cx| window.start_window_move()),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let navigation = div()
            .id("sidebar-navigation")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_4()
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
                    .child(self.render_nav_item("nav-skills", "技能", Section::Skills, cx))
                    .child(self.render_nav_item("nav-usage", "用量", Section::Usage, cx))
                    .child(self.render_nav_item("nav-sessions", "会话", Section::Sessions, cx))
                    .child(self.render_nav_item("nav-tools", "高级工具", Section::Tools, cx)),
            )
            .child(Self::render_sidebar_group("网络"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item("nav-gateway", "中转网关", Section::Gateway, cx)),
            )
            .child(Self::render_sidebar_group("系统"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item("nav-themes", "主题", Section::Themes, cx))
                    .child(self.render_nav_item("nav-settings", "设置", Section::Settings, cx))
                    .when(std::env::var_os("MS_GALLERY").is_some(), |col| {
                        col.child(self.render_nav_item(
                            "nav-gallery",
                            "组件画廊",
                            Section::Gallery,
                            cx,
                        ))
                    }),
            );

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
            .child(self.render_sidebar_drag_region(cx))
            .child(navigation)
    }

    fn render_provider_card(
        &self,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_current = !self.selected_app.is_additive_mode() && provider.id == self.current;
        let id = provider.id.clone();
        let edit_provider = provider.clone();
        let confirm_provider = provider.clone();
        let live_id = provider.id.clone();
        let reorder_id_up = provider.id.clone();
        let reorder_id_down = provider.id.clone();
        let visible_ids = self.visible_provider_ids();
        let visible_position = visible_ids
            .iter()
            .position(|provider_id| provider_id == &provider.id);
        let can_move_up = visible_position.is_some_and(|position| position > 0);
        let can_move_down =
            visible_position.is_some_and(|position| position + 1 < visible_ids.len());
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
                                        s.child(components::badge(BadgeTone::Accent, "当前"))
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
                    .when(can_move_up, |row| {
                        row.child(
                            components::button(
                                SharedString::from(format!("move-up-{}", provider.id)),
                                "↑",
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .aria_label(SharedString::from(format!("上移 {}", provider.name)))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.move_provider(reorder_id_up.clone(), -1, cx);
                                },
                            )),
                        )
                    })
                    .when(can_move_down, |row| {
                        row.child(
                            components::button(
                                SharedString::from(format!("move-down-{}", provider.id)),
                                "↓",
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .aria_label(SharedString::from(format!("下移 {}", provider.name)))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.move_provider(reorder_id_down.clone(), 1, cx);
                                },
                            )),
                        )
                    })
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
                                this.confirm_delete = Some(confirm_provider.clone());
                                cx.notify();
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
                            .child(components::badge(BadgeTone::Success, "已启用")),
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
        let card_ixs: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| !is_switch || p.id != self.current)
            .map(|(ix, _)| ix)
            .collect();
        let no_providers = self.providers.is_empty();
        let others_empty = card_ixs.is_empty();

        let mut plan: Vec<ProviderRow> = Vec::new();
        if is_switch {
            plan.push(ProviderRow::Hero);
        }
        if is_switch && !others_empty {
            plan.push(ProviderRow::OthersLabel);
        }
        if no_providers {
            plan.push(ProviderRow::EmptyState);
        }
        if is_switch && !no_providers && others_empty {
            plan.push(ProviderRow::OthersEmptyHint);
        }
        plan.extend(card_ixs.into_iter().map(ProviderRow::Card));
        if self.provider_list_state.item_count() != plan.len() {
            self.provider_list_state.reset(plan.len());
        }

        let mode = if app.is_additive_mode() {
            "累加模式"
        } else {
            "切换模式"
        };
        let subtitle = SharedString::from(format!("{mode} · {} 个供应商", self.providers.len()));

        let actions = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                components::icon_button("add-provider", "新增", IconName::Add, true)
                    .aria_label("新增供应商")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.open_add_editor(cx);
                    })),
            )
            .child(
                components::icon_button("import-default", "导入工具配置", IconName::Archive, false)
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.do_import_default(cx);
                    })),
            )
            .when(can_import_live, |s| {
                s.child(
                    components::icon_button("import-live", "批量导入", IconName::Cloud, false)
                        .aria_label("从工具配置批量导入供应商")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.do_import_live(cx);
                        })),
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
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.open_app_settings(cx);
                    })),
                )
            });

        let list = gpui::list(
            self.provider_list_state.clone(),
            cx.processor(move |this, ix: usize, _window, cx| {
                // Each row carries its own bottom spacing (the list draws no
                // inter-item gap); pb_3 mirrors wide_column's default gap.
                let block = div().w_full().pb_3();
                match plan.get(ix).copied() {
                    Some(ProviderRow::Hero) => {
                        block.child(this.render_active_hero(cx)).into_any_element()
                    }
                    Some(ProviderRow::OthersLabel) => block
                        .child(
                            div()
                                .pt_1()
                                .text_color(theme::subtext())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("切换到其他供应商"),
                        )
                        .into_any_element(),
                    Some(ProviderRow::EmptyState) => block
                        .child(
                            components::card().p_0().child(components::empty_state(
                                IconName::Folder,
                                "还没有供应商",
                                "点击“新增”或“导入工具配置”创建一个。",
                                Some(
                                    components::icon_button(
                                        "empty-add-provider",
                                        "新增供应商",
                                        IconName::Add,
                                        true,
                                    )
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_add_editor(cx);
                                    }))
                                    .into_any_element(),
                                ),
                            )),
                        )
                        .into_any_element(),
                    Some(ProviderRow::OthersEmptyHint) => block
                        .child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child("暂无其他供应商，点击“新增”可添加更多。"),
                        )
                        .into_any_element(),
                    Some(ProviderRow::Card(pix)) => match this.providers.get(pix) {
                        Some(provider) => {
                            let card = this.render_provider_card(provider, cx);
                            block.child(card).into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    },
                    None => gpui::Empty.into_any_element(),
                }
            }),
        );

        layout::page()
            .child(layout::page_header(Self::app_label(app), Some(subtitle)).child(actions))
            .child(layout::wide_virtual_body(list))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.section {
            Section::Settings => self.settings_view.clone().into_any_element(),
            Section::Gateway => self.gateway_view.clone().into_any_element(),
            Section::Mcp => self.mcp_view.clone().into_any_element(),
            Section::Skills => self.skills_view.clone().into_any_element(),
            Section::Usage => self.usage_view.clone().into_any_element(),
            Section::Sessions => self.sessions_view.clone().into_any_element(),
            Section::Tools => self.tools_view.clone().into_any_element(),
            Section::Themes => self.theme_view.clone().into_any_element(),
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
            .id("app-root")
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family("Helvetica Neue")
            .relative()
            .key_context("App")
            .on_action(cx.listener(Self::save_active))
            .on_action(cx.listener(Self::cancel_active))
            .on_action(cx.listener(Self::close_window))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(cx)),
            )
            .child(self.render_content_drag_region(cx))
            .child(self.notifications.clone())
            .when_some(self.confirm_delete.clone(), |root, provider| {
                let delete_id = provider.id.clone();
                let message = SharedString::from(format!(
                    "确定删除供应商「{}」吗？此操作不可撤销。",
                    provider.name
                ));
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除供应商"))
                        .child(
                            components::modal_body()
                                .child(div().text_color(theme::subtext()).text_sm().child(message)),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "confirm-delete-cancel",
                                "取消",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(delete_id.clone(), cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .when(self.show_first_run_notice, |root| {
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("欢迎使用 OcHub"))
                        .child(components::modal_body().child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .text_color(theme::subtext())
                                .text_sm()
                                .child("OcHub 会直接读写各 AI 工具的配置，并在本地保存供应商与网关数据。")
                                .child("建议首次使用前备份现有配置；之后可在“设置”与各应用页面调整行为。"),
                        ))
                        .child(components::modal_footer(vec![
                            components::button(
                                "first-run-confirm",
                                "我知道了",
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.acknowledge_first_run(cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}
