//! Passive overview for the selected workspace. Configuration relationships
//! are never presented as observed historical request paths.

use chrono::{DateTime, FixedOffset, Utc};
use gpui::{
    AnyElement, Context, Entity, FontWeight, ScrollHandle, SharedString, Window, div, point,
    prelude::*, px,
};
use ochub_core::AppId;
use ochub_core::application::{
    OverviewChannel, OverviewManagedApp, OverviewProviderPerformance, OverviewRequest,
    OverviewRoute, OverviewRouting, OverviewSnapshot, OverviewTrendBucket, OverviewUsageSource,
};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::remote::WorkspaceBackend;
use crate::text_input::{TextInput, TextInputEvent};
use crate::{tf, theme};

mod routing;

const ROUTE_PREVIEW_LIMIT: usize = 8;
const APP_PREVIEW_LIMIT: usize = 8;
const PERFORMANCE_PREVIEW_LIMIT: usize = 5;

pub enum OverviewEvent {
    App(AppId),
    Routes,
    Usage,
    Sessions,
    Remote,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum TimeRange {
    #[default]
    Day,
    Week,
    Month,
}

impl TimeRange {
    fn index(self) -> usize {
        match self {
            Self::Day => 0,
            Self::Week => 1,
            Self::Month => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Week,
            2 => Self::Month,
            _ => Self::Day,
        }
    }

    fn seconds(self) -> i64 {
        match self {
            Self::Day => 86_400,
            Self::Week => 7 * 86_400,
            Self::Month => 30 * 86_400,
        }
    }
}

pub struct OverviewView {
    backend: WorkspaceBackend,
    workspace_available: bool,
    scope_label: SharedString,
    snapshot: Option<OverviewSnapshot>,
    loading: bool,
    load_failed: bool,
    generation: u64,
    range: TimeRange,
    search: Entity<TextInput>,
    selected_route: Option<String>,
    route_model: Entity<TextInput>,
    route_binding: Option<String>,
    route_binding_open: bool,
    route_dialect: ochub_core::gateway::Dialect,
    show_excluded_candidates: bool,
    show_trend_data: bool,
    scroll: ScrollHandle,
}

impl gpui::EventEmitter<OverviewEvent> for OverviewView {}

impl OverviewView {
    pub fn new(backend: WorkspaceBackend, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, t(k::OVERVIEW_SEARCH)));
        cx.subscribe(&search, |_: &mut Self, _, _: &TextInputEvent, cx| {
            cx.notify()
        })
        .detach();
        let route_model = cx.new(|cx| TextInput::new(cx, t(k::OVERVIEW_MODEL_PLACEHOLDER)));
        cx.subscribe(&route_model, |_: &mut Self, _, _: &TextInputEvent, cx| {
            cx.notify()
        })
        .detach();
        let mut this = Self {
            backend,
            workspace_available: true,
            scope_label: t(k::OVERVIEW_LOCAL),
            snapshot: None,
            loading: false,
            load_failed: false,
            generation: 0,
            range: TimeRange::Day,
            search,
            selected_route: None,
            route_model,
            route_binding: None,
            route_binding_open: false,
            route_dialect: ochub_core::gateway::Dialect::Messages,
            show_excluded_candidates: false,
            show_trend_data: false,
            scroll: ScrollHandle::new(),
        };
        this.reload(cx);
        this
    }

    pub fn set_workspace(
        &mut self,
        backend: WorkspaceBackend,
        scope_label: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.backend = backend;
        self.scope_label = scope_label;
        self.workspace_available = true;
        self.snapshot = None;
        self.selected_route = None;
        self.route_binding = None;
        self.route_binding_open = false;
        self.show_excluded_candidates = false;
        self.route_model
            .update(cx, |input, cx| input.set_content("", cx));
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.search
            .update(cx, |input, cx| input.set_content("", cx));
        self.reload(cx);
    }

    pub fn set_workspace_unavailable(&mut self, scope_label: SharedString, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.scope_label = scope_label;
        self.workspace_available = false;
        self.loading = false;
        self.load_failed = false;
        self.snapshot = None;
        self.selected_route = None;
        self.route_binding = None;
        self.route_binding_open = false;
        self.show_excluded_candidates = false;
        self.route_model
            .update(cx, |input, cx| input.set_content("", cx));
        cx.notify();
    }

    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        self.route_model.update(cx, |input, cx| {
            input.set_placeholder(t(k::OVERVIEW_MODEL_PLACEHOLDER), cx)
        });
        self.search.update(cx, |input, cx| {
            input.set_placeholder(t(k::OVERVIEW_SEARCH), cx)
        });
        if self.workspace_available && !self.backend.is_remote() {
            self.scope_label = t(k::OVERVIEW_LOCAL);
        }
        cx.notify();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        if !self.workspace_available {
            return;
        }
        let generation = self.generation;
        let backend = self.backend.clone();
        let end = Utc::now().timestamp();
        let start = end - self.range.seconds();
        self.loading = true;
        self.load_failed = false;
        // Do not display the previous environment/window under a new label.
        self.snapshot = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend.overview_snapshot(Some(start), Some(end)).await
            })
            .await;
            this.update(cx, |this, cx| {
                if generation != this.generation || !this.workspace_available {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(snapshot) => {
                        let selected_exists =
                            this.selected_route.as_ref().is_some_and(|id| {
                                snapshot.routing.data.as_ref().is_some_and(|routing| {
                                    routing.routes.iter().any(|r| &r.id == id)
                                })
                            });
                        if !selected_exists {
                            this.selected_route = snapshot
                                .routing
                                .data
                                .as_ref()
                                .and_then(|routing| routing.routes.first())
                                .map(|route| route.id.clone());
                        }
                        this.snapshot = Some(snapshot);
                    }
                    Err(_) => {
                        // Transport diagnostics can include private paths or
                        // content. Keep the overview failure generic; node
                        // management owns its explicit diagnostic surface.
                        this.load_failed = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn section_heading(&self, title: SharedString, action: AnyElement) -> gpui::Div {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .min_w_0()
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(title),
            )
            .child(action)
    }

    fn unavailable_section(&self) -> AnyElement {
        note(t(k::OVERVIEW_SECTION_FAILED), true).into_any_element()
    }

    fn render_gateway(&self, snapshot: &OverviewSnapshot) -> AnyElement {
        let Some(gateway) = &snapshot.gateway.data else {
            return note(t(k::OVERVIEW_GATEWAY_FAILED), true).into_any_element();
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .text_xs()
            .text_color(theme::muted())
            .child(components::status_dot_sized(
                if gateway.running {
                    theme::green()
                } else {
                    theme::muted()
                },
                6.,
            ))
            .child(if gateway.running {
                tf!(k::OVERVIEW_GATEWAY_RUNNING, port = gateway.port)
            } else {
                raw(k::OVERVIEW_GATEWAY_STOPPED).to_string()
            })
            .child(t(k::OVERVIEW_GATEWAY_SCOPE))
            .child(div().flex_1())
            .child(tf!(
                k::OVERVIEW_READ_AT,
                time = timestamp_label(
                    snapshot.generated_at,
                    snapshot.window.node_utc_offset_seconds,
                    false
                )
            ))
            .into_any_element()
    }

    fn render_apps(&self, snapshot: &OverviewSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let mut section = div().flex().flex_col().gap_3().min_w_0().child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .child(t(k::OVERVIEW_APPLICATIONS)),
        );
        let Some(apps) = &snapshot.apps.data else {
            return section.child(self.unavailable_section()).into_any_element();
        };
        let mut visible: Vec<_> = apps
            .iter()
            .filter(|app| app.enabled && app.supports_provider)
            .collect();
        visible.sort_by_key(|app| {
            (
                app.connection_count == 0 && app.draft_count == 0,
                app.provider_count == 0,
            )
        });
        if visible.is_empty() {
            return section
                .child(note(t(k::OVERVIEW_APPS_EMPTY), false))
                .into_any_element();
        }
        for app in visible.iter().take(APP_PREVIEW_LIMIT) {
            section = section.child(self.render_app(app, cx));
        }
        if visible.len() > APP_PREVIEW_LIMIT {
            section = section.child(note(
                tf!(
                    k::OVERVIEW_PREVIEW_LIMIT,
                    shown = APP_PREVIEW_LIMIT,
                    total = visible.len()
                )
                .into(),
                false,
            ));
        }
        section
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::OVERVIEW_APP_STATE_SCOPE)),
            )
            .into_any_element()
    }

    fn render_app(&self, app: &OverviewManagedApp, cx: &mut Context<Self>) -> AnyElement {
        let connection = app.connections.first();
        let summary = connection
            .map(|connection| connection.name.clone())
            .unwrap_or_else(|| {
                if app.provider_count == 0 {
                    raw(k::OVERVIEW_APP_NO_CONNECTION).to_string()
                } else {
                    tf!(k::OVERVIEW_APP_STORED, count = app.provider_count)
                }
            });
        let row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .min_w_0()
            .py_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_w(px(220.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(app.display_name.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(theme::muted())
                                    .child(summary),
                            ),
                    )
                    .when(
                        !app.config_available
                            || app.stale_local_selection
                            || app.connection_count > 1,
                        |info| {
                            info.child(div().text_xs().text_color(theme::muted()).child(
                                if !app.config_available {
                                    raw(k::OVERVIEW_APP_CONFIG_UNAVAILABLE).to_string()
                                } else if app.stale_local_selection {
                                    raw(k::OVERVIEW_APP_SELECTION_STALE).to_string()
                                } else if app.connection_count > 1 {
                                    tf!(k::OVERVIEW_APP_MANAGED_COUNT, count = app.connection_count)
                                } else if let Some(connection) = connection {
                                    if connection.gateway_route_id.is_some() {
                                        raw(k::OVERVIEW_APP_ROUTE_LINK).to_string()
                                    } else {
                                        raw(k::OVERVIEW_APP_ROUTE_UNLINKED).to_string()
                                    }
                                } else {
                                    raw(k::OVERVIEW_APP_CLIENT_UNKNOWN).to_string()
                                },
                            ))
                        },
                    ),
            )
            .when(app.draft_count > 0, |row| {
                row.child(components::badge(
                    BadgeTone::Neutral,
                    tf!(k::OVERVIEW_DRAFTS, count = app.draft_count),
                ))
            });
        if let Ok(app_id) = AppId::parse(&app.app) {
            ochub_ui::keyboard::interactive(
                row.id(SharedString::from(format!("overview-app-{}", app.app)))
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(tf!(
                        k::OVERVIEW_APP_OPEN_ARIA,
                        app = app.display_name
                    )))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .child(icon(IconName::ChevronRight, theme::muted(), 14.))
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(OverviewEvent::App(app_id.clone()))),
                    ),
            )
            .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    fn render_usage(&self, snapshot: &OverviewSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child(self.section_heading(t(k::OVERVIEW_USAGE), gpui::Empty.into_any_element()));
        if let Some(usage) = &snapshot.usage.data {
            section = section.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_6()
                    .child(metric(
                        t(k::OVERVIEW_USAGE_RECORDS),
                        format_count(usage.summary.total_requests),
                    ))
                    .child(metric(
                        t(k::OVERVIEW_USAGE_COST),
                        format!("${}", usage.summary.total_cost),
                    ))
                    .child(metric(
                        t(k::OVERVIEW_USAGE_TOKENS),
                        format_count(usage.summary.real_total_tokens),
                    )),
            );
        } else {
            section = section.child(self.unavailable_section());
        }
        if let Some(trend) = &snapshot.trend.data {
            if trend.is_empty() || trend.iter().all(|bucket| bucket.request_count == 0) {
                section = section.child(note(t(k::OVERVIEW_TREND_EMPTY), false));
            } else {
                let max = trend
                    .iter()
                    .map(|bucket| bucket.request_count)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let unit = match snapshot.window.trend_bucket {
                    OverviewTrendBucket::Hour => t(k::OVERVIEW_TREND_HOUR),
                    OverviewTrendBucket::Day => t(k::OVERVIEW_TREND_DAY),
                };
                section = section.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(unit)
                        .child(
                            components::button(
                                "overview-trend-data",
                                if self.show_trend_data {
                                    t(k::OVERVIEW_HIDE_DATA)
                                } else {
                                    t(k::OVERVIEW_SHOW_DATA)
                                },
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_trend_data = !this.show_trend_data;
                                cx.notify();
                            })),
                        ),
                );
                let mut bars = div()
                    .flex()
                    .items_end()
                    .gap(px(4.))
                    .h(px(100.))
                    .flex_1()
                    .min_w_0()
                    .border_b_1()
                    .border_color(theme::border());
                for (index, bucket) in trend.iter().enumerate() {
                    // An empty bucket belongs exactly on the zero baseline;
                    // a decorative minimum height would invent activity.
                    let height = trend_bar_height(bucket.request_count, max);
                    bars = bars.child(div().flex_1().min_w_0().h(px(height)).rounded_t_sm().bg(
                        theme::accent().alpha(if index + 1 == trend.len() { 0.9 } else { 0.5 }),
                    ));
                }
                section = section
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .justify_between()
                                    .h(px(100.))
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(format_count(max))
                                    .child("0"),
                            )
                            .child(bars),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(
                                trend
                                    .first()
                                    .map(|item| bucket_label(&item.date))
                                    .unwrap_or_default(),
                            )
                            .child(tf!(
                                k::OVERVIEW_NODE_ZONE,
                                zone = offset_label(snapshot.window.node_utc_offset_seconds)
                            ))
                            .child(
                                trend
                                    .last()
                                    .map(|item| bucket_label(&item.date))
                                    .unwrap_or_default(),
                            ),
                    );
                if self.show_trend_data {
                    let mut data = div()
                        .id("overview-trend-values")
                        .role(gpui::Role::Table)
                        .flex()
                        .flex_col()
                        .w_full()
                        .min_w_0()
                        .child(
                            div()
                                .id("heading")
                                .role(gpui::Role::Row)
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .py_1()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child(
                                    div()
                                        .id("time")
                                        .role(gpui::Role::ColumnHeader)
                                        .child(t(k::OVERVIEW_TREND_BUCKET)),
                                )
                                .child(
                                    div()
                                        .id("count")
                                        .role(gpui::Role::ColumnHeader)
                                        .child(t(k::OVERVIEW_USAGE_RECORDS)),
                                ),
                        );
                    for (index, bucket) in trend.iter().enumerate() {
                        data = data.child(
                            div()
                                .id(index)
                                .role(gpui::Role::Row)
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .py_1()
                                .border_b_1()
                                .border_color(theme::border())
                                .text_xs()
                                .child(
                                    div()
                                        .id("time")
                                        .role(gpui::Role::Cell)
                                        .text_color(theme::muted())
                                        .child(bucket_label(&bucket.date)),
                                )
                                .child(
                                    div()
                                        .id("count")
                                        .role(gpui::Role::Cell)
                                        .child(format_count(bucket.request_count)),
                                ),
                        );
                    }
                    section = section.child(data);
                }
            }
        } else {
            section = section.child(note(t(k::OVERVIEW_TREND_FAILED), true));
        }
        section
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::OVERVIEW_USAGE_SCOPE)),
            )
            .into_any_element()
    }

    fn render_requests(&self, snapshot: &OverviewSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let open = components::button(
            "overview-open-records",
            t(k::OVERVIEW_OPEN_RECORDS),
            ButtonTone::Ghost,
            ButtonSize::Sm,
        )
        .on_click(cx.listener(|_, _, _, cx| cx.emit(OverviewEvent::Usage)));
        let mut section = div()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child(self.section_heading(t(k::OVERVIEW_REQUESTS), open.into_any_element()));
        let Some(requests) = &snapshot.recent_requests.data else {
            return section.child(self.unavailable_section()).into_any_element();
        };
        if requests.is_empty() {
            section = section.child(note(t(k::OVERVIEW_REQUESTS_EMPTY), false));
        }
        for request in requests.iter().take(6) {
            section =
                section.child(self.request_row(request, snapshot.window.node_utc_offset_seconds));
        }
        section
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::OVERVIEW_REQUESTS_SCOPE)),
            )
            .child(
                components::button(
                    "overview-open-sessions",
                    t(k::OVERVIEW_OPEN_SESSIONS),
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(OverviewEvent::Sessions))),
            )
            .into_any_element()
    }

    fn request_row(&self, request: &OverviewRequest, offset: i32) -> AnyElement {
        let source = match request.source {
            OverviewUsageSource::Captured => t(k::OVERVIEW_SOURCE_CAPTURED),
            OverviewUsageSource::Session => t(k::OVERVIEW_SOURCE_SESSION),
            OverviewUsageSource::Unknown => t(k::OVERVIEW_SOURCE_UNKNOWN),
        };
        let latency = request
            .latency_ms
            .map(|ms| format!("{ms} ms"))
            .unwrap_or_else(|| raw(k::OVERVIEW_TIMING_UNKNOWN).to_string());
        div()
            .flex()
            .items_start()
            .gap_3()
            .min_w_0()
            .py_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .w(px(92.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(timestamp_label(request.created_at, offset, true)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(theme::text())
                            .child(request.provider_name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(format!("{} · {}", request.model, source)),
                    )
                    .child(div().text_xs().text_color(theme::muted()).child(latency)),
            )
            .child(components::badge(
                if request.status_code >= 400 {
                    BadgeTone::Danger
                } else {
                    BadgeTone::Neutral
                },
                if request.source == OverviewUsageSource::Session {
                    t(k::OVERVIEW_IMPORTED)
                } else {
                    SharedString::from(request.status_code.to_string())
                },
            ))
            .into_any_element()
    }

    fn render_performance(
        &self,
        snapshot: &OverviewSnapshot,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = div().flex().flex_col().gap_3().min_w_0().child(
            self.section_heading(t(k::OVERVIEW_PERFORMANCE), gpui::Empty.into_any_element()),
        );
        let Some(performance) = &snapshot.performance.data else {
            return section.child(self.unavailable_section()).into_any_element();
        };
        if performance.is_empty() {
            return section
                .child(note(t(k::OVERVIEW_PERFORMANCE_EMPTY), false))
                .into_any_element();
        }
        for group in performance.iter().take(PERFORMANCE_PREVIEW_LIMIT) {
            section = section.child(performance_row(group));
        }
        if performance.len() > PERFORMANCE_PREVIEW_LIMIT {
            section = section.child(note(
                tf!(
                    k::OVERVIEW_PREVIEW_LIMIT,
                    shown = PERFORMANCE_PREVIEW_LIMIT,
                    total = performance.len()
                )
                .into(),
                false,
            ));
        }
        section
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::OVERVIEW_PERFORMANCE_SCOPE)),
            )
            .into_any_element()
    }

    fn render_content(
        &self,
        snapshot: &OverviewSnapshot,
        wide: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let failed_recent = snapshot
            .recent_requests
            .data
            .as_ref()
            .map(|requests| {
                requests
                    .iter()
                    .filter(|request| {
                        request.source == OverviewUsageSource::Captured
                            && request.status_code >= 400
                    })
                    .count()
            })
            .unwrap_or(0);
        let mut column = div()
            .flex()
            .flex_col()
            .gap_6()
            .w_full()
            .min_w_0()
            .max_w(px(1220.))
            .child(self.render_gateway(snapshot));
        if failed_recent > 0 {
            column = column.child(note(
                tf!(k::OVERVIEW_RECENT_FAILURES, count = failed_recent).into(),
                true,
            ));
        }
        column = column
            .child(self.render_routes(snapshot, cx))
            .child(self.render_apps(snapshot, cx));
        let lower = div()
            .flex()
            .gap_6()
            .min_w_0()
            .when(!wide, |row| row.flex_col())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_usage(snapshot, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_requests(snapshot, cx)),
            );
        column
            .child(lower)
            .child(self.render_performance(snapshot, cx))
            .into_any_element()
    }
}

impl Render for OverviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let select_range = cx.listener(|this, index: &usize, _, cx| {
            let range = TimeRange::from_index(*index);
            if this.range != range {
                this.range = range;
                this.reload(cx);
            }
        });
        let range_options = [
            raw(k::OVERVIEW_RANGE_DAY),
            raw(k::OVERVIEW_RANGE_WEEK),
            raw(k::OVERVIEW_RANGE_MONTH),
        ];
        let range = if self.workspace_available {
            components::segmented(
                "overview-time-range",
                &range_options,
                self.range.index(),
                move |index, window, cx| select_range(&index, window, cx),
            )
        } else {
            components::segmented_readonly(
                "overview-time-range",
                &range_options,
                self.range.index(),
            )
        };
        let refresh = if self.loading {
            components::busy_button(
                "overview-refresh",
                t(k::OVERVIEW_REFRESH),
                ButtonTone::Ghost,
                ButtonSize::Sm,
                false,
            )
        } else if !self.workspace_available {
            components::disabled_button(
                "overview-refresh",
                t(k::OVERVIEW_REFRESH),
                ButtonTone::Ghost,
                ButtonSize::Sm,
                true,
            )
        } else {
            components::button(
                "overview-refresh",
                t(k::OVERVIEW_REFRESH),
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.loading {
                    this.reload(cx)
                }
            }))
        };
        let header = layout::page_header(t(k::OVERVIEW_TITLE), Some(self.scope_label.clone()))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(range)
                    .child(refresh),
            );
        let content = if !self.workspace_available {
            div()
                .flex()
                .flex_col()
                .gap_4()
                .w_full()
                .max_w(px(800.))
                .child(note(t(k::OVERVIEW_OFFLINE), true))
                .child(
                    components::button(
                        "overview-manage-remote",
                        t(k::OVERVIEW_OPEN_REMOTE),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(OverviewEvent::Remote))),
                )
                .into_any_element()
        } else if self.loading {
            note(t(k::OVERVIEW_LOADING), false).into_any_element()
        } else if self.load_failed {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .child(note(t(k::OVERVIEW_LOAD_FAILED), true))
                .when(self.backend.is_remote(), |column| {
                    column.child(
                        components::button(
                            "overview-load-remote",
                            t(k::OVERVIEW_OPEN_REMOTE),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(OverviewEvent::Remote))),
                    )
                })
                .into_any_element()
        } else if let Some(snapshot) = &self.snapshot {
            self.render_content(snapshot, f32::from(window.bounds().size.width) >= 1180., cx)
        } else {
            note(t(k::OVERVIEW_LOADING), false).into_any_element()
        };
        layout::page().child(header).child(layout::scroll_body(
            "overview-scroll",
            &self.scroll,
            content,
        ))
    }
}

fn note(message: SharedString, warning: bool) -> gpui::Div {
    div()
        .px_3()
        .py_3()
        .text_xs()
        .text_color(if warning {
            theme::yellow()
        } else {
            theme::muted()
        })
        .child(message)
}

fn metric(label: SharedString, value: String) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(theme::muted()).child(label))
        .child(
            div()
                .text_size(px(24.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text())
                .child(value),
        )
}

fn performance_row(group: &OverviewProviderPerformance) -> gpui::Div {
    let source = match group.source {
        OverviewUsageSource::Captured => t(k::OVERVIEW_SOURCE_CAPTURED),
        OverviewUsageSource::Session => t(k::OVERVIEW_SOURCE_SESSION),
        OverviewUsageSource::Unknown => t(k::OVERVIEW_SOURCE_UNKNOWN),
    };
    let latency = group
        .avg_latency_ms
        .map(|ms| tf!(k::OVERVIEW_AVERAGE_LATENCY, value = ms))
        .unwrap_or_else(|| raw(k::OVERVIEW_TIMING_UNKNOWN).to_string());
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_3()
        .min_w_0()
        .py_2()
        .border_b_1()
        .border_color(theme::border())
        .child(
            div()
                .flex_1()
                .min_w(px(180.))
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(theme::text())
                        .child(group.provider_name.clone()),
                )
                .child(div().text_xs().text_color(theme::muted()).child(source)),
        )
        .child(div().text_xs().text_color(theme::muted()).child(tf!(
            k::OVERVIEW_RECORD_COUNT,
            count = format_count(group.request_count)
        )))
        .child(
            div()
                .min_w(px(92.))
                .text_xs()
                .text_color(theme::text())
                .child(format!("${}", group.estimated_cost_usd)),
        )
        .child(div().text_xs().text_color(theme::muted()).child(latency))
}

fn visible_route_selection<'a>(
    matches: &[&'a OverviewRoute],
    selected: Option<&str>,
) -> Option<&'a OverviewRoute> {
    matches
        .iter()
        .take(ROUTE_PREVIEW_LIMIT)
        .copied()
        .find(|route| selected == Some(route.id.as_str()))
        .or_else(|| matches.first().copied())
}

fn trend_bar_height(count: u64, maximum: u64) -> f32 {
    if maximum == 0 {
        return 0.;
    }
    (count as f64 / maximum as f64).clamp(0., 1.) as f32 * 100.
}

fn route_matches(route: &OverviewRoute, routing: &OverviewRouting, query: &str) -> bool {
    query.is_empty()
        || route.name.to_lowercase().contains(query)
        || route.id.to_lowercase().contains(query)
        || route
            .app
            .as_ref()
            .is_some_and(|app| app.to_lowercase().contains(query))
        || routing
            .channels
            .iter()
            .filter(|channel| route_allows_channel(route, channel))
            .any(|channel| {
                channel.name.to_lowercase().contains(query)
                    || channel
                        .origin
                        .as_ref()
                        .is_some_and(|origin| origin.to_lowercase().contains(query))
            })
}

fn route_allows_channel(route: &OverviewRoute, channel: &OverviewChannel) -> bool {
    if route.uses_all_channels {
        channel.enabled
    } else {
        route.channel_ids.contains(&channel.id)
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

fn timestamp_label(seconds: i64, offset: i32, date: bool) -> String {
    DateTime::from_timestamp(seconds, 0)
        .and_then(|time| FixedOffset::east_opt(offset).map(|offset| time.with_timezone(&offset)))
        .map(|time| {
            time.format(if date { "%m-%d %H:%M" } else { "%H:%M" })
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

fn offset_label(offset: i32) -> String {
    let sign = if offset >= 0 { '+' } else { '-' };
    let minutes = offset.unsigned_abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

fn bucket_label(value: &str) -> String {
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        // Hourly buckets carry their own offset. Keep it so a DST clock
        // change cannot produce two indistinguishable local-hour labels.
        time.format("%m-%d %H:%M %:z").to_string()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ochub_core::application::OverviewReachability;
    use ochub_core::gateway::Dialect;

    #[test]
    fn candidate_search_obeys_empty_allowlist_and_explicit_disabled_members() {
        let channel = |id: &str, enabled| OverviewChannel {
            id: id.into(),
            endpoint_id: None,
            name: id.into(),
            dialect: Dialect::Responses,
            origin: None,
            enabled,
            priority: 0,
            weight: 1,
            model_count: 0,
            accepts_all_models: true,
            models: vec![],
            model_override: None,
            reachability: OverviewReachability::Unknown,
            reachability_observed_at: None,
        };
        let mut route = OverviewRoute {
            id: "route".into(),
            name: "Route".into(),
            app: None,
            enabled: true,
            is_station: false,
            uses_all_channels: true,
            channel_ids: vec![],
            enabled_channel_count: 1,
            missing_channel_ids: vec![],
            default_model: None,
            model_rule_count: 0,
            model_rules: vec![],
        };
        let routing = OverviewRouting {
            routes: vec![],
            channels: vec![channel("active", true), channel("disabled", false)],
            bindings: vec![],
            policy_details_available: true,
        };
        assert!(route_matches(&route, &routing, "active"));
        assert!(!route_matches(&route, &routing, "disabled"));
        route.uses_all_channels = false;
        route.channel_ids.push("disabled".into());
        assert!(route_matches(&route, &routing, "disabled"));
        assert!(!route_matches(&route, &routing, "active"));
    }

    #[test]
    fn node_time_is_not_formatted_in_the_controlling_desktop_timezone() {
        assert_eq!(timestamp_label(0, 28_800, false), "08:00");
        assert_eq!(timestamp_label(0, -18_000, false), "19:00");
        assert_eq!(offset_label(-19_800), "UTC-05:30");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(timestamp_label(0, -18_000, true), "12-31 19:00");
        assert_eq!(timestamp_label(86_400, -18_000, true), "01-01 19:00");
        assert_ne!(
            bucket_label("2026-11-01T01:00:00-04:00"),
            bucket_label("2026-11-01T01:00:00-05:00")
        );
    }

    #[test]
    fn route_inspector_selection_must_be_a_visible_search_result() {
        let routes: Vec<_> = (0..=ROUTE_PREVIEW_LIMIT)
            .map(|index| OverviewRoute {
                id: index.to_string(),
                name: format!("Route {index}"),
                app: None,
                enabled: true,
                is_station: false,
                uses_all_channels: true,
                channel_ids: vec![],
                enabled_channel_count: 0,
                missing_channel_ids: vec![],
                default_model: None,
                model_rule_count: 0,
                model_rules: vec![],
            })
            .collect();
        let all: Vec<_> = routes.iter().collect();
        // Clearing a query after inspecting the ninth route must not leave a
        // hidden item selected while the eight visible rows look unselected.
        assert_eq!(visible_route_selection(&all, Some("8")).unwrap().id, "0");
        assert_eq!(visible_route_selection(&all, Some("2")).unwrap().id, "2");
        let filtered = vec![&routes[8]];
        assert_eq!(
            visible_route_selection(&filtered, Some("2")).unwrap().id,
            "8"
        );
        assert!(visible_route_selection(&[], Some("2")).is_none());
    }

    #[test]
    fn count_chart_preserves_zero_and_relative_scale() {
        assert_eq!(trend_bar_height(0, 20), 0.);
        assert_eq!(trend_bar_height(1, 20), 5.);
        assert_eq!(trend_bar_height(20, 20), 100.);
        assert_eq!(trend_bar_height(0, 0), 0.);
        assert_eq!(trend_bar_height(u64::MAX, u64::MAX), 100.);
        assert!(trend_bar_height(1, u64::MAX).is_finite());
    }
}
