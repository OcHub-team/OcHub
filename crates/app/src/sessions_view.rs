//! Sessions panel. Lists recent CLI sessions discovered on disk via
//! `session_manager::scan_sessions()` and supports deleting one. Scanning and
//! transcript loading are filesystem-heavy, so both run on the background
//! executor; scan results are cached for [`SCAN_TTL`] so re-entering the
//! section doesn't rescan every time.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{
    Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike,
};
use gpui::{
    anchored, deferred, div, point, prelude::*, px, Anchor, AnyElement, Context, ElementId, Entity,
    FontWeight, ListAlignment, ListState, MouseButton, ScrollHandle, SharedString, Window,
};
use ochub_core::session_manager::{self, SessionMessage, SessionMeta};
use ochub_core::AppState;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::{icon, IconName};
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

const DEFAULT_PAGE_SIZE: usize = 20;
const PAGE_SIZE_OPTIONS: &[u32] = &[20, 50, 100];

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
    Custom { start_ms: i64, end_ms: i64 },
}

impl SessionDateFilter {
    fn label(self) -> String {
        match self {
            Self::All => "全部时间".to_string(),
            Self::Today => "今天".to_string(),
            Self::SevenDays => "最近 7 天".to_string(),
            Self::ThirtyDays => "最近 30 天".to_string(),
            Self::Custom { start_ms, end_ms } => {
                let start = Local
                    .timestamp_millis_opt(start_ms)
                    .single()
                    .map(|value| value.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "自定义".to_string());
                let end = Local
                    .timestamp_millis_opt(end_ms)
                    .single()
                    .map(|value| value.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                format!("{start} – {end}")
            }
        }
    }

    fn matches(self, timestamp_ms: Option<i64>) -> bool {
        if self == Self::All {
            return true;
        }
        let Some(timestamp_ms) = timestamp_ms else {
            return false;
        };
        let Some(active_time) = Local.timestamp_millis_opt(timestamp_ms).single() else {
            return false;
        };
        let active_date = active_time.date_naive();
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
            Self::Custom { start_ms, end_ms } => timestamp_ms >= start_ms && timestamp_ms <= end_ms,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionFilterPopover {
    Date,
    App,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionRangeEndpoint {
    Start,
    End,
}

struct PreparedSessionMessage {
    role: String,
    content: SharedString,
    preview: SharedString,
    is_long: bool,
    ts: Option<i64>,
}

#[derive(Default)]
struct SessionStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_messages: usize,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

/// An opened session: its metadata plus the loaded conversation transcript.
struct SessionDetail {
    meta: SessionMeta,
    messages: Vec<PreparedSessionMessage>,
    stats: SessionStats,
    error: Option<SharedString>,
}

pub struct SessionsView {
    #[allow(dead_code)]
    app: Arc<AppState>,
    sessions: Vec<SessionMeta>,
    status: Option<SharedString>,
    /// Zero-based current page into `sessions`.
    page: usize,
    page_size: usize,
    page_size_open: bool,
    filtered_indices: Vec<usize>,
    visible_session_indices: Vec<usize>,
    app_options: Vec<String>,
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
    active_datetime_picker: Option<SessionRangeEndpoint>,
    picker_year: i32,
    picker_month: u32,
    picker_hour_scroll: ScrollHandle,
    picker_minute_scroll: ScrollHandle,
    app_filter_scroll: ScrollHandle,
    empty_scroll: ScrollHandle,
    session_list_state: ListState,
    /// Drives the transcript's variable-height virtual list.
    transcript_list_state: ListState,
    /// Message indexes explicitly expanded by the user.
    expanded_messages: HashSet<usize>,
    page_input: Entity<TextInput>,
    range_start_input: Entity<TextInput>,
    range_end_input: Entity<TextInput>,
}

impl SessionsView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let now = Local::now();
        let this = Self {
            app,
            sessions: Vec::new(),
            status: None,
            page: 0,
            page_size: DEFAULT_PAGE_SIZE,
            page_size_open: false,
            filtered_indices: Vec::new(),
            visible_session_indices: Vec::new(),
            app_options: Vec::new(),
            detail: None,
            confirm_delete: None,
            scanning: false,
            loading_detail: None,
            last_scan: None,
            date_filter: SessionDateFilter::All,
            app_filter: None,
            open_filter_popover: None,
            date_filter_error: None,
            active_datetime_picker: None,
            picker_year: now.year(),
            picker_month: now.month(),
            picker_hour_scroll: ScrollHandle::new(),
            picker_minute_scroll: ScrollHandle::new(),
            app_filter_scroll: ScrollHandle::new(),
            empty_scroll: ScrollHandle::new(),
            session_list_state: ListState::new(0, ListAlignment::Top, px(96.)),
            transcript_list_state: ListState::new(0, ListAlignment::Top, px(320.)),
            expanded_messages: HashSet::new(),
            page_input: cx.new(|cx| text_input(cx, "页码").compact()),
            range_start_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss")),
            range_end_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss")),
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
        let apply_start = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.apply_custom_range(cx);
        });
        this.range_start_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| apply_start(&(), window, cx));
        });
        let apply_end = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            this.apply_custom_range(cx);
        });
        this.range_end_input.update(cx, |input, _| {
            input.set_on_enter(move |window, cx| apply_end(&(), window, cx));
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
                this.rebuild_session_index();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn total_pages(&self) -> usize {
        self.filtered_indices.len().div_ceil(self.page_size).max(1)
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
        self.filtered_indices.len()
    }

    fn rebuild_session_index(&mut self) {
        self.app_options = self
            .sessions
            .iter()
            .map(|session| session.provider_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| self.session_matches_filters(session).then_some(index))
            .collect();
        let max_page = self.total_pages().saturating_sub(1);
        self.page = self.page.min(max_page);
        self.rebuild_visible_sessions();
    }

    fn rebuild_visible_sessions(&mut self) {
        let start = self.page.saturating_mul(self.page_size);
        let end = (start + self.page_size).min(self.filtered_indices.len());
        self.visible_session_indices = if start < end {
            self.filtered_indices[start..end].to_vec()
        } else {
            Vec::new()
        };
        self.session_list_state
            .reset(self.visible_session_indices.len());
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let max_page = self.total_pages().saturating_sub(1);
        let page = page.min(max_page);
        if page != self.page {
            self.page = page;
            self.page_size_open = false;
            self.rebuild_visible_sessions();
            cx.notify();
        }
    }

    fn toggle_page_size(&mut self, cx: &mut Context<Self>) {
        self.page_size_open = !self.page_size_open;
        cx.notify();
    }

    fn set_page_size(&mut self, page_size: usize, cx: &mut Context<Self>) {
        if self.page_size != page_size {
            self.page_size = page_size;
            self.page = 0;
            self.rebuild_visible_sessions();
        }
        self.page_size_open = false;
        cx.notify();
    }

    fn set_date_filter(&mut self, filter: SessionDateFilter, cx: &mut Context<Self>) {
        self.date_filter = filter;
        if !matches!(filter, SessionDateFilter::Custom { .. }) {
            self.range_start_input
                .update(cx, |input, cx| input.set_content("", cx));
            self.range_end_input
                .update(cx, |input, cx| input.set_content("", cx));
        }
        self.page = 0;
        self.open_filter_popover = None;
        self.active_datetime_picker = None;
        self.date_filter_error = None;
        self.rebuild_session_index();
        cx.notify();
    }

    fn set_app_filter(&mut self, app: Option<String>, cx: &mut Context<Self>) {
        self.app_filter = app;
        self.page = 0;
        self.open_filter_popover = None;
        self.rebuild_session_index();
        cx.notify();
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.date_filter = SessionDateFilter::All;
        self.app_filter = None;
        self.page = 0;
        self.open_filter_popover = None;
        self.active_datetime_picker = None;
        self.date_filter_error = None;
        self.range_start_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.range_end_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.rebuild_session_index();
        cx.notify();
    }

    fn toggle_filter_popover(&mut self, popover: SessionFilterPopover, cx: &mut Context<Self>) {
        self.open_filter_popover = if self.open_filter_popover == Some(popover) {
            None
        } else {
            Some(popover)
        };
        if popover != SessionFilterPopover::Date {
            self.active_datetime_picker = None;
        }
        self.date_filter_error = None;
        cx.notify();
    }

    fn endpoint_input(&self, endpoint: SessionRangeEndpoint) -> &Entity<TextInput> {
        match endpoint {
            SessionRangeEndpoint::Start => &self.range_start_input,
            SessionRangeEndpoint::End => &self.range_end_input,
        }
    }

    fn endpoint_datetime(
        &self,
        endpoint: SessionRangeEndpoint,
        cx: &mut Context<Self>,
    ) -> chrono::DateTime<Local> {
        let value = self
            .endpoint_input(endpoint)
            .read(cx)
            .content()
            .trim()
            .to_string();
        if let Some(value) = parse_local_datetime(&value, endpoint == SessionRangeEndpoint::End) {
            return value;
        }
        if let SessionDateFilter::Custom { start_ms, end_ms } = self.date_filter {
            let timestamp = match endpoint {
                SessionRangeEndpoint::Start => start_ms,
                SessionRangeEndpoint::End => end_ms,
            };
            if let Some(value) = Local.timestamp_millis_opt(timestamp).single() {
                return value;
            }
        }
        Local::now()
    }

    fn toggle_datetime_picker(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        if self.active_datetime_picker == Some(endpoint) {
            self.active_datetime_picker = None;
        } else {
            let selected = self.endpoint_datetime(endpoint, cx);
            self.picker_year = selected.year();
            self.picker_month = selected.month();
            self.picker_hour_scroll
                .scroll_to_top_of_item(selected.hour() as usize);
            self.picker_minute_scroll
                .scroll_to_top_of_item(selected.minute() as usize);
            self.active_datetime_picker = Some(endpoint);
        }
        cx.notify();
    }

    fn update_datetime_endpoint(
        &mut self,
        endpoint: SessionRangeEndpoint,
        date: NaiveDate,
        hour: u32,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let second = if endpoint == SessionRangeEndpoint::End {
            59
        } else {
            0
        };
        let Some(value) = Local
            .with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, second)
            .earliest()
        else {
            return;
        };
        let input = self.endpoint_input(endpoint).clone();
        input.update(cx, |input, cx| {
            input.set_content(value.format("%Y/%m/%d %H:%M:%S").to_string(), cx)
        });
        self.date_filter_error = None;
        cx.notify();
    }

    fn select_picker_date(
        &mut self,
        endpoint: SessionRangeEndpoint,
        date: NaiveDate,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.picker_year = date.year();
        self.picker_month = date.month();
        self.update_datetime_endpoint(endpoint, date, current.hour(), current.minute(), cx);
    }

    fn select_picker_hour(
        &mut self,
        endpoint: SessionRangeEndpoint,
        hour: u32,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), hour, current.minute(), cx);
        self.picker_hour_scroll.scroll_to_top_of_item(hour as usize);
    }

    fn select_picker_minute(
        &mut self,
        endpoint: SessionRangeEndpoint,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), current.hour(), minute, cx);
        self.picker_minute_scroll
            .scroll_to_top_of_item(minute as usize);
    }

    fn select_picker_today(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        let current = self.endpoint_datetime(endpoint, cx);
        let today = Local::now().date_naive();
        self.picker_year = today.year();
        self.picker_month = today.month();
        self.update_datetime_endpoint(endpoint, today, current.hour(), current.minute(), cx);
    }

    fn clear_picker_value(&mut self, endpoint: SessionRangeEndpoint, cx: &mut Context<Self>) {
        let input = self.endpoint_input(endpoint).clone();
        input.update(cx, |input, cx| input.set_content("", cx));
        self.active_datetime_picker = None;
        cx.notify();
    }

    fn shift_picker_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        let month_index = self.picker_year * 12 + self.picker_month as i32 - 1 + delta;
        self.picker_year = month_index.div_euclid(12);
        self.picker_month = month_index.rem_euclid(12) as u32 + 1;
        cx.notify();
    }

    fn apply_custom_range(&mut self, cx: &mut Context<Self>) {
        let start_text = self.range_start_input.read(cx).content().trim().to_string();
        let end_text = self.range_end_input.read(cx).content().trim().to_string();
        let Some(start) = parse_local_datetime(&start_text, false) else {
            self.date_filter_error = Some(SharedString::from("请选择或输入有效的开始时间"));
            cx.notify();
            return;
        };
        let Some(end) = parse_local_datetime(&end_text, true) else {
            self.date_filter_error = Some(SharedString::from("请选择或输入有效的结束时间"));
            cx.notify();
            return;
        };
        if start > end {
            self.date_filter_error = Some(SharedString::from("开始时间不能晚于结束时间"));
            cx.notify();
            return;
        }
        self.set_date_filter(
            SessionDateFilter::Custom {
                start_ms: start.timestamp_millis(),
                end_ms: end.timestamp_millis(),
            },
            cx,
        );
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
                self.rebuild_session_index();
            }
            Ok(false) => {
                self.status = Some(SharedString::from("未找到会话"));
                self.force_reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        cx.notify();
    }

    fn prepare_messages(
        messages: Vec<SessionMessage>,
    ) -> (Vec<PreparedSessionMessage>, SessionStats) {
        let mut stats = SessionStats::default();
        let messages = messages
            .into_iter()
            .map(|message| {
                match message.role.as_str() {
                    "user" => stats.user_messages += 1,
                    "assistant" => stats.assistant_messages += 1,
                    "tool" | "system" => stats.tool_messages += 1,
                    _ => {}
                }
                if let Some(timestamp) = message.ts {
                    stats.first_ts = Some(
                        stats
                            .first_ts
                            .map_or(timestamp, |current| current.min(timestamp)),
                    );
                    stats.last_ts = Some(
                        stats
                            .last_ts
                            .map_or(timestamp, |current| current.max(timestamp)),
                    );
                }
                let (preview, is_long) = Self::message_content(&message.content, false);
                let content = if message.content.trim().is_empty() {
                    SharedString::from("（空消息）")
                } else {
                    SharedString::from(message.content)
                };
                PreparedSessionMessage {
                    role: message.role,
                    content,
                    preview,
                    is_long,
                    ts: message.ts,
                }
            })
            .collect();
        (messages, stats)
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
                        .map(Self::prepare_messages)
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading_detail = None;
                let detail = match loaded {
                    Ok((messages, stats)) => SessionDetail {
                        meta: session,
                        messages,
                        stats,
                        error: None,
                    },
                    Err(err) => SessionDetail {
                        meta: session,
                        messages: Vec::new(),
                        stats: SessionStats::default(),
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
        self.transcript_list_state
            .remeasure_items(index..index.saturating_add(1));
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
        let content = if expanded || !message.is_long {
            message.content.clone()
        } else {
            message.preview.clone()
        };
        let timestamp = message.ts.and_then(|timestamp| {
            Local
                .timestamp_millis_opt(timestamp)
                .single()
                .map(|value| SharedString::from(value.format("%H:%M:%S").to_string()))
        });
        let is_trace = matches!(message.role.as_str(), "tool" | "system");

        div()
            .w_full()
            .pb_2()
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(if is_trace {
                        theme::border()
                    } else {
                        accent.alpha(0.36)
                    })
                    .bg(if is_trace {
                        theme::inset()
                    } else if message.role == "user" {
                        soft.alpha(0.42)
                    } else {
                        theme::surface()
                    })
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
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(format!("#{}", index + 1))),
                            )
                            .child(div().flex_1())
                            .when_some(timestamp, |row, timestamp| {
                                row.child(
                                    div().text_xs().text_color(theme::muted()).child(timestamp),
                                )
                            }),
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
                    .when(message.is_long, |card| {
                        card.child(
                            div().flex().flex_row().child(
                                components::button(
                                    SharedString::from(format!("session-message-toggle-{index}")),
                                    if expanded { "收起" } else { "展开详情" },
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

    fn detail_metric(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .min_w(px(92.))
            .child(div().text_xs().text_color(theme::muted()).child(label))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(value.into()),
            )
    }

    fn duration_label(stats: &SessionStats) -> Option<SharedString> {
        let duration_ms = stats.last_ts?.saturating_sub(stats.first_ts?);
        let seconds = duration_ms / 1_000;
        Some(SharedString::from(if seconds < 60 {
            format!("{seconds} 秒")
        } else if seconds < 3_600 {
            format!("{} 分 {} 秒", seconds / 60, seconds % 60)
        } else {
            format!("{} 小时 {} 分", seconds / 3_600, seconds % 3_600 / 60)
        }))
    }

    fn render_detail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = self.detail.as_ref() else {
            return layout::page().into_any_element();
        };
        let title = Self::title_for(&detail.meta);
        let provider = Self::app_label(&detail.meta.provider_id);
        let count = detail.messages.len();
        let error = detail.error.clone();
        let user_messages = detail.stats.user_messages;
        let assistant_messages = detail.stats.assistant_messages;
        let tool_messages = detail.stats.tool_messages;
        let duration = Self::duration_label(&detail.stats);
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
                &self.empty_scroll,
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
                &self.empty_scroll,
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
            layout::virtual_body("session-transcript-body", list, &self.transcript_list_state)
                .into_any_element()
        };

        let metrics = components::card().p_3().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_5()
                .child(Self::detail_metric("消息", count.to_string()))
                .child(Self::detail_metric("用户", user_messages.to_string()))
                .child(Self::detail_metric("助手", assistant_messages.to_string()))
                .child(Self::detail_metric(
                    "工具 / 系统",
                    tool_messages.to_string(),
                ))
                .when_some(duration, |row, duration| {
                    row.child(Self::detail_metric("会话跨度", duration))
                }),
        );

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
            .child(
                div()
                    .px_6()
                    .pt_4()
                    .child(layout::content_column().child(metrics)),
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

    fn render_session_list_item(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(session_index) = self.visible_session_indices.get(index).copied() else {
            return div().into_any_element();
        };
        let Some(session) = self.sessions.get(session_index) else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .pb_3()
            .child(self.render_card(session_index, session, cx))
            .into_any_element()
    }

    fn render_datetime_picker(
        &self,
        endpoint: SessionRangeEndpoint,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.endpoint_datetime(endpoint, cx);
        let picker_id = match endpoint {
            SessionRangeEndpoint::Start => "sessions-start-datetime",
            SessionRangeEndpoint::End => "sessions-end-datetime",
        };
        let shift_month = cx.listener(|this, delta: &i32, _window, cx| {
            this.shift_picker_month(*delta, cx);
        });
        let select_date = cx.listener(move |this, date: &NaiveDate, _window, cx| {
            this.select_picker_date(endpoint, *date, cx);
        });
        let select_hour = cx.listener(move |this, hour: &u32, _window, cx| {
            this.select_picker_hour(endpoint, *hour, cx);
        });
        let select_minute = cx.listener(move |this, minute: &u32, _window, cx| {
            this.select_picker_minute(endpoint, *minute, cx);
        });
        let select_today = cx.listener(move |this, _event: &(), _window, cx| {
            this.select_picker_today(endpoint, cx);
        });
        let clear = cx.listener(move |this, _event: &(), _window, cx| {
            this.clear_picker_value(endpoint, cx);
        });

        components::datetime_picker(
            picker_id,
            selected,
            self.picker_year,
            self.picker_month,
            &self.picker_hour_scroll,
            &self.picker_minute_scroll,
            move |delta, window, cx| shift_month(&delta, window, cx),
            move |date, window, cx| select_date(&date, window, cx),
            move |hour, window, cx| select_hour(&hour, window, cx),
            move |minute, window, cx| select_minute(&minute, window, cx),
            move |window, cx| select_today(&(), window, cx),
            move |window, cx| clear(&(), window, cx),
        )
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let date_open = self.open_filter_popover == Some(SessionFilterPopover::Date);
        let start_picker_open = self.active_datetime_picker == Some(SessionRangeEndpoint::Start);
        let start_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "sessions-start-datetime-field",
                    "开始时间",
                    self.range_start_input.clone(),
                    start_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_datetime_picker(SessionRangeEndpoint::Start, cx);
                    }),
                ),
            )
            .when(start_picker_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(SessionRangeEndpoint::Start, cx)),
                    )
                    .priority(20),
                )
            });
        let end_picker_open = self.active_datetime_picker == Some(SessionRangeEndpoint::End);
        let end_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "sessions-end-datetime-field",
                    "结束时间",
                    self.range_end_input.clone(),
                    end_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_datetime_picker(SessionRangeEndpoint::End, cx);
                    }),
                ),
            )
            .when(end_picker_open, |control| {
                control.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(SessionRangeEndpoint::End, cx)),
                    )
                    .priority(20),
                )
            });
        let mut date_popover = session_filter_popover("sessions-date-popover", 380.)
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
                    .gap_3()
                    .p_3()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::muted())
                            .child("自定义范围"),
                    )
                    .child(start_control)
                    .child(end_control)
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
                                this.apply_custom_range(cx);
                            },
                        )),
                    ),
            );
        date_popover = date_popover.when(self.active_datetime_picker.is_none(), |popover| {
            popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(SessionFilterPopover::Date) {
                    this.open_filter_popover = None;
                    this.date_filter_error = None;
                    cx.notify();
                }
            }))
        });
        let date_control = div()
            .relative()
            .flex_none()
            .child(
                session_filter_trigger(
                    "sessions-date-filter",
                    self.date_filter.label(),
                    IconName::Calendar,
                    date_open,
                    220.,
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
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(date_popover),
                    )
                    .priority(10),
                )
            });

        let app_open = self.open_filter_popover == Some(SessionFilterPopover::App);
        let mut app_options = div()
            .id("sessions-app-options")
            .max_h(px(280.))
            .overflow_y_scroll()
            .track_scroll(&self.app_filter_scroll)
            .p_1()
            .child(
                session_dropdown_option("sessions-app-all", "全部应用", self.app_filter.is_none())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.set_app_filter(None, cx);
                    })),
            );
        for (index, app) in self.app_options.iter().cloned().enumerate() {
            let selected = self.app_filter.as_deref() == Some(app.as_str());
            let app_for_click = app.clone();
            app_options = app_options.child(
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
        let app_popover = session_filter_popover("sessions-app-popover", 220.)
            .relative()
            .p_0()
            .child(app_options)
            .child(crate::scrollbar::VerticalScrollbar::new(
                "sessions-app-options-scrollbar",
                self.app_filter_scroll.clone(),
            ))
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
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
                            .offset(point(px(0.), px(4.)))
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
        let toggle_page_size = cx.listener(|this, _event: &(), _window, cx| {
            this.toggle_page_size(cx);
        });
        let set_page_size = cx.listener(|this, page_size: &u32, _window, cx| {
            this.set_page_size(*page_size as usize, cx);
        });
        div().px_6().child(components::pagination_bar(
            "sessions-pages",
            self.page as u32,
            self.total_pages() as u32,
            Some(self.filtered_session_count() as u64),
            self.page_size as u32,
            PAGE_SIZE_OPTIONS,
            self.page_size_open,
            &self.page_input,
            move |page, window, cx| go(&page, window, cx),
            move |window, cx| toggle_page_size(&(), window, cx),
            move |page_size, window, cx| set_page_size(&page_size, window, cx),
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

fn parse_local_datetime(value: &str, end_of_day: bool) -> Option<chrono::DateTime<Local>> {
    let normalized = value.trim().replace('/', "-");
    for pattern in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(&normalized, pattern) {
            return Local.from_local_datetime(&value).earliest();
        }
    }

    let date = components::parse_jump_date(&normalized)?;
    let value = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Local.from_local_datetime(&value).earliest()
}

impl Render for SessionsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.detail.is_some() {
            return self.render_detail(cx);
        }
        let total = self.filtered_session_count();
        let has_no_sessions = self.sessions.is_empty();
        let has_no_matches = !has_no_sessions && total == 0;
        let scanning = self.scanning;
        let show_pagination = total > 0;
        let confirm = self.confirm_delete.and_then(|idx| {
            self.sessions
                .get(idx)
                .map(Self::title_for)
                .map(|t| (idx, t))
        });
        let body = if has_no_sessions {
            layout::scroll_body(
                "session-empty-body",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
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
                )),
            )
            .into_any_element()
        } else if has_no_matches {
            layout::scroll_body(
                "session-no-matches-body",
                &self.empty_scroll,
                layout::content_column().child(components::empty_state(
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
                )),
            )
            .into_any_element()
        } else {
            let list = gpui::list(
                self.session_list_state.clone(),
                cx.processor(|this, index, _window, cx| this.render_session_list_item(index, cx)),
            );
            layout::virtual_body("session-list-body", list, &self.session_list_state)
                .into_any_element()
        };

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
            .child(
                div()
                    .px_6()
                    .pt_6()
                    .child(layout::content_column().child(self.render_filters(cx))),
            )
            .child(body)
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
    fn custom_date_filter_uses_inclusive_local_range() {
        let start = Local
            .with_ymd_and_hms(2026, 7, 22, 9, 0, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis();
        let end = Local
            .with_ymd_and_hms(2026, 7, 22, 17, 0, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis();
        let filter = SessionDateFilter::Custom {
            start_ms: start,
            end_ms: end,
        };

        assert!(filter.matches(Some(start)));
        assert!(filter.matches(Some(end)));
        assert!(!filter.matches(Some(start - 1)));
        assert!(!filter.matches(Some(end + 1)));
        assert!(SessionDateFilter::All.matches(None));
        assert!(!SessionDateFilter::Today.matches(None));
    }

    #[test]
    fn local_datetime_parser_supports_dates_and_minutes() {
        let start = parse_local_datetime("2026-07-22", false).expect("valid start");
        let end = parse_local_datetime("2026/07/22", true).expect("valid end");
        let minute = parse_local_datetime("2026-07-22 12:34", false).expect("valid minute");

        assert_eq!((start.hour(), start.minute(), start.second()), (0, 0, 0));
        assert_eq!((end.hour(), end.minute(), end.second()), (23, 59, 59));
        assert_eq!((minute.hour(), minute.minute()), (12, 34));
        assert!(parse_local_datetime("2026-02-30", false).is_none());
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
