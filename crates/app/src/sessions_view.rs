//! Sessions panel. Lists recent CLI sessions discovered on disk via
//! `session_manager::scan_sessions()` and supports deleting one. Scanning and
//! transcript loading are filesystem-heavy, so both run on the background
//! executor; scan results are cached for [`SCAN_TTL`] so re-entering the
//! section doesn't rescan every time.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, Local, NaiveDate, TimeZone};
use gpui::{
    anchored, deferred, div, point, prelude::*, px, Anchor, AnyElement, Context, ElementId, Entity,
    FontWeight, ListAlignment, ListState, MouseButton, SharedString, Window,
};
use ochub_core::session_manager::{self, SessionMessage, SessionMeta};
use ochub_core::AppState;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::{icon, IconName};
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

/// Sessions per page in the list (avoids rendering hundreds of rows at once).
const PAGE_SIZE: usize = 20;

/// How long a completed scan stays fresh; re-entering the section within this
/// window shows the cached list instantly (刷新按钮无视 TTL 强制重扫).
const SCAN_TTL: Duration = Duration::from_secs(30);

/// Long tool outputs and pasted files can contain hundreds of thousands of
/// characters. Keep the default layout bounded; users can still expand any
/// individual message when they need the full text.
const MESSAGE_PREVIEW_CHARS: usize = 3_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionDateFilter {
    All,
    Today,
    SevenDays,
    ThirtyDays,
    Exact(NaiveDate),
}

impl SessionDateFilter {
    fn label(self) -> String {
        match self {
            Self::All => "全部时间".to_string(),
            Self::Today => "今天".to_string(),
            Self::SevenDays => "最近 7 天".to_string(),
            Self::ThirtyDays => "最近 30 天".to_string(),
            Self::Exact(date) => date.format("%Y-%m-%d").to_string(),
        }
    }

    fn matches(self, timestamp_ms: Option<i64>) -> bool {
        if self == Self::All {
            return true;
        }
        let Some(timestamp_ms) = timestamp_ms else {
            return false;
        };
        let Some(active_date) = Local
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .map(|time| time.date_naive())
        else {
            return false;
        };
        let today = Local::now().date_naive();
        match self {
            Self::All => true,
            Self::Today => active_date == today,
            Self::SevenDays => {
                active_date >= today - ChronoDuration::days(6) && active_date <= today
            }
            Self::ThirtyDays => {
                active_date >= today - ChronoDuration::days(29) && active_date <= today
            }
            Self::Exact(date) => active_date == date,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionFilterPopover {
    Date,
    App,
}

/// An opened session: its metadata plus the loaded conversation transcript.
struct SessionDetail {
    meta: SessionMeta,
    messages: Vec<SessionMessage>,
    error: Option<SharedString>,
}

pub struct SessionsView {
    #[allow(dead_code)]
    app: Arc<AppState>,
    sessions: Vec<SessionMeta>,
    status: Option<SharedString>,
    /// Zero-based current page into `sessions`.
    page: usize,
    /// When `Some`, the transcript viewer replaces the list.
    detail: Option<SessionDetail>,
    /// Session index pending deletion confirmation; when `Some`, a modal is shown.
    confirm_delete: Option<usize>,
    /// A background scan is in flight (suppresses duplicate scans).
    scanning: bool,
    /// Session index whose transcript is currently loading.
    loading_detail: Option<usize>,
    /// When the last scan finished; drives the [`SCAN_TTL`] freshness check.
    last_scan: Option<Instant>,
    date_filter: SessionDateFilter,
    app_filter: Option<String>,
    open_filter_popover: Option<SessionFilterPopover>,
    date_filter_error: Option<SharedString>,
    /// Drives the transcript's variable-height virtual list.
    transcript_list_state: ListState,
    /// Message indexes explicitly expanded by the user.
    expanded_messages: HashSet<usize>,
    page_input: Entity<TextInput>,
    date_input: Entity<TextInput>,
}

impl SessionsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let this = Self {
            app,
            sessions: Vec::new(),
            status: None,
            page: 0,
            detail: None,
            confirm_delete: None,
            scanning: false,
            loading_detail: None,
            last_scan: None,
            date_filter: SessionDateFilter::All,
            app_filter: None,
            open_filter_popover: None,
            date_filter_error: None,
            transcript_list_state: ListState::new(0, ListAlignment::Top, px(320.)),
            expanded_messages: HashSet::new(),
            page_input: cx.new(|cx| text_input(cx, "页码")),
            date_input: cx.new(|cx| text_input(cx, "YYYY-MM-DD 或 MM-DD")),
        };
        // Do not scan here: AppRoot eagerly constructs every section. The
        // shell calls `reload` when Sessions is actually selected.
        // “跳至 X 页”回车提交。
        let jump = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            let text = this.page_input.read(cx).content().trim().to_string();
            if let Ok(target) = text.parse::<usize>() {
                if target >= 1 {
                    let last = this.total_pages().saturating_sub(1);
                    this.set_page((target - 1).min(last), cx);
                }
            }
        });
        this.page_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| jump(&(), window, cx));
        });
        // 指定日期输入框：回车直接应用筛选。
        let apply_date = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.apply_exact_date_filter(cx);
        });
        this.date_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| apply_date(&(), window, cx));
        });
        this
    }

    /// Re-entering the section: close any open transcript and rescan in the
    /// background — unless the cached list is still fresh, in which case it
    /// shows instantly with no IO at all.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        let fresh = self.last_scan.is_some_and(|at| at.elapsed() < SCAN_TTL);
        if fresh || self.scanning {
            cx.notify();
            return;
        }
        self.start_scan(cx);
    }

    /// The refresh button: always rescan, ignoring the TTL.
    fn force_reload(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        if !self.scanning {
            self.start_scan(cx);
        }
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        self.scanning = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let sessions = cx
                .background_spawn(async move { session_manager::scan_sessions() })
                .await;
            this.update(cx, |this, cx| {
                this.sessions = sessions;
                this.scanning = false;
                this.last_scan = Some(Instant::now());
                // Keep the current page in range after the list size changes.
                let max_page = this.total_pages().saturating_sub(1);
                if this.page > max_page {
                    this.page = max_page;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn total_pages(&self) -> usize {
        self.filtered_session_count().div_ceil(PAGE_SIZE).max(1)
    }

    fn session_matches_filters(&self, session: &SessionMeta) -> bool {
        let app_matches = self
            .app_filter
            .as_deref()
            .is_none_or(|app| session.provider_id == app);
        let timestamp = session.last_active_at.or(session.created_at);
        app_matches && self.date_filter.matches(timestamp)
    }

    fn filtered_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| self.session_matches_filters(session))
            .count()
    }

    fn filtered_session_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, session)| self.session_matches_filters(session).then_some(idx))
            .collect()
    }

    fn app_options(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| session.provider_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let max_page = self.total_pages().saturating_sub(1);
        let page = page.min(max_page);
        if page != self.page {
            self.page = page;
            cx.notify();
        }
    }

    fn set_date_filter(&mut self, filter: SessionDateFilter, cx: &mut Context<Self>) {
        self.date_filter = filter;
        if !matches!(filter, SessionDateFilter::Exact(_)) {
            self.date_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        self.page = 0;
        self.open_filter_popover = None;
        self.date_filter_error = None;
        self.date_input
            .update(cx, |input, cx| input.set_content("", cx));
        cx.notify();
    }

    fn set_app_filter(&mut self, app: Option<String>, cx: &mut Context<Self>) {
        self.app_filter = app;
        self.page = 0;
        self.open_filter_popover = None;
        cx.notify();
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.date_filter = SessionDateFilter::All;
        self.app_filter = None;
        self.page = 0;
        self.open_filter_popover = None;
        self.date_filter_error = None;
        cx.notify();
    }

    fn toggle_filter_popover(&mut self, popover: SessionFilterPopover, cx: &mut Context<Self>) {
        self.open_filter_popover = if self.open_filter_popover == Some(popover) {
            None
        } else {
            Some(popover)
        };
        self.date_filter_error = None;
        cx.notify();
    }

    fn apply_exact_date_filter(&mut self, cx: &mut Context<Self>) {
        let value = self.date_input.read(cx).content().trim().to_string();
        if value.is_empty() {
            self.set_date_filter(SessionDateFilter::All, cx);
            return;
        }
        if let Some(date) = components::parse_jump_date(&value) {
            self.set_date_filter(SessionDateFilter::Exact(date), cx);
        } else {
            self.date_filter_error = Some(SharedString::from("请输入 YYYY-MM-DD 或 MM-DD"));
            cx.notify();
        }
    }

    fn title_for(session: &SessionMeta) -> String {
        session
            .title
            .clone()
            .or_else(|| session.summary.clone())
            .unwrap_or_else(|| session.session_id.clone())
    }

    fn do_delete(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(idx) else {
            return;
        };
        let source_path = session.source_path.clone().unwrap_or_default();
        match session_manager::delete_session(
            &session.provider_id,
            &session.session_id,
            &source_path,
        ) {
            Ok(true) => {
                self.status = Some(SharedString::from("会话已删除"));
                // 列表本地同步移除即可，无需整库重扫。
                self.sessions.remove(idx);
                let max_page = self.total_pages().saturating_sub(1);
                if self.page > max_page {
                    self.page = max_page;
                }
            }
            Ok(false) => {
                self.status = Some(SharedString::from("未找到会话"));
                self.force_reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        cx.notify();
    }

    /// Load a session's full transcript (background — files can be MBs) and
    /// switch to the detail viewer when it arrives.
    fn open_detail(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.loading_detail.is_some() {
            return;
        }
        let Some(session) = self.sessions.get(idx).cloned() else {
            return;
        };
        self.loading_detail = Some(idx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let source_path = session.source_path.clone().unwrap_or_default();
            let provider_id = session.provider_id.clone();
            let loaded = cx
                .background_spawn(async move {
                    session_manager::load_messages(&provider_id, &source_path)
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading_detail = None;
                let detail = match loaded {
                    Ok(messages) => SessionDetail {
                        meta: session,
                        messages,
                        error: None,
                    },
                    Err(err) => SessionDetail {
                        meta: session,
                        messages: Vec::new(),
                        error: Some(SharedString::from(format!("加载对话失败: {err}"))),
                    },
                };
                this.transcript_list_state.reset(detail.messages.len());
                this.expanded_messages.clear();
                this.detail = Some(detail);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        self.transcript_list_state.reset(0);
        self.expanded_messages.clear();
        cx.notify();
    }

    /// Per-role accent color + soft background for a transcript bubble.
    fn role_colors(role: &str) -> (gpui::Rgba, gpui::Rgba) {
        match role {
            "user" => (theme::accent(), theme::accent_soft()),
            "assistant" => (theme::green(), theme::green_soft()),
            "system" => (theme::muted(), theme::inset()),
            _ => (theme::mauve(), theme::surface_hover()),
        }
    }

    fn role_label(role: &str) -> SharedString {
        match role {
            "user" => SharedString::from("用户"),
            "assistant" => SharedString::from("助手"),
            "system" => SharedString::from("系统"),
            "tool" => SharedString::from("工具"),
            other => SharedString::from(other.to_string()),
        }
    }

    fn app_label(provider_id: &str) -> SharedString {
        SharedString::from(
            match provider_id {
                "claude" => "Claude Code",
                "codex" => "Codex",
                "gemini" => "Gemini",
                "opencode" => "OpenCode",
                "openclaw" => "OpenClaw",
                "hermes" => "Hermes",
                other => other,
            }
            .to_string(),
        )
    }

    fn active_time(session: &SessionMeta, include_year: bool) -> Option<SharedString> {
        let timestamp = session.last_active_at.or(session.created_at)?;
        Local.timestamp_millis_opt(timestamp).single().map(|time| {
            SharedString::from(
                time.format(if include_year {
                    "%Y-%m-%d %H:%M"
                } else {
                    "%m-%d %H:%M"
                })
                .to_string(),
            )
        })
    }

    fn message_content(content: &str, expanded: bool) -> (SharedString, bool) {
        if content.trim().is_empty() {
            return (SharedString::from("（空消息）"), false);
        }
        let cutoff = content
            .char_indices()
            .nth(MESSAGE_PREVIEW_CHARS)
            .map(|(byte_index, _)| byte_index);
        let is_long = cutoff.is_some();
        if expanded || !is_long {
            return (SharedString::from(content.to_string()), is_long);
        }
        let preview = &content[..cutoff.unwrap_or(content.len())];
        (SharedString::from(format!("{preview}\n\n…")), true)
    }

    fn toggle_message(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.expanded_messages.remove(&index) {
            self.expanded_messages.insert(index);
        }
        self.transcript_list_state.remeasure();
        cx.notify();
    }

    fn render_message(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(message) = self
            .detail
            .as_ref()
            .and_then(|detail| detail.messages.get(index))
        else {
            return div().into_any_element();
        };
        let expanded = self.expanded_messages.contains(&index);
        let (accent, soft) = Self::role_colors(&message.role);
        let label = Self::role_label(&message.role);
        let (content, is_long) = Self::message_content(&message.content, expanded);

        div()
            .w_full()
            .pb_3()
            .child(
                components::card()
                    .flex_shrink_0()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(components::status_dot_sized(accent, 6.))
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(soft)
                                    .text_color(accent)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(theme::text())
                            .text_sm()
                            .line_height(px(20.))
                            .child(content),
                    )
                    .when(is_long, |card| {
                        card.child(
                            div().flex().flex_row().child(
                                components::button(
                                    SharedString::from(format!("session-message-toggle-{index}")),
                                    if expanded { "收起" } else { "展开全文" },
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.toggle_message(index, cx);
                                    },
                                )),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_detail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = self.detail.as_ref() else {
            return layout::page().into_any_element();
        };
        let title = Self::title_for(&detail.meta);
        let provider = Self::app_label(&detail.meta.provider_id);
        let count = detail.messages.len();
        let error = detail.error.clone();
        let subtitle = match Self::active_time(&detail.meta, true) {
            Some(time) => SharedString::from(format!("{count} 条消息 · {time}")),
            None => SharedString::from(format!("{count} 条消息")),
        };
        if self.transcript_list_state.item_count() != count {
            self.transcript_list_state.reset(count);
        }

        let body = if let Some(error) = error {
            layout::scroll_body(
                "session-transcript-error",
                layout::content_column().child(components::empty_state(
                    IconName::Message,
                    "无法加载对话",
                    error,
                    None,
                )),
            )
            .into_any_element()
        } else if count == 0 {
            layout::scroll_body(
                "session-transcript-empty",
                layout::content_column().child(components::empty_state(
                    IconName::Message,
                    "没有可显示的消息",
                    "这条会话没有可显示的消息。",
                    None,
                )),
            )
            .into_any_element()
        } else {
            let list = gpui::list(
                self.transcript_list_state.clone(),
                cx.processor(|this, index, _window, cx| this.render_message(index, cx)),
            );
            layout::virtual_body(list).into_any_element()
        };

        layout::page()
            .child(
                layout::page_header(title, Some(subtitle)).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(
                            components::icon_button_tone(
                                "session-back",
                                "返回",
                                IconName::ChevronLeft,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.close_detail(cx)),
                            ),
                        )
                        .child(components::badge(BadgeTone::Teal, provider)),
                ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_card(
        &self,
        idx: usize,
        session: &SessionMeta,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = Self::title_for(session);
        let provider = Self::app_label(&session.provider_id);
        let active_time = Self::active_time(session, false);
        let is_loading = self.loading_detail == Some(idx);

        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .id(SharedString::from(format!("session-open-{idx}")))
                    .role(gpui::Role::Button)
                    .aria_label("查看完整对话")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_detail(idx, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(components::badge(BadgeTone::Teal, provider))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(SharedString::from(title)),
                            ),
                    )
                    .when_some(active_time, |s, time| {
                        s.child(
                            div()
                                .min_w_0()
                                .text_color(theme::muted())
                                .text_xs()
                                .child(time),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        components::button(
                            SharedString::from(format!("session-view-{idx}")),
                            if is_loading { "加载中…" } else { "查看" },
                            if is_loading {
                                ButtonTone::Neutral
                            } else {
                                ButtonTone::Primary
                            },
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_detail(idx, cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("session-delete-{idx}")),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(idx);
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let date_open = self.open_filter_popover == Some(SessionFilterPopover::Date);
        let mut date_popover = session_filter_popover("sessions-date-popover", 264.)
            .p_1()
            .child(
                session_dropdown_option(
                    "sessions-date-all",
                    "全部时间",
                    self.date_filter == SessionDateFilter::All,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::All, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-today",
                    "今天",
                    self.date_filter == SessionDateFilter::Today,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::Today, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-week",
                    "最近 7 天",
                    self.date_filter == SessionDateFilter::SevenDays,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::SevenDays, cx);
                })),
            )
            .child(
                session_dropdown_option(
                    "sessions-date-month",
                    "最近 30 天",
                    self.date_filter == SessionDateFilter::ThirtyDays,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.set_date_filter(SessionDateFilter::ThirtyDays, cx);
                })),
            )
            .child(div().mx_2().my_1().h(px(1.)).bg(theme::border()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::muted())
                            .child("指定日期"),
                    )
                    .child(self.date_input.clone())
                    .when_some(self.date_filter_error.clone(), |column, error| {
                        column.child(div().text_xs().text_color(theme::red()).child(error))
                    })
                    .child(
                        components::button(
                            "sessions-date-apply",
                            "应用",
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.apply_exact_date_filter(cx);
                            },
                        )),
                    ),
            );
        date_popover = date_popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
            if this.open_filter_popover == Some(SessionFilterPopover::Date) {
                this.open_filter_popover = None;
                this.date_filter_error = None;
                cx.notify();
            }
        }));
        let date_control = div()
            .relative()
            .flex_none()
            .child(
                session_filter_trigger(
                    "sessions-date-filter",
                    self.date_filter.label(),
                    IconName::Calendar,
                    date_open,
                    176.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.toggle_filter_popover(SessionFilterPopover::Date, cx);
                    }),
                ),
            )
            .when(date_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(40.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(date_popover),
                    )
                    .priority(10),
                )
            });

        let app_open = self.open_filter_popover == Some(SessionFilterPopover::App);
        let mut app_popover = session_filter_popover("sessions-app-popover", 220.)
            .p_1()
            .max_h(px(280.))
            .overflow_y_scroll()
            .child(
                session_dropdown_option("sessions-app-all", "全部应用", self.app_filter.is_none())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.set_app_filter(None, cx);
                    })),
            );
        for (index, app) in self.app_options().into_iter().enumerate() {
            let selected = self.app_filter.as_deref() == Some(app.as_str());
            let app_for_click = app.clone();
            app_popover = app_popover.child(
                session_dropdown_option(
                    ElementId::Name(format!("sessions-app-option-{index}").into()),
                    Self::app_label(&app),
                    selected,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_app_filter(Some(app_for_click.clone()), cx);
                })),
            );
        }
        app_popover = app_popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
            if this.open_filter_popover == Some(SessionFilterPopover::App) {
                this.open_filter_popover = None;
                cx.notify();
            }
        }));
        let app_label = self
            .app_filter
            .as_deref()
            .map(Self::app_label)
            .unwrap_or_else(|| SharedString::from("全部应用"));
        let app_control = div()
            .relative()
            .flex_none()
            .child(
                session_filter_trigger(
                    "sessions-app-filter",
                    app_label,
                    IconName::Layers,
                    app_open,
                    176.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.toggle_filter_popover(SessionFilterPopover::App, cx);
                    }),
                ),
            )
            .when(app_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(40.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(app_popover),
                    )
                    .priority(10),
                )
            });

        let has_active_filters =
            self.date_filter != SessionDateFilter::All || self.app_filter.is_some();
        components::card().p_3().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(date_control)
                .child(app_control)
                .when(has_active_filters, |row| {
                    row.child(
                        components::button(
                            "sessions-clear-filters",
                            "重置",
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.clear_filters(cx);
                            },
                        )),
                    )
                }),
        )
    }

    fn render_pagination(&self, cx: &mut Context<Self>) -> gpui::Div {
        let go = cx.listener(|this, page: &u32, _window, cx| {
            this.set_page(*page as usize, cx);
        });
        div().px_6().child(components::pagination_bar(
            "sessions-pages",
            self.page as u32,
            self.total_pages() as u32,
            Some(self.filtered_session_count() as u64),
            &self.page_input,
            move |page, window, cx| go(&page, window, cx),
        ))
    }
}

fn session_filter_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: IconName,
    expanded: bool,
    width: f32,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_expanded(expanded)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w(px(width))
        .h(px(36.))
        .px_3()
        .rounded_lg()
        .border_1()
        .border_color(if expanded {
            theme::accent()
        } else {
            theme::border_strong()
        })
        .bg(theme::surface())
        .cursor_pointer()
        .text_sm()
        .text_color(theme::text())
        .hover(|style| style.border_color(theme::accent()).bg(theme::panel()))
        .child(icon(icon_name, theme::muted(), 15.))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .child(icon(IconName::ChevronDown, theme::muted(), 13.))
}

fn session_filter_popover(id: &'static str, width: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w(px(width))
        .rounded_lg()
        .border_1()
        .border_color(theme::border())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude()
}

fn session_dropdown_option(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let option = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_selected(selected)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .min_h(px(34.))
        .px_3()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .child(div().min_w_0().flex_1().truncate().child(label));
    if selected {
        option
            .bg(theme::accent_soft())
            .text_color(theme::accent())
            .font_weight(FontWeight::MEDIUM)
            .child(icon(IconName::Check, theme::accent(), 13.))
    } else {
        option
            .text_color(theme::subtext())
            .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()))
    }
}

fn text_input(cx: &mut Context<TextInput>, placeholder: &str) -> TextInput {
    TextInput::new(cx, placeholder)
}

impl Render for SessionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.detail.is_some() {
            return self.render_detail(cx);
        }
        let filtered_indices = self.filtered_session_indices();
        let total = filtered_indices.len();
        let max_page = total.div_ceil(PAGE_SIZE).max(1).saturating_sub(1);
        if self.page > max_page {
            self.page = max_page;
        }
        let start = self.page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(total);
        let cards: Vec<_> = filtered_indices[start..end]
            .iter()
            .filter_map(|idx| {
                self.sessions
                    .get(*idx)
                    .map(|session| self.render_card(*idx, session, cx))
            })
            .collect();
        let has_no_sessions = self.sessions.is_empty();
        let has_no_matches = !has_no_sessions && total == 0;
        let scanning = self.scanning;
        let show_pagination = total > PAGE_SIZE;
        let confirm = self.confirm_delete.and_then(|idx| {
            self.sessions
                .get(idx)
                .map(Self::title_for)
                .map(|t| (idx, t))
        });

        layout::page()
            .relative()
            .child(
                layout::page_header("会话", Some("浏览与管理本机 CLI 的对话记录。".into())).child(
                    components::icon_button_tone(
                        "sessions-refresh",
                        if scanning { "扫描中…" } else { "刷新" },
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .flex_shrink_0()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.force_reload(cx);
                    })),
                ),
            )
            .child(layout::scroll_body(
                "session-list",
                layout::content_column()
                    .child(self.render_filters(cx))
                    .when(has_no_sessions, |s| {
                        s.child(components::empty_state(
                            IconName::Clock,
                            if scanning {
                                "正在扫描会话…"
                            } else {
                                "没有找到会话"
                            },
                            if scanning {
                                "正在读取本机 CLI 的会话记录。"
                            } else {
                                "扫描到的 CLI 会话会显示在这里。"
                            },
                            None,
                        ))
                    })
                    .when(has_no_matches, |s| {
                        s.child(components::empty_state(
                            IconName::Search,
                            "没有符合筛选条件的会话",
                            "调整日期或应用筛选后再试。",
                            Some(
                                components::button(
                                    "sessions-empty-clear-filters",
                                    "清除筛选",
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.clear_filters(cx);
                                }))
                                .into_any_element(),
                            ),
                        ))
                    })
                    .children(cards),
            ))
            .when(show_pagination, |s| s.child(self.render_pagination(cx)))
            .when_some(confirm, |root, (idx, title)| {
                let message =
                    SharedString::from(format!("确定删除会话「{title}」吗？此操作不可撤销。"));
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除会话"))
                        .child(
                            components::modal_body()
                                .child(div().text_color(theme::subtext()).text_sm().child(message)),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "session-confirm-delete-cancel",
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
                                "session-confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(idx, cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }
}

crate::notifications::impl_status_toasts!(SessionsView);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_date_filter_uses_local_session_day() {
        let timestamp = Local
            .with_ymd_and_hms(2026, 7, 22, 23, 45, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis();

        assert!(
            SessionDateFilter::Exact(NaiveDate::from_ymd_opt(2026, 7, 22).unwrap())
                .matches(Some(timestamp))
        );
        assert!(
            !SessionDateFilter::Exact(NaiveDate::from_ymd_opt(2026, 7, 21).unwrap())
                .matches(Some(timestamp))
        );
        assert!(SessionDateFilter::All.matches(None));
        assert!(!SessionDateFilter::Today.matches(None));
    }

    #[test]
    fn long_messages_are_collapsed_on_unicode_boundaries() {
        let content = "你".repeat(MESSAGE_PREVIEW_CHARS + 20);
        let (preview, is_long) = SessionsView::message_content(&content, false);
        assert!(is_long);
        assert!(preview.ends_with('…'));
        assert!(preview.len() < content.len());

        let (expanded, is_long) = SessionsView::message_content(&content, true);
        assert!(is_long);
        assert_eq!(expanded.as_ref(), content);
    }

    #[test]
    fn short_messages_are_not_marked_as_collapsed() {
        let (content, is_long) = SessionsView::message_content("hello", false);
        assert!(!is_long);
        assert_eq!(content.as_ref(), "hello");
    }
}
