//! The OcHub root view: an app switcher sidebar plus a main panel that
//! can show the provider list, a provider editor, the settings panel, or the
//! gateway panel, all wired to live `ochub-core` data via an in-process `AppState`.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    div, prelude::*, px, App, Context, Entity, FontWeight, ListAlignment, ListState, MouseButton,
    ScrollHandle, SharedString, Window, WindowAppearance,
};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{GatewayKey, GatewayRoute};
use ochub_core::services::provider::{self, ProviderService};
use ochub_core::{AppState, AppType, Provider};

use crate::app_settings_view::{app_has_settings, AppSettingsEvent, AppSettingsView};
use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::gallery_view::GalleryView;
use crate::gateway_view::GatewayView;
use crate::i18n::{k, raw, t};
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
use crate::tf;
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

/// A degradation detected during startup, before any window exists.
///
/// Deliberately *not* a rendered sentence. These are constructed before
/// `i18n::install` has run, and the user can change language afterwards, so a
/// notice stores only the condition and the runtime values that belong in the
/// text — the port, the OS error. [`AppRoot::render_startup_notice`] turns that
/// into prose on every frame, which is what makes the banner follow the
/// current locale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupNotice {
    /// The control API port is held by another process.
    ControlApiPortInUse { port: u16 },
    /// Binding the control API port failed for any other reason.
    ControlApiBindFailed { port: u16, error: String },
    /// The port was bound, but the listener could not be configured.
    ControlApiListenerFailed { port: u16, error: String },
    /// The background service thread could not be spawned, so neither the
    /// control API nor gateway autostart is running.
    ServicesUnavailable { error: String },
}

impl StartupNotice {
    /// The heading, in the current locale.
    pub fn title(&self) -> SharedString {
        match self {
            Self::ControlApiPortInUse { .. }
            | Self::ControlApiBindFailed { .. }
            | Self::ControlApiListenerFailed { .. } => t(k::STARTUP_CONTROL_API_TITLE),
            Self::ServicesUnavailable { .. } => t(k::STARTUP_SERVICES_TITLE),
        }
    }

    /// What degraded and what still works, in the current locale.
    pub fn message(&self) -> String {
        match self {
            Self::ControlApiPortInUse { port } => {
                tf!(k::STARTUP_CONTROL_API_PORT_IN_USE, port = port)
            }
            Self::ControlApiBindFailed { port, error } => {
                tf!(
                    k::STARTUP_CONTROL_API_BIND_FAILED,
                    port = port,
                    error = error
                )
            }
            Self::ControlApiListenerFailed { port, error } => {
                tf!(
                    k::STARTUP_CONTROL_API_LISTENER_FAILED,
                    port = port,
                    error = error
                )
            }
            Self::ServicesUnavailable { error } => tf!(k::STARTUP_SERVICES_FAILED, error = error),
        }
    }
}

pub struct AppRoot {
    app: Arc<AppState>,
    selected_app: AppType,
    section: Section,
    providers: Vec<Provider>,
    current: String,
    gateway_routes: Vec<GatewayRoute>,
    gateway_keys: Vec<GatewayKey>,
    notifications: Entity<NotificationHost>,
    /// Active provider editor (add or edit); when `Some`, replaces the list.
    editor: Option<Entity<ProviderEditor>>,
    /// Provider pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<Provider>,
    /// One-time acknowledgement shown after the first successful launch.
    show_first_run_notice: bool,
    /// Persistent startup degradation notice, such as a control API port
    /// conflict. Unlike a toast, this remains visible while the condition lasts.
    startup_notice: Option<StartupNotice>,
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
    /// Ephemeral, presentation-only state used while a provider card is being
    /// dragged. The persisted provider order is updated only when the drag is
    /// dropped inside the provider list.
    provider_drag_state: Option<ProviderDragState>,
    sidebar_scroll_handle: ScrollHandle,
}

/// Row plan for the virtualized provider list. Rebuilt每帧并被 list 的
/// processor 捕获，保证一帧内索引与内容一致。`Card` 存 `providers` 的下标。
#[derive(Clone, Copy)]
enum ProviderRow {
    Hero,
    GatewayRoutes,
    DirectLabel,
    GatewayLabel,
    GatewayCta,
    EmptyState,
    Card(usize),
}

#[derive(Clone)]
struct DraggedProvider {
    id: String,
    name: SharedString,
    base_url: SharedString,
    source_position: usize,
    app_icon: IconName,
}

const PROVIDER_REORDER_ANIMATION: Duration = Duration::from_millis(150);
const PROVIDER_REORDER_HYSTERESIS: f32 = 6.;
const PROVIDER_REORDER_EDGE_ZONE: f32 = 36.;
const PROVIDER_REORDER_SCROLL_STEP: f32 = 16.;

struct ProviderDragState {
    source_id: String,
    source_position: usize,
    target_position: usize,
    transition_started: Instant,
    from_offsets: HashMap<String, f32>,
    to_offsets: HashMap<String, f32>,
}

impl ProviderDragState {
    fn new(dragged: &DraggedProvider) -> Self {
        Self {
            source_id: dragged.id.clone(),
            source_position: dragged.source_position,
            target_position: dragged.source_position,
            transition_started: Instant::now(),
            from_offsets: HashMap::new(),
            to_offsets: HashMap::new(),
        }
    }

    fn animation_progress(&self, now: Instant, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 1.;
        }
        (now.saturating_duration_since(self.transition_started)
            .as_secs_f32()
            / PROVIDER_REORDER_ANIMATION.as_secs_f32())
        .clamp(0., 1.)
    }

    fn offset_for(&self, provider_id: &str, now: Instant, reduce_motion: bool) -> f32 {
        let progress = self.animation_progress(now, reduce_motion);
        // Quintic ease-out: quick acknowledgement, then a quiet deceleration.
        let eased = 1. - (1. - progress).powi(5);
        let from = self.from_offsets.get(provider_id).copied().unwrap_or(0.);
        let to = self.to_offsets.get(provider_id).copied().unwrap_or(0.);
        from + (to - from) * eased
    }

    fn is_animating(&self, now: Instant, reduce_motion: bool) -> bool {
        if reduce_motion || self.animation_progress(now, false) >= 1. {
            return false;
        }
        self.from_offsets
            .keys()
            .chain(self.to_offsets.keys())
            .any(|provider_id| {
                let from = self.from_offsets.get(provider_id).copied().unwrap_or(0.);
                let to = self.to_offsets.get(provider_id).copied().unwrap_or(0.);
                (from - to).abs() > f32::EPSILON
            })
    }

    fn retarget(
        &mut self,
        target_position: usize,
        provider_ids: &[String],
        row_tops: &[f32],
        now: Instant,
        reduce_motion: bool,
    ) {
        let current_offsets = provider_ids
            .iter()
            .filter_map(|provider_id| {
                let offset = self.offset_for(provider_id, now, reduce_motion);
                (offset.abs() > f32::EPSILON).then(|| (provider_id.clone(), offset))
            })
            .collect();
        let desired_offsets = reorder_slot_offsets(
            provider_ids,
            row_tops,
            self.source_position,
            target_position,
        );

        self.target_position = target_position;
        self.transition_started = now;
        self.from_offsets = if reduce_motion {
            desired_offsets.clone()
        } else {
            current_offsets
        };
        self.to_offsets = desired_offsets;
    }
}

fn reorder_slot_offsets(
    provider_ids: &[String],
    row_tops: &[f32],
    source_position: usize,
    target_position: usize,
) -> HashMap<String, f32> {
    if provider_ids.len() != row_tops.len()
        || source_position >= provider_ids.len()
        || target_position >= provider_ids.len()
        || source_position == target_position
    {
        return HashMap::new();
    }

    let mut offsets = HashMap::new();
    if source_position < target_position {
        for position in (source_position + 1)..=target_position {
            offsets.insert(
                provider_ids[position].clone(),
                row_tops[position - 1] - row_tops[position],
            );
        }
    } else {
        for position in target_position..source_position {
            offsets.insert(
                provider_ids[position].clone(),
                row_tops[position + 1] - row_tops[position],
            );
        }
    }
    offsets
}

struct ProviderDragPreview {
    name: SharedString,
    base_url: SharedString,
    app_icon: IconName,
}

impl Render for ProviderDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_stretch()
            .w(px(420.))
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(theme::border_strong())
            .bg(theme::surface())
            .shadow(theme::shadow_hover())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(32.))
                    .flex_none()
                    .bg(theme::accent_soft())
                    .child(icon(IconName::DragHandle, theme::accent(), 16.)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .min_w_0()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(30.))
                            .h(px(30.))
                            .flex_none()
                            .rounded_md()
                            .bg(theme::surface_hover())
                            .child(icon(self.app_icon, theme::subtext(), 16.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.name.clone()),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(self.base_url.clone()),
                            ),
                    ),
            )
    }
}

struct ProviderDragTooltip;

impl Render for ProviderDragTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .text_color(theme::subtext())
            .text_xs()
            .shadow_xs()
            .child(t(k::SHELL_CARD_DRAG_TOOLTIP))
    }
}

fn move_items_between_slots<T: Clone>(
    items: &mut [T],
    slots: &[usize],
    source_position: usize,
    target_position: usize,
) -> bool {
    if source_position == target_position
        || source_position >= slots.len()
        || target_position >= slots.len()
        || slots.iter().any(|slot| *slot >= items.len())
    {
        return false;
    }

    let mut reordered: Vec<T> = slots.iter().map(|slot| items[*slot].clone()).collect();
    let moved = reordered.remove(source_position);
    reordered.insert(target_position, moved);
    for (slot, item) in slots.iter().copied().zip(reordered) {
        items[slot] = item;
    }
    true
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
        if cx.stop_active_drag(window) {
            self.provider_drag_state = None;
            cx.notify();
            return;
        }
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

    pub fn new(
        app: Arc<AppState>,
        startup_notice: Option<StartupNotice>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings_view = cx.new(|cx| SettingsView::new(app.clone(), cx));
        let gateway_view = cx.new(|cx| GatewayView::new(app.clone(), cx));
        let mcp_view = cx.new(|cx| McpView::new(app.clone(), cx));
        let notifications = cx.new(|_| NotificationHost::new());
        let skills_view = cx.new(|cx| SkillsView::new(app.clone(), cx));
        let usage_view = cx.new(|cx| UsageView::new(app.clone(), cx));
        let sessions_view = cx.new(|cx| SessionsView::new(app.clone(), cx));
        let tools_view = cx.new(|cx| ToolsView::new(app.clone(), cx));
        let theme_view = cx.new(ThemeView::new);
        let gallery_view = cx.new(GalleryView::new);
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
            gateway_routes: Vec::new(),
            gateway_keys: Vec::new(),
            notifications,
            editor: None,
            confirm_delete: None,
            show_first_run_notice: crate::shell_support::first_run_notice_pending(),
            startup_notice,
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
            provider_drag_state: None,
            sidebar_scroll_handle: ScrollHandle::new(),
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
            crate::settings_view::SettingsEvent::LocaleChanged => {
                this.relocalize(cx);
            }
        })
        .detach();
        cx.subscribe(&this.gateway_view, |this, _view, event, cx| match event {
            crate::gateway_view::GatewayEvent::OpenProviders(app) => {
                this.selected_app = *app;
                this.select_section(Section::Providers, cx);
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
            let (message, level) = source.update(cx, |source, _| {
                (source.take_toast(), source.take_toast_level())
            });
            if let Some(message) = message {
                notifications.update(cx, |host, cx| {
                    host.status_leveled(level, message, cx);
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
        let (message, level) = source.update(cx, |source, _| {
            (source.take_toast(), source.take_toast_level())
        });
        if let Some(message) = message {
            notifications.update(cx, |host, cx| {
                host.status_leveled(level, message, cx);
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
                message.unwrap_or_else(|| raw(k::SHELL_NOTICE_WARNING_FALLBACK).to_string()),
                cx,
            ),
            NotificationLevel::Error => self.notify_error(
                title,
                message.unwrap_or_else(|| raw(k::SHELL_NOTICE_UNKNOWN_ERROR).to_string()),
                cx,
            ),
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
        if let Err(err) = ProviderService::auto_import_live_providers(&self.app, self.selected_app)
        {
            log::debug!(
                "automatic provider discovery skipped for {}: {err}",
                self.selected_app.as_str()
            );
        }
        match ProviderService::list(&self.app, self.selected_app) {
            Ok(map) => self.providers = map.into_values().collect(),
            Err(err) => {
                self.providers = Vec::new();
                self.notify_error(t(k::SHELL_PROVIDER_LOAD_FAILED), err.to_string(), cx);
            }
        }
        self.current = ProviderService::current(&self.app, self.selected_app).unwrap_or_default();
        self.gateway_routes = self.app.db.get_gateway_routes().unwrap_or_default();
        self.gateway_keys = self.app.db.get_gateway_keys().unwrap_or_default();
        // 行数变化由 render 里的 reset 处理；这里只失效高度缓存。
        self.provider_list_state.remeasure();
    }

    /// If the currently selected app got disabled (or unregistered), move the
    /// selection to the first enabled app and close any per-app panels.
    /// Push a locale change into everything a repaint cannot reach.
    ///
    /// `cx.refresh_windows()` (issued by the settings view) re-renders the view
    /// tree, which covers all rendered text. It does not touch the native menu
    /// bar and Dock menu, which live outside the window tree, nor the memoized
    /// item heights inside each virtualized list.
    fn relocalize(&mut self, cx: &mut Context<Self>) {
        crate::shell_menu::refresh(&self.app, cx);
        self.provider_list_state.remeasure();
        self.settings_view
            .update(cx, |view, cx| view.relocalize(cx));
        self.tools_view.update(cx, |view, cx| view.relocalize(cx));
        self.theme_view.update(cx, |view, cx| view.relocalize(cx));
        self.usage_view.update(cx, |view, cx| view.relocalize(cx));
        self.skills_view.update(cx, |view, cx| view.relocalize(cx));
        self.mcp_view.update(cx, |view, cx| view.relocalize(cx));
        self.sessions_view
            .update(cx, |view, cx| view.relocalize(cx));
        self.gateway_view.update(cx, |view, cx| view.relocalize(cx));
        // The provider editor is built per open, but it survives a locale
        // change while it is on screen, and it holds text inputs whose
        // placeholders were captured when it opened.
        if let Some(editor) = self.editor.clone() {
            editor.update(cx, |view, cx| view.relocalize(cx));
        }
        cx.notify();
    }

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
                Section::Settings => self.settings_view.update(cx, |v, cx| v.reload(cx)),
                _ => {}
            }
            self.flush_section_toast(section, cx);
            cx.notify();
        }
    }

    fn do_switch(&mut self, id: String, cx: &mut Context<Self>) {
        if id == apply::GATEWAY_PROVIDER_ID {
            self.connect_local_gateway(cx);
            return;
        }
        let name = self
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .map(|provider| provider.name.clone())
            .unwrap_or_else(|| id.clone());
        match ProviderService::switch(&self.app, self.selected_app, &id) {
            Ok(result) => {
                if result.warnings.is_empty() {
                    self.notify_success(tf!(k::SHELL_PROVIDER_SWITCHED, name = name), cx);
                } else {
                    self.notify_warning(
                        tf!(k::SHELL_PROVIDER_SWITCHED, name = name),
                        Self::warnings_summary(&result.warnings),
                        cx,
                    );
                }
            }
            Err(err) => {
                self.notify_error(t(k::SHELL_PROVIDER_SWITCH_FAILED), err.to_string(), cx);
            }
        }
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    /// The enabled station route currently bound to the selected app, if any.
    fn bound_station_route(&self) -> Option<&GatewayRoute> {
        let route_id = self
            .gateway_keys
            .iter()
            .find(|key| key.name == self.selected_app.as_str() && key.enabled)
            .and_then(|key| key.route_id.as_deref())
            .filter(|route_id| route_id.starts_with(apply::STATION_ROUTE_PREFIX))?;
        self.gateway_routes
            .iter()
            .find(|route| route.id == route_id && route.enabled)
    }

    /// Toast body for config-write warnings: count plus the first few actual
    /// messages, so users never see a bare number with no detail.
    fn warnings_summary(warnings: &[String]) -> String {
        let shown: Vec<&str> = warnings.iter().take(3).map(String::as_str).collect();
        let mut text = tf!(
            k::SHELL_WARNINGS_SUMMARY,
            count = warnings.len(),
            details = shown.join(raw(k::SHELL_WARNINGS_SEPARATOR)),
        );
        if warnings.len() > 3 {
            text.push('…');
        }
        text
    }

    fn connect_local_gateway(&mut self, cx: &mut Context<Self>) {
        if self.bound_station_route().is_none() {
            self.notify_warning(
                t(k::SHELL_GATEWAY_NEEDS_STATION_TITLE),
                t(k::SHELL_GATEWAY_NEEDS_STATION_MESSAGE),
                cx,
            );
            self.select_section(Section::Gateway, cx);
            return;
        }
        let mut config = match self.app.db.get_gateway_config() {
            Ok(config) => config,
            Err(err) => {
                self.notify_error(t(k::SHELL_GATEWAY_CONFIG_READ_FAILED), err.to_string(), cx);
                return;
            }
        };
        if !config.enabled {
            config.enabled = true;
            if let Err(err) = self.app.db.set_gateway_config(&config) {
                self.notify_error(t(k::SHELL_GATEWAY_START_FAILED), err.to_string(), cx);
                return;
            }
        }
        self.notify_info(t(k::SHELL_GATEWAY_SWITCHING), cx);
        let app = self.app.clone();
        let app_type = self.selected_app;
        cx.spawn(async move |this, cx| {
            let result = match app.gateway.start().await {
                Ok(_) => {
                    let app_for_switch = app.clone();
                    cx.background_spawn(async move {
                        ProviderService::switch(
                            &app_for_switch,
                            app_type,
                            apply::GATEWAY_PROVIDER_ID,
                        )
                    })
                    .await
                }
                Err(err) => Err(err),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(result) if result.warnings.is_empty() => {
                        this.notify_success(t(k::SHELL_GATEWAY_SWITCHED), cx);
                    }
                    Ok(result) => {
                        this.notify_warning(
                            t(k::SHELL_GATEWAY_SWITCHED),
                            Self::warnings_summary(&result.warnings),
                            cx,
                        );
                    }
                    Err(err) => {
                        this.notify_error(t(k::SHELL_GATEWAY_SWITCH_FAILED), err.to_string(), cx);
                    }
                }
                this.reload(cx);
                shell_menu::refresh(&this.app, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn activate_gateway_route(&mut self, route_id: String, cx: &mut Context<Self>) {
        match apply::activate_route_for_app(&self.app, self.selected_app, &route_id) {
            Ok(_) => {
                let route_name = self
                    .gateway_routes
                    .iter()
                    .find(|route| route.id == route_id)
                    .map(|route| route.name.clone())
                    .unwrap_or(route_id);
                self.notify_success(tf!(k::SHELL_GATEWAY_ROUTES_SWITCHED, name = route_name), cx);
            }
            Err(err) => {
                self.notify_error(
                    t(k::SHELL_GATEWAY_ROUTES_SWITCH_FAILED),
                    err.to_string(),
                    cx,
                );
            }
        }
        self.reload(cx);
        cx.notify();
    }

    fn do_remove_from_live(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::remove_from_live_config(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.notify_success(t(k::SHELL_PROVIDER_REMOVED_FROM_TOOL), cx);
            }
            Err(err) => self.notify_error(
                t(k::SHELL_PROVIDER_REMOVE_FROM_TOOL_FAILED),
                err.to_string(),
                cx,
            ),
        }
        self.reload(cx);
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn provider_row_plan(&self) -> Vec<ProviderRow> {
        let is_switch = !self.selected_app.is_additive_mode();
        let current_is_gateway = self
            .providers
            .iter()
            .find(|provider| provider.id == self.current)
            .is_some_and(Provider::is_local_gateway);
        let direct_ixs: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| {
                !provider.is_local_gateway() && (!is_switch || provider.id != self.current)
            })
            .map(|(index, _)| index)
            .collect();
        let gateway_ixs: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| {
                provider.is_local_gateway() && (!is_switch || provider.id != self.current)
            })
            .map(|(index, _)| index)
            .collect();
        let supports_gateway = apply::supported_apps().contains(&self.selected_app);
        let no_providers = self.providers.is_empty();

        let mut plan = Vec::new();
        if is_switch {
            plan.push(ProviderRow::Hero);
            if current_is_gateway {
                plan.push(ProviderRow::GatewayRoutes);
            }
            if !direct_ixs.is_empty() {
                plan.push(ProviderRow::DirectLabel);
                plan.extend(direct_ixs.iter().copied().map(ProviderRow::Card));
            }
            if supports_gateway && !current_is_gateway {
                plan.push(ProviderRow::GatewayLabel);
                if gateway_ixs.is_empty() {
                    plan.push(ProviderRow::GatewayCta);
                } else {
                    plan.extend(gateway_ixs.iter().copied().map(ProviderRow::Card));
                }
            }
            if no_providers && !supports_gateway {
                plan.push(ProviderRow::EmptyState);
            }
        } else {
            if no_providers {
                plan.push(ProviderRow::EmptyState);
            }
            plan.extend(direct_ixs.iter().copied().map(ProviderRow::Card));
        }
        plan
    }

    fn sortable_provider_slots(&self) -> Vec<usize> {
        let hide_current = !self.selected_app.is_additive_mode();
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| {
                !provider.is_local_gateway() && (!hide_current || provider.id != self.current)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn sortable_provider_rows(&self) -> Vec<(String, usize)> {
        let plan = self.provider_row_plan();
        self.sortable_provider_slots()
            .into_iter()
            .filter_map(|provider_index| {
                let row_index = plan.iter().position(
                    |row| matches!(row, ProviderRow::Card(index) if *index == provider_index),
                )?;
                Some((self.providers[provider_index].id.clone(), row_index))
            })
            .collect()
    }

    fn begin_provider_drag(&mut self, dragged: &DraggedProvider, cx: &mut Context<Self>) {
        self.provider_drag_state = Some(ProviderDragState::new(dragged));
        cx.notify();
    }

    fn handle_provider_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<DraggedProvider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dragged = event.drag(cx);
        let source_id = dragged.id.clone();
        let source_position = dragged.source_position;
        if self
            .provider_drag_state
            .as_ref()
            .is_none_or(|state| state.source_id != source_id)
        {
            self.provider_drag_state = Some(ProviderDragState::new(dragged));
        }

        let rows = self.sortable_provider_rows();
        if rows.len() < 2 || source_position >= rows.len() {
            return;
        }

        let viewport = self.provider_list_state.viewport_bounds();
        let pointer_y = event.event.position.y;
        if viewport.size.height > px(0.) {
            let scroll_delta = if pointer_y < viewport.top() + px(PROVIDER_REORDER_EDGE_ZONE) {
                -PROVIDER_REORDER_SCROLL_STEP
            } else if pointer_y > viewport.bottom() - px(PROVIDER_REORDER_EDGE_ZONE) {
                PROVIDER_REORDER_SCROLL_STEP
            } else {
                0.
            };
            if scroll_delta != 0. {
                self.provider_list_state.scroll_by(px(scroll_delta));
                window.request_animation_frame();
            }
        }

        let current_target = self
            .provider_drag_state
            .as_ref()
            .map_or(source_position, |state| state.target_position);
        let mut target_position = current_target.min(rows.len() - 1);
        while target_position + 1 < rows.len() {
            let next_row = rows[target_position + 1].1;
            let Some(bounds) = self.provider_list_state.bounds_for_item(next_row) else {
                break;
            };
            if pointer_y > bounds.center().y + px(PROVIDER_REORDER_HYSTERESIS) {
                target_position += 1;
            } else {
                break;
            }
        }
        while target_position > 0 {
            let previous_row = rows[target_position - 1].1;
            let Some(bounds) = self.provider_list_state.bounds_for_item(previous_row) else {
                break;
            };
            if pointer_y < bounds.center().y - px(PROVIDER_REORDER_HYSTERESIS) {
                target_position -= 1;
            } else {
                break;
            }
        }
        if target_position == current_target {
            return;
        }

        let measured_rows: Vec<_> = rows
            .iter()
            .enumerate()
            .filter_map(|(position, (_, row_index))| {
                self.provider_list_state
                    .bounds_for_item(*row_index)
                    .map(|bounds| (position, bounds.top().as_f32(), bounds.size.height.as_f32()))
            })
            .collect();
        let fallback_pitch = measured_rows
            .windows(2)
            .find_map(|pair| {
                let pitch = pair[1].1 - pair[0].1;
                (pitch > 0.).then_some(pitch)
            })
            .or_else(|| measured_rows.first().map(|row| row.2))
            .unwrap_or(68.);
        let Some(&(anchor_position, anchor_top, _)) = measured_rows.first() else {
            return;
        };
        let row_tops: Vec<f32> = rows
            .iter()
            .enumerate()
            .map(|(position, (_, row_index))| {
                self.provider_list_state
                    .bounds_for_item(*row_index)
                    .map(|bounds| bounds.top().as_f32())
                    .unwrap_or_else(|| {
                        anchor_top
                            + (position as isize - anchor_position as isize) as f32 * fallback_pitch
                    })
            })
            .collect();
        let provider_ids: Vec<String> = rows.into_iter().map(|(id, _)| id).collect();
        if let Some(state) = self.provider_drag_state.as_mut() {
            state.retarget(
                target_position,
                &provider_ids,
                &row_tops,
                Instant::now(),
                cx.reduce_motion(),
            );
            cx.notify();
        }
    }

    fn drop_provider_drag(
        &mut self,
        dragged: &DraggedProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_position = self
            .provider_drag_state
            .as_ref()
            .filter(|state| state.source_id == dragged.id)
            .map_or(dragged.source_position, |state| state.target_position);
        let dropped_inside_list = self
            .provider_list_state
            .viewport_bounds()
            .contains(&window.mouse_position());
        self.provider_drag_state = None;

        let slots = self.sortable_provider_slots();
        let target_id = slots
            .get(target_position)
            .and_then(|slot| self.providers.get(*slot))
            .map(|provider| provider.id.clone());
        if dropped_inside_list && target_position != dragged.source_position {
            if let Some(target_id) = target_id {
                self.reorder_provider(dragged.id.clone(), target_id, cx);
                return;
            }
        }
        cx.notify();
    }

    fn provider_drag_offset(&self, provider_id: &str, reduce_motion: bool) -> f32 {
        self.provider_drag_state.as_ref().map_or(0., |state| {
            state.offset_for(provider_id, Instant::now(), reduce_motion)
        })
    }

    fn reorder_provider(&mut self, source_id: String, target_id: String, cx: &mut Context<Self>) {
        let slots = self.sortable_provider_slots();
        let Some(source_position) = slots
            .iter()
            .position(|slot| self.providers[*slot].id == source_id)
        else {
            return;
        };
        let Some(target_position) = slots
            .iter()
            .position(|slot| self.providers[*slot].id == target_id)
        else {
            return;
        };
        if !move_items_between_slots(
            &mut self.providers,
            &slots,
            source_position,
            target_position,
        ) {
            return;
        }

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
            self.notify_error(t(k::SHELL_PROVIDER_REORDER_FAILED), err.to_string(), cx);
        }
        self.reload(cx);
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match ProviderService::delete(&self.app, self.selected_app, &id) {
            Ok(()) => {
                self.notify_success(t(k::SHELL_PROVIDER_DELETED), cx);
            }
            Err(err) => self.notify_error(t(k::SHELL_PROVIDER_DELETE_FAILED), err.to_string(), cx),
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
                this.notify_success(t(k::SHELL_PROVIDER_SAVED), cx);
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

    fn render_sidebar_item(
        &self,
        app: AppType,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected_app == app && self.section == Section::Providers;
        let accent = Self::app_accent(app);
        div()
            .id(SharedString::from(format!("app-{}", app.as_str())))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(tf!(
                k::SHELL_SIDEBAR_OPEN_ARIA,
                name = Self::app_label(app)
            )))
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
                theme::sidebar_glass_muted(appearance)
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
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.section == section;
        let fg = if selected {
            theme::accent()
        } else {
            theme::sidebar_glass_muted(appearance)
        };
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(tf!(
                k::SHELL_SIDEBAR_OPEN_ARIA,
                name = label
            )))
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

    fn render_sidebar_group(label: &'static str, appearance: WindowAppearance) -> impl IntoElement {
        div()
            .mt_4()
            .mb_1()
            .px_3()
            .text_color(theme::sidebar_glass_muted(appearance))
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

    fn render_sidebar(
        &self,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let navigation = div()
            .id("sidebar-navigation")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll_handle)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.sidebar_scroll_handle.clone(),
            ))
            .pb_4()
            .child(Self::render_sidebar_group(
                raw(k::SHELL_SIDEBAR_GROUP_APPS),
                appearance,
            ))
            .child(
                div().flex().flex_col().gap_1().px_2().children(
                    Self::visible_apps()
                        .into_iter()
                        .map(|app| self.render_sidebar_item(app, appearance, cx)),
                ),
            )
            .child(Self::render_sidebar_group(
                raw(k::SHELL_SIDEBAR_GROUP_TOOLS),
                appearance,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item(
                        "nav-mcp",
                        raw(k::SHELL_SIDEBAR_NAV_MCP),
                        Section::Mcp,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-skills",
                        raw(k::SHELL_SIDEBAR_NAV_SKILLS),
                        Section::Skills,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-usage",
                        raw(k::SHELL_SIDEBAR_NAV_USAGE),
                        Section::Usage,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-sessions",
                        raw(k::SHELL_SIDEBAR_NAV_SESSIONS),
                        Section::Sessions,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-tools",
                        raw(k::SHELL_SIDEBAR_NAV_TOOLS),
                        Section::Tools,
                        appearance,
                        cx,
                    )),
            )
            .child(Self::render_sidebar_group(
                raw(k::SHELL_SIDEBAR_GROUP_NETWORK),
                appearance,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item(
                        "nav-gateway",
                        raw(k::SHELL_SIDEBAR_NAV_GATEWAY),
                        Section::Gateway,
                        appearance,
                        cx,
                    )),
            )
            .child(Self::render_sidebar_group(
                raw(k::SHELL_SIDEBAR_GROUP_SYSTEM),
                appearance,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .child(self.render_nav_item(
                        "nav-themes",
                        raw(k::SHELL_SIDEBAR_NAV_THEMES),
                        Section::Themes,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-settings",
                        raw(k::SHELL_SIDEBAR_NAV_SETTINGS),
                        Section::Settings,
                        appearance,
                        cx,
                    ))
                    .when(std::env::var_os("MS_GALLERY").is_some(), |col| {
                        col.child(self.render_nav_item(
                            "nav-gallery",
                            raw(k::SHELL_SIDEBAR_NAV_GALLERY),
                            Section::Gallery,
                            appearance,
                            cx,
                        ))
                    }),
            );

        div()
            .id("sidebar")
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w(px(252.))
            .flex_shrink_0()
            .bg(theme::sidebar_background())
            .text_color(theme::sidebar_glass_text(appearance))
            .border_r_1()
            .border_color(theme::border())
            .shadow_xs()
            .child(self.render_sidebar_drag_region(cx))
            .child(navigation)
            .child(crate::scrollbar::VerticalScrollbar::new(
                "sidebar-navigation-scrollbar",
                self.sidebar_scroll_handle.clone(),
            ))
    }

    fn render_provider_card(
        &self,
        provider: &Provider,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_current = !self.selected_app.is_additive_mode() && provider.id == self.current;
        let is_gateway = provider.is_local_gateway();
        let id = provider.id.clone();
        let edit_provider = provider.clone();
        let confirm_provider = provider.clone();
        let live_id = provider.id.clone();
        let sortable_slots = self.sortable_provider_slots();
        let sortable_position = sortable_slots
            .iter()
            .position(|slot| self.providers[*slot].id == provider.id);
        let is_sortable = sortable_slots.len() > 1 && sortable_position.is_some();
        let is_drag_source = self
            .provider_drag_state
            .as_ref()
            .is_some_and(|state| state.source_id == provider.id);
        let drag_offset = self.provider_drag_offset(&provider.id, cx.reduce_motion());
        let base_url = if is_gateway {
            self.gateway_via_station_line()
        } else {
            self.provider_base_url(provider)
        };
        let is_additive = self.selected_app.is_additive_mode();
        let is_in_live = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.live_config_managed)
            .unwrap_or(!is_additive);
        // In switch mode the gateway card needs a bound station before it can
        // be switched to; without one the button becomes a setup shortcut.
        let gateway_needs_setup =
            is_gateway && !is_additive && self.bound_station_route().is_none();
        // One key per branch, label and aria sentence together: a screen reader
        // cannot be handed a verb and a name to glue into a sentence itself.
        let (main_label_key, main_aria_key) = if gateway_needs_setup {
            (
                k::SHELL_ACTION_SETUP_RELAY,
                k::SHELL_ACTION_SETUP_RELAY_ARIA,
            )
        } else if is_additive {
            if is_in_live {
                (
                    k::SHELL_ACTION_REMOVE_FROM_TOOL,
                    k::SHELL_ACTION_REMOVE_FROM_TOOL_ARIA,
                )
            } else {
                (
                    k::SHELL_ACTION_ADD_TO_TOOL,
                    k::SHELL_ACTION_ADD_TO_TOOL_ARIA,
                )
            }
        } else if is_current {
            // Unreachable in switch mode (the current provider is filtered out
            // of the list); kept for the additive rendering path.
            (k::SHELL_ACTION_ENABLED, k::SHELL_ACTION_ENABLED_ARIA)
        } else {
            (k::SHELL_ACTION_SWITCH, k::SHELL_ACTION_SWITCH_ARIA)
        };
        let (edit_label_key, edit_aria_key) = if is_gateway {
            (k::SHELL_ACTION_MANAGE, k::SHELL_ACTION_MANAGE_ARIA)
        } else {
            (k::SHELL_ACTION_EDIT, k::SHELL_ACTION_EDIT_ARIA)
        };

        let drag_handle = sortable_position.map(|source_position| {
            let root = cx.entity();
            let dragged = DraggedProvider {
                id: provider.id.clone(),
                name: SharedString::from(provider.name.clone()),
                base_url: SharedString::from(base_url.clone()),
                source_position,
                app_icon: Self::app_icon(self.selected_app),
            };
            div()
                .id(SharedString::from(format!("drag-provider-{}", provider.id)))
                .role(gpui::Role::Button)
                .flex()
                .items_center()
                .justify_center()
                .w(px(32.))
                .flex_none()
                .cursor_grab()
                .aria_label(SharedString::from(tf!(
                    k::SHELL_CARD_DRAG_ARIA,
                    name = provider.name,
                    position = source_position + 1,
                    total = sortable_slots.len(),
                )))
                .aria_description(t(k::SHELL_CARD_DRAG_DESCRIPTION))
                .hover(|style| style.bg(theme::surface_hover()))
                .active(|style| style.bg(theme::accent_soft()))
                .tooltip(|_window, cx| cx.new(|_| ProviderDragTooltip).into())
                .child(icon(IconName::DragHandle, theme::muted(), 16.))
                .on_drag(dragged, move |provider, _offset, _window, cx| {
                    root.update(cx, |this, cx| {
                        this.begin_provider_drag(provider, cx);
                    });
                    cx.new(|_| ProviderDragPreview {
                        name: provider.name.clone(),
                        base_url: provider.base_url.clone(),
                        app_icon: provider.app_icon,
                    })
                })
        });

        components::panel()
            .id(SharedString::from(format!("provider-card-{}", provider.id)))
            .relative()
            .top(px(drag_offset))
            .opacity(if is_drag_source { 0. } else { 1. })
            .flex()
            .flex_row()
            .items_stretch()
            .w_full()
            .overflow_hidden()
            .border_color(if is_current {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|s| {
                s.border_color(theme::border_strong())
                    .shadow(theme::shadow_hover())
            })
            .when_some(
                if is_sortable { drag_handle } else { None },
                |card, handle| card.child(handle),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .px_4()
                    .py_3()
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
                                } else if is_gateway {
                                    IconName::Layers
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
                                        s.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::SHELL_BADGE_CURRENT),
                                        ))
                                    })
                                    .when(is_gateway, |s| {
                                        s.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::SHELL_BADGE_RELAY),
                                        ))
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
                    .flex_none()
                    .gap_2()
                    .py_3()
                    .pr_4()
                    .child(
                        components::action_button(
                            SharedString::from(format!("edit-{}", provider.id)),
                            t(edit_label_key),
                            false,
                        )
                        .aria_label(SharedString::from(tf!(edit_aria_key, name = provider.name)))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                if is_gateway {
                                    this.select_section(Section::Gateway, cx);
                                } else {
                                    this.open_edit_editor(edit_provider.clone(), cx);
                                }
                            },
                        )),
                    )
                    .when(!is_gateway, |row| {
                        row.child(
                            components::action_button_tone(
                                SharedString::from(format!("delete-{}", provider.id)),
                                t(k::SHELL_ACTION_DELETE),
                                ButtonTone::Danger,
                            )
                            .aria_label(SharedString::from(tf!(
                                k::SHELL_ACTION_DELETE_ARIA,
                                name = provider.name
                            )))
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.confirm_delete = Some(confirm_provider.clone());
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        components::action_button(
                            SharedString::from(format!("switch-{}", provider.id)),
                            t(main_label_key),
                            !(is_current || (is_additive && is_in_live)),
                        )
                        .aria_label(SharedString::from(tf!(main_aria_key, name = provider.name)))
                        .aria_selected(is_current || (is_additive && is_in_live))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                if gateway_needs_setup {
                                    this.select_section(Section::Gateway, cx);
                                } else if is_additive && is_in_live {
                                    this.do_remove_from_live(live_id.clone(), cx);
                                } else {
                                    this.do_switch(id.clone(), cx);
                                }
                            },
                        )),
                    ),
            )
    }

    /// Third line for gateway cards/hero: name the station actually serving
    /// the selected app instead of a generic explanation.
    fn gateway_via_station_line(&self) -> String {
        match self.bound_station_route() {
            Some(route) => tf!(k::SHELL_GATEWAY_VIA_STATION, name = route.name),
            None => raw(k::SHELL_GATEWAY_NO_STATION).to_string(),
        }
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
        let is_gateway = current.as_ref().is_some_and(Provider::is_local_gateway);

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
                if is_gateway {
                    IconName::Layers
                } else {
                    Self::app_icon(app)
                },
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
                                    .child(t(k::SHELL_HERO_CURRENT)),
                            )
                            .child(components::badge(
                                if is_gateway {
                                    BadgeTone::Accent
                                } else {
                                    BadgeTone::Success
                                },
                                if is_gateway {
                                    t(k::SHELL_BADGE_RELAY)
                                } else {
                                    t(k::SHELL_BADGE_DIRECT)
                                },
                            )),
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
                            .child(icon(
                                if is_gateway {
                                    IconName::Layers
                                } else {
                                    IconName::Cloud
                                },
                                theme::muted(),
                                12.,
                            ))
                            .child(div().text_color(theme::muted()).text_xs().truncate().child(
                                SharedString::from(if is_gateway {
                                    self.gateway_via_station_line()
                                } else {
                                    base_url
                                }),
                            )),
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
                        .child(t(k::SHELL_HERO_CURRENT)),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .child(t(k::SHELL_HERO_EMPTY_TITLE)),
                )
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(t(k::SHELL_HERO_EMPTY_HINT)),
                ),
        };

        let actions = current.map(|provider| {
            let edit_provider = provider.clone();
            div().flex().flex_row().items_center().gap_2().child(
                components::action_button(
                    SharedString::from(format!("hero-edit-{}", provider.id)),
                    if is_gateway {
                        t(k::SHELL_ACTION_MANAGE_RELAY)
                    } else {
                        t(k::SHELL_ACTION_EDIT)
                    },
                    false,
                )
                .aria_label(if is_gateway {
                    t(k::SHELL_ACTION_MANAGE_RELAY)
                } else {
                    SharedString::from(tf!(k::SHELL_ACTION_EDIT_ARIA, name = provider.name))
                })
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    if is_gateway {
                        this.select_section(Section::Gateway, cx);
                    } else {
                        this.open_edit_editor(edit_provider.clone(), cx);
                    }
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

    fn render_gateway_cta(&self, cx: &mut Context<Self>) -> impl IntoElement {
        components::panel()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_3()
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
                            .w(px(32.))
                            .h(px(32.))
                            .rounded_md()
                            .bg(theme::surface_hover())
                            .child(icon(IconName::Layers, theme::subtext(), 16.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::SHELL_GATEWAY_CTA_TITLE)),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(t(k::SHELL_GATEWAY_CTA_DESC)),
                            ),
                    ),
            )
            .child(
                components::button(
                    "setup-local-gateway",
                    t(k::SHELL_ACTION_SETUP_RELAY),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.select_section(Section::Gateway, cx);
                })),
            )
    }

    fn render_gateway_route_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.selected_app;
        let active_route_id = self
            .gateway_keys
            .iter()
            .find(|key| key.name == app.as_str() && key.enabled)
            .and_then(|key| key.route_id.as_deref());
        let routes: Vec<&GatewayRoute> = self
            .gateway_routes
            .iter()
            .filter(|route| {
                route.enabled
                    && route.id.starts_with(apply::STATION_ROUTE_PREFIX)
                    && route
                        .app_type
                        .as_deref()
                        .is_none_or(|bound| bound == app.as_str())
            })
            .collect();

        let manage_button = components::button(
            "manage-relay-stations",
            t(k::SHELL_ACTION_MANAGE_RELAY),
            ButtonTone::Ghost,
            ButtonSize::Sm,
        )
        .on_click(cx.listener(|this, _event, _window, cx| {
            this.select_section(Section::Gateway, cx);
        }));

        components::panel()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .p_4()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::SHELL_GATEWAY_ROUTES_TITLE)),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(t(k::SHELL_GATEWAY_ROUTES_DESC)),
                            ),
                    )
                    .child(manage_button),
            )
            .when(routes.is_empty(), |panel| {
                panel.child(
                    div()
                        .text_color(theme::muted())
                        .text_sm()
                        .child(t(k::SHELL_GATEWAY_ROUTES_EMPTY)),
                )
            })
            .children(routes.into_iter().map(|route| {
                let route_id = route.id.clone();
                let active = active_route_id == Some(route.id.as_str());
                let model = route
                    .default_model
                    .as_deref()
                    .map(|model| tf!(k::SHELL_GATEWAY_ROUTES_DEFAULT_MODEL, model = model))
                    .unwrap_or_else(|| {
                        if route.model_rules.is_empty() {
                            raw(k::SHELL_GATEWAY_ROUTES_PASSTHROUGH).to_string()
                        } else {
                            tf!(
                                k::SHELL_GATEWAY_ROUTES_MODEL_RULES,
                                count = route.model_rules.len()
                            )
                        }
                    });
                let button = components::button(
                    SharedString::from(format!("quick-station-{}", route.id)),
                    if active {
                        t(k::SHELL_GATEWAY_ROUTES_IN_USE)
                    } else {
                        t(k::SHELL_ACTION_SWITCH)
                    },
                    if active {
                        ButtonTone::Neutral
                    } else {
                        ButtonTone::Primary
                    },
                    ButtonSize::Sm,
                );
                let button = if active {
                    button
                        .cursor_not_allowed()
                        .opacity(components::DISABLED_OPACITY)
                } else {
                    button.on_click(cx.listener(move |this, _event, _window, cx| {
                        this.activate_gateway_route(route_id.clone(), cx);
                    }))
                };
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(if active {
                        theme::sidebar_selected()
                    } else {
                        theme::surface_hover()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(theme::text())
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(SharedString::from(route.name.clone())),
                                    )
                                    .when(active, |row| {
                                        row.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::SHELL_BADGE_CURRENT),
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(SharedString::from(model)),
                            ),
                    )
                    .child(button)
            }))
    }

    fn render_provider_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.selected_app;
        let current_is_gateway = self
            .providers
            .iter()
            .find(|provider| provider.id == self.current)
            .is_some_and(Provider::is_local_gateway);
        let plan = self.provider_row_plan();
        if self.provider_list_state.item_count() != plan.len() {
            self.provider_list_state.reset(plan.len());
        }

        let direct_count = self
            .providers
            .iter()
            .filter(|provider| !provider.is_local_gateway())
            .count();
        // A whole sentence per mode: the count sits inside the phrase, and no
        // locale has to build one out of a mode fragment and a tail.
        let subtitle = SharedString::from(if app.is_additive_mode() {
            tf!(k::SHELL_LIST_SUBTITLE_ADDITIVE, count = direct_count)
        } else if current_is_gateway {
            tf!(k::SHELL_LIST_SUBTITLE_RELAY, count = direct_count)
        } else {
            tf!(k::SHELL_LIST_SUBTITLE_DIRECT, count = direct_count)
        });

        let actions = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                components::icon_button(
                    "add-provider",
                    t(k::SHELL_LIST_ADD_LABEL),
                    IconName::Add,
                    true,
                )
                .aria_label(t(k::SHELL_LIST_ADD_ARIA))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_add_editor(cx);
                })),
            )
            .when(app_has_settings(app), |s| {
                s.child(
                    components::icon_button(
                        "app-settings-gear",
                        t(k::SHELL_LIST_APP_SETTINGS_LABEL),
                        IconName::Settings,
                        false,
                    )
                    .aria_label(t(k::SHELL_LIST_APP_SETTINGS_ARIA))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.open_app_settings(cx);
                    })),
                )
            });

        let list = gpui::list(
            self.provider_list_state.clone(),
            cx.processor(move |this, ix: usize, window, cx| {
                // Each row carries its own bottom spacing (the list draws no
                // inter-item gap); pb_3 mirrors wide_column's default gap.
                let block = div().w_full().pb_3();
                match plan.get(ix).copied() {
                    Some(ProviderRow::Hero) => {
                        block.child(this.render_active_hero(cx)).into_any_element()
                    }
                    Some(ProviderRow::GatewayRoutes) => block
                        .child(this.render_gateway_route_switcher(cx))
                        .into_any_element(),
                    Some(ProviderRow::DirectLabel) => block
                        .child(
                            div()
                                .pt_1()
                                .text_color(theme::subtext())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(t(k::SHELL_LIST_SECTION_DIRECT)),
                        )
                        .into_any_element(),
                    Some(ProviderRow::GatewayLabel) => block
                        .child(
                            div()
                                .pt_1()
                                .text_color(theme::subtext())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(t(k::SHELL_LIST_SECTION_RELAY)),
                        )
                        .into_any_element(),
                    Some(ProviderRow::GatewayCta) => {
                        block.child(this.render_gateway_cta(cx)).into_any_element()
                    }
                    Some(ProviderRow::EmptyState) => block
                        .child(
                            components::card().p_0().child(components::empty_state(
                                IconName::Folder,
                                t(k::SHELL_LIST_EMPTY_TITLE),
                                t(k::SHELL_LIST_EMPTY_DESC),
                                Some(
                                    components::icon_button(
                                        "empty-add-provider",
                                        t(k::SHELL_LIST_EMPTY_ACTION),
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
                    Some(ProviderRow::Card(pix)) => match this.providers.get(pix) {
                        Some(provider) => {
                            let card = this.render_provider_card(provider, window, cx);
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
            .child(layout::wide_virtual_body(
                "provider-list-body",
                list,
                &self.provider_list_state,
            ))
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

    /// Resolve a startup notice against the current locale.
    ///
    /// This runs per frame, so the banner is translated *now* rather than when
    /// the condition was detected — which for these notices is before the UI,
    /// and the locale, exist at all.
    fn render_startup_notice(notice: &StartupNotice) -> impl IntoElement {
        div()
            .id("startup-degradation-notice")
            .flex()
            .flex_row()
            .items_start()
            .gap_3()
            .px_5()
            .py_3()
            .flex_none()
            .border_b_1()
            .border_color(theme::yellow().alpha(0.32))
            .bg(theme::yellow_soft())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(24.))
                    .h(px(24.))
                    .flex_none()
                    .rounded_md()
                    .bg(theme::yellow().alpha(0.12))
                    .child(icon(IconName::Diamond, theme::yellow(), 15.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(notice.title()),
                    )
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_xs()
                            .child(SharedString::from(notice.message())),
                    ),
            )
    }
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.provider_drag_state.is_some() && !cx.has_active_drag() {
            self.provider_drag_state = None;
        }
        if self
            .provider_drag_state
            .as_ref()
            .is_some_and(|state| state.is_animating(Instant::now(), cx.reduce_motion()))
        {
            window.request_animation_frame();
        }

        let appearance = window.appearance();
        let main_content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h(px(0.))
            .when_some(self.startup_notice.as_ref(), |content, notice| {
                content.child(Self::render_startup_notice(notice))
            })
            .child(self.render_content(cx));
        div()
            .id("app-root")
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::window_base_background())
            .text_color(theme::text())
            .font_family("Helvetica Neue")
            .relative()
            .key_context("App")
            .on_action(cx.listener(Self::save_active))
            .on_action(cx.listener(Self::cancel_active))
            .on_action(cx.listener(Self::close_window))
            .on_drag_move::<DraggedProvider>(cx.listener(Self::handle_provider_drag_move))
            .on_drop(cx.listener(Self::drop_provider_drag))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(appearance, cx))
                    .child(main_content),
            )
            .child(self.render_content_drag_region(cx))
            .child(self.notifications.clone())
            .when_some(self.confirm_delete.clone(), |root, provider| {
                let delete_id = provider.id.clone();
                let message =
                    SharedString::from(tf!(k::SHELL_DELETE_MESSAGE, name = provider.name));
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::SHELL_DELETE_TITLE)))
                        .child(
                            components::modal_body()
                                .child(div().text_color(theme::subtext()).text_sm().child(message)),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "confirm-delete-cancel",
                                t(k::SHELL_DELETE_CANCEL),
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
                                t(k::SHELL_DELETE_CONFIRM),
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
                        .child(components::modal_header(t(k::SHELL_FIRST_RUN_TITLE)))
                        .child(
                            components::modal_body().child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(t(k::SHELL_FIRST_RUN_STORAGE))
                                    .child(t(k::SHELL_FIRST_RUN_BACKUP)),
                            ),
                        )
                        .child(components::modal_footer(vec![components::button(
                            "first-run-confirm",
                            t(k::SHELL_FIRST_RUN_CONFIRM),
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.acknowledge_first_run(cx);
                        }))
                        .into_any_element()])),
                ))
            })
    }
}

#[cfg(test)]
mod provider_reorder_tests {
    use super::{move_items_between_slots, reorder_slot_offsets};

    #[test]
    fn reorders_only_sortable_slots_and_preserves_hidden_rows() {
        let mut items = vec!["current", "alpha", "gateway", "beta", "gamma"];

        assert!(move_items_between_slots(&mut items, &[1, 3, 4], 2, 0));
        assert_eq!(items, ["current", "gamma", "gateway", "alpha", "beta"]);
    }

    #[test]
    fn supports_moving_a_provider_toward_the_end() {
        let mut items = vec!["alpha", "current", "beta", "gamma"];

        assert!(move_items_between_slots(&mut items, &[0, 2, 3], 0, 2));
        assert_eq!(items, ["beta", "current", "gamma", "alpha"]);
    }

    #[test]
    fn rejects_noop_or_invalid_moves() {
        let original = vec!["alpha", "beta", "gamma"];
        let mut items = original.clone();

        assert!(!move_items_between_slots(&mut items, &[0, 1, 2], 1, 1));
        assert!(!move_items_between_slots(&mut items, &[0, 4], 0, 1));
        assert_eq!(items, original);
    }

    #[test]
    fn dragging_down_moves_intervening_cards_up_one_slot() {
        let ids = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let offsets = reorder_slot_offsets(&ids, &[100., 170., 250.], 0, 2);

        assert_eq!(offsets.get("alpha"), None);
        assert_eq!(offsets.get("beta"), Some(&-70.));
        assert_eq!(offsets.get("gamma"), Some(&-80.));
    }

    #[test]
    fn dragging_up_moves_intervening_cards_down_one_slot() {
        let ids = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let offsets = reorder_slot_offsets(&ids, &[100., 170., 250.], 2, 0);

        assert_eq!(offsets.get("alpha"), Some(&70.));
        assert_eq!(offsets.get("beta"), Some(&80.));
        assert_eq!(offsets.get("gamma"), None);
    }
}
