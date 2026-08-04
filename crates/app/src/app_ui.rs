//! The OcHub root view: an app switcher sidebar plus a main panel that
//! can show the provider list, a provider editor, the settings panel, or the
//! gateway panel, all wired to live `ochub-core` data via an in-process `AppState`.

use std::{
    collections::HashMap,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, Context, Element, ElementId, Entity, FontWeight, GlobalElementId,
    InspectorElementId, LayoutId, ListAlignment, ListState, MouseButton, Pixels, ScrollHandle,
    SharedString, Window, WindowAppearance, div, point, prelude::*, px,
};
use ochub_core::db::import_ccswitch::{self, DetectedSource};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{GatewayKey, GatewayRoute};
use ochub_core::services::provider::{DriftConflict, DriftResolution, LiveDrift, ProviderService};
use ochub_core::{AppState, AppType, Provider, UsageResult};

use crate::about_view::AboutView;
use crate::app_settings_view::{AppSettingsEvent, AppSettingsView, app_has_settings};
use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::diff_view;
use crate::gallery_view::GalleryView;
use crate::gateway_view::GatewayView;
use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::mcp_view::McpView;
use crate::network_view::NetworkView;
use crate::notifications::{NotificationHost, NotificationLevel, ToastSource};
use crate::provider_editor::{EditorEvent, ProviderEditor};
use crate::remote::{ProviderSwitchHandle, WorkspaceBackend};
use crate::remote_view::{RemoteEvent, RemoteView};
use crate::sessions_view::SessionsView;
use crate::settings_view::SettingsView;
use crate::shell_menu;
use crate::shortcuts::{Cancel, Save};
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

pub(crate) fn open_settings_in_roots(cx: &mut App) {
    for window in cx.windows() {
        if let Some(root) = window.downcast::<AppRoot>() {
            let _ = root.update(cx, |root, _window, cx| {
                root.select_section(Section::Settings, cx);
            });
        }
    }
}

pub(crate) fn open_deeplink_in_roots(cx: &mut App, uri: &str) {
    let manifest = ochub_core::parse_deeplink_url(uri)
        .and_then(|request| ochub_core::decode_model_provider_request(&request))
        .map_err(|error| error.to_string());
    let mut delivered = false;
    for window in cx.windows() {
        let Some(root) = window.downcast::<AppRoot>() else {
            continue;
        };
        delivered = true;
        match manifest.clone() {
            Ok(manifest) => {
                let _ = root.update(cx, |root, _window, cx| {
                    root.select_section(Section::Gateway, cx);
                    root.gateway_view.update(cx, |view, cx| {
                        view.open_model_provider_import(manifest, cx);
                    });
                });
            }
            Err(error) => {
                let _ = root.update(cx, |root, _window, cx| {
                    root.report_shell_notice(
                        None,
                        NotificationLevel::Error,
                        raw(k::GATEWAY_DEEPLINK_TITLE).to_string(),
                        Some(error),
                        cx,
                    );
                });
            }
        }
    }
    if !delivered {
        match manifest {
            Ok(_) => log::warn!("model-provider deep link arrived before the main window existed"),
            Err(error) => log::warn!("invalid model-provider deep link: {error}"),
        }
    }
}

fn open_cherry_studio_deeplink(deeplink: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(deeplink).status();

    #[cfg(target_os = "windows")]
    let status = Command::new("cmd")
        .args(["/C", "start", "", deeplink])
        .status();

    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open").arg(deeplink).status();

    status
        .map_err(|error| error.to_string())
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("system URL opener exited with {status}"))
            }
        })
}

/// Which top-level section the main panel renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Providers,
    About,
    Mcp,
    Skills,
    Usage,
    Sessions,
    Tools,
    Themes,
    Settings,
    Gateway,
    Network,
    Remote,
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
            "network" | "proxy" => Self::Network,
            "remote" | "nodes" => Self::Remote,
            "gallery" => Self::Gallery,
            "about" => Self::About,
            _ => Self::Providers,
        }
    }
}

pub struct AppRoot {
    app: Arc<AppState>,
    selected_app: AppType,
    /// Enabled builtin apps in registry order. Refreshed only when settings
    /// emit `AppsChanged`; sidebar frames never clone global settings.
    visible_apps: Arc<[AppType]>,
    section: Section,
    providers: Vec<Provider>,
    /// Render-only provider data. Rebuilt when provider data changes so a
    /// scroll/animation frame never parses tool configuration or allocates
    /// display strings.
    provider_presentations: HashMap<String, ProviderPresentation>,
    /// Immutable provider-list plan shared by GPUI's list processor. Replaced
    /// only when the provider set/current selection changes.
    provider_rows: Arc<[ProviderRow]>,
    /// Direct-provider slots and their aligned row indices. Keeping these
    /// cached makes drag-move processing linear only in the rows whose bounds
    /// must actually be sampled.
    provider_sortable_slots: Arc<[usize]>,
    provider_sortable_rows: Arc<[usize]>,
    provider_sortable_positions: HashMap<String, usize>,
    provider_loaded_app: Option<AppType>,
    provider_loaded_scope: Option<String>,
    provider_reload_generation: u64,
    provider_action_in_flight: bool,
    /// Latest official-account quota result, keyed by `<app>:<provider>`.
    /// Kept presentation-only so a failed refresh never mutates provider data,
    /// and kept unformatted so the card line and the detail dialog read the
    /// same numbers.
    provider_quota_results: HashMap<String, UsageResult>,
    provider_quota_in_flight: Option<String>,
    /// Provider whose quota detail dialog is open, as `(provider id, name)`.
    provider_quota_detail: Option<(String, String)>,
    /// Set while the local Codex desktop launcher is waiting for its main CDP
    /// renderer. This is independent from provider mutations and only affects
    /// the Codex page header action.
    codex_launch_in_flight: bool,
    current: String,
    gateway_routes: Vec<GatewayRoute>,
    gateway_keys: Vec<GatewayKey>,
    notifications: Entity<NotificationHost>,
    /// Active provider editor (add or edit); when `Some`, replaces the list.
    editor: Option<Entity<ProviderEditor>>,
    /// Provider pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<ProviderDeleteTarget>,
    /// An external edit to the live config found while previewing a switch.
    /// Nothing has been written yet: the file is the user's to rule on.
    pending_drift: Option<PendingDrift>,
    /// One-time acknowledgement shown after the first successful launch.
    show_first_run_notice: bool,
    /// cc-switch data found on disk that the first-run notice is offering to
    /// import. `None` when there is nothing to import or the user was already
    /// asked once — the notice then keeps its plain single-button form.
    ccswitch_import: Option<DetectedSource>,
    /// Set while the import runs, so the modal cannot be answered twice.
    ccswitch_importing: bool,
    settings_view: Entity<SettingsView>,
    gateway_view: Entity<GatewayView>,
    network_view: Entity<NetworkView>,
    remote_view: Entity<RemoteView>,
    mcp_view: Entity<McpView>,
    skills_view: Entity<SkillsView>,
    usage_view: Entity<UsageView>,
    sessions_view: Entity<SessionsView>,
    tools_view: Entity<ToolsView>,
    theme_view: Entity<ThemeView>,
    about_view: Entity<AboutView>,
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
    workspace_scope_open: bool,
    active_remote_scope: Option<String>,
    /// Version of an available update, once a check has found one. Marks 关于 in
    /// the sidebar, and unlike the one-shot toast it persists for as long as the
    /// update does — the dot is the affordance a user comes back to.
    available_update: Option<SharedString>,
    /// Full result from the most recent automatic poll. The sidebar only needs
    /// the version above; About also needs install eligibility so its action
    /// can become “Update now” without making the user check a second time.
    automatic_update_check: Option<ochub_core::services::UpdateCheckResult>,
}

/// Cached row plan for the virtualized provider list. It is replaced only when
/// provider/current state changes; `Card` stores an index into `providers`.
#[derive(Clone, Copy)]
enum ProviderRow {
    Hero,
    DirectLabel,
    EmptyState,
    Card(usize),
}

#[derive(Clone)]
struct ProviderPresentation {
    name: SharedString,
    base_url: SharedString,
}

struct ProviderPageLoad {
    providers: Result<Vec<Provider>, String>,
    base_urls: HashMap<String, String>,
    current: String,
    gateway_routes: Vec<GatewayRoute>,
    gateway_keys: Vec<GatewayKey>,
}

impl ProviderPageLoad {
    async fn load(app: Arc<AppState>, backend: WorkspaceBackend, app_type: AppType) -> Self {
        let app_id = app_type.app_id();
        if !backend.is_remote()
            && let Err(err) = backend.import_live_providers(&app_id).await
        {
            log::debug!(
                "automatic provider discovery skipped for {}: {err}",
                app_type.as_str()
            );
        }

        let listed = backend.list_providers(&app_id).await;
        let current = listed
            .as_ref()
            .ok()
            .and_then(|providers| providers.iter().find(|provider| provider.current))
            .map(|provider| provider.id.clone())
            .unwrap_or_default();
        let base_urls = listed
            .as_ref()
            .map(|providers| {
                providers
                    .iter()
                    .map(|provider| (provider.id.clone(), provider.base_url.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let providers = match listed {
            Ok(items) => {
                let requests = items.into_iter().map(|item| {
                    let backend = backend.clone();
                    let app_id = app_id.clone();
                    async move {
                        backend
                            .get_provider(&app_id, &item.id)
                            .await
                            .map(|details| details.provider)
                    }
                });
                futures::future::try_join_all(requests)
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        };

        Self {
            providers,
            base_urls,
            current,
            gateway_routes: if backend.is_remote() {
                Vec::new()
            } else {
                app.db.get_gateway_routes().unwrap_or_default()
            },
            gateway_keys: if backend.is_remote() {
                Vec::new()
            } else {
                app.db.get_gateway_keys().unwrap_or_default()
            },
        }
    }
}

#[derive(Clone)]
struct ProviderDeleteTarget {
    id: String,
    name: SharedString,
}

/// A switch held at the door because the live config was edited outside OcHub.
#[derive(Clone)]
struct PendingDrift {
    provider_id: String,
    provider_name: String,
    /// The file the edits are in, abbreviated for display.
    path: SharedString,
    drift: LiveDrift,
}

enum ProviderGatewayConnectError {
    Config(String),
    Start(String),
    Switch(String),
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
    /// Paint offsets are indexed by sortable position. This keeps pointer-move
    /// animation bookkeeping allocation-free with respect to provider IDs.
    from_offsets: Vec<f32>,
    to_offsets: Vec<f32>,
}

impl ProviderDragState {
    fn new(dragged: &DraggedProvider) -> Self {
        Self {
            source_id: dragged.id.clone(),
            source_position: dragged.source_position,
            target_position: dragged.source_position,
            transition_started: Instant::now(),
            from_offsets: Vec::new(),
            to_offsets: Vec::new(),
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

    fn offset_for(&self, position: usize, now: Instant, reduce_motion: bool) -> f32 {
        let progress = self.animation_progress(now, reduce_motion);
        // Quintic ease-out: quick acknowledgement, then a quiet deceleration.
        let eased = 1. - (1. - progress).powi(5);
        let from = self.from_offsets.get(position).copied().unwrap_or(0.);
        let to = self.to_offsets.get(position).copied().unwrap_or(0.);
        from + (to - from) * eased
    }

    fn is_animating(&self, now: Instant, reduce_motion: bool) -> bool {
        if reduce_motion || self.animation_progress(now, false) >= 1. {
            return false;
        }
        let count = self.from_offsets.len().max(self.to_offsets.len());
        (0..count).any(|position| {
            let from = self.from_offsets.get(position).copied().unwrap_or(0.);
            let to = self.to_offsets.get(position).copied().unwrap_or(0.);
            (from - to).abs() > f32::EPSILON
        })
    }

    fn retarget(
        &mut self,
        target_position: usize,
        row_tops: &[f32],
        now: Instant,
        reduce_motion: bool,
    ) {
        let current_offsets = (0..row_tops.len())
            .map(|position| self.offset_for(position, now, reduce_motion))
            .collect();
        let desired_offsets = reorder_slot_offsets(row_tops, self.source_position, target_position);

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
    row_tops: &[f32],
    source_position: usize,
    target_position: usize,
) -> Vec<f32> {
    if source_position >= row_tops.len()
        || target_position >= row_tops.len()
        || source_position == target_position
    {
        return vec![0.; row_tops.len()];
    }

    let mut offsets = vec![0.; row_tops.len()];
    if source_position < target_position {
        for position in (source_position + 1)..=target_position {
            offsets[position] = row_tops[position - 1] - row_tops[position];
        }
    } else {
        for position in target_position..source_position {
            offsets[position] = row_tops[position + 1] - row_tops[position];
        }
    }
    offsets
}

/// Moves a subtree during prepaint, after layout has completed. Unlike
/// `relative().top(...)`, this does not make Taffy recompute card layout on
/// every animation frame, while hitboxes and clipping still follow the card.
struct PaintOffsetY {
    offset: Pixels,
    child: AnyElement,
}

impl PaintOffsetY {
    fn new(offset: Pixels, child: impl IntoElement) -> Self {
        Self {
            offset,
            child: child.into_any_element(),
        }
    }
}

impl IntoElement for PaintOffsetY {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PaintOffsetY {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_element_offset(point(px(0.), self.offset), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
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
        if self.confirm_delete.is_some() || self.pending_drift.is_some() {
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
        // Read-only, so it closes before anything with unsaved state below it.
        if self.provider_quota_detail.take().is_some() {
            cx.notify();
            return;
        }
        // Escape leaves the file exactly as the user left it.
        if self.pending_drift.take().is_some() {
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

    /// The badge state to start with, before this process has checked anything.
    ///
    /// Checks are gated to one a day, so a launch usually performs none at all —
    /// without seeding, restarting the app would hide a pending update until the
    /// gate reopened. `skipped_update_version` is the newest release any earlier
    /// check announced, and comparing it against the running build is what
    /// retires the badge once that version is installed.
    ///
    /// `auto_update_check` is honoured here rather than only at the poll: with
    /// the switch off there is nothing to seed *from* that the user asked for,
    /// and a version announced while it was still on must not outlive it.
    fn seeded_badge(auto_update_check: bool, announced: Option<String>) -> Option<SharedString> {
        if !auto_update_check {
            return None;
        }
        announced
            .filter(|version| ochub_core::services::update::is_newer_than_current(version))
            .map(SharedString::from)
    }

    /// The version a badge should advertise, or `None` when a check found
    /// nothing to install. A release that reports no version still gets a
    /// badge — knowing *that* an update exists is the actionable part.
    fn badge_version(info: &ochub_core::services::UpdateCheckResult) -> Option<SharedString> {
        info.has_update.then(|| {
            info.latest_version
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::from(raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION)))
        })
    }

    fn set_available_update(&mut self, version: Option<SharedString>, cx: &mut Context<Self>) {
        if self.available_update != version {
            self.available_update = version;
            cx.notify();
        }
    }

    /// Mirror the About page's manual check into the badge.
    ///
    /// Observed rather than pushed so that both outcomes land: finding an
    /// update lights the badge, and a check that comes back up to date (the
    /// state right after installing) clears it. A page that has not checked
    /// reports `None` and must not clear what the background poll found.
    ///
    /// The 自动检查更新 switch lives on that same page, so every notification is
    /// also the moment to re-read it. Turning it off is a request to stop being
    /// told about releases, and the dot is a telling — it goes immediately,
    /// without waiting for a restart, and comes back if the switch does. The
    /// About row itself still reports whatever a manual check found; only the
    /// mark that follows the user around the sidebar is withdrawn.
    fn observe_about_update_checks(&self, cx: &mut Context<Self>) {
        cx.observe(&self.about_view, |this, about, cx| {
            let settings = ochub_core::settings::get_settings();
            if !settings.auto_update_check {
                this.set_available_update(None, cx);
                return;
            }
            let badge = match about.read(cx).last_update_check() {
                Some(info) => Self::badge_version(&info),
                // Nothing checked here: fall back to the seed, so re-enabling
                // the switch restores a dot this observer had cleared.
                None => Self::seeded_badge(true, settings.skipped_update_version),
            };
            this.set_available_update(badge, cx);
        })
        .detach();
    }

    /// Poll for a new release in the background, at most once a day.
    ///
    /// The first check is delayed so it competes with nothing at launch, and
    /// the loop then re-evaluates hourly — the day-level spacing itself lives
    /// in `auto_check_due`, keyed off a persisted timestamp, so quitting and
    /// relaunching does not re-check every time.
    ///
    /// A found version is announced once and then recorded, so the same release
    /// never nags twice. Nothing is downloaded here; installing stays an
    /// explicit click in settings.
    fn spawn_auto_update_check(&self, cx: &mut Context<Self>) {
        /// Long enough that startup work is done before a request goes out.
        const FIRST_DELAY: Duration = Duration::from_secs(30);
        /// The day-level gate is `auto_check_due`; this only decides how often
        /// we re-ask it, so a process left running for days still checks.
        const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_DELAY).await;
            loop {
                let settings = ochub_core::settings::get_settings();
                let now = chrono::Utc::now().timestamp();
                if ochub_core::services::update::auto_check_due(
                    settings.auto_update_check,
                    settings.last_update_check_at,
                    now,
                ) {
                    let result = crate::core_async::run(
                        ochub_core::services::update::check_for_updates(None),
                    )
                    .await;
                    match result {
                        Ok(info) => {
                            let notify = ochub_core::services::update::should_notify(
                                &info,
                                settings.skipped_update_version.as_deref(),
                            );
                            let latest = info.latest_version.clone();
                            let badge = Self::badge_version(&info);
                            // Record before announcing: a failure to draw the
                            // toast must not turn into a check every hour.
                            let _ = ochub_core::settings::mutate_settings(|settings| {
                                settings.last_update_check_at = Some(now);
                                if notify {
                                    settings.skipped_update_version = latest.clone();
                                }
                            });
                            let version = info.latest_version.clone().unwrap_or_else(|| {
                                raw(k::SETTINGS_UPDATE_UNKNOWN_VERSION).to_string()
                            });
                            // The badge is set on every check, not only when a
                            // toast goes out: the announcement is once per
                            // version, the badge lasts as long as the update.
                            if this
                                .update(cx, |this, cx| {
                                    this.automatic_update_check = Some(info.clone());
                                    this.set_available_update(badge, cx);
                                    if this.active_remote_scope.is_none() {
                                        this.about_view.update(cx, |about, cx| {
                                            about.adopt_automatic_update_check(info.clone(), cx);
                                        });
                                    }
                                    if notify {
                                        this.notifications.update(cx, |host, cx| {
                                            host.info(
                                                tf!(
                                                    k::SETTINGS_UPDATE_AVAILABLE,
                                                    latest = version,
                                                    current = info.current_version
                                                ),
                                                cx,
                                            );
                                        });
                                    }
                                })
                                .is_err()
                            {
                                // The window is gone; so is the loop's point.
                                return;
                            }
                        }
                        // Offline is the common case here, so this stays a log
                        // line rather than a toast the user did not ask for.
                        Err(error) => log::debug!("[Update] automatic check failed: {error}"),
                    }
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let settings_view = cx.new(|cx| SettingsView::new(app.clone(), cx));
        let gateway_view = cx.new(|cx| GatewayView::new(app.clone(), cx));
        let network_view = cx.new(|cx| NetworkView::new(app.clone(), cx));
        let remote_view = cx.new(RemoteView::new);
        let mcp_view = cx.new(|cx| McpView::new(app.clone(), cx));
        let notifications = cx.new(|_| NotificationHost::new());
        let skills_view = cx.new(|cx| SkillsView::new(app.clone(), cx));
        let usage_view = cx.new(|cx| UsageView::new(app.clone(), cx));
        let sessions_view = cx.new(|cx| SessionsView::new(app.clone(), cx));
        let tools_view = cx.new(|cx| ToolsView::new(app.clone(), cx));
        let theme_view = cx.new(ThemeView::new);
        let about_view = cx.new(|cx| AboutView::new(app.clone(), cx));
        let gallery_view = cx.new(GalleryView::new);
        let show_first_run_notice = crate::shell_support::first_run_notice_pending();
        let initial_section = Section::from_env();
        let enabled = Self::load_visible_apps();
        let initial_app = std::env::var("MS_START_APP")
            .ok()
            .and_then(|value| value.parse::<AppType>().ok())
            .filter(|app| enabled.contains(app))
            .or_else(|| enabled.first().copied())
            .unwrap_or(AppType::Claude);
        let app_settings_view = cx.new(|cx| AppSettingsView::new(app.clone(), initial_app, cx));
        let mut this = Self {
            app,
            selected_app: initial_app,
            visible_apps: enabled.into(),
            section: initial_section,
            providers: Vec::new(),
            provider_presentations: HashMap::new(),
            provider_rows: Vec::new().into(),
            provider_sortable_slots: Vec::new().into(),
            provider_sortable_rows: Vec::new().into(),
            provider_sortable_positions: HashMap::new(),
            provider_loaded_app: None,
            provider_loaded_scope: None,
            provider_reload_generation: 0,
            provider_action_in_flight: false,
            provider_quota_results: HashMap::new(),
            provider_quota_in_flight: None,
            provider_quota_detail: None,
            codex_launch_in_flight: false,
            current: String::new(),
            gateway_routes: Vec::new(),
            gateway_keys: Vec::new(),
            notifications,
            editor: None,
            confirm_delete: None,
            pending_drift: None,
            show_first_run_notice,
            ccswitch_import: None,
            ccswitch_importing: false,
            settings_view,
            gateway_view,
            network_view,
            remote_view,
            mcp_view,
            skills_view,
            usage_view,
            sessions_view,
            tools_view,
            theme_view,
            about_view,
            gallery_view,
            app_settings_view,
            showing_app_settings: false,
            provider_list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            provider_drag_state: None,
            sidebar_scroll_handle: ScrollHandle::new(),
            workspace_scope_open: false,
            active_remote_scope: None,
            available_update: {
                let settings = ochub_core::settings::get_settings();
                Self::seeded_badge(settings.auto_update_check, settings.skipped_update_version)
            },
            automatic_update_check: None,
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
        cx.subscribe(&this.remote_view, |this, view, event, cx| {
            if let RemoteEvent::ManageRequested { id } = event {
                this.select_remote_scope(id.clone(), cx);
                return;
            }
            let RemoteEvent::ConnectionChanged { id, connected } = event else {
                unreachable!();
            };
            if this.active_remote_scope.as_deref() != Some(id.as_str()) {
                return;
            }
            this.editor = None;
            this.provider_loaded_scope = None;
            if *connected {
                let enabled = view.read(cx).enabled_builtin_apps();
                if !enabled.is_empty() {
                    this.visible_apps = enabled.into();
                    if !this.visible_apps.contains(&this.selected_app) {
                        this.selected_app = this.visible_apps[0];
                    }
                }
                this.reload(cx);
                if this.section == Section::Mcp {
                    this.reload_mcp_workspace(cx);
                }
                if this.section == Section::Skills {
                    this.reload_skills_workspace(cx);
                }
                if this.section == Section::Usage {
                    this.reload_usage_workspace(cx);
                }
                if this.section == Section::Sessions {
                    this.reload_sessions_workspace(cx);
                }
                if this.section == Section::Gateway {
                    this.reload_gateway_workspace(cx);
                }
                if this.section == Section::Network {
                    this.reload_network_workspace(cx);
                }
                if this.section == Section::Settings {
                    this.reload_settings_workspace(cx);
                }
                if this.section == Section::Tools {
                    this.reload_tools_workspace(cx);
                }
                if this.section == Section::About {
                    this.reload_about_workspace(cx);
                }
                if this.section == Section::Providers && this.showing_app_settings {
                    this.open_app_settings(cx);
                }
            } else {
                this.clear_provider_page();
                if this.section == Section::Mcp {
                    this.reload_mcp_workspace(cx);
                }
                if this.section == Section::Skills {
                    this.reload_skills_workspace(cx);
                }
                if this.section == Section::Usage {
                    this.reload_usage_workspace(cx);
                }
                if this.section == Section::Sessions {
                    this.reload_sessions_workspace(cx);
                }
                if this.section == Section::Gateway {
                    this.reload_gateway_workspace(cx);
                }
                if this.section == Section::Network {
                    this.reload_network_workspace(cx);
                }
                if this.section == Section::Settings {
                    this.reload_settings_workspace(cx);
                }
                if this.section == Section::Tools {
                    this.reload_tools_workspace(cx);
                }
                if this.section == Section::About {
                    this.reload_about_workspace(cx);
                }
                if this.section == Section::Providers && this.showing_app_settings {
                    this.app_settings_view
                        .update(cx, |view, cx| view.set_workspace_unavailable(cx));
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&this.settings_view, |this, _view, event, cx| match event {
            crate::settings_view::SettingsEvent::AppsChanged => {
                if this.active_remote_scope.is_some() {
                    this.reload_visible_workspace_apps(cx);
                } else {
                    this.visible_apps = Self::load_visible_apps().into();
                    this.skills_view
                        .update(cx, |view, cx| view.refresh_apps(cx));
                    this.ensure_valid_selection(cx);
                }
                this.mcp_view.update(cx, |view, cx| view.refresh_apps(cx));
                cx.notify();
            }
            crate::settings_view::SettingsEvent::LocaleChanged => {
                this.relocalize(cx);
            }
            crate::settings_view::SettingsEvent::DataImported => {
                if this.active_remote_scope.is_some() {
                    this.reload_visible_workspace_apps(cx);
                    this.reload(cx);
                    this.reload_mcp_workspace(cx);
                    this.reload_skills_workspace(cx);
                    this.reload_usage_workspace(cx);
                    this.reload_sessions_workspace(cx);
                    this.reload_gateway_workspace(cx);
                    this.reload_settings_workspace(cx);
                    this.reload_tools_workspace(cx);
                    cx.notify();
                } else {
                    this.reload_after_ccswitch_import(cx);
                }
            }
        })
        .detach();
        cx.subscribe(&this.gateway_view, |this, _view, event, cx| match event {
            crate::gateway_view::GatewayEvent::OpenProviders(app) => {
                this.selected_app = *app;
                this.select_section(Section::Providers, cx);
            }
            crate::gateway_view::GatewayEvent::ImportFinished(app) => {
                if let Some(app) = app.filter(|app| this.visible_apps.contains(app)) {
                    this.selected_app = app;
                }
                this.select_section(Section::Providers, cx);
                this.reload(cx);
            }
            crate::gateway_view::GatewayEvent::ImportCancelled => {
                this.select_section(Section::Providers, cx);
            }
        })
        .detach();
        this.connect_toast_sources(cx);
        this.observe_about_update_checks(cx);
        this.reload(cx);
        if show_first_run_notice {
            this.detect_pending_ccswitch_import(cx);
        }
        this.spawn_auto_update_check(cx);
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
            Section::Mcp => this.mcp_view.update(cx, |v, cx| v.reload(cx)),
            Section::Skills => this.reload_skills_workspace(cx),
            Section::Gateway => this.reload_gateway_workspace(cx),
            Section::Network => this.reload_network_workspace(cx),
            Section::Remote => this.remote_view.update(cx, |v, cx| v.reload(cx)),
            Section::Usage => this.reload_usage_workspace(cx),
            Section::Sessions => this.reload_sessions_workspace(cx),
            Section::Tools => this.reload_tools_workspace(cx),
            Section::Settings => this.reload_settings_workspace(cx),
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
        Self::observe_toasts(&self.network_view, &self.notifications, cx);
        Self::observe_toasts(&self.remote_view, &self.notifications, cx);
        Self::observe_toasts(&self.mcp_view, &self.notifications, cx);
        Self::observe_toasts(&self.skills_view, &self.notifications, cx);
        Self::observe_toasts(&self.usage_view, &self.notifications, cx);
        Self::observe_toasts(&self.sessions_view, &self.notifications, cx);
        Self::observe_toasts(&self.tools_view, &self.notifications, cx);
        Self::observe_toasts(&self.theme_view, &self.notifications, cx);
        Self::observe_toasts(&self.about_view, &self.notifications, cx);
        Self::observe_toasts(&self.app_settings_view, &self.notifications, cx);

        Self::forward_toast(&self.settings_view, &self.notifications, cx);
        Self::forward_toast(&self.gateway_view, &self.notifications, cx);
        Self::forward_toast(&self.network_view, &self.notifications, cx);
        Self::forward_toast(&self.remote_view, &self.notifications, cx);
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
            Section::Network => Self::forward_toast(&self.network_view, &self.notifications, cx),
            Section::Remote => Self::forward_toast(&self.remote_view, &self.notifications, cx),
            Section::Mcp => Self::forward_toast(&self.mcp_view, &self.notifications, cx),
            Section::Skills => Self::forward_toast(&self.skills_view, &self.notifications, cx),
            Section::Usage => Self::forward_toast(&self.usage_view, &self.notifications, cx),
            Section::Sessions => Self::forward_toast(&self.sessions_view, &self.notifications, cx),
            Section::Tools => Self::forward_toast(&self.tools_view, &self.notifications, cx),
            Section::Themes => Self::forward_toast(&self.theme_view, &self.notifications, cx),
            Section::About => Self::forward_toast(&self.about_view, &self.notifications, cx),
            Section::Providers | Section::Gallery => {}
        }
    }

    fn load_visible_apps() -> Vec<AppType> {
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
            Section::About => IconName::Diamond,
            Section::Settings => IconName::Settings,
            Section::Gateway => IconName::Cloud,
            Section::Network => IconName::Globe,
            Section::Remote => IconName::Desktop,
            Section::Providers => IconName::Cloud,
            Section::Gallery => IconName::Layers,
        }
    }

    /// (Re)load providers + current id for the selected app from the store.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let app_type = self.selected_app;
        let scope = self
            .active_remote_scope
            .clone()
            .unwrap_or_else(|| "local".to_string());
        if self.provider_loaded_app != Some(app_type)
            || self.provider_loaded_scope.as_deref() != Some(scope.as_str())
        {
            self.clear_provider_page();
        }

        self.provider_reload_generation = self.provider_reload_generation.wrapping_add(1);
        let generation = self.provider_reload_generation;
        let app = self.app.clone();
        let Some(backend) = self.workspace_backend(cx) else {
            cx.notify();
            return;
        };
        let loaded_scope = scope.clone();
        cx.spawn(async move |this, cx| {
            let data = crate::core_async::run(ProviderPageLoad::load(app, backend, app_type)).await;
            this.update(cx, |this, cx| {
                let active_scope = this.active_remote_scope.as_deref().unwrap_or("local");
                if generation != this.provider_reload_generation
                    || app_type != this.selected_app
                    || loaded_scope != active_scope
                {
                    return;
                }
                match data.providers {
                    Ok(providers) => this.providers = providers,
                    Err(error) => {
                        this.providers.clear();
                        this.notify_error(t(k::SHELL_PROVIDER_LOAD_FAILED), error, cx);
                    }
                }
                this.current = data.current;
                this.gateway_routes = data.gateway_routes;
                this.gateway_keys = data.gateway_keys;
                this.rebuild_provider_render_cache(&data.base_urls);
                this.provider_loaded_app = Some(app_type);
                this.provider_loaded_scope = Some(loaded_scope);
                // Row heights and count follow the newly applied snapshot.
                this.provider_list_state.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn clear_provider_page(&mut self) {
        self.providers.clear();
        self.provider_presentations.clear();
        self.provider_rows = Vec::new().into();
        self.provider_sortable_slots = Vec::new().into();
        self.provider_sortable_rows = Vec::new().into();
        self.provider_sortable_positions.clear();
        self.current.clear();
        self.gateway_routes.clear();
        self.gateway_keys.clear();
        self.provider_loaded_app = None;
        self.provider_loaded_scope = None;
        self.provider_list_state.reset(0);
    }

    fn workspace_backend(&self, cx: &App) -> Option<WorkspaceBackend> {
        match self.active_remote_scope.as_deref() {
            Some(id) => self.remote_view.read(cx).backend_for_scope(id),
            None => Some(WorkspaceBackend::local(self.app.clone())),
        }
    }

    fn reload_mcp_workspace(&mut self, cx: &mut Context<Self>) {
        let apps = self
            .visible_apps
            .iter()
            .copied()
            .filter(|app| {
                matches!(
                    app,
                    AppType::Claude
                        | AppType::Codex
                        | AppType::GrokBuild
                        | AppType::OpenCode
                        | AppType::Hermes
                )
            })
            .collect::<Vec<_>>();
        if let Some(backend) = self.workspace_backend(cx) {
            self.mcp_view
                .update(cx, |view, cx| view.set_workspace(backend, apps, cx));
        } else {
            self.mcp_view
                .update(cx, |view, cx| view.set_workspace_unavailable(apps, cx));
        }
    }

    fn reload_skills_workspace(&mut self, cx: &mut Context<Self>) {
        let apps = self
            .visible_apps
            .iter()
            .copied()
            .filter(|app| {
                matches!(
                    app,
                    AppType::Claude | AppType::Codex | AppType::OpenCode | AppType::Hermes
                )
            })
            .collect::<Vec<_>>();
        if let Some(backend) = self.workspace_backend(cx) {
            self.skills_view
                .update(cx, |view, cx| view.set_workspace(backend, apps, cx));
        } else {
            self.skills_view
                .update(cx, |view, cx| view.set_workspace_unavailable(apps, cx));
        }
    }

    fn reload_usage_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.usage_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.usage_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_sessions_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.sessions_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.sessions_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_gateway_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.gateway_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.gateway_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_network_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.network_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.network_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_settings_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.settings_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.settings_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_tools_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            self.tools_view
                .update(cx, |view, cx| view.set_workspace(backend, cx));
        } else {
            self.tools_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_about_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = self.workspace_backend(cx) {
            let automatic_update_check = self
                .active_remote_scope
                .is_none()
                .then(|| self.automatic_update_check.clone())
                .flatten();
            self.about_view.update(cx, |view, cx| {
                view.set_workspace(backend, cx);
                if let Some(info) = automatic_update_check {
                    view.adopt_automatic_update_check(info, cx);
                }
            });
        } else {
            self.about_view
                .update(cx, |view, cx| view.set_workspace_unavailable(cx));
        }
    }

    fn reload_visible_workspace_apps(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.workspace_backend(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move { backend.list_apps().await }).await;
            this.update(cx, |this, cx| {
                if let Ok(apps) = result {
                    let enabled = apps
                        .into_iter()
                        .filter(|app| app.enabled)
                        .filter_map(|app| app.id.parse::<AppType>().ok())
                        .collect::<Vec<_>>();
                    if !enabled.is_empty() {
                        this.visible_apps = enabled.into();
                        this.ensure_valid_selection(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        if self.visible_apps.contains(&self.selected_app) {
            return;
        }
        let Some(first) = self.visible_apps.first().copied() else {
            return;
        };
        self.selected_app = first;
        self.editor = None;
        self.showing_app_settings = false;
        self.reload(cx);
        cx.notify();
    }

    fn select_app(&mut self, app: AppType, cx: &mut Context<Self>) {
        if !self.visible_apps.contains(&app) {
            return;
        }
        let changed = self.selected_app != app || self.section != Section::Providers;
        if changed || self.showing_app_settings {
            self.selected_app = app;
            self.section = Section::Providers;
            self.editor = None;
            // Quota results are keyed by app, so a dialog left open would be
            // showing another app's numbers under this app's provider name.
            self.provider_quota_detail = None;
            self.showing_app_settings = false;
            self.reload(cx);
            cx.notify();
        }
    }

    fn official_quota_key(app: AppType, provider_id: &str) -> String {
        format!("{}:{provider_id}", app.as_str())
    }

    fn is_official_quota_provider(app: AppType, provider: &Provider) -> bool {
        provider.category.as_deref() == Some("official")
            && matches!(
                app,
                AppType::Claude | AppType::Codex | AppType::KimiCode | AppType::GrokBuild
            )
    }

    /// The quota line for an official-account provider.
    ///
    /// The hero and the list row render the same one from the same stored
    /// result, so the card at the top of the page can never disagree with the
    /// row for the same provider further down.
    fn render_quota_line(
        &self,
        id_prefix: &str,
        provider_id: &str,
        name: &SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = Self::official_quota_key(self.selected_app, provider_id);
        let entries = self
            .provider_quota_results
            .get(&key)
            .map(|result| crate::quota::parse(result).map_err(|_| ()));
        let loading = self.provider_quota_in_flight.as_deref() == Some(key.as_str());
        let state = match (loading, &entries) {
            (true, _) => crate::quota::QuotaState::Loading,
            (false, None) => crate::quota::QuotaState::Idle,
            (false, Some(Ok(entries))) => crate::quota::QuotaState::Ready(entries),
            (false, Some(Err(()))) => crate::quota::QuotaState::Failed,
        };
        let refresh_id = provider_id.to_string();
        let refresh_name = name.to_string();
        let detail_id = provider_id.to_string();
        let detail_name = name.to_string();
        crate::quota::line(
            &format!("{id_prefix}-quota-{provider_id}"),
            name,
            state,
            cx.listener(move |this, _event, _window, cx| {
                this.do_query_provider_quota(refresh_id.clone(), refresh_name.clone(), cx);
            }),
            cx.listener(move |this, _event, _window, cx| {
                this.provider_quota_detail = Some((detail_id.clone(), detail_name.clone()));
                cx.notify();
            }),
        )
        .into_any_element()
    }

    /// The quota detail dialog: what the card's one-line summary had to cut.
    fn render_quota_detail(
        &self,
        provider_id: String,
        name: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = Self::official_quota_key(self.selected_app, &provider_id);
        let entries = self
            .provider_quota_results
            .get(&key)
            .and_then(|result| crate::quota::parse(result).ok())
            .unwrap_or_default();
        let loading = self.provider_quota_in_flight.as_deref() == Some(key.as_str());
        let refresh_name = name.clone();
        components::modal_overlay(
            components::modal_card()
                .child(components::modal_header(t(k::QUOTA_DETAIL_TITLE)))
                .child(
                    components::modal_body()
                        .child(
                            div()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(SharedString::from(name)),
                        )
                        .child(crate::quota::detail_body(&entries)),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "provider-quota-detail-refresh",
                        if loading {
                            t(k::QUOTA_ACTION_QUERYING)
                        } else {
                            t(k::QUOTA_DETAIL_REFRESH)
                        },
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.do_query_provider_quota(provider_id.clone(), refresh_name.clone(), cx);
                    }))
                    .into_any_element(),
                    components::button(
                        "provider-quota-detail-close",
                        t(k::QUOTA_DETAIL_CLOSE),
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.provider_quota_detail = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                ])),
        )
        // Read-only dialog: clicking away is the fastest way out, and the card
        // itself is occluded so only the backdrop dismisses.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                this.provider_quota_detail = None;
                cx.notify();
            }),
        )
        .into_any_element()
    }

    fn do_query_provider_quota(
        &mut self,
        provider_id: String,
        provider_name: String,
        cx: &mut Context<Self>,
    ) {
        let key = Self::official_quota_key(self.selected_app, &provider_id);
        if self.provider_quota_in_flight.is_some() {
            return;
        }
        let Some(backend) = self.workspace_backend(cx) else {
            self.notify_error(
                SharedString::from(tf!(k::SHELL_QUOTA_FAILED, name = provider_name)),
                "remote workspace is not connected".to_string(),
                cx,
            );
            return;
        };

        // No "querying…" notification: the card's own line says so, in place.
        self.provider_quota_in_flight = Some(key.clone());
        let app_id = self.selected_app.app_id();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .provider_network_operation(
                        ochub_protocol::methods::PROVIDER_QUOTA,
                        &app_id,
                        &provider_id,
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<UsageResult>(value)
                            .map_err(|error| error.to_string())
                    })
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_quota_in_flight = None;
                // Success is reported by the card line itself; only the failure
                // reason needs a notification, since the line has no room for it.
                let result = match result {
                    Ok(result) => result,
                    Err(error) => UsageResult {
                        success: false,
                        data: None,
                        error: Some(error),
                    },
                };
                if let Err(error) = crate::quota::parse(&result) {
                    this.notify_error(
                        SharedString::from(tf!(k::SHELL_QUOTA_FAILED, name = provider_name)),
                        error,
                        cx,
                    );
                }
                this.provider_quota_results.insert(key, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn open_app_settings(&mut self, cx: &mut Context<Self>) {
        let app = self.selected_app;
        if let Some(backend) = self.workspace_backend(cx) {
            self.app_settings_view.update(cx, |view, cx| {
                view.set_workspace(backend, cx);
                view.reload_for(app, cx);
            });
        } else {
            self.app_settings_view.update(cx, |view, cx| {
                view.set_workspace_unavailable(cx);
                view.reload_for(app, cx);
            });
        }
        self.editor = None;
        self.showing_app_settings = true;
        cx.notify();
    }

    fn launch_codex_app(&mut self, cx: &mut Context<Self>) {
        if self.codex_launch_in_flight || self.active_remote_scope.is_some() {
            return;
        }
        self.codex_launch_in_flight = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(ochub_core::apps::codex_app_launcher::launch_codex_app())
                    .await
                    .map_err(|error| error.to_string());
            this.update(cx, |this, cx| {
                this.codex_launch_in_flight = false;
                match result {
                    Ok(launch) if launch.reused => {
                        this.notify_success(t(k::SHELL_CODEX_LAUNCH_REUSED), cx)
                    }
                    Ok(_) => this.notify_success(t(k::SHELL_CODEX_LAUNCH_SUCCEEDED), cx),
                    Err(error) => this.notify_error(t(k::SHELL_CODEX_LAUNCH_FAILED), error, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section || self.showing_app_settings {
            self.section = section;
            self.editor = None;
            self.showing_app_settings = false;
            // Reload the destination view's data so it reflects current state.
            match section {
                Section::Mcp => {
                    self.reload_mcp_workspace(cx);
                }
                Section::Skills => self.reload_skills_workspace(cx),
                Section::Gateway => self.reload_gateway_workspace(cx),
                Section::Network => self.reload_network_workspace(cx),
                Section::Remote => self.remote_view.update(cx, |v, cx| v.reload(cx)),
                Section::Usage => self.reload_usage_workspace(cx),
                Section::Sessions => self.reload_sessions_workspace(cx),
                Section::Tools => self.reload_tools_workspace(cx),
                Section::Settings => self.reload_settings_workspace(cx),
                Section::About => self.reload_about_workspace(cx),
                _ => {}
            }
            self.flush_section_toast(section, cx);
            cx.notify();
        }
    }

    fn select_local_scope(&mut self, cx: &mut Context<Self>) {
        if self.active_remote_scope.take().is_some() {
            self.editor = None;
            self.showing_app_settings = false;
            self.clear_provider_page();
            self.visible_apps = Self::load_visible_apps().into();
            if !self.visible_apps.contains(&self.selected_app)
                && let Some(first) = self.visible_apps.first().copied()
            {
                self.selected_app = first;
            }
        }
        self.select_section(Section::Providers, cx);
        self.reload(cx);
    }

    fn select_remote_scope(&mut self, id: String, cx: &mut Context<Self>) {
        if self.active_remote_scope.as_deref() != Some(id.as_str()) {
            self.active_remote_scope = Some(id.clone());
            self.editor = None;
            self.showing_app_settings = false;
            self.clear_provider_page();
        }
        self.section = Section::Providers;
        self.remote_view
            .update(cx, |view, cx| view.activate_scope(id, cx));
        self.reload(cx);
        cx.notify();
    }

    fn do_switch(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_app == AppType::CherryStudio {
            self.do_cherry_studio_import(id, cx);
            return;
        }
        if self.active_remote_scope.is_none()
            && self
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .is_some_and(Provider::is_local_gateway)
        {
            self.connect_local_gateway(id, cx);
            return;
        }
        // Station-sourced channels re-embed the live gateway origin + key
        // before switching, so a changed listen port never strands them.
        if self.active_remote_scope.is_none()
            && self
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .and_then(|provider| provider.meta.as_ref())
                .is_some_and(|meta| meta.gateway_route_id.is_some())
        {
            self.switch_station_channel(id, cx);
            return;
        }
        if self.provider_action_in_flight {
            return;
        }
        let name = self
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .map(|provider| provider.name.clone())
            .unwrap_or_else(|| id.clone());
        let Some(backend) = self.workspace_backend(cx) else {
            self.notify_error(
                t(k::SHELL_PROVIDER_SWITCH_FAILED),
                "remote workspace is not connected".to_string(),
                cx,
            );
            return;
        };
        self.provider_action_in_flight = true;
        let app_id = self.selected_app.app_id();
        let preview_id = id.clone();
        cx.spawn(async move |this, cx| {
            let preview = crate::core_async::run(async move {
                backend
                    .plan_provider_switch(
                        &app_id,
                        &preview_id,
                        ochub_core::application::ProviderSwitchPolicy::Abort,
                    )
                    .await
            })
            .await;
            this.update(cx, |this, cx| match preview {
                // Something outside OcHub edited the file. Write nothing until
                // the user has seen it and said what to do.
                Ok(handle) if !handle.plan().drift.is_empty() => {
                    this.provider_action_in_flight = false;
                    let plan = handle.plan();
                    this.pending_drift = Some(PendingDrift {
                        provider_id: id,
                        provider_name: name,
                        path: SharedString::from(plan.config_path.clone()),
                        drift: plan.drift.clone(),
                    });
                    cx.notify();
                }
                Ok(handle) => this.apply_switch_handle(handle, name, cx),
                Err(error) => {
                    this.provider_action_in_flight = false;
                    this.notify_error(t(k::SHELL_PROVIDER_SWITCH_FAILED), error.to_string(), cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn do_cherry_studio_import(&mut self, id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        if self.active_remote_scope.is_some() {
            self.notify_error(
                t(k::SHELL_PROVIDER_IMPORT_FAILED),
                t(k::SHELL_PROVIDER_IMPORT_REMOTE_UNSUPPORTED),
                cx,
            );
            return;
        }

        // The list backend intentionally returns redacted provider data. Read
        // the local SSOT only at click time so the API key never enters view
        // state, rendering, logs, or the clipboard.
        let provider = match self
            .app
            .db
            .get_provider_by_id(&id, AppType::CherryStudio.as_str())
        {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                self.notify_error(
                    t(k::SHELL_PROVIDER_IMPORT_FAILED),
                    "provider not found".to_string(),
                    cx,
                );
                return;
            }
            Err(error) => {
                self.notify_error(t(k::SHELL_PROVIDER_IMPORT_FAILED), error.to_string(), cx);
                return;
            }
        };
        let name = provider.name.clone();
        let deeplink =
            match ochub_core::apps::cherry_studio::build_provider_import_deeplink(&provider) {
                Ok(deeplink) => deeplink,
                Err(error) => {
                    self.notify_error(t(k::SHELL_PROVIDER_IMPORT_FAILED), error.to_string(), cx);
                    return;
                }
            };

        self.provider_action_in_flight = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { open_cherry_studio_deeplink(&deeplink) })
                .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(()) => this.notify_success(
                        SharedString::from(tf!(k::SHELL_PROVIDER_IMPORT_OPENED, name = name)),
                        cx,
                    ),
                    Err(error) => this.notify_error(t(k::SHELL_PROVIDER_IMPORT_FAILED), error, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn apply_switch(
        &mut self,
        id: String,
        name: String,
        resolution: DriftResolution,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            self.notify_error(
                t(k::SHELL_PROVIDER_SWITCH_FAILED),
                "remote workspace is not connected".to_string(),
                cx,
            );
            return;
        };
        self.provider_action_in_flight = true;
        let app_id = self.selected_app.app_id();
        let policy = match resolution {
            DriftResolution::Preserve => ochub_core::application::ProviderSwitchPolicy::Preserve,
            DriftResolution::Discard => ochub_core::application::ProviderSwitchPolicy::Discard,
        };
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let handle = backend
                    .plan_provider_switch(&app_id, &id, policy)
                    .await
                    .map_err(|error| error.to_string())?;
                backend
                    .apply_provider_switch(handle)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(result) => {
                        let warnings = Self::response_warnings(&result);
                        if warnings.is_empty() {
                            this.notify_success(tf!(k::SHELL_PROVIDER_SWITCHED, name = name), cx);
                        } else {
                            this.notify_warning(
                                tf!(k::SHELL_PROVIDER_SWITCHED, name = name),
                                Self::warnings_summary(&warnings),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        this.notify_error(t(k::SHELL_PROVIDER_SWITCH_FAILED), error, cx);
                    }
                }
                this.reload(cx);
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn apply_switch_handle(
        &mut self,
        handle: ProviderSwitchHandle,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .apply_provider_switch(handle)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(result) => {
                        let warnings = Self::response_warnings(&result);
                        if warnings.is_empty() {
                            this.notify_success(tf!(k::SHELL_PROVIDER_SWITCHED, name = name), cx)
                        } else {
                            this.notify_warning(
                                tf!(k::SHELL_PROVIDER_SWITCHED, name = name),
                                Self::warnings_summary(&warnings),
                                cx,
                            )
                        }
                    }
                    Err(error) => this.notify_error(t(k::SHELL_PROVIDER_SWITCH_FAILED), error, cx),
                }
                this.reload(cx);
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switch to a station-sourced channel: refresh its stored endpoint/key
    /// from the running gateway, then switch. Mirrors `connect_local_gateway`
    /// (gateway implied-running, no drift prompt — the refresh regenerates
    /// the managed part of the config anyway).
    fn switch_station_channel(&mut self, provider_id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        let name = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(|provider| provider.name.clone())
            .unwrap_or_else(|| provider_id.clone());
        self.provider_action_in_flight = true;
        self.notify_info(t(k::SHELL_GATEWAY_SWITCHING), cx);
        let app = self.app.clone();
        let app_type = self.selected_app;
        cx.spawn(async move |this, cx| {
            let prepare_app = app.clone();
            let prepare = cx
                .background_spawn(async move {
                    let mut config = prepare_app
                        .db
                        .get_gateway_config()
                        .map_err(|error| error.to_string())?;
                    if !config.enabled {
                        config.enabled = true;
                        prepare_app
                            .db
                            .set_gateway_config(&config)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok::<(), String>(())
                })
                .await;
            let result = async {
                prepare?;
                let status = app
                    .gateway
                    .start()
                    .await
                    .map_err(|error| error.to_string())?;
                let app_for_write = app.clone();
                let base_url = status.base_url;
                let provider_id_for_write = provider_id.clone();
                cx.background_spawn(async move {
                    let provider = app_for_write
                        .db
                        .get_provider_by_id(&provider_id_for_write, app_type.as_str())
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "provider disappeared before switch".to_string())?;
                    let (settings, meta) = apply::refresh_station_channel_settings(
                        &app_for_write,
                        app_type,
                        &provider,
                        &base_url,
                    )
                    .map_err(|error| error.to_string())?;
                    let mut updated = provider;
                    updated.settings_config = settings;
                    updated.meta = meta;
                    ProviderService::update(
                        &app_for_write,
                        app_type,
                        Some(&provider_id_for_write),
                        updated,
                    )
                    .map_err(|error| error.to_string())?;
                    ProviderService::switch(&app_for_write, app_type, &provider_id_for_write)
                        .map_err(|error| error.to_string())
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(_) => {
                        this.notify_success(tf!(k::SHELL_PROVIDER_SWITCHED, name = name), cx);
                    }
                    Err(error) => {
                        this.notify_error(t(k::SHELL_PROVIDER_SWITCH_FAILED), error, cx);
                    }
                }
                this.reload(cx);
                shell_menu::refresh(&this.app, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// One side of a conflict as the text the diff compares.
    ///
    /// A deletion has no text at all rather than a placeholder: the empty column
    /// beside the other version is what makes it read as a deletion.
    fn drift_value_text(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(text) => text.clone(),
            // Tables and arrays are laid out one field per line so the diff can
            // point at the field that changed instead of at the whole value.
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        }
    }

    /// `+N` for the entries a section does not have room for. Silently showing
    /// the first few would read as "that's all of them".
    fn drift_overflow(total: usize, shown: usize) -> Option<gpui::Div> {
        (total > shown).then(|| {
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(SharedString::from(format!("+{}", total - shown)))
        })
    }

    /// A section that only needs to name the keys involved.
    fn drift_path_section(tone: BadgeTone, heading: String, paths: &[String]) -> Option<gpui::Div> {
        const SHOWN: usize = 6;
        if paths.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(components::badge(tone, heading))
                .children(paths.iter().take(SHOWN).map(|path| {
                    div()
                        .text_color(theme::subtext())
                        .text_xs()
                        .child(SharedString::from(path.clone()))
                }))
                .children(Self::drift_overflow(paths.len(), SHOWN)),
        )
    }

    /// One conflict, as the two versions side by side.
    fn drift_conflict_diff(conflict: &DriftConflict) -> gpui::Div {
        /// Enough to read a changed block; longer values say how much is left.
        const ROWS: usize = 20;

        let live = Self::drift_value_text(&conflict.live);
        let incoming = Self::drift_value_text(&conflict.incoming);
        let (rows, hidden) = diff_view::side_by_side(&live, &incoming, ROWS);

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_color(theme::text())
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(conflict.path.clone())),
            )
            .child(diff_view::render(
                &rows,
                hidden,
                Some(diff_view::header_row(
                    t(k::SHELL_DRIFT_CONFLICT_YOURS),
                    t(k::SHELL_DRIFT_CONFLICT_INCOMING),
                )),
                |count| SharedString::from(tf!(k::SHELL_DRIFT_DIFF_FOLDED, count = count)),
                |count| SharedString::from(tf!(k::SHELL_DRIFT_DIFF_TRUNCATED, count = count)),
            ))
    }

    /// The half the user actually has to rule on: both sides changed the same
    /// key, so one of them is about to lose.
    fn drift_conflict_section(conflicts: &[DriftConflict]) -> Option<gpui::Div> {
        const SHOWN: usize = 4;
        if conflicts.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(components::badge(
                    BadgeTone::Warning,
                    tf!(k::SHELL_DRIFT_CONFLICT_HEADING, count = conflicts.len()),
                ))
                .children(conflicts.iter().take(SHOWN).map(Self::drift_conflict_diff))
                .children(Self::drift_overflow(conflicts.len(), SHOWN)),
        )
    }

    fn render_drift_modal(&self, pending: PendingDrift, cx: &mut Context<Self>) -> gpui::Div {
        let PendingDrift {
            provider_id,
            provider_name,
            path,
            drift,
        } = pending;
        let body = SharedString::from(tf!(k::SHELL_DRIFT_BODY, path = path, name = provider_name));
        let discard = (provider_id.clone(), provider_name.clone());
        let preserve = (provider_id, provider_name);

        components::modal_overlay(
            components::modal_card()
                // Wide enough for two columns of config text; a diff squeezed
                // into one column is the thing this dialog exists to avoid.
                .w(px(760.))
                .max_h(px(600.))
                .child(components::modal_header(t(k::SHELL_DRIFT_TITLE)))
                .child(
                    components::modal_body()
                        .id("drift-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(div().text_color(theme::subtext()).text_sm().child(body))
                        // Conflicts first: everything below them is already
                        // decided in the user's favour.
                        .children(Self::drift_conflict_section(&drift.conflicts))
                        .children(Self::drift_path_section(
                            BadgeTone::Success,
                            tf!(k::SHELL_DRIFT_KEPT_HEADING, count = drift.preserved.len()),
                            &drift.preserved,
                        ))
                        .children(Self::drift_path_section(
                            BadgeTone::Neutral,
                            tf!(k::SHELL_DRIFT_REMOVED_HEADING, count = drift.removed.len()),
                            &drift.removed,
                        )),
                )
                .child(components::modal_footer(vec![
                    components::button(
                        "drift-cancel",
                        t(k::SHELL_DRIFT_ACTION_CANCEL),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.pending_drift = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                    components::button(
                        "drift-discard",
                        t(k::SHELL_DRIFT_ACTION_DISCARD),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.pending_drift = None;
                        let (id, name) = discard.clone();
                        this.apply_switch(id, name, DriftResolution::Discard, cx);
                    }))
                    .into_any_element(),
                    components::button(
                        "drift-preserve",
                        t(k::SHELL_DRIFT_ACTION_PRESERVE),
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.pending_drift = None;
                        let (id, name) = preserve.clone();
                        this.apply_switch(id, name, DriftResolution::Preserve, cx);
                    }))
                    .into_any_element(),
                ])),
        )
    }

    fn station_route_for_provider(&self, provider: &Provider) -> Option<&GatewayRoute> {
        let route_id = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.gateway_route_id.as_deref())
            .or_else(|| {
                (provider.id == apply::GATEWAY_PROVIDER_ID)
                    .then(|| {
                        self.gateway_keys
                            .iter()
                            .find(|key| key.name == self.selected_app.as_str() && key.enabled)
                            .and_then(|key| key.route_id.as_deref())
                    })
                    .flatten()
            })
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

    fn response_warnings(value: &serde_json::Value) -> Vec<String> {
        value
            .get("warnings")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    }

    fn connect_local_gateway(&mut self, provider_id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        let station_route_id = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .and_then(|provider| self.station_route_for_provider(provider))
            .map(|route| route.id.clone());
        let Some(station_route_id) = station_route_id else {
            self.notify_warning(
                t(k::SHELL_GATEWAY_NEEDS_STATION_TITLE),
                t(k::SHELL_GATEWAY_NEEDS_STATION_MESSAGE),
                cx,
            );
            self.open_edit_editor_by_id(&provider_id, cx);
            return;
        };
        self.connect_station_route(station_route_id, cx);
    }

    /// Put this app into relay mode on one station: start the gateway, then
    /// write the app's config to point at it. This is what creates the managed
    /// gateway provider the first time, so it is also the entry point offered
    /// when the app has no relay entry yet.
    fn connect_station_route(&mut self, station_route_id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        self.provider_action_in_flight = true;
        self.notify_info(t(k::SHELL_GATEWAY_SWITCHING), cx);
        let app = self.app.clone();
        let app_type = self.selected_app;
        cx.spawn(async move |this, cx| {
            let prepare_app = app.clone();
            let prepare = cx
                .background_spawn(async move {
                    let mut config = prepare_app
                        .db
                        .get_gateway_config()
                        .map_err(|error| ProviderGatewayConnectError::Config(error.to_string()))?;
                    if !config.enabled {
                        config.enabled = true;
                        prepare_app
                            .db
                            .set_gateway_config(&config)
                            .map_err(|error| {
                                ProviderGatewayConnectError::Start(error.to_string())
                            })?;
                    }
                    Ok::<(), ProviderGatewayConnectError>(())
                })
                .await;
            let result = async {
                prepare?;
                let status = app
                    .gateway
                    .start()
                    .await
                    .map_err(|error| ProviderGatewayConnectError::Start(error.to_string()))?;
                let app_for_switch = app.clone();
                let base_url = status.base_url;
                cx.background_spawn(async move {
                    apply::apply_station_to_app(
                        &app_for_switch,
                        app_type,
                        &base_url,
                        &station_route_id,
                    )
                    .map_err(|error| ProviderGatewayConnectError::Switch(error.to_string()))
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(_) => {
                        this.notify_success(t(k::SHELL_GATEWAY_SWITCHED), cx);
                    }
                    Err(ProviderGatewayConnectError::Config(error)) => {
                        this.notify_error(t(k::SHELL_GATEWAY_CONFIG_READ_FAILED), error, cx);
                    }
                    Err(ProviderGatewayConnectError::Start(error)) => {
                        this.notify_error(t(k::SHELL_GATEWAY_START_FAILED), error, cx);
                    }
                    Err(ProviderGatewayConnectError::Switch(error)) => {
                        this.notify_error(t(k::SHELL_GATEWAY_SWITCH_FAILED), error, cx);
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

    fn do_remove_from_live(&mut self, id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        self.provider_action_in_flight = true;
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            return;
        };
        let app_id = self.selected_app.app_id();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .set_provider_live(&app_id, &id, false)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(()) => {
                        this.notify_success(t(k::SHELL_PROVIDER_REMOVED_FROM_TOOL), cx);
                    }
                    Err(error) => {
                        this.notify_error(t(k::SHELL_PROVIDER_REMOVE_FROM_TOOL_FAILED), error, cx)
                    }
                }
                this.reload(cx);
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Rebuild all provider-list structure in one pass. This is deliberately
    /// called only after data/current-order changes, never from `render`.
    fn rebuild_provider_structure_cache(&mut self) {
        let is_import = self.selected_app == AppType::CherryStudio;
        let is_switch = !self.selected_app.is_additive_mode() && !is_import;
        let connection_ixs: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| !is_switch || provider.id != self.current)
            .map(|(index, _)| index)
            .collect();
        let no_providers = self.providers.is_empty();

        let mut plan = Vec::new();
        if is_switch {
            plan.push(ProviderRow::Hero);
            if !connection_ixs.is_empty() {
                plan.push(ProviderRow::DirectLabel);
                plan.extend(connection_ixs.iter().copied().map(ProviderRow::Card));
            }
        } else {
            if no_providers {
                plan.push(ProviderRow::EmptyState);
            }
            plan.extend(connection_ixs.iter().copied().map(ProviderRow::Card));
        }

        let mut row_by_provider = vec![None; self.providers.len()];
        for (row_index, row) in plan.iter().enumerate() {
            if let ProviderRow::Card(provider_index) = row {
                row_by_provider[*provider_index] = Some(row_index);
            }
        }

        // Every visible connection participates in the same ordering model.
        // Model-provider-backed connections are ordinary app connections in
        // this view, so excluding them would leave a visual hole and prevent
        // users from positioning them relative to direct connections.
        let sortable_ixs = connection_ixs.clone();
        let mut sortable_rows = Vec::with_capacity(sortable_ixs.len());
        let mut sortable_positions = HashMap::with_capacity(sortable_ixs.len());
        for (position, provider_index) in sortable_ixs.iter().copied().enumerate() {
            let Some(row_index) = row_by_provider[provider_index] else {
                continue;
            };
            let id = self.providers[provider_index].id.clone();
            sortable_positions.insert(id, position);
            sortable_rows.push(row_index);
        }

        self.provider_rows = plan.into();
        self.provider_sortable_slots = sortable_ixs.into();
        self.provider_sortable_rows = sortable_rows.into();
        self.provider_sortable_positions = sortable_positions;
    }

    /// Resolve display-only provider data outside the frame loop. A Codex
    /// provider can require parsing TOML; doing it here means once per reload
    /// instead of once per visible card and animation frame.
    fn rebuild_provider_presentations(&mut self, base_urls: &HashMap<String, String>) {
        self.provider_presentations = self
            .providers
            .iter()
            .map(|provider| {
                let name = self
                    .station_route_for_provider(provider)
                    .map(|route| route.name.clone())
                    .unwrap_or_else(|| provider.name.clone());
                let base_url = if provider.is_local_gateway() {
                    SharedString::default()
                } else if let Some(route_id) = provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.gateway_route_id.as_deref())
                {
                    // Station-sourced channel: the raw loopback URL means
                    // nothing to the user — name the station instead.
                    self.gateway_routes
                        .iter()
                        .find(|route| route.id == route_id)
                        .map(|route| {
                            SharedString::from(tf!(k::SHELL_GATEWAY_VIA_STATION, name = route.name))
                        })
                        .unwrap_or_else(|| SharedString::new_static("—"))
                } else {
                    let base_url = base_urls.get(&provider.id).cloned().unwrap_or_default();
                    if base_url.is_empty() {
                        SharedString::new_static("—")
                    } else {
                        SharedString::from(base_url)
                    }
                };
                (
                    provider.id.clone(),
                    ProviderPresentation {
                        name: SharedString::from(name),
                        base_url,
                    },
                )
            })
            .collect();
    }

    fn rebuild_provider_render_cache(&mut self, base_urls: &HashMap<String, String>) {
        self.rebuild_provider_presentations(base_urls);
        self.rebuild_provider_structure_cache();
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

        let rows = self.provider_sortable_rows.as_ref();
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
            let next_row = rows[target_position + 1];
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
            let previous_row = rows[target_position - 1];
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

        let mut anchor = None;
        let mut previous_measured = None;
        let mut fallback_pitch = None;
        for (position, row_index) in rows.iter().enumerate() {
            let Some(bounds) = self.provider_list_state.bounds_for_item(*row_index) else {
                continue;
            };
            let top = bounds.top().as_f32();
            anchor.get_or_insert((position, top, bounds.size.height.as_f32()));
            if let Some((previous_position, previous_top)) = previous_measured {
                let row_distance = position - previous_position;
                let pitch = (top - previous_top) / row_distance as f32;
                if pitch > 0. {
                    fallback_pitch = Some(pitch);
                    break;
                }
            }
            previous_measured = Some((position, top));
        }
        let Some((anchor_position, anchor_top, anchor_height)) = anchor else {
            return;
        };
        let fallback_pitch = fallback_pitch.unwrap_or(anchor_height.max(1.));
        let row_tops: Vec<f32> = rows
            .iter()
            .enumerate()
            .map(|(position, row_index)| {
                self.provider_list_state
                    .bounds_for_item(*row_index)
                    .map(|bounds| bounds.top().as_f32())
                    .unwrap_or_else(|| {
                        anchor_top
                            + (position as isize - anchor_position as isize) as f32 * fallback_pitch
                    })
            })
            .collect();
        if let Some(state) = self.provider_drag_state.as_mut() {
            state.retarget(
                target_position,
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

        let slots = self.provider_sortable_slots.clone();
        let target_id = slots
            .get(target_position)
            .and_then(|slot| self.providers.get(*slot))
            .map(|provider| provider.id.clone());
        if dropped_inside_list
            && target_position != dragged.source_position
            && let Some(target_id) = target_id
        {
            self.reorder_provider(dragged.id.clone(), target_id, cx);
            return;
        }
        cx.notify();
    }

    fn provider_drag_offset(&self, provider_id: &str, reduce_motion: bool) -> f32 {
        let Some(position) = self.provider_sortable_positions.get(provider_id).copied() else {
            return 0.;
        };
        self.provider_drag_state.as_ref().map_or(0., |state| {
            state.offset_for(position, Instant::now(), reduce_motion)
        })
    }

    fn reorder_provider(&mut self, source_id: String, target_id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        let slots = self.provider_sortable_slots.clone();
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
            slots.as_ref(),
            source_position,
            target_position,
        ) {
            return;
        }
        self.rebuild_provider_structure_cache();

        let ids = self
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        self.provider_action_in_flight = true;
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            return;
        };
        let app_id = self.selected_app.app_id();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .sort_providers(&app_id, ids)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                if let Err(error) = result {
                    this.notify_error(t(k::SHELL_PROVIDER_REORDER_FAILED), error, cx);
                }
                this.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        self.provider_action_in_flight = true;
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            return;
        };
        let app_id = self.selected_app.app_id();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .delete_provider(&app_id, &id)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(()) => this.notify_success(t(k::SHELL_PROVIDER_DELETED), cx),
                    Err(error) => this.notify_error(t(k::SHELL_PROVIDER_DELETE_FAILED), error, cx),
                }
                this.reload(cx);
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Duplicate a provider in place. No confirmation step and no editor: the
    /// copy is inert (not current, not in any live config), so the undo is the
    /// delete button on the card that just appeared.
    fn do_duplicate(&mut self, id: String, cx: &mut Context<Self>) {
        if self.provider_action_in_flight {
            return;
        }
        self.provider_action_in_flight = true;
        let Some(backend) = self.workspace_backend(cx) else {
            self.provider_action_in_flight = false;
            return;
        };
        let app_id = self.selected_app.app_id();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .duplicate_provider(&app_id, &id)
                    .await
                    .map(|details| details.provider.name)
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.provider_action_in_flight = false;
                match result {
                    Ok(name) => {
                        this.notify_success(tf!(k::SHELL_PROVIDER_DUPLICATED, name = name), cx)
                    }
                    Err(error) => {
                        this.notify_error(t(k::SHELL_PROVIDER_DUPLICATE_FAILED), error, cx)
                    }
                }
                this.reload(cx);
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn acknowledge_first_run(&mut self, cx: &mut Context<Self>) {
        self.show_first_run_notice = false;
        cx.notify();
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_spawn(async {
                    ochub_core::settings::mutate_settings(|settings| {
                        settings.first_run_notice_confirmed = Some(true)
                    })
                })
                .await;
            if let Err(error) = result {
                log::warn!("保存首次运行提示确认状态失败: {error}");
            }
        })
        .detach();
    }

    fn detect_pending_ccswitch_import(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let source = cx
                .background_spawn(async move {
                    if app.db.ccswitch_import_decision().ok().flatten().is_some() {
                        None
                    } else {
                        import_ccswitch::detect_source().filter(|source| !source.is_empty())
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.show_first_run_notice {
                    this.ccswitch_import = source;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Re-read everything a cc-switch import rewrites. Shared by the first-run
    /// modal and the Settings → Data entry, which land the same rows.
    fn reload_after_ccswitch_import(&mut self, cx: &mut Context<Self>) {
        self.ensure_valid_selection(cx);
        self.reload(cx);
        self.mcp_view.update(cx, |view, cx| view.reload(cx));
        self.skills_view.update(cx, |view, cx| view.reload(cx));
        shell_menu::refresh(&self.app, cx);
        cx.notify();
    }

    fn skip_ccswitch_import(&mut self, cx: &mut Context<Self>) {
        self.ccswitch_import = None;
        let app = self.app.clone();
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_spawn(async move { app.db.set_ccswitch_import_decision("skipped") })
                .await;
            if let Err(error) = result {
                log::warn!("保存 cc-switch 导入选择失败: {error}");
            }
        })
        .detach();
        self.acknowledge_first_run(cx);
    }

    /// Import, then dismiss the notice and reload everything the new rows feed.
    ///
    /// The copy runs on a background thread: a cc-switch database carrying
    /// months of usage history takes long enough that doing it inline would
    /// freeze the window mid-answer.
    fn run_ccswitch_import(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.ccswitch_import.clone() else {
            return;
        };
        if self.ccswitch_importing {
            return;
        }
        self.ccswitch_importing = true;
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = app.db.import_from_ccswitch_source(&source);
                    if result.is_ok()
                        && let Err(error) = app.db.set_ccswitch_import_decision("imported")
                    {
                        log::warn!("保存 cc-switch 导入选择失败: {error}");
                    }
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                this.ccswitch_importing = false;
                match result {
                    Ok(report) => {
                        this.ccswitch_import = None;
                        this.notify_success(
                            tf!(
                                k::SHELL_FIRST_RUN_IMPORT_SUCCEEDED,
                                rows = report.total_rows()
                            ),
                            cx,
                        );
                        this.reload_after_ccswitch_import(cx);
                    }
                    // A failed import leaves the decision unrecorded on
                    // purpose, so the offer comes back on the next launch.
                    Err(err) => {
                        this.notify_error(t(k::SHELL_FIRST_RUN_IMPORT_FAILED), err.to_string(), cx)
                    }
                }
                this.acknowledge_first_run(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The import offer: a heading naming the source, the app's own grouped
    /// row card listing what comes across, and one line of reassurance.
    ///
    /// Built from `layout::group` / `layout::row` rather than a bespoke
    /// diagram, so it uses the label-left / value-right grid the rest of the
    /// app already reads on every settings page. One alignment axis, one level
    /// of nesting, and nothing shaped like a button the user might try to press.
    ///
    /// Absent when there is nothing to offer, which leaves the plain notice.
    fn render_ccswitch_import_card(&self) -> Option<gpui::Div> {
        let source = self.ccswitch_import.as_ref()?;
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(layout::section_header(
                    t(k::SHELL_FIRST_RUN_IMPORT_HEADING),
                    Some(SharedString::from(ochub_core::paths::abbreviate_home(
                        &source.path,
                    ))),
                ))
                .child(layout::group(Self::import_item_rows(source)))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(t(k::SHELL_FIRST_RUN_IMPORT_SOURCE_NOTE)),
                ),
        )
    }

    /// One row per kind of record: the icon it carries in the sidebar, its
    /// name, and the count right-aligned so the numbers line up and can be
    /// compared at a glance. Kinds the source has none of are left out rather
    /// than shown as a zero.
    fn import_item_rows(source: &DetectedSource) -> Vec<gpui::AnyElement> {
        [
            (
                IconName::Cloud,
                t(k::SHELL_FIRST_RUN_IMPORT_PROVIDERS),
                source.providers,
            ),
            (
                IconName::Blocks,
                t(k::SHELL_FIRST_RUN_IMPORT_MCP),
                source.mcp_servers,
            ),
            (
                IconName::Wrench,
                t(k::SHELL_FIRST_RUN_IMPORT_REPOS),
                source.skill_repos,
            ),
        ]
        .into_iter()
        .filter(|(_, _, count)| *count > 0)
        .map(|(name, label, count)| {
            layout::row()
                .child(icon(name, theme::muted(), 15.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(theme::text())
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text())
                        .child(SharedString::from(count.to_string())),
                )
                .into_any_element()
        })
        .collect()
    }

    /// With something to import the notice becomes a choice (skip / import);
    /// without, it stays the single acknowledgement it has always been.
    fn first_run_actions(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(_) = self.ccswitch_import.as_ref() else {
            return vec![
                components::button(
                    "first-run-confirm",
                    t(k::SHELL_FIRST_RUN_CONFIRM),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.acknowledge_first_run(cx);
                }))
                .into_any_element(),
            ];
        };

        if self.ccswitch_importing {
            return vec![
                components::disabled_button(
                    "first-run-import-busy",
                    t(k::SHELL_FIRST_RUN_IMPORT_BUSY),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                    true,
                )
                .into_any_element(),
            ];
        }

        vec![
            components::button(
                "first-run-import-skip",
                t(k::SHELL_FIRST_RUN_IMPORT_SKIP),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.skip_ccswitch_import(cx);
            }))
            .into_any_element(),
            components::button(
                "first-run-import-confirm",
                t(k::SHELL_FIRST_RUN_IMPORT_CONFIRM),
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.run_ccswitch_import(cx);
            }))
            .into_any_element(),
        ]
    }

    fn open_add_editor(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let Some(backend) = self.workspace_backend(cx) else {
            self.notify_error(
                t(k::SHELL_PROVIDER_LOAD_FAILED),
                "remote workspace is not connected".to_string(),
                cx,
            );
            return;
        };
        let editor = cx.new(|cx| ProviderEditor::new_add(app, backend, app_type, cx));
        self.subscribe_editor(&editor, cx);
        self.editor = Some(editor);
        cx.notify();
    }

    fn open_edit_editor(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let app_type = self.selected_app;
        let Some(backend) = self.workspace_backend(cx) else {
            self.notify_error(
                t(k::SHELL_PROVIDER_LOAD_FAILED),
                "remote workspace is not connected".to_string(),
                cx,
            );
            return;
        };
        let editor = cx.new(|cx| ProviderEditor::new_edit(app, backend, app_type, &provider, cx));
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
                if this.active_remote_scope.is_none() {
                    shell_menu::refresh(&this.app, cx);
                }
                cx.notify();
            }
            EditorEvent::Cancelled => {
                this.editor = None;
                cx.notify();
            }
        })
        .detach();
    }

    fn provider_base_url(&self, provider: &Provider) -> SharedString {
        self.provider_presentations
            .get(&provider.id)
            .map(|presentation| presentation.base_url.clone())
            .unwrap_or_else(|| SharedString::new_static("—"))
    }

    fn provider_name(&self, provider: &Provider) -> SharedString {
        self.provider_presentations
            .get(&provider.id)
            .map(|presentation| presentation.name.clone())
            .unwrap_or_else(|| SharedString::from(provider.name.clone()))
    }

    fn open_edit_editor_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        // The list backend redacts secrets on purpose, so the cached copy cannot
        // seed an edit form: the form decodes its API key field from it and the
        // save writes the whole record back, which would put `******` where the
        // credential was. Read the local SSOT at click time instead.
        //
        // A remote workspace has no unredacted copy to read — the node never
        // sends the secret — so it still edits from the redacted record and
        // relies on the save path restoring masked fields from what is stored.
        let local = self.active_remote_scope.is_none().then(|| {
            self.app
                .db
                .get_provider_by_id(id, self.selected_app.as_str())
        });
        let provider = match local {
            Some(Ok(Some(provider))) => Some(provider),
            Some(Err(error)) => {
                self.notify_error(t(k::SHELL_PROVIDER_LOAD_FAILED), error.to_string(), cx);
                return;
            }
            _ => self
                .providers
                .iter()
                .find(|provider| provider.id == id)
                .cloned(),
        };
        if let Some(provider) = provider {
            self.open_edit_editor(provider, cx);
        }
    }

    fn render_sidebar_item(
        &self,
        app: AppType,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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
    ) -> impl IntoElement + use<> {
        let selected = self.section == section;
        let fg = if selected {
            theme::accent()
        } else {
            theme::sidebar_glass_muted(appearance)
        };
        // An available update is marked on 关于 rather than announced in its own
        // row: that row is already the destination, so the dot needs no second
        // affordance and the sidebar's resting layout is untouched.
        let pending_update = (section == Section::About)
            .then(|| self.available_update.clone())
            .flatten();
        div()
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(match &pending_update {
                Some(version) => SharedString::from(tf!(
                    k::SHELL_SIDEBAR_UPDATE_BADGE_ARIA,
                    name = label,
                    version = version
                )),
                None => SharedString::from(tf!(k::SHELL_SIDEBAR_OPEN_ARIA, name = label)),
            })
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
            // Wrapped so the dot keeps its place when a translated label is
            // long enough to need truncating.
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when_some(pending_update, |row, _version| {
                row.child(
                    div()
                        .w(px(7.))
                        .h(px(7.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme::accent()),
                )
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_section(section, cx);
            }))
    }

    fn render_workspace_scope(
        &self,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let remote = self.remote_view.read(cx);
        let items = remote.scope_items();
        let active_remote = self.active_remote_scope.clone();
        let mut labels = vec![t(k::SHELL_SIDEBAR_SCOPE_LOCAL).to_string()];
        let mut targets = vec![None];
        labels.extend(items.iter().map(|item| item.name.clone()));
        targets.extend(items.iter().map(|item| Some(item.id.clone())));
        let selected = active_remote
            .as_deref()
            .and_then(|active| items.iter().position(|item| item.id == active))
            .map(|index| index + 1)
            .unwrap_or(0);
        let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let open = self.workspace_scope_open;
        let on_event = cx.listener(
            move |this, event: &components::SelectDropdownEvent, _window, cx| match *event {
                components::SelectDropdownEvent::Open(open) => {
                    this.workspace_scope_open = open;
                    cx.notify();
                }
                components::SelectDropdownEvent::Select(index) => {
                    this.workspace_scope_open = false;
                    match targets.get(index).cloned().flatten() {
                        Some(id) => this.select_remote_scope(id, cx),
                        None => this.select_local_scope(cx),
                    }
                }
            },
        );
        div()
            .w_full()
            .px_2()
            .child(components::select_dropdown_sidebar(
                "workspace-scope",
                &label_refs,
                selected,
                open,
                appearance,
                move |event, window, cx| on_event(&event, window, cx),
            ))
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
    fn render_sidebar_drag_region(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
    fn render_content_drag_region(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
    ) -> impl IntoElement + use<> {
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
                raw(k::SHELL_SIDEBAR_GROUP_WORKSPACE),
                appearance,
            ))
            .child(self.render_workspace_scope(appearance, cx))
            .child(Self::render_sidebar_group(
                raw(k::SHELL_SIDEBAR_GROUP_APPS),
                appearance,
            ))
            .child(
                div().flex().flex_col().gap_1().px_2().children(
                    self.visible_apps
                        .iter()
                        .copied()
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
                    ))
                    .child(self.render_nav_item(
                        "nav-network-proxy",
                        raw(k::SHELL_SIDEBAR_NAV_PROXY),
                        Section::Network,
                        appearance,
                        cx,
                    ))
                    .child(self.render_nav_item(
                        "nav-remote-nodes",
                        raw(k::SHELL_SIDEBAR_NAV_REMOTE),
                        Section::Remote,
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
                    .child(self.render_nav_item(
                        "nav-about",
                        raw(k::SHELL_SIDEBAR_NAV_ABOUT),
                        Section::About,
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
    }

    fn render_provider_card(
        &self,
        provider: &Provider,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let is_import = self.selected_app == AppType::CherryStudio;
        let is_current =
            !self.selected_app.is_additive_mode() && !is_import && provider.id == self.current;
        let is_gateway = provider.is_local_gateway();
        let id = provider.id.clone();
        let edit_id = id.clone();
        let setup_edit_id = id.clone();
        let duplicate_id = id.clone();
        let delete_target = ProviderDeleteTarget {
            id: id.clone(),
            name: self.provider_name(provider),
        };
        let live_id = provider.id.clone();
        let sortable_position = self.provider_sortable_positions.get(&provider.id).copied();
        let sortable_count = self.provider_sortable_slots.len();
        let is_sortable = sortable_count > 1 && sortable_position.is_some();
        let is_drag_source = self
            .provider_drag_state
            .as_ref()
            .is_some_and(|state| state.source_id == provider.id);
        let drag_offset = self.provider_drag_offset(&provider.id, cx.reduce_motion());
        let base_url = if is_gateway {
            SharedString::from(self.gateway_via_station_line(provider))
        } else {
            self.provider_base_url(provider)
        };
        let provider_name = self.provider_name(provider);
        let is_official_quota = Self::is_official_quota_provider(self.selected_app, provider);
        let quota_line = is_official_quota
            .then(|| self.render_quota_line("provider", &provider.id, &provider_name, cx));
        let is_additive = self.selected_app.is_additive_mode();
        let is_in_live = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.live_config_managed)
            .unwrap_or(!is_additive);
        // In switch mode the gateway card needs a bound station before it can
        // be switched to; without one the button becomes a setup shortcut.
        let gateway_needs_setup =
            is_gateway && !is_additive && self.station_route_for_provider(provider).is_none();
        // One key per branch, label and aria sentence together: a screen reader
        // cannot be handed a verb and a name to glue into a sentence itself.
        let (main_label_key, main_aria_key) = if is_import {
            (k::SHELL_ACTION_IMPORT, k::SHELL_ACTION_IMPORT_ARIA)
        } else if gateway_needs_setup {
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
        let (edit_label_key, edit_aria_key) = (k::SHELL_ACTION_EDIT, k::SHELL_ACTION_EDIT_ARIA);

        let drag_handle = sortable_position.map(|source_position| {
            let root = cx.entity();
            let dragged = DraggedProvider {
                id: provider.id.clone(),
                name: provider_name.clone(),
                base_url: base_url.clone(),
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
                    name = provider_name,
                    position = source_position + 1,
                    total = sortable_count,
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

        let card = components::panel()
            .id(SharedString::from(format!("provider-card-{}", provider.id)))
            .relative()
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
                                            .child(provider_name.clone()),
                                    )
                                    .when(is_current, |s| {
                                        s.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::SHELL_BADGE_CURRENT),
                                        ))
                                    }),
                            )
                            .child(div().text_color(theme::muted()).text_xs().child(base_url))
                            .children(quota_line),
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
                        .aria_label(SharedString::from(tf!(edit_aria_key, name = provider_name)))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_edit_editor_by_id(&edit_id, cx);
                            },
                        )),
                    )
                    .child(
                        components::action_button(
                            SharedString::from(format!("duplicate-{}", provider.id)),
                            t(k::SHELL_ACTION_DUPLICATE),
                            false,
                        )
                        .aria_label(SharedString::from(tf!(
                            k::SHELL_ACTION_DUPLICATE_ARIA,
                            name = provider_name
                        )))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.do_duplicate(duplicate_id.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::action_button_tone(
                            SharedString::from(format!("delete-{}", provider.id)),
                            t(k::SHELL_ACTION_DELETE),
                            ButtonTone::Danger,
                        )
                        .aria_label(SharedString::from(tf!(
                            k::SHELL_ACTION_DELETE_ARIA,
                            name = provider_name
                        )))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(delete_target.clone());
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        components::action_button(
                            SharedString::from(format!("switch-{}", provider.id)),
                            t(main_label_key),
                            is_import || !(is_current || (is_additive && is_in_live)),
                        )
                        .aria_label(SharedString::from(tf!(main_aria_key, name = provider_name)))
                        .aria_selected(!is_import && (is_current || (is_additive && is_in_live)))
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                if gateway_needs_setup {
                                    this.open_edit_editor_by_id(&setup_edit_id, cx);
                                } else if is_additive && is_in_live {
                                    this.do_remove_from_live(live_id.clone(), cx);
                                } else {
                                    this.do_switch(id.clone(), cx);
                                }
                            },
                        )),
                    ),
            );

        PaintOffsetY::new(px(drag_offset), card)
    }

    /// Third line for gateway cards/hero: name the station actually serving
    /// the selected app instead of a generic explanation.
    fn gateway_via_station_line(&self, provider: &Provider) -> String {
        match self.station_route_for_provider(provider) {
            Some(route) => tf!(k::SHELL_GATEWAY_VIA_STATION, name = route.name),
            None => raw(k::SHELL_GATEWAY_NO_STATION).to_string(),
        }
    }

    /// The "console" hero: a single prominent card that answers *which provider is
    /// live right now* for the selected app. Shown only in switch (non-additive) mode,
    /// above the list of switchable alternatives.
    fn render_active_hero(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let app = self.selected_app;
        let accent = Self::app_accent(app);
        let current = self.providers.iter().find(|p| p.id == self.current);
        let has_current = current.is_some();
        let is_gateway = current.is_some_and(Provider::is_local_gateway);
        let hero_quota_line = current
            .filter(|provider| Self::is_official_quota_provider(app, provider))
            .map(|provider| {
                let name = self.provider_name(provider);
                self.render_quota_line("hero", &provider.id, &name, cx)
            });

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

        let info = match current {
            Some(provider) => {
                let base_url = self.provider_base_url(provider);
                let provider_name = self.provider_name(provider);
                let endpoint = if is_gateway {
                    SharedString::from(self.gateway_via_station_line(provider))
                } else {
                    base_url
                };
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
                                BadgeTone::Success,
                                t(k::SHELL_BADGE_DIRECT),
                            )),
                    )
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(provider_name),
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
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .truncate()
                                    .child(endpoint),
                            ),
                    )
                    .children(hero_quota_line)
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
            let edit_id = provider.id.clone();
            let provider_name = self.provider_name(provider);
            div().flex().flex_row().items_center().gap_2().child(
                components::action_button(
                    SharedString::from(format!("hero-edit-{}", provider.id)),
                    t(k::SHELL_ACTION_EDIT),
                    false,
                )
                .aria_label(SharedString::from(tf!(
                    k::SHELL_ACTION_EDIT_ARIA,
                    name = provider_name
                )))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_edit_editor_by_id(&edit_id, cx);
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

    fn render_provider_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let app = self.selected_app;
        let plan = self.provider_rows.clone();
        if self.provider_list_state.item_count() != plan.len() {
            self.provider_list_state.reset(plan.len());
        }

        let connection_count = self.providers.len();
        // A whole sentence per mode: the count sits inside the phrase, and no
        // locale has to build one out of a mode fragment and a tail.
        let subtitle = SharedString::from(if app == AppType::CherryStudio {
            tf!(k::SHELL_LIST_SUBTITLE_IMPORT, count = connection_count)
        } else if app.is_additive_mode() {
            tf!(k::SHELL_LIST_SUBTITLE_ADDITIVE, count = connection_count)
        } else {
            tf!(k::SHELL_LIST_SUBTITLE_DIRECT, count = connection_count)
        });

        let actions = div()
            .flex()
            .flex_row()
            .gap_2()
            .when(
                app == AppType::Codex && self.active_remote_scope.is_none(),
                |s| {
                    if self.codex_launch_in_flight {
                        s.child(components::disabled_button(
                            "launch-codex-app",
                            t(k::SHELL_CODEX_LAUNCHING),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                            true,
                        ))
                    } else {
                        s.child(
                            components::icon_button(
                                "launch-codex-app",
                                t(k::SHELL_CODEX_LAUNCH),
                                IconName::AgentCodex,
                                false,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.launch_codex_app(cx);
                                },
                            )),
                        )
                    }
                },
            )
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
            Section::Network => self.network_view.clone().into_any_element(),
            Section::Remote => self.remote_view.clone().into_any_element(),
            Section::Mcp => self.mcp_view.clone().into_any_element(),
            Section::Skills => self.skills_view.clone().into_any_element(),
            Section::Usage => self.usage_view.clone().into_any_element(),
            Section::Sessions => self.sessions_view.clone().into_any_element(),
            Section::Tools => self.tools_view.clone().into_any_element(),
            Section::Themes => self.theme_view.clone().into_any_element(),
            Section::About => self.about_view.clone().into_any_element(),
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
            .when_some(self.provider_quota_detail.clone(), |root, (id, name)| {
                root.child(self.render_quota_detail(id, name, cx))
            })
            .when_some(self.pending_drift.clone(), |root, pending| {
                root.child(self.render_drift_modal(pending, cx))
            })
            .when(self.show_first_run_notice, |root| {
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::SHELL_FIRST_RUN_TITLE)))
                        .child(
                            components::modal_body()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .text_color(theme::subtext())
                                        .text_sm()
                                        .child(t(k::SHELL_FIRST_RUN_STORAGE))
                                        // The backup advice is for someone with
                                        // nothing to import. When there is an
                                        // import on screen it is a fourth
                                        // paragraph in front of a yes/no, and
                                        // the diagram already says cc-switch is
                                        // left alone.
                                        .when(self.ccswitch_import.is_none(), |body| {
                                            body.child(t(k::SHELL_FIRST_RUN_BACKUP))
                                        }),
                                )
                                .children(self.render_ccswitch_import_card()),
                        )
                        .child(components::modal_footer(self.first_run_actions(cx))),
                ))
            })
    }
}

#[cfg(test)]
mod update_badge_tests {
    use super::AppRoot;
    use ochub_core::services::UpdateCheckResult;

    fn result(has_update: bool, latest: Option<&str>) -> UpdateCheckResult {
        UpdateCheckResult {
            current_version: "0.4.0".to_string(),
            latest_version: latest.map(ToString::to_string),
            has_update,
            release_url: String::new(),
            release_notes: None,
            published_at: None,
            install_channel: "macos-app".to_string(),
            can_self_install: true,
        }
    }

    #[test]
    fn a_pending_update_is_badged_with_its_version() {
        assert_eq!(
            AppRoot::badge_version(&result(true, Some("0.4.1"))).as_deref(),
            Some("0.4.1")
        );
    }

    #[test]
    fn being_up_to_date_shows_no_badge() {
        assert!(AppRoot::badge_version(&result(false, Some("0.4.0"))).is_none());
        // A check that came back up to date must clear the badge even when the
        // release it saw has no parseable version.
        assert!(AppRoot::badge_version(&result(false, None)).is_none());
    }

    #[test]
    fn a_previously_announced_update_is_badged_before_any_check_runs() {
        // The daily gate means most launches check nothing; the badge has to
        // survive a restart on the strength of what was already announced.
        assert_eq!(
            AppRoot::seeded_badge(true, Some("99.0.0".to_string())).as_deref(),
            Some("99.0.0")
        );
    }

    #[test]
    fn an_announcement_already_installed_is_not_badged() {
        assert!(AppRoot::seeded_badge(true, Some("0.0.1".to_string())).is_none());
        assert!(AppRoot::seeded_badge(true, None).is_none());
    }

    #[test]
    fn turning_off_the_automatic_check_retires_the_badge() {
        // The switch is a request to stop being told about releases. An
        // announcement made while it was on must not keep marking 关于 after it
        // is off, on this launch or any later one.
        assert!(AppRoot::seeded_badge(false, Some("99.0.0".to_string())).is_none());
    }

    #[test]
    fn an_update_without_a_version_still_badges() {
        // Knowing that something is installable is the actionable part; the
        // label falls back to the localized placeholder rather than hiding.
        assert!(AppRoot::badge_version(&result(true, None)).is_some());
    }
}

#[cfg(test)]
mod drift_dialog_tests {
    use super::AppRoot;
    use serde_json::json;

    #[test]
    fn a_string_value_is_shown_without_its_json_quotes() {
        assert_eq!(
            AppRoot::drift_value_text(&json!("https://relay.example/v1")),
            "https://relay.example/v1"
        );
    }

    #[test]
    fn a_deletion_has_no_text_so_its_column_reads_as_empty() {
        assert_eq!(AppRoot::drift_value_text(&json!(null)), "");
    }

    #[test]
    fn a_structured_value_keeps_one_field_per_line_for_the_diff() {
        let text = AppRoot::drift_value_text(&json!({ "matcher": "Bash", "type": "command" }));
        assert!(text.contains("matcher"));
        assert!(
            text.lines().count() > 2,
            "the diff needs line granularity: {text}"
        );
    }

    #[test]
    fn a_long_value_is_left_whole_for_the_diff_to_fold() {
        // Truncating here would hide the very line the user has to rule on;
        // folding unchanged runs is the diff's job.
        let text = AppRoot::drift_value_text(&json!("x".repeat(200)));
        assert_eq!(text.chars().count(), 200);
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
        let offsets = reorder_slot_offsets(&[100., 170., 250.], 0, 2);

        assert_eq!(offsets, [0., -70., -80.]);
    }

    #[test]
    fn dragging_up_moves_intervening_cards_down_one_slot() {
        let offsets = reorder_slot_offsets(&[100., 170., 250.], 2, 0);

        assert_eq!(offsets, [70., 80., 0.]);
    }
}
