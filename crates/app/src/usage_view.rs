//! Usage statistics workbench. Mirrors the reference cc-switch dashboard while
//! staying native GPUI: scoped filters, trends, provider/model tables, request
//! detail, pricing configuration, and stream-check parameters.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use gpui::{
    anchored, deferred, div, ease_out_quint, point, prelude::*, px, relative, Anchor, Animation,
    AnimationExt, Context, ElementId, Entity, FontWeight, ListAlignment, ListState, MouseButton,
    ScrollHandle, SharedString, Window,
};
use ochub_core::db::StreamCheckConfig;
use ochub_core::services::session_usage::{
    get_data_source_breakdown, sync_claude_session_logs, DataSourceSummary, SessionSyncResult,
};
use ochub_core::services::usage_stats::{
    DailyStats, LogFilters, ModelPricingInfo, ModelStats, ProviderStats, RequestLogDetail,
    UsageSummaryByApp,
};
use ochub_core::{services, AppState, UsageSummary};

use crate::components::{self, format_local_timestamp, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::{icon, IconName};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

const DEFAULT_LOG_PAGE_SIZE: u32 = 20;
const LOG_PAGE_SIZE_OPTIONS: &[u32] = &[20, 50, 100];
const PRICING_APPS: [&str; 2] = ["claude", "codex"];
const DATETIME_PICKER_GAP: f32 = 4.;

/// Number of top-level blocks rendered by [`UsageView::render_block`] into the
/// virtualized list (filters, data sources, summary, trend, scope, tabs,
/// active section, pricing, stream config).
const USAGE_BLOCK_COUNT: usize = 9;

/// Everything [`UsageView::reload`] fetches, loaded in one background pass so
/// the UI thread never blocks on the SQLite connection while the gateway records usage.
struct UsageData {
    summary: Option<UsageSummary>,
    summary_by_app: Vec<UsageSummaryByApp>,
    daily: Vec<DailyStats>,
    providers: Vec<ProviderStats>,
    provider_options: Vec<String>,
    models: Vec<ModelStats>,
    model_options: Vec<String>,
    logs: Vec<RequestLogDetail>,
    log_total: u32,
    data_sources: Vec<DataSourceSummary>,
    pricing: Vec<ModelPricingInfo>,
    stream_config: StreamCheckConfig,
    errors: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn load_usage_data(
    app: &AppState,
    start: Option<i64>,
    end: Option<i64>,
    app_filter: Option<&str>,
    provider_filter: Option<&str>,
    model_filter: Option<&str>,
    status_filter: Option<u16>,
    log_page: u32,
    log_page_size: u32,
) -> UsageData {
    let mut errors = Vec::new();

    let summary =
        match app
            .db
            .get_usage_summary(start, end, app_filter, provider_filter, model_filter)
        {
            Ok(summary) => Some(summary),
            Err(err) => {
                errors.push(tf!(k::USAGE_STATUS_LOAD_FAILED, error = err));
                None
            }
        };

    let filters = LogFilters {
        app_type: app_filter.map(str::to_string),
        provider_name: provider_filter.map(str::to_string),
        model: model_filter.map(str::to_string),
        status_code: status_filter,
        start_date: start,
        end_date: end,
    };
    let (logs, log_total) = match app.db.get_request_logs(&filters, log_page, log_page_size) {
        Ok(page) => (page.data, page.total),
        Err(err) => {
            errors.push(tf!(k::USAGE_STATUS_LOGS_LOAD_FAILED, error = err));
            (Vec::new(), 0)
        }
    };

    let providers = app
        .db
        .get_provider_stats(start, end, app_filter, provider_filter, model_filter)
        .unwrap_or_default();
    let mut provider_options = if provider_filter.is_none() && model_filter.is_none() {
        providers
            .iter()
            .map(|stats| stats.provider_name.clone())
            .collect::<Vec<_>>()
    } else {
        app.db
            .get_provider_stats(start, end, app_filter, None, None)
            .unwrap_or_default()
            .into_iter()
            .map(|stats| stats.provider_name)
            .collect::<Vec<_>>()
    };
    if let Some(selected) = provider_filter {
        if !provider_options.iter().any(|provider| provider == selected) {
            provider_options.push(selected.to_string());
        }
    }
    provider_options.sort_by_key(|provider| provider.to_lowercase());
    provider_options.dedup();

    let models = app
        .db
        .get_model_stats(start, end, app_filter, provider_filter, model_filter)
        .unwrap_or_default();
    let mut model_options = if model_filter.is_none() {
        models
            .iter()
            .map(|stats| stats.model.clone())
            .collect::<Vec<_>>()
    } else {
        app.db
            .get_model_stats(start, end, app_filter, provider_filter, None)
            .unwrap_or_default()
            .into_iter()
            .map(|stats| stats.model)
            .collect::<Vec<_>>()
    };
    if let Some(selected) = model_filter {
        if !model_options.iter().any(|model| model == selected) {
            model_options.push(selected.to_string());
        }
    }
    model_options.sort_by_key(|model| model.to_lowercase());
    model_options.dedup();

    UsageData {
        summary,
        summary_by_app: app
            .db
            .get_usage_summary_by_app(start, end, provider_filter, model_filter)
            .unwrap_or_default(),
        daily: app
            .db
            .get_daily_trends(start, end, app_filter, provider_filter, model_filter)
            .unwrap_or_default(),
        providers,
        provider_options,
        models,
        model_options,
        logs,
        log_total,
        data_sources: get_data_source_breakdown(&app.db).unwrap_or_default(),
        pricing: app.db.get_model_pricing().unwrap_or_default(),
        stream_config: app.db.get_stream_check_config().unwrap_or_default(),
        errors,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageRange {
    Today,
    SevenDays,
    ThirtyDays,
    /// 自定义时间范围（本地时区，精确到秒）。
    Custom {
        start: i64,
        end: i64,
    },
}

impl UsageRange {
    /// The quick picks offered by the time popover, in display order. `Custom`
    /// is not one of them: it only exists once a range has been typed in.
    fn all() -> &'static [Self] {
        &[Self::Today, Self::SevenDays, Self::ThirtyDays]
    }

    fn label(self) -> &'static str {
        raw(match self {
            Self::Today => k::USAGE_RANGE_TODAY,
            Self::SevenDays => k::USAGE_RANGE_SEVEN_DAYS,
            Self::ThirtyDays => k::USAGE_RANGE_THIRTY_DAYS,
            Self::Custom { .. } => k::USAGE_RANGE_CUSTOM,
        })
    }

    fn bounds(self) -> (Option<i64>, Option<i64>) {
        let now = Local::now();
        let end = now.timestamp();
        let start = match self {
            Self::Today => Local
                .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
                .single()
                .map(|date| date.timestamp())
                .unwrap_or(end - 24 * 60 * 60),
            Self::SevenDays => (now - Duration::days(7)).timestamp(),
            Self::ThirtyDays => (now - Duration::days(30)).timestamp(),
            Self::Custom { start, end } => return (Some(start), Some(end)),
        };
        (Some(start), Some(end))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageSection {
    Logs,
    Providers,
    Models,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterPopover {
    Time,
    Provider,
    Model,
    Status,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RangeEndpoint {
    Start,
    End,
}

impl UsageSection {
    fn all() -> &'static [(Self, IconName)] {
        &[
            (Self::Logs, IconName::Message),
            (Self::Providers, IconName::Cloud),
            (Self::Models, IconName::Chart),
        ]
    }

    fn label(self) -> &'static str {
        raw(match self {
            Self::Logs => k::USAGE_SECTION_LOGS,
            Self::Providers => k::USAGE_SECTION_PROVIDERS,
            Self::Models => k::USAGE_SECTION_MODELS,
        })
    }
}

pub struct UsageView {
    app: Arc<AppState>,
    summary: Option<UsageSummary>,
    summary_by_app: Vec<UsageSummaryByApp>,
    daily: Vec<DailyStats>,
    providers: Vec<ProviderStats>,
    provider_options: Vec<String>,
    models: Vec<ModelStats>,
    model_options: Vec<String>,
    logs: Vec<RequestLogDetail>,
    log_total: u32,
    data_sources: Vec<DataSourceSummary>,
    pricing: Vec<ModelPricingInfo>,
    stream_config: StreamCheckConfig,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    range: UsageRange,
    app_filter: Option<String>,
    provider_filter: Option<String>,
    model_filter: Option<String>,
    status_filter: Option<u16>,
    section: UsageSection,
    log_page: u32,
    log_page_size: u32,
    selected_log: Option<RequestLogDetail>,
    show_trend: bool,
    show_scope_options: bool,
    show_pricing: bool,
    show_stream_config: bool,
    open_filter_popover: Option<FilterPopover>,
    log_page_size_open: bool,
    active_datetime_picker: Option<RangeEndpoint>,
    picker_year: i32,
    picker_month: u32,
    picker_hour_scroll: ScrollHandle,
    picker_minute_scroll: ScrollHandle,
    provider_filter_scroll: ScrollHandle,
    model_filter_scroll: ScrollHandle,
    /// 待确认删除的定价模型 ID；`Some` 时展示确认模态。
    confirm_delete_pricing: Option<String>,
    pricing_sources: BTreeMap<String, String>,
    pricing_model_id: Entity<TextInput>,
    pricing_display_name: Entity<TextInput>,
    pricing_input_cost: Entity<TextInput>,
    pricing_output_cost: Entity<TextInput>,
    pricing_cache_read_cost: Entity<TextInput>,
    pricing_cache_creation_cost: Entity<TextInput>,
    multiplier_claude: Entity<TextInput>,
    multiplier_codex: Entity<TextInput>,
    stream_timeout_secs: Entity<TextInput>,
    stream_max_retries: Entity<TextInput>,
    stream_degraded_threshold_ms: Entity<TextInput>,
    log_page_input: Entity<TextInput>,
    range_start_input: Entity<TextInput>,
    range_end_input: Entity<TextInput>,
    /// Whether the current `status` message came from a failed data load, so a
    /// later successful reload knows it may clear it (and only it).
    load_error: bool,
    /// Drives the virtualized page body (one item per top-level block).
    list_state: ListState,
}

impl UsageView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // The placeholder is captured when the input is constructed, and this
        // view is built once at startup, so it needs pushing in by hand. The
        // other inputs hold examples and numbers, which do not translate.
        self.log_page_input.update(cx, |input, cx| {
            input.set_placeholder(t(k::USAGE_PAGINATION_PAGE_PLACEHOLDER), cx)
        });
        self.list_state.remeasure();
        cx.notify();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let now = Local::now();
        let mut this = Self {
            app,
            summary: None,
            summary_by_app: Vec::new(),
            daily: Vec::new(),
            providers: Vec::new(),
            provider_options: Vec::new(),
            models: Vec::new(),
            model_options: Vec::new(),
            logs: Vec::new(),
            log_total: 0,
            data_sources: Vec::new(),
            pricing: Vec::new(),
            stream_config: StreamCheckConfig::default(),
            status: None,
            status_level: None,
            range: UsageRange::Today,
            app_filter: None,
            provider_filter: None,
            model_filter: None,
            status_filter: None,
            section: UsageSection::Logs,
            log_page: 0,
            log_page_size: DEFAULT_LOG_PAGE_SIZE,
            selected_log: None,
            show_trend: true,
            show_scope_options: false,
            show_pricing: false,
            show_stream_config: false,
            open_filter_popover: None,
            log_page_size_open: false,
            active_datetime_picker: None,
            picker_year: now.year(),
            picker_month: now.month(),
            picker_hour_scroll: ScrollHandle::new(),
            picker_minute_scroll: ScrollHandle::new(),
            provider_filter_scroll: ScrollHandle::new(),
            model_filter_scroll: ScrollHandle::new(),
            confirm_delete_pricing: None,
            pricing_sources: PRICING_APPS
                .iter()
                .map(|app| ((*app).to_string(), "response".to_string()))
                .collect(),
            pricing_model_id: cx.new(|cx| text_input(cx, "claude-3-5-sonnet", "")),
            pricing_display_name: cx.new(|cx| text_input(cx, "Claude 3.5 Sonnet", "")),
            pricing_input_cost: cx.new(|cx| text_input(cx, "3", "0")),
            pricing_output_cost: cx.new(|cx| text_input(cx, "15", "0")),
            pricing_cache_read_cost: cx.new(|cx| text_input(cx, "0.3", "0")),
            pricing_cache_creation_cost: cx.new(|cx| text_input(cx, "3.75", "0")),
            multiplier_claude: cx.new(|cx| text_input(cx, "1", "1")),
            multiplier_codex: cx.new(|cx| text_input(cx, "1", "1")),
            stream_timeout_secs: cx.new(|cx| text_input(cx, "8", "8")),
            stream_max_retries: cx.new(|cx| text_input(cx, "1", "1")),
            stream_degraded_threshold_ms: cx.new(|cx| text_input(cx, "6000", "6000")),
            log_page_input: cx
                .new(|cx| text_input(cx, raw(k::USAGE_PAGINATION_PAGE_PLACEHOLDER), "").compact()),
            range_start_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss", "")),
            range_end_input: cx.new(|cx| text_input(cx, "YYYY/MM/DD HH:mm:ss", "")),
            load_error: false,
            list_state: ListState::new(USAGE_BLOCK_COUNT, ListAlignment::Top, px(600.)),
        };
        this.reload(cx);
        this.load_config_forms(cx);
        // “跳至 X 页”回车提交。
        let jump = cx.listener(|this: &mut Self, _event: &(), _window, cx| {
            let text = input_value(&this.log_page_input, cx);
            if let Ok(target) = text.parse::<u32>() {
                if target >= 1 {
                    let total_pages = this.log_total.div_ceil(this.log_page_size).max(1);
                    this.set_log_page((target - 1).min(total_pages - 1), cx);
                }
            }
        });
        this.log_page_input.update(cx, |input, _| {
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

    /// Every status toast carries its severity explicitly. Guessing it from the
    /// wording mis-reads several of these messages — a finished session sync
    /// that merely mentions skipped files is not a warning — and stops working
    /// entirely once the copy is translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(text.into());
        self.status_level = Some(level);
        cx.notify();
    }

    /// Drop the current toast without emitting a new one.
    fn clear_status(&mut self) {
        self.status = None;
        self.status_level = None;
    }

    /// Kick off a background reload of everything the page shows. The queries
    /// share the SQLite connection with the gateway's usage logging, so they
    /// must never run on the UI thread.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let (start, end) = self.range.bounds();
        let app_filter = self.app_filter.clone();
        let provider_filter = self.provider_filter.clone();
        let model_filter = self.model_filter.clone();
        let status_filter = self.status_filter;
        let log_page = self.log_page;
        let log_page_size = self.log_page_size;
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_spawn(async move {
                    load_usage_data(
                        &app,
                        start,
                        end,
                        app_filter.as_deref(),
                        provider_filter.as_deref(),
                        model_filter.as_deref(),
                        status_filter,
                        log_page,
                        log_page_size,
                    )
                })
                .await;
            this.update(cx, |this, cx| this.apply_data(data, cx)).ok();
        })
        .detach();
    }

    fn apply_data(&mut self, data: UsageData, cx: &mut Context<Self>) {
        if data.errors.is_empty() {
            if self.load_error {
                self.clear_status();
                self.load_error = false;
            }
        } else {
            self.set_status(
                NotificationLevel::Error,
                data.errors.join(raw(k::USAGE_STATUS_ERROR_SEPARATOR)),
                cx,
            );
            self.load_error = true;
        }
        self.summary = data.summary;
        self.summary_by_app = data.summary_by_app;
        self.daily = data.daily;
        self.providers = data.providers;
        self.provider_options = data.provider_options;
        self.models = data.models;
        self.model_options = data.model_options;
        self.logs = data.logs;
        self.log_total = data.log_total;
        self.data_sources = data.data_sources;
        self.pricing = data.pricing;
        self.stream_config = data.stream_config;
        self.list_state.remeasure();
        cx.notify();
    }

    fn load_config_forms(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let stream_config = app.db.get_stream_check_config().ok();
                    let mut pricing_defaults = Vec::new();
                    for name in PRICING_APPS {
                        let multiplier = app
                            .db
                            .get_default_cost_multiplier(name)
                            .await
                            .unwrap_or_else(|_| "1".to_string());
                        let source = app
                            .db
                            .get_pricing_model_source(name)
                            .await
                            .unwrap_or_else(|_| "response".to_string());
                        pricing_defaults.push((name, multiplier, source));
                    }
                    (stream_config, pricing_defaults)
                })
                .await;
            this.update(cx, |this, cx| {
                let (stream_config, pricing_defaults) = loaded;
                if let Some(config) = stream_config {
                    this.stream_config = config.clone();
                    set_input(
                        &this.stream_timeout_secs,
                        config.timeout_secs.to_string(),
                        cx,
                    );
                    set_input(&this.stream_max_retries, config.max_retries.to_string(), cx);
                    set_input(
                        &this.stream_degraded_threshold_ms,
                        config.degraded_threshold_ms.to_string(),
                        cx,
                    );
                }
                for (name, multiplier, source) in pricing_defaults {
                    this.pricing_sources.insert(name.to_string(), source);
                    match name {
                        "claude" => set_input(&this.multiplier_claude, multiplier, cx),
                        "codex" => set_input(&this.multiplier_codex, multiplier, cx),
                        _ => {}
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reset_log_page(&mut self) {
        self.log_page = 0;
        self.selected_log = None;
    }

    fn set_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        self.range = range;
        self.reset_log_page();
        self.reload(cx);
        cx.notify();
    }

    fn toggle_filter_popover(&mut self, popover: FilterPopover, cx: &mut Context<Self>) {
        if self.open_filter_popover == Some(popover) {
            self.open_filter_popover = None;
            self.active_datetime_picker = None;
        } else {
            if popover == FilterPopover::Time {
                self.sync_range_inputs(cx);
            }
            self.active_datetime_picker = None;
            self.open_filter_popover = Some(popover);
        }
        cx.notify();
    }

    fn sync_range_inputs(&mut self, cx: &mut Context<Self>) {
        let (Some(start), Some(end)) = self.range.bounds() else {
            return;
        };
        set_input(
            &self.range_start_input,
            format_local_timestamp(start, true),
            cx,
        );
        set_input(&self.range_end_input, format_local_timestamp(end, true), cx);
    }

    fn endpoint_input(&self, endpoint: RangeEndpoint) -> &Entity<TextInput> {
        match endpoint {
            RangeEndpoint::Start => &self.range_start_input,
            RangeEndpoint::End => &self.range_end_input,
        }
    }

    fn endpoint_datetime(
        &self,
        endpoint: RangeEndpoint,
        cx: &mut Context<Self>,
    ) -> chrono::DateTime<Local> {
        let input = input_value(self.endpoint_input(endpoint), cx);
        let parsed = parse_local_timestamp(&input, endpoint == RangeEndpoint::End)
            .and_then(|timestamp| Local.timestamp_opt(timestamp, 0).single());
        if let Some(value) = parsed {
            return value;
        }

        let (start, end) = self.range.bounds();
        let fallback = match endpoint {
            RangeEndpoint::Start => start,
            RangeEndpoint::End => end,
        };
        fallback
            .and_then(|timestamp| Local.timestamp_opt(timestamp, 0).single())
            .unwrap_or_else(Local::now)
    }

    fn toggle_datetime_picker(&mut self, endpoint: RangeEndpoint, cx: &mut Context<Self>) {
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
        endpoint: RangeEndpoint,
        date: NaiveDate,
        hour: u32,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let second = if endpoint == RangeEndpoint::End {
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
        set_input(&input, format_local_timestamp(value.timestamp(), true), cx);
        cx.notify();
    }

    fn select_picker_date(
        &mut self,
        endpoint: RangeEndpoint,
        date: NaiveDate,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.picker_year = date.year();
        self.picker_month = date.month();
        self.update_datetime_endpoint(endpoint, date, current.hour(), current.minute(), cx);
    }

    fn select_picker_hour(&mut self, endpoint: RangeEndpoint, hour: u32, cx: &mut Context<Self>) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), hour, current.minute(), cx);
        self.picker_hour_scroll.scroll_to_top_of_item(hour as usize);
    }

    fn select_picker_minute(
        &mut self,
        endpoint: RangeEndpoint,
        minute: u32,
        cx: &mut Context<Self>,
    ) {
        let current = self.endpoint_datetime(endpoint, cx);
        self.update_datetime_endpoint(endpoint, current.date_naive(), current.hour(), minute, cx);
        self.picker_minute_scroll
            .scroll_to_top_of_item(minute as usize);
    }

    fn select_picker_today(&mut self, endpoint: RangeEndpoint, cx: &mut Context<Self>) {
        let current = self.endpoint_datetime(endpoint, cx);
        let today = Local::now().date_naive();
        self.picker_year = today.year();
        self.picker_month = today.month();
        self.update_datetime_endpoint(endpoint, today, current.hour(), current.minute(), cx);
    }

    fn clear_picker_value(&mut self, endpoint: RangeEndpoint, cx: &mut Context<Self>) {
        let input = self.endpoint_input(endpoint).clone();
        set_input(&input, "", cx);
        self.active_datetime_picker = None;
        cx.notify();
    }

    fn shift_picker_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        (self.picker_year, self.picker_month) =
            shifted_year_month(self.picker_year, self.picker_month, delta);
        cx.notify();
    }

    /// 应用时间弹层里的自定义范围（本地时区，精确到秒）。
    fn apply_custom_range(&mut self, cx: &mut Context<Self>) {
        let start_text = input_value(&self.range_start_input, cx);
        let end_text = input_value(&self.range_end_input, cx);
        let Some(start) = parse_local_timestamp(&start_text, false) else {
            self.set_status(
                NotificationLevel::Error,
                t(k::USAGE_FILTER_ERROR_START_FORMAT),
                cx,
            );
            return;
        };
        let Some(end) = parse_local_timestamp(&end_text, true) else {
            self.set_status(
                NotificationLevel::Error,
                t(k::USAGE_FILTER_ERROR_END_FORMAT),
                cx,
            );
            return;
        };
        if start > end {
            self.set_status(
                NotificationLevel::Error,
                t(k::USAGE_FILTER_ERROR_RANGE_ORDER),
                cx,
            );
            return;
        }
        self.open_filter_popover = None;
        self.active_datetime_picker = None;
        self.set_range(UsageRange::Custom { start, end }, cx);
    }

    fn set_provider_filter(&mut self, provider_name: Option<String>, cx: &mut Context<Self>) {
        if self.provider_filter != provider_name {
            self.model_filter = None;
        }
        self.provider_filter = provider_name;
        self.reset_log_page();
        self.reload(cx);
        cx.notify();
    }

    fn set_model_filter(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        self.model_filter = model;
        self.reset_log_page();
        self.reload(cx);
        cx.notify();
    }

    fn set_status_filter(&mut self, status: Option<u16>, cx: &mut Context<Self>) {
        self.status_filter = status;
        self.reset_log_page();
        self.reload(cx);
        cx.notify();
    }

    fn set_section(&mut self, section: UsageSection, cx: &mut Context<Self>) {
        self.section = section;
        self.list_state.remeasure();
        cx.notify();
    }

    fn set_log_page(&mut self, page: u32, cx: &mut Context<Self>) {
        self.log_page = page;
        self.selected_log = None;
        self.log_page_size_open = false;
        self.reload(cx);
        cx.notify();
    }

    fn toggle_log_page_size(&mut self, cx: &mut Context<Self>) {
        self.log_page_size_open = !self.log_page_size_open;
        cx.notify();
    }

    fn set_log_page_size(&mut self, page_size: u32, cx: &mut Context<Self>) {
        if self.log_page_size != page_size {
            self.log_page_size = page_size;
            self.log_page = 0;
            self.selected_log = None;
            self.reload(cx);
        }
        self.log_page_size_open = false;
        cx.notify();
    }

    fn select_log(&mut self, request_id: String, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let detail = cx
                .background_spawn(async move { app.db.get_request_detail(&request_id) })
                .await;
            this.update(cx, |this, cx| {
                match detail {
                    Ok(detail) => {
                        this.selected_log = detail;
                        this.clear_status();
                    }
                    Err(err) => {
                        this.selected_log = None;
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::USAGE_STATUS_DETAIL_LOAD_FAILED, error = err),
                            cx,
                        );
                    }
                }
                this.list_state.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sync_sessions(&mut self, cx: &mut Context<Self>) {
        self.set_status(NotificationLevel::Info, t(k::USAGE_STATUS_SYNCING), cx);
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let mut result = match sync_claude_session_logs(&app.db) {
                        Ok(result) => result,
                        Err(err) => SessionSyncResult {
                            imported: 0,
                            skipped: 0,
                            files_scanned: 0,
                            errors: vec![format!("Claude sync failed: {err}")],
                        },
                    };

                    for (label, sync_result) in [
                        (
                            "Codex",
                            services::session_usage_codex::sync_codex_usage(&app.db),
                        ),
                        (
                            "OpenCode",
                            services::session_usage_opencode::sync_opencode_usage(&app.db),
                        ),
                    ] {
                        match sync_result {
                            Ok(r) => {
                                result.imported += r.imported;
                                result.skipped += r.skipped;
                                result.files_scanned += r.files_scanned;
                                result.errors.extend(r.errors);
                            }
                            Err(err) => result.errors.push(format!("{label} sync failed: {err}")),
                        }
                    }
                    result
                })
                .await;
            this.update(cx, |this, cx| {
                // A sync that finished with per-file errors still imported what
                // it could, so it is a warning rather than a failure.
                let level = if result.errors.is_empty() {
                    NotificationLevel::Success
                } else {
                    NotificationLevel::Warning
                };
                // Two whole sentences rather than one with an appended clause:
                // only the reporting language knows where the error count goes.
                let summary = if result.errors.is_empty() {
                    tf!(
                        k::USAGE_STATUS_SYNC_DONE,
                        imported = result.imported,
                        skipped = result.skipped,
                        files = result.files_scanned,
                    )
                } else {
                    tf!(
                        k::USAGE_STATUS_SYNC_DONE_WITH_ERRORS,
                        imported = result.imported,
                        skipped = result.skipped,
                        files = result.files_scanned,
                        errors = result.errors.len(),
                    )
                };
                this.set_status(level, summary, cx);
                this.load_error = false;
                this.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn edit_pricing(&mut self, pricing: ModelPricingInfo, cx: &mut Context<Self>) {
        set_input(&self.pricing_model_id, pricing.model_id, cx);
        set_input(&self.pricing_display_name, pricing.display_name, cx);
        set_input(&self.pricing_input_cost, pricing.input_cost_per_million, cx);
        set_input(
            &self.pricing_output_cost,
            pricing.output_cost_per_million,
            cx,
        );
        set_input(
            &self.pricing_cache_read_cost,
            pricing.cache_read_cost_per_million,
            cx,
        );
        set_input(
            &self.pricing_cache_creation_cost,
            pricing.cache_creation_cost_per_million,
            cx,
        );
        self.show_pricing = true;
        cx.notify();
    }

    fn save_pricing(&mut self, cx: &mut Context<Self>) {
        let model_id = input_value(&self.pricing_model_id, cx);
        let display_name = input_value(&self.pricing_display_name, cx);
        let input_cost = input_value(&self.pricing_input_cost, cx);
        let output_cost = input_value(&self.pricing_output_cost, cx);
        let cache_read = input_value(&self.pricing_cache_read_cost, cx);
        let cache_creation = input_value(&self.pricing_cache_creation_cost, cx);

        match self.app.db.update_model_pricing(
            &model_id,
            &display_name,
            &input_cost,
            &output_cost,
            &cache_read,
            &cache_creation,
        ) {
            Ok(()) => {
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::USAGE_PRICING_SAVED, model = model_id),
                    cx,
                );
                self.reload(cx);
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::USAGE_PRICING_SAVE_FAILED, error = err),
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn delete_pricing(&mut self, model_id: String, cx: &mut Context<Self>) {
        match self.app.db.delete_model_pricing(&model_id) {
            Ok(()) => {
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::USAGE_PRICING_DELETED, model = model_id),
                    cx,
                );
                self.reload(cx);
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::USAGE_PRICING_DELETE_FAILED, error = err),
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn set_pricing_source(
        &mut self,
        app: &'static str,
        source: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.pricing_sources
            .insert(app.to_string(), source.to_string());
        cx.notify();
    }

    fn save_pricing_defaults(&mut self, cx: &mut Context<Self>) {
        let configs = [
            (
                "claude",
                input_value(&self.multiplier_claude, cx),
                self.pricing_sources
                    .get("claude")
                    .cloned()
                    .unwrap_or_else(|| "response".to_string()),
            ),
            (
                "codex",
                input_value(&self.multiplier_codex, cx),
                self.pricing_sources
                    .get("codex")
                    .cloned()
                    .unwrap_or_else(|| "response".to_string()),
            ),
        ];

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    for (name, multiplier, source) in configs {
                        app.db
                            .set_default_cost_multiplier(name, multiplier.trim())
                            .await?;
                        app.db.set_pricing_model_source(name, source.trim()).await?;
                    }
                    Ok::<(), ochub_core::AppError>(())
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.set_status(
                        NotificationLevel::Success,
                        t(k::USAGE_PRICING_DEFAULTS_SAVED),
                        cx,
                    ),
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::USAGE_PRICING_DEFAULTS_SAVE_FAILED, error = err),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_stream_config(&mut self, cx: &mut Context<Self>) {
        let config = match parse_stream_config(self, cx) {
            Ok(config) => config,
            Err(err) => {
                self.set_status(NotificationLevel::Error, err, cx);
                return;
            }
        };

        match self.app.db.save_stream_check_config(&config) {
            Ok(()) => {
                self.stream_config = config;
                self.set_status(NotificationLevel::Success, t(k::USAGE_STREAM_SAVED), cx);
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::USAGE_STREAM_SAVE_FAILED, error = err),
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn render_datetime_picker(
        &self,
        endpoint: RangeEndpoint,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.endpoint_datetime(endpoint, cx);
        let picker_id = match endpoint {
            RangeEndpoint::Start => "usage-start-datetime",
            RangeEndpoint::End => "usage-end-datetime",
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

    #[allow(dead_code)]
    fn render_datetime_picker_legacy(
        &self,
        endpoint: RangeEndpoint,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.endpoint_datetime(endpoint, cx);
        let selected_date = selected.date_naive();
        let selected_hour = selected.hour();
        let selected_minute = selected.minute();
        let today = Local::now().date_naive();
        let first_of_month =
            NaiveDate::from_ymd_opt(self.picker_year, self.picker_month, 1).unwrap_or(today);
        let calendar_start =
            first_of_month - Duration::days(first_of_month.weekday().num_days_from_sunday() as i64);

        let mut weekday_header = div().grid().grid_cols(7).gap(px(2.)).w_full();
        // The calendar starts on Sunday, so the initials follow that order.
        for weekday in [
            t(k::COMMON_CALENDAR_WEEKDAY_SUN),
            t(k::COMMON_CALENDAR_WEEKDAY_MON),
            t(k::COMMON_CALENDAR_WEEKDAY_TUE),
            t(k::COMMON_CALENDAR_WEEKDAY_WED),
            t(k::COMMON_CALENDAR_WEEKDAY_THU),
            t(k::COMMON_CALENDAR_WEEKDAY_FRI),
            t(k::COMMON_CALENDAR_WEEKDAY_SAT),
        ] {
            weekday_header = weekday_header.child(
                div()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::muted())
                    .child(weekday),
            );
        }

        let mut day_grid = div().grid().grid_cols(7).gap(px(2.)).w_full();
        for ix in 0..42 {
            let date = calendar_start + Duration::days(ix);
            let in_current_month = date.month() == self.picker_month;
            let is_selected = date == selected_date;
            let is_today = date == today;
            day_grid = day_grid.child(
                calendar_day_button(
                    ElementId::Name(
                        format!(
                            "usage-picker-day-{}-{}-{}",
                            date.year(),
                            date.month(),
                            date.day()
                        )
                        .into(),
                    ),
                    date,
                    in_current_month,
                    is_selected,
                    is_today,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_picker_date(endpoint, date, cx);
                })),
            );
        }

        let calendar = div()
            .w(px(236.))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.))
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(SharedString::from(tf!(
                                k::COMMON_CALENDAR_MONTH_TITLE,
                                year = self.picker_year,
                                month = picker_month_label(self.picker_month)
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                calendar_nav_button(
                                    "usage-picker-previous-month",
                                    raw(k::COMMON_CALENDAR_PREVIOUS_MONTH_ARIA),
                                    IconName::ChevronLeft,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.shift_picker_month(-1, cx);
                                    },
                                )),
                            )
                            .child(
                                calendar_nav_button(
                                    "usage-picker-next-month",
                                    raw(k::COMMON_CALENDAR_NEXT_MONTH_ARIA),
                                    IconName::ChevronRight,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.shift_picker_month(1, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(div().px_3().child(weekday_header))
            .child(div().px_3().pt_1().child(day_grid))
            .child(div().flex_1())
            .child(
                div()
                    .h(px(40.))
                    .px_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        calendar_footer_button(
                            "usage-picker-clear",
                            t(k::COMMON_CALENDAR_CLEAR_LABEL),
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.clear_picker_value(endpoint, cx);
                            },
                        )),
                    )
                    .child(
                        calendar_footer_button(
                            "usage-picker-today",
                            t(k::COMMON_CALENDAR_TODAY_LABEL),
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.select_picker_today(endpoint, cx);
                            },
                        )),
                    ),
            );

        let hour_scroll_id = match endpoint {
            RangeEndpoint::Start => "usage-picker-start-hours",
            RangeEndpoint::End => "usage-picker-end-hours",
        };
        let mut hour_options = div()
            .id(hour_scroll_id)
            .w(px(54.))
            .h(px(252.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.picker_hour_scroll)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.picker_hour_scroll.clone(),
            ));
        for hour in 0..24u32 {
            hour_options = hour_options.child(
                time_value_button(
                    ElementId::Name(format!("{hour_scroll_id}-{hour}").into()),
                    hour,
                    hour == selected_hour,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_picker_hour(endpoint, hour, cx);
                })),
            );
        }

        let minute_scroll_id = match endpoint {
            RangeEndpoint::Start => "usage-picker-start-minutes",
            RangeEndpoint::End => "usage-picker-end-minutes",
        };
        let mut minute_options = div()
            .id(minute_scroll_id)
            .w(px(54.))
            .h(px(252.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.picker_minute_scroll)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.picker_minute_scroll.clone(),
            ));
        for minute in 0..60u32 {
            minute_options = minute_options.child(
                time_value_button(
                    ElementId::Name(format!("{minute_scroll_id}-{minute}").into()),
                    minute,
                    minute == selected_minute,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_picker_minute(endpoint, minute, cx);
                })),
            );
        }

        let time_selector = div()
            .w(px(111.))
            .h_full()
            .flex_none()
            .flex()
            .flex_row()
            .child(
                div()
                    .w(px(55.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(time_column_label(raw(k::COMMON_CALENDAR_HOUR_LABEL)))
                    .child(hour_options),
            )
            .child(
                div()
                    .w(px(56.))
                    .border_l_1()
                    .border_color(theme::border())
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(time_column_label(raw(k::COMMON_CALENDAR_MINUTE_LABEL)))
                    .child(minute_options),
            );

        let popover_id = match endpoint {
            RangeEndpoint::Start => "usage-start-datetime-popover",
            RangeEndpoint::End => "usage-end-datetime-popover",
        };
        filter_popover_panel(popover_id, 348.)
            .h(px(296.))
            .p_0()
            .flex_row()
            .child(calendar)
            .child(div().w(px(1.)).h_full().bg(theme::border()))
            .child(time_selector)
            .on_mouse_down_out(cx.listener(move |this, _event, _window, cx| {
                if this.active_datetime_picker == Some(endpoint) {
                    this.active_datetime_picker = None;
                    cx.notify();
                }
            }))
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Not a `const`: the "all" label is a catalog lookup, and the status
        // codes below it are numbers rather than prose.
        let status_filters: [(Option<u16>, &'static str); 6] = [
            (None, raw(k::USAGE_FILTER_STATUS_ALL)),
            (Some(200), "200"),
            (Some(400), "400"),
            (Some(401), "401"),
            (Some(429), "429"),
            (Some(500), "500"),
        ];

        let time_open = self.open_filter_popover == Some(FilterPopover::Time);
        let mut quick_ranges = div().flex().flex_row().flex_wrap().gap_2();
        for (ix, range) in UsageRange::all().iter().enumerate() {
            let range = *range;
            quick_ranges = quick_ranges.child(
                quick_range_button(
                    ElementId::Name(format!("usage-quick-range-{ix}").into()),
                    range.label(),
                    self.range == range,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.active_datetime_picker = None;
                    this.set_range(range, cx);
                })),
            );
        }

        let start_picker_open = self.active_datetime_picker == Some(RangeEndpoint::Start);
        let start_datetime_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "usage-start-datetime-field",
                    raw(k::USAGE_FILTER_RANGE_START_LABEL),
                    self.range_start_input.clone(),
                    start_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if start_picker_open {
                            this.active_datetime_picker = None;
                            cx.notify();
                        } else {
                            this.toggle_datetime_picker(RangeEndpoint::Start, cx);
                        }
                    }),
                ),
            )
            .when(start_picker_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(DATETIME_PICKER_GAP)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(RangeEndpoint::Start, cx)),
                    )
                    .priority(20),
                )
            });

        let end_picker_open = self.active_datetime_picker == Some(RangeEndpoint::End);
        let end_datetime_control = div()
            .relative()
            .w_full()
            .child(
                components::datetime_filter_field(
                    "usage-end-datetime-field",
                    raw(k::USAGE_FILTER_RANGE_END_LABEL),
                    self.range_end_input.clone(),
                    end_picker_open,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if end_picker_open {
                            this.active_datetime_picker = None;
                            cx.notify();
                        } else {
                            this.toggle_datetime_picker(RangeEndpoint::End, cx);
                        }
                    }),
                ),
            )
            .when(end_picker_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(DATETIME_PICKER_GAP)))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_datetime_picker(RangeEndpoint::End, cx)),
                    )
                    .priority(20),
                )
            });

        let time_popover = filter_popover_panel("usage-time-popover", 300.)
            .gap_3()
            .p_3()
            .child(filter_section_label(raw(k::USAGE_FILTER_TIME_QUICK_PICKS)))
            .child(quick_ranges)
            .child(div().w_full().h(px(1.)).bg(theme::border()))
            .child(filter_section_label(raw(k::USAGE_FILTER_TIME_CUSTOM_RANGE)))
            .child(start_datetime_control)
            .child(end_datetime_control)
            .child(
                components::button(
                    "usage-apply-range",
                    t(k::USAGE_FILTER_TIME_APPLY),
                    ButtonTone::Primary,
                    ButtonSize::Md,
                )
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.apply_custom_range(cx);
                })),
            )
            .when(self.active_datetime_picker.is_none(), |s| {
                s.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                    if this.open_filter_popover == Some(FilterPopover::Time) {
                        this.open_filter_popover = None;
                        this.active_datetime_picker = None;
                        cx.notify();
                    }
                }))
            });
        let time_control = div()
            .relative()
            .flex_none()
            .child(
                filter_trigger(
                    "usage-time-filter",
                    range_filter_label(self.range),
                    IconName::Calendar,
                    time_open,
                    300.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if time_open {
                            this.open_filter_popover = None;
                            this.active_datetime_picker = None;
                            cx.notify();
                        } else {
                            this.toggle_filter_popover(FilterPopover::Time, cx);
                        }
                    }),
                ),
            )
            .when(time_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(time_popover),
                    )
                    .priority(10),
                )
            });

        let provider_open = self.open_filter_popover == Some(FilterPopover::Provider);
        let mut provider_popover = filter_popover_panel("usage-provider-popover", 240.)
            .relative()
            .p_1()
            .max_h(px(280.))
            .overflow_y_scroll()
            .track_scroll(&self.provider_filter_scroll)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.provider_filter_scroll.clone(),
            ))
            .child(
                dropdown_option(
                    "usage-provider-all",
                    t(k::USAGE_FILTER_PROVIDER_ALL),
                    self.provider_filter.is_none(),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.set_provider_filter(None, cx);
                })),
            );
        for (ix, provider) in self.provider_options.iter().enumerate() {
            let selected = self.provider_filter.as_deref() == Some(provider.as_str());
            let provider_for_click = provider.clone();
            provider_popover = provider_popover.child(
                dropdown_option(
                    ElementId::Name(format!("usage-provider-option-{ix}").into()),
                    provider.clone(),
                    selected,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.set_provider_filter(Some(provider_for_click.clone()), cx);
                })),
            );
        }
        provider_popover = provider_popover.child(crate::scrollbar::VerticalScrollbar::new(
            "usage-provider-options-scrollbar",
            self.provider_filter_scroll.clone(),
        ));
        if self.provider_options.is_empty() {
            provider_popover = provider_popover.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::USAGE_FILTER_PROVIDER_EMPTY)),
            );
        }
        provider_popover =
            provider_popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(FilterPopover::Provider) {
                    this.open_filter_popover = None;
                    cx.notify();
                }
            }));
        let provider_control = div()
            .relative()
            .flex_none()
            .child(
                filter_trigger(
                    "usage-provider-filter",
                    self.provider_filter
                        .clone()
                        .unwrap_or_else(|| raw(k::USAGE_FILTER_PROVIDER_ALL).to_string()),
                    IconName::Cloud,
                    provider_open,
                    180.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if provider_open {
                            this.open_filter_popover = None;
                            cx.notify();
                        } else {
                            this.toggle_filter_popover(FilterPopover::Provider, cx);
                        }
                    }),
                ),
            )
            .when(provider_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(provider_popover),
                    )
                    .priority(10),
                )
            });

        let model_open = self.open_filter_popover == Some(FilterPopover::Model);
        let mut model_popover = filter_popover_panel("usage-model-popover", 240.)
            .relative()
            .p_1()
            .max_h(px(280.))
            .overflow_y_scroll()
            .track_scroll(&self.model_filter_scroll)
            .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                self.model_filter_scroll.clone(),
            ))
            .child(
                dropdown_option(
                    "usage-model-all",
                    t(k::USAGE_FILTER_MODEL_ALL),
                    self.model_filter.is_none(),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.set_model_filter(None, cx);
                })),
            );
        for (ix, model) in self.model_options.iter().enumerate() {
            let selected = self.model_filter.as_deref() == Some(model.as_str());
            let model_for_click = model.clone();
            model_popover = model_popover.child(
                dropdown_option(
                    ElementId::Name(format!("usage-model-option-{ix}").into()),
                    model.clone(),
                    selected,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.set_model_filter(Some(model_for_click.clone()), cx);
                })),
            );
        }
        if self.model_options.is_empty() {
            model_popover = model_popover.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::USAGE_FILTER_MODEL_EMPTY)),
            );
        }
        model_popover = model_popover.child(crate::scrollbar::VerticalScrollbar::new(
            "usage-model-options-scrollbar",
            self.model_filter_scroll.clone(),
        ));
        model_popover =
            model_popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(FilterPopover::Model) {
                    this.open_filter_popover = None;
                    cx.notify();
                }
            }));
        let model_control = div()
            .relative()
            .flex_none()
            .child(
                filter_trigger(
                    "usage-model-filter",
                    self.model_filter
                        .clone()
                        .unwrap_or_else(|| raw(k::USAGE_FILTER_MODEL_ALL).to_string()),
                    IconName::Layers,
                    model_open,
                    180.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if model_open {
                            this.open_filter_popover = None;
                            cx.notify();
                        } else {
                            this.toggle_filter_popover(FilterPopover::Model, cx);
                        }
                    }),
                ),
            )
            .when(model_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(model_popover),
                    )
                    .priority(10),
                )
            });

        let status_open = self.open_filter_popover == Some(FilterPopover::Status);
        let mut status_popover = filter_popover_panel("usage-status-popover", 148.).p_1();
        for (ix, (status, label)) in status_filters.iter().enumerate() {
            let status = *status;
            status_popover = status_popover.child(
                dropdown_option(
                    ElementId::Name(format!("usage-status-option-{ix}").into()),
                    *label,
                    self.status_filter == status,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.open_filter_popover = None;
                    this.set_status_filter(status, cx);
                })),
            );
        }
        status_popover =
            status_popover.on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                if this.open_filter_popover == Some(FilterPopover::Status) {
                    this.open_filter_popover = None;
                    cx.notify();
                }
            }));
        let status_label = status_filters
            .iter()
            .find_map(|(status, label)| (self.status_filter == *status).then_some(*label))
            .unwrap_or_else(|| raw(k::USAGE_FILTER_STATUS_ALL));
        let status_control = div()
            .relative()
            .flex_none()
            .child(
                filter_trigger(
                    "usage-status-filter",
                    status_label,
                    IconName::Check,
                    status_open,
                    136.,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if status_open {
                            this.open_filter_popover = None;
                            cx.notify();
                        } else {
                            this.toggle_filter_popover(FilterPopover::Status, cx);
                        }
                    }),
                ),
            )
            .when(status_open, |s| {
                s.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.), px(4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(status_popover),
                    )
                    .priority(10),
                )
            });

        let has_active_filters = self.range != UsageRange::Today
            || self.app_filter.is_some()
            || self.provider_filter.is_some()
            || self.model_filter.is_some()
            || self.status_filter.is_some();

        components::card().p_3().gap_2().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(time_control)
                .child(provider_control)
                .child(model_control)
                .child(status_control)
                .when(has_active_filters, |s| {
                    s.child(
                        components::button(
                            "usage-clear-filters",
                            t(k::USAGE_FILTER_RESET),
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.app_filter = None;
                                this.provider_filter = None;
                                this.model_filter = None;
                                this.status_filter = None;
                                this.range = UsageRange::Today;
                                this.open_filter_popover = None;
                                this.active_datetime_picker = None;
                                set_input(&this.range_start_input, "", cx);
                                set_input(&this.range_end_input, "", cx);
                                this.reset_log_page();
                                this.reload(cx);
                                cx.notify();
                            },
                        )),
                    )
                }),
        )
    }

    fn render_data_sources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sources =
            self.data_sources
                .iter()
                .map(|source| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme::surface())
                        .border_1()
                        .border_color(theme::border())
                        .child(icon(
                            data_source_icon(&source.data_source),
                            theme::subtext(),
                            13.,
                        ))
                        .child(
                            div()
                                .text_color(theme::text())
                                .text_xs()
                                .child(SharedString::from(data_source_label(&source.data_source))),
                        )
                        .child(div().text_color(theme::muted()).text_xs().child(
                            SharedString::from(tf!(
                                k::USAGE_DATA_SOURCE_REQUESTS,
                                count = source.request_count
                            )),
                        ))
                })
                .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_3()
            .w_full()
            .p_3()
            .rounded_lg()
            .bg(theme::surface_hover())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::USAGE_DATA_SOURCE_TITLE)),
                    )
                    .children(sources),
            )
            .child(
                components::icon_button_tone(
                    "usage-sync-sessions",
                    t(k::USAGE_DATA_SOURCE_SYNC),
                    IconName::Refresh,
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.sync_sessions(cx);
                })),
            )
    }

    fn render_summary(&self) -> impl IntoElement {
        let cards = self.summary.clone().map(|summary| {
            let cache_hit_rate = summary.cache_hit_rate * 100.0;
            div()
                .grid()
                .grid_cols(2)
                .gap_3()
                .child(components::stat_tile(
                    Some(IconName::Message),
                    theme::green(),
                    t(k::USAGE_SUMMARY_REQUESTS_LABEL),
                    summary.total_requests.to_string(),
                    tf!(
                        k::USAGE_SUMMARY_REQUESTS_DETAIL,
                        rate = format!("{:.1}", summary.success_rate)
                    ),
                ))
                .child(components::stat_tile(
                    Some(IconName::Diamond),
                    theme::peach(),
                    t(k::USAGE_SUMMARY_COST_LABEL),
                    format!("${}", format_money(&summary.total_cost, 6)),
                    tf!(
                        k::USAGE_SUMMARY_COST_DETAIL,
                        input = summary.total_input_tokens,
                        output = summary.total_output_tokens
                    ),
                ))
                .child(components::stat_tile(
                    Some(IconName::Layers),
                    theme::accent(),
                    t(k::USAGE_SUMMARY_TOKENS_LABEL),
                    summary.real_total_tokens.to_string(),
                    tf!(
                        k::USAGE_SUMMARY_TOKENS_DETAIL,
                        created = summary.total_cache_creation_tokens,
                        read = summary.total_cache_read_tokens
                    ),
                ))
                .child(components::stat_tile(
                    Some(IconName::Cloud),
                    theme::teal(),
                    t(k::USAGE_SUMMARY_CACHE_LABEL),
                    format!("{cache_hit_rate:.1}%"),
                    t(k::USAGE_SUMMARY_CACHE_DETAIL),
                ))
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .when_some(cards, |s, cards| s.child(cards))
            .child(self.render_app_breakdown())
    }

    fn render_app_breakdown(&self) -> impl IntoElement {
        let max_tokens = self
            .summary_by_app
            .iter()
            .map(|item| item.summary.real_total_tokens)
            .max()
            .unwrap_or(1)
            .max(1);
        let rows = self
            .summary_by_app
            .iter()
            .take(6)
            .map(|item| {
                // 相对轨道宽度的占比，最短保留 2% 以保证条可见。
                let ratio = ((item.summary.real_total_tokens as f64 / max_tokens as f64) as f32)
                    .clamp(0.02, 1.0);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(app_label(&item.app_type))),
                            )
                            .child(div().text_color(theme::muted()).text_xs().child(
                                SharedString::from(tf!(
                                    k::USAGE_BREAKDOWN_ROW_DETAIL,
                                    count = item.summary.total_requests,
                                    cost = format_money(&item.summary.total_cost, 4)
                                )),
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(8.))
                            .rounded_md()
                            .bg(theme::surface_hover())
                            .child(
                                div()
                                    .w(relative(ratio))
                                    .h(px(8.))
                                    .rounded_md()
                                    .bg(app_tone(&item.app_type)),
                            ),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(icon(IconName::Layers, theme::accent(), 15.))
                    .child(
                        div()
                            .text_color(theme::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::USAGE_BREAKDOWN_TITLE)),
                    ),
            )
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Layers,
                    t(k::USAGE_BREAKDOWN_EMPTY_TITLE),
                    t(k::USAGE_BREAKDOWN_EMPTY_HINT),
                    None,
                ))
            })
            .children(rows)
    }

    fn render_trend(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Oldest → newest, capped so dense ranges stay legible.
        let visible = {
            let mut v = self.daily.iter().rev().take(30).collect::<Vec<_>>();
            v.reverse();
            v
        };
        let values: Vec<f32> = visible
            .iter()
            .map(|stat| stat.total_cost.parse::<f32>().unwrap_or(0.0))
            .collect();
        // 悬停提示：每个时间桶一条 “时间 · $成本 · N 次”。
        let hover_labels: Vec<SharedString> = visible
            .iter()
            .map(|stat| {
                SharedString::from(tf!(
                    k::USAGE_TREND_HOVER,
                    time = trend_bucket_label(&stat.date),
                    cost = format_money(&stat.total_cost, 4),
                    count = stat.request_count
                ))
            })
            .collect();
        let total_cost: f32 = values.iter().copied().sum();
        let peak_cost = values.iter().copied().fold(0.0_f32, f32::max);
        let first_label = visible
            .first()
            .map(|stat| short_date_label(&stat.date))
            .unwrap_or_default();
        let last_label = visible
            .last()
            .map(|stat| short_date_label(&stat.date))
            .unwrap_or_default();
        // Re-key the animation on range/shape changes so it replays on filter switches.
        let anim_id = SharedString::from(format!(
            "usage-trend-{}-{}",
            self.range.label(),
            values.len()
        ));

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(icon(IconName::Chart, theme::accent(), 16.))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::USAGE_TREND_TITLE)),
                            ),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(self.range.label())),
                    ),
            )
            .when(values.len() < 2, |s| {
                s.child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(t(k::USAGE_TREND_INSUFFICIENT)),
                )
            })
            .when(values.len() >= 2, |s| {
                s.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_4()
                        .child(Self::trend_stat(
                            raw(k::USAGE_TREND_TOTAL_COST),
                            format_money(&format!("{total_cost}"), 3),
                        ))
                        .child(div().w(px(1.)).h(px(26.)).bg(theme::border()))
                        .child(Self::trend_stat(
                            raw(k::USAGE_TREND_PEAK),
                            format_money(&format!("{peak_cost}"), 3),
                        )),
                )
                .child(
                    // Hover 需要逐帧读取鼠标位置：进入/离开和移动时 notify 触发
                    // 重绘，canvas 在 paint 阶段自行画十字线和 tooltip。
                    div()
                        .id("usage-trend-hotspot")
                        .w_full()
                        .on_mouse_move(cx.listener(|_this, _event, _window, cx| {
                            cx.notify();
                        }))
                        .on_hover(cx.listener(|_this, _hovered, _window, cx| {
                            cx.notify();
                        }))
                        .child(
                            crate::chart::AreaChart::new(values)
                                .hover_labels(hover_labels)
                                .height(176.)
                                .with_animation(
                                    anim_id,
                                    Animation::new(std::time::Duration::from_millis(720))
                                        .with_easing(ease_out_quint()),
                                    |chart, delta| chart.progress(delta),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_color(theme::subtext())
                                .text_xs()
                                .child(SharedString::from(first_label)),
                        )
                        .child(
                            div()
                                .text_color(theme::subtext())
                                .text_xs()
                                .child(SharedString::from(last_label)),
                        ),
                )
            })
    }

    /// A small label-over-value stat used in the trend card header strip.
    fn trend_stat(label: &'static str, value: String) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(div().text_color(theme::muted()).text_xs().child(label))
            .child(
                div()
                    .text_color(theme::text())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(value)),
            )
    }

    fn render_section_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let labels: Vec<&str> = UsageSection::all()
            .iter()
            .map(|(section, _)| section.label())
            .collect();
        let selected = UsageSection::all()
            .iter()
            .position(|(section, _)| *section == self.section)
            .unwrap_or(0);
        let on_select = cx.listener(|this, ix: &usize, _window, cx| {
            if let Some((section, _)) = UsageSection::all().get(*ix) {
                this.set_section(*section, cx);
            }
        });
        components::segmented("usage-section", &labels, selected, move |ix, window, cx| {
            on_select(&ix, window, cx)
        })
    }

    fn render_active_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.section {
            UsageSection::Logs => self.render_logs(cx).into_any_element(),
            UsageSection::Providers => self.render_providers(cx).into_any_element(),
            UsageSection::Models => self.render_models(cx).into_any_element(),
        }
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.providers.len();
        let rows = self
            .providers
            .iter()
            .enumerate()
            .map(|(ix, stats)| {
                let provider = stats.provider_name.clone();
                components::table_row(
                    vec![
                        text_cell(provider.clone()).into_any_element(),
                        text_cell(stats.request_count.to_string()).into_any_element(),
                        text_cell(stats.total_tokens.to_string()).into_any_element(),
                        text_cell(format!("${}", format_money(&stats.total_cost, 4)))
                            .into_any_element(),
                        text_cell(format!("{:.1}%", stats.success_rate)).into_any_element(),
                        text_cell(format!("{}ms", stats.avg_latency_ms)).into_any_element(),
                    ],
                    6,
                    ix + 1 == row_count,
                )
                .id(ElementId::Name(
                    format!("usage-provider-row-{provider}").into(),
                ))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(tf!(
                    k::USAGE_PROVIDERS_ROW_ARIA,
                    name = provider
                )))
                .aria_selected(self.provider_filter.as_deref() == Some(provider.as_str()))
                .cursor_pointer()
                .hover(|s| s.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_provider_filter(Some(provider.clone()), cx);
                }))
            })
            .collect::<Vec<_>>();

        components::card()
            .p_0()
            .child(table_title(
                raw(k::USAGE_PROVIDERS_TITLE),
                t(k::USAGE_PROVIDERS_SUBTITLE),
            ))
            .child(components::table_header(&[
                raw(k::USAGE_PROVIDERS_COL_PROVIDER),
                raw(k::USAGE_PROVIDERS_COL_REQUESTS),
                raw(k::USAGE_PROVIDERS_COL_TOKENS),
                raw(k::USAGE_PROVIDERS_COL_COST),
                raw(k::USAGE_PROVIDERS_COL_SUCCESS_RATE),
                raw(k::USAGE_PROVIDERS_COL_LATENCY),
            ]))
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Cloud,
                    t(k::USAGE_EMPTY_NO_DATA),
                    t(k::USAGE_PROVIDERS_EMPTY_HINT),
                    None,
                ))
            })
            .children(rows)
    }

    fn render_models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.models.len();
        let rows = self
            .models
            .iter()
            .enumerate()
            .map(|(ix, stats)| {
                let model = stats.model.clone();
                components::table_row(
                    vec![
                        text_cell(model.clone()).into_any_element(),
                        text_cell(stats.request_count.to_string()).into_any_element(),
                        text_cell(stats.total_tokens.to_string()).into_any_element(),
                        text_cell(format!("${}", format_money(&stats.total_cost, 4)))
                            .into_any_element(),
                        text_cell(format!("${}", format_money(&stats.avg_cost_per_request, 6)))
                            .into_any_element(),
                    ],
                    5,
                    ix + 1 == row_count,
                )
                .id(ElementId::Name(format!("usage-model-row-{model}").into()))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(tf!(
                    k::USAGE_MODELS_ROW_ARIA,
                    name = model
                )))
                .aria_selected(self.model_filter.as_deref() == Some(model.as_str()))
                .cursor_pointer()
                .hover(|s| s.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_model_filter(Some(model.clone()), cx);
                }))
            })
            .collect::<Vec<_>>();

        components::card()
            .p_0()
            .child(table_title(
                raw(k::USAGE_MODELS_TITLE),
                t(k::USAGE_MODELS_SUBTITLE),
            ))
            .child(components::table_header(&[
                raw(k::USAGE_MODELS_COL_MODEL),
                raw(k::USAGE_MODELS_COL_REQUESTS),
                raw(k::USAGE_MODELS_COL_TOKENS),
                raw(k::USAGE_MODELS_COL_COST),
                raw(k::USAGE_MODELS_COL_AVG_COST),
            ]))
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Chart,
                    t(k::USAGE_EMPTY_NO_DATA),
                    t(k::USAGE_MODELS_EMPTY_HINT),
                    None,
                ))
            })
            .children(rows)
    }

    fn render_logs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_pages = self.log_total.div_ceil(self.log_page_size).max(1);
        let page = self.log_page.min(total_pages - 1);
        let row_count = self.logs.len();
        let rows = self
            .logs
            .iter()
            .enumerate()
            .map(|(ix, log)| {
                let provider = log
                    .provider_name
                    .clone()
                    .unwrap_or_else(|| log.provider_id.clone());
                let ok = (200..300).contains(&log.status_code);
                let request_id = log.request_id.clone();
                let time_cell = div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_xs()
                            .child(SharedString::from(short_time(log.created_at))),
                    )
                    .child(div().text_color(theme::muted()).text_xs().truncate().child(
                        SharedString::from(format!(
                            "{} · {}",
                            provider,
                            log.data_source.clone().unwrap_or_else(|| "proxy".into())
                        )),
                    ));
                components::table_row(
                    vec![
                        time_cell.into_any_element(),
                        text_cell(effective_model_label(log)).into_any_element(),
                        text_cell(tf!(
                            k::USAGE_LOGS_TOKENS,
                            input = fresh_input_tokens(log),
                            output = log.output_tokens
                        ))
                        .into_any_element(),
                        text_cell(format!(
                            "${} · {}ms",
                            format_money(&log.total_cost_usd, 4),
                            log.latency_ms
                        ))
                        .into_any_element(),
                        components::badge(
                            if ok {
                                BadgeTone::Success
                            } else {
                                BadgeTone::Danger
                            },
                            log.status_code.to_string(),
                        )
                        .into_any_element(),
                    ],
                    5,
                    ix + 1 == row_count,
                )
                .id(ElementId::Name(
                    format!("usage-log-row-{request_id}").into(),
                ))
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(tf!(
                    k::USAGE_LOGS_ROW_ARIA,
                    model = log.model,
                    status = log.status_code
                )))
                .cursor_pointer()
                .hover(|s| s.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_log(request_id.clone(), cx);
                }))
            })
            .collect::<Vec<_>>();

        let go = cx.listener(|this, page: &u32, _window, cx| {
            this.set_log_page(*page, cx);
        });
        let toggle_page_size = cx.listener(|this, _event: &(), _window, cx| {
            this.toggle_log_page_size(cx);
        });
        let set_page_size = cx.listener(|this, page_size: &u32, _window, cx| {
            this.set_log_page_size(*page_size, cx);
        });
        let pagination = components::pagination_bar(
            "usage-log-pages",
            page,
            total_pages,
            Some(self.log_total as u64),
            self.log_page_size,
            LOG_PAGE_SIZE_OPTIONS,
            self.log_page_size_open,
            &self.log_page_input,
            move |page, window, cx| go(&page, window, cx),
            move |window, cx| toggle_page_size(&(), window, cx),
            move |page_size, window, cx| set_page_size(&page_size, window, cx),
        );

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                components::card()
                    .p_0()
                    .child(table_title(
                        raw(k::USAGE_LOGS_TITLE),
                        tf!(
                            k::USAGE_LOGS_SUBTITLE,
                            page = page + 1,
                            pages = total_pages,
                            total = self.log_total
                        ),
                    ))
                    .child(components::table_header(&[
                        raw(k::USAGE_LOGS_COL_TIME),
                        raw(k::USAGE_LOGS_COL_MODEL),
                        raw(k::USAGE_LOGS_COL_TOKENS),
                        raw(k::USAGE_LOGS_COL_COST),
                        raw(k::USAGE_LOGS_COL_STATUS),
                    ]))
                    .when(rows.is_empty(), |s| {
                        s.child(components::empty_state(
                            IconName::Message,
                            t(k::USAGE_EMPTY_NO_DATA),
                            t(k::USAGE_LOGS_EMPTY_HINT),
                            None,
                        ))
                    })
                    .children(rows),
            )
            .child(pagination)
            .when_some(self.selected_log.clone(), |s, log| {
                s.child(Self::render_request_detail(log))
            })
    }

    fn render_request_detail(log: RequestLogDetail) -> impl IntoElement {
        let provider = log
            .provider_name
            .clone()
            .unwrap_or_else(|| log.provider_id.clone());
        components::card()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(icon(IconName::Message, theme::accent(), 15.))
                    .child(
                        div()
                            .text_color(theme::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::USAGE_DETAIL_TITLE)),
                    ),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_REQUEST_ID),
                        log.request_id.clone(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_TIME),
                        full_time(log.created_at),
                    ))
                    .child(detail_cell(raw(k::USAGE_DETAIL_APP), log.app_type.clone()))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_PROVIDER),
                        format!("{provider} · {}", log.provider_id.clone()),
                    ))
                    .child(detail_cell(raw(k::USAGE_DETAIL_MODEL), log.model.clone()))
                    .when_some(log.request_model.clone(), |s, value| {
                        s.child(detail_cell(raw(k::USAGE_DETAIL_REQUEST_MODEL), value))
                    })
                    .when_some(log.pricing_model.clone(), |s, value| {
                        s.child(detail_cell(raw(k::USAGE_DETAIL_PRICING_MODEL), value))
                    })
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_INPUT_TOKENS),
                        fresh_input_tokens(&log).to_string(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_OUTPUT_TOKENS),
                        log.output_tokens.to_string(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_CACHE_READ),
                        log.cache_read_tokens.to_string(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_CACHE_WRITE),
                        log.cache_creation_tokens.to_string(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_INPUT_COST),
                        format!("${}", log.input_cost_usd),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_OUTPUT_COST),
                        format!("${}", log.output_cost_usd),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_CACHE_READ_COST),
                        format!("${}", log.cache_read_cost_usd),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_CACHE_WRITE_COST),
                        format!("${}", log.cache_creation_cost_usd),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_TOTAL_COST),
                        format!("${}", log.total_cost_usd),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_MULTIPLIER),
                        format!("×{}", log.cost_multiplier),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_LATENCY),
                        format!("{}ms", log.latency_ms),
                    ))
                    .when_some(log.first_token_ms, |s, value| {
                        s.child(detail_cell(
                            raw(k::USAGE_DETAIL_FIRST_TOKEN),
                            format!("{value}ms"),
                        ))
                    })
                    .when_some(log.duration_ms, |s, value| {
                        s.child(detail_cell(
                            raw(k::USAGE_DETAIL_DURATION),
                            format!("{value}ms"),
                        ))
                    })
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_STATUS),
                        log.status_code.to_string(),
                    ))
                    .child(detail_cell(
                        raw(k::USAGE_DETAIL_SOURCE),
                        log.data_source.unwrap_or_else(|| "proxy".into()),
                    )),
            )
            .when_some(log.error_message, |s, err| {
                s.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(theme::error_surface())
                        .border_1()
                        .border_color(theme::red())
                        .text_color(theme::red())
                        .text_xs()
                        .child(SharedString::from(tf!(k::USAGE_DETAIL_ERROR, error = err))),
                )
            })
    }

    fn render_scope_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let provider_chips = self
            .providers
            .iter()
            .take(8)
            .map(|provider| {
                let name = provider.provider_name.clone();
                filter_chip(
                    ElementId::Name(format!("usage-provider-option-{name}").into()),
                    name.clone(),
                    self.provider_filter.as_deref() == Some(name.as_str()),
                    Some(IconName::Cloud),
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_provider_filter(Some(name.clone()), cx);
                }))
            })
            .collect::<Vec<_>>();
        let model_chips = self
            .models
            .iter()
            .take(8)
            .map(|model| {
                let name = model.model.clone();
                filter_chip(
                    ElementId::Name(format!("usage-model-option-{name}").into()),
                    name.clone(),
                    self.model_filter.as_deref() == Some(name.as_str()),
                    Some(IconName::Layers),
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.set_model_filter(Some(name.clone()), cx);
                }))
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(section_label(raw(k::USAGE_SCOPE_PROVIDER_LABEL)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .children(provider_chips),
            )
            .child(section_label(raw(k::USAGE_SCOPE_MODEL_LABEL)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .children(model_chips),
            )
    }

    fn render_pricing_config(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.pricing.len().min(12);
        let pricing_rows = self
            .pricing
            .iter()
            .take(12)
            .enumerate()
            .map(|(ix, item)| {
                let edit_item = item.clone();
                let delete_id = item.model_id.clone();
                components::table_row(
                    vec![
                        text_cell(item.model_id.clone()).into_any_element(),
                        text_cell(item.display_name.clone()).into_any_element(),
                        text_cell(format!("${}", item.input_cost_per_million)).into_any_element(),
                        text_cell(format!("${}", item.output_cost_per_million)).into_any_element(),
                        text_cell(format!("${}", item.cache_read_cost_per_million))
                            .into_any_element(),
                        text_cell(format!("${}", item.cache_creation_cost_per_million))
                            .into_any_element(),
                        components::icon_button_tone(
                            ElementId::Name(format!("pricing-edit-{}", item.model_id).into()),
                            t(k::USAGE_PRICING_EDIT),
                            IconName::Settings,
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.edit_pricing(edit_item.clone(), cx);
                        }))
                        .into_any_element(),
                        components::button(
                            ElementId::Name(format!("pricing-delete-{}", item.model_id).into()),
                            t(k::USAGE_PRICING_DELETE),
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.confirm_delete_pricing = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .into_any_element(),
                    ],
                    8,
                    ix + 1 == row_count,
                )
                .id(ElementId::Name(
                    format!("pricing-row-{}", item.model_id).into(),
                ))
            })
            .collect::<Vec<_>>();

        components::card()
            .gap_4()
            .child(section_label(raw(k::USAGE_PRICING_DEFAULTS_TITLE)))
            .child(Self::pricing_defaults_header())
            .children(
                PRICING_APPS
                    .iter()
                    .map(|app| self.render_pricing_default_row(app, cx)),
            )
            // 按钮包一层 row 保持内容宽——card 是纵列，直接放会被交叉轴拉满。
            .child(
                div().flex().flex_row().child(
                    components::icon_button_tone(
                        "usage-save-pricing-defaults",
                        t(k::USAGE_PRICING_DEFAULTS_SAVE),
                        IconName::Check,
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_pricing_defaults(cx);
                    })),
                ),
            )
            .child(section_label(raw(k::USAGE_PRICING_TABLE_TITLE)))
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_MODEL_ID),
                        false,
                        None,
                        self.pricing_model_id.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_DISPLAY_NAME),
                        false,
                        None,
                        self.pricing_display_name.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_INPUT),
                        false,
                        None,
                        self.pricing_input_cost.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_OUTPUT),
                        false,
                        None,
                        self.pricing_output_cost.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_CACHE_READ),
                        false,
                        None,
                        self.pricing_cache_read_cost.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_PRICING_FIELD_CACHE_WRITE),
                        false,
                        None,
                        self.pricing_cache_creation_cost.clone(),
                    )),
            )
            .child(
                div().flex().flex_row().child(
                    components::icon_button_tone(
                        "usage-save-pricing",
                        t(k::USAGE_PRICING_SAVE),
                        IconName::Check,
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_pricing(cx);
                    })),
                ),
            )
            .child(
                components::card()
                    .p_0()
                    .child(table_title(
                        raw(k::USAGE_PRICING_LIST_TITLE),
                        t(k::USAGE_PRICING_LIST_SUBTITLE),
                    ))
                    // The last two columns hold the edit and delete buttons,
                    // which label themselves.
                    .child(components::table_header(&[
                        raw(k::USAGE_PRICING_COL_MODEL_ID),
                        raw(k::USAGE_PRICING_COL_DISPLAY_NAME),
                        raw(k::USAGE_PRICING_COL_INPUT),
                        raw(k::USAGE_PRICING_COL_OUTPUT),
                        raw(k::USAGE_PRICING_COL_CACHE_READ),
                        raw(k::USAGE_PRICING_COL_CACHE_WRITE),
                        "",
                        "",
                    ]))
                    .when(pricing_rows.is_empty(), |s| {
                        s.child(components::empty_state(
                            IconName::Diamond,
                            t(k::USAGE_EMPTY_NO_DATA),
                            t(k::USAGE_PRICING_EMPTY_HINT),
                            None,
                        ))
                    })
                    .children(pricing_rows),
            )
    }

    /// 计费默认配置的列标签行：标签只出现一次（表头式），下方每行不再重复。
    /// 列宽与 [`Self::render_pricing_default_row`] 保持一致。
    fn pricing_defaults_header() -> impl IntoElement {
        let header_cell = |label: &'static str| {
            div()
                .text_color(theme::subtext())
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .child(label)
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(header_cell(raw(k::USAGE_PRICING_DEFAULTS_COL_APP)).w(px(100.)))
            .child(header_cell(raw(k::USAGE_PRICING_DEFAULTS_COL_MULTIPLIER)).w(px(96.)))
            .child(header_cell(raw(k::USAGE_PRICING_DEFAULTS_COL_SOURCE)))
    }

    fn render_pricing_default_row(
        &self,
        app: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let source = self
            .pricing_sources
            .get(app)
            .map(String::as_str)
            .unwrap_or("response");
        let input = match app {
            "claude" => self.multiplier_claude.clone(),
            "codex" => self.multiplier_codex.clone(),
            _ => self.multiplier_claude.clone(),
        };
        let on_source = cx.listener(move |this, ix: &usize, _window, cx| {
            this.set_pricing_source(app, if *ix == 0 { "response" } else { "request" }, cx);
        });

        // 无标签的整齐三列（列标签在 pricing_defaults_header），行内元素同高居中。
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(100.))
                    .min_w_0()
                    .text_color(theme::text())
                    .text_sm()
                    .truncate()
                    .child(SharedString::from(app_label(app))),
            )
            .child(div().w(px(96.)).child(input))
            .child(components::segmented(
                SharedString::from(format!("pricing-source-{app}")),
                &[
                    raw(k::USAGE_PRICING_SOURCE_RESPONSE),
                    raw(k::USAGE_PRICING_SOURCE_REQUEST),
                ],
                if source == "response" { 0 } else { 1 },
                move |ix, window, cx| on_source(&ix, window, cx),
            ))
    }

    fn render_stream_config(&self, cx: &mut Context<Self>) -> impl IntoElement {
        components::card()
            .gap_4()
            .child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(theme::surface_hover())
                    .text_color(theme::subtext())
                    .text_xs()
                    .child(t(k::USAGE_STREAM_NOTICE)),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(components::field(
                        t(k::USAGE_STREAM_FIELD_TIMEOUT),
                        false,
                        None,
                        self.stream_timeout_secs.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_STREAM_FIELD_RETRIES),
                        false,
                        None,
                        self.stream_max_retries.clone(),
                    ))
                    .child(components::field(
                        t(k::USAGE_STREAM_FIELD_THRESHOLD),
                        false,
                        None,
                        self.stream_degraded_threshold_ms.clone(),
                    )),
            )
            .child(
                div().flex().flex_row().child(
                    components::icon_button_tone(
                        "usage-save-stream-config",
                        t(k::USAGE_STREAM_SAVE),
                        IconName::Check,
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_stream_config(cx);
                    })),
                ),
            )
    }

    /// Render one top-level page block as a virtualized list item. Only the
    /// on-screen blocks (plus overdraw) are built each frame — see
    /// [`crate::layout::wide_virtual_body`]. Each item carries its own bottom
    /// spacing (the list draws no inter-item gap).
    fn render_block(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let block = div().w_full().pb_4();
        match ix {
            0 => block.child(self.render_filters(cx)).into_any_element(),
            1 => block.child(self.render_data_sources(cx)).into_any_element(),
            2 => block.child(self.render_summary()).into_any_element(),
            3 => block
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    components::disclosure(
                        "usage-trend-toggle",
                        t(k::USAGE_TREND_TOGGLE_TITLE),
                        tf!(k::USAGE_TREND_TOGGLE_DETAIL, count = self.daily.len()),
                        self.show_trend,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_trend = !this.show_trend;
                        this.list_state.remeasure();
                        cx.notify();
                    })),
                )
                .when(self.show_trend, |s| s.child(self.render_trend(cx)))
                .into_any_element(),
            4 => block
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    components::disclosure(
                        "usage-scope-toggle",
                        t(k::USAGE_SCOPE_TOGGLE_TITLE),
                        t(k::USAGE_SCOPE_TOGGLE_DETAIL),
                        self.show_scope_options,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_scope_options = !this.show_scope_options;
                        this.list_state.remeasure();
                        cx.notify();
                    })),
                )
                .when(self.show_scope_options, |s| {
                    s.child(self.render_scope_options(cx))
                })
                .into_any_element(),
            5 => block.child(self.render_section_tabs(cx)).into_any_element(),
            6 => block
                .child(self.render_active_section(cx))
                .into_any_element(),
            7 => block
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    components::disclosure(
                        "usage-pricing-toggle",
                        t(k::USAGE_PRICING_TOGGLE_TITLE),
                        tf!(k::USAGE_PRICING_TOGGLE_DETAIL, count = self.pricing.len()),
                        self.show_pricing,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_pricing = !this.show_pricing;
                        this.list_state.remeasure();
                        cx.notify();
                    })),
                )
                .when(self.show_pricing, |s| {
                    s.child(self.render_pricing_config(cx))
                })
                .into_any_element(),
            8 => block
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    components::disclosure(
                        "usage-stream-toggle",
                        t(k::USAGE_STREAM_TOGGLE_TITLE),
                        tf!(
                            k::USAGE_STREAM_TOGGLE_DETAIL,
                            timeout = self.stream_config.timeout_secs,
                            retries = self.stream_config.max_retries,
                            threshold = self.stream_config.degraded_threshold_ms
                        ),
                        self.show_stream_config,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_stream_config = !this.show_stream_config;
                        this.list_state.remeasure();
                        cx.notify();
                    })),
                )
                .when(self.show_stream_config, |s| {
                    s.child(self.render_stream_config(cx))
                })
                .into_any_element(),
            _ => gpui::Empty.into_any_element(),
        }
    }
}

impl Render for UsageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        layout::page()
            .relative()
            .child(
                layout::page_header(t(k::USAGE_HEADER_TITLE), Some(t(k::USAGE_HEADER_SUBTITLE)))
                    .child(
                        components::icon_button_tone(
                            "usage-refresh",
                            t(k::USAGE_HEADER_REFRESH),
                            IconName::Refresh,
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.reload(cx);
                            },
                        )),
                    ),
            )
            .child(layout::wide_virtual_body(
                "usage-body",
                gpui::list(
                    self.list_state.clone(),
                    cx.processor(|this, ix, window, cx| this.render_block(ix, window, cx)),
                ),
                &self.list_state,
            ))
            .when_some(self.confirm_delete_pricing.clone(), |root, model_id| {
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::USAGE_CONFIRM_DELETE_TITLE)))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(tf!(
                                        k::USAGE_CONFIRM_DELETE_MESSAGE,
                                        name = model_id
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "usage-confirm-delete-cancel",
                                t(k::USAGE_CONFIRM_DELETE_CANCEL),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete_pricing = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "usage-confirm-delete-ok",
                                t(k::USAGE_CONFIRM_DELETE_CONFIRM),
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete_pricing = None;
                                this.delete_pricing(model_id.clone(), cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

fn range_filter_label(range: UsageRange) -> String {
    let (Some(start), Some(end)) = range.bounds() else {
        return range.label().to_string();
    };
    format!(
        "{} ~ {}",
        format_local_timestamp(start, false),
        format_local_timestamp(end, false)
    )
}

fn shifted_year_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let absolute_month = year * 12 + month as i32 - 1 + delta;
    (
        absolute_month.div_euclid(12),
        absolute_month.rem_euclid(12) as u32 + 1,
    )
}

fn parse_local_timestamp(value: &str, end_of_day: bool) -> Option<i64> {
    let normalized = value.trim().replace('/', "-");
    for pattern in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(&normalized, pattern) {
            return Local
                .from_local_datetime(&value)
                .earliest()
                .map(|time| time.timestamp());
        }
    }

    let date = components::parse_jump_date(&normalized)?;
    let value = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Local
        .from_local_datetime(&value)
        .earliest()
        .map(|time| time.timestamp())
}

fn filter_trigger(
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
        .hover(|s| s.border_color(theme::accent()).bg(theme::panel()))
        .child(icon(icon_name, theme::muted(), 15.))
        .child(div().min_w_0().flex_1().truncate().child(label))
        .child(icon(IconName::ChevronDown, theme::muted(), 13.))
}

fn filter_popover_panel(id: &'static str, width: f32) -> gpui::Stateful<gpui::Div> {
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

fn filter_section_label(label: &'static str) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::muted())
        .child(label)
}

fn quick_range_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_selected(selected)
        .h(px(34.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(label);
    if selected {
        button.bg(theme::accent_soft()).text_color(theme::accent())
    } else {
        button
            .bg(theme::panel())
            .text_color(theme::subtext())
            .hover(|s| s.bg(theme::surface_hover()).text_color(theme::text()))
    }
}

/// The month as the calendar header spells it: a name in English, digits in
/// Chinese and Japanese. Mirrors `components::datetime_picker`, which owns the
/// picker this legacy one is kept alongside.
fn picker_month_label(month: u32) -> String {
    match month {
        1 => raw(k::COMMON_CALENDAR_MONTH_NAME_01),
        2 => raw(k::COMMON_CALENDAR_MONTH_NAME_02),
        3 => raw(k::COMMON_CALENDAR_MONTH_NAME_03),
        4 => raw(k::COMMON_CALENDAR_MONTH_NAME_04),
        5 => raw(k::COMMON_CALENDAR_MONTH_NAME_05),
        6 => raw(k::COMMON_CALENDAR_MONTH_NAME_06),
        7 => raw(k::COMMON_CALENDAR_MONTH_NAME_07),
        8 => raw(k::COMMON_CALENDAR_MONTH_NAME_08),
        9 => raw(k::COMMON_CALENDAR_MONTH_NAME_09),
        10 => raw(k::COMMON_CALENDAR_MONTH_NAME_10),
        11 => raw(k::COMMON_CALENDAR_MONTH_NAME_11),
        12 => raw(k::COMMON_CALENDAR_MONTH_NAME_12),
        // Unreachable for a real date; keeps the pre-catalog `{month:02}`.
        other => return format!("{other:02}"),
    }
    .to_string()
}

fn calendar_nav_button(
    id: impl Into<ElementId>,
    label: &'static str,
    icon_name: IconName,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(theme::surface_hover()))
        .child(icon(icon_name, theme::subtext(), 14.))
}

fn calendar_day_button(
    id: impl Into<ElementId>,
    date: NaiveDate,
    in_current_month: bool,
    selected: bool,
    today: bool,
) -> gpui::Stateful<gpui::Div> {
    let day = date.day();
    let mut button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(tf!(
            k::COMMON_CALENDAR_DAY_ARIA,
            year = date.year(),
            month = date.month(),
            day = day
        )))
        .aria_selected(selected)
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(theme::surface().alpha(0.))
        .cursor_pointer()
        .text_sm()
        .child(SharedString::from(day.to_string()));
    if selected {
        button = button
            .bg(theme::accent_fill())
            .border_color(theme::accent())
            .text_color(theme::accent_text())
            .font_weight(FontWeight::SEMIBOLD);
    } else if today {
        button = button
            .border_color(theme::accent())
            .text_color(theme::accent())
            .font_weight(FontWeight::MEDIUM)
            .hover(|s| s.bg(theme::accent_soft()));
    } else if in_current_month {
        button = button
            .text_color(theme::text())
            .hover(|s| s.bg(theme::surface_hover()));
    } else {
        button = button
            .text_color(theme::muted())
            .hover(|s| s.bg(theme::surface_hover()).text_color(theme::subtext()));
    }
    button
}

fn calendar_footer_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .px_1()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::accent())
        .hover(|s| s.bg(theme::accent_soft()))
        .child(label)
}

fn time_column_label(label: &'static str) -> gpui::Div {
    div()
        .h(px(42.))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::muted())
        .child(label)
}

fn time_value_button(
    id: impl Into<ElementId>,
    value: u32,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(format!("{value:02}")))
        .aria_selected(selected)
        .w_full()
        .h(px(34.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_sm()
        .child(SharedString::from(format!("{value:02}")));
    if selected {
        button
            .bg(theme::accent_fill())
            .text_color(theme::accent_text())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        button
            .text_color(theme::text())
            .hover(|s| s.bg(theme::surface_hover()))
    }
}

fn dropdown_option(
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
            .hover(|s| s.bg(theme::surface_hover()).text_color(theme::text()))
    }
}

fn text_input(cx: &mut Context<TextInput>, placeholder: &str, value: &str) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn input_value(input: &Entity<TextInput>, cx: &mut Context<UsageView>) -> String {
    input.read(cx).content().trim().to_string()
}

fn set_input(
    input: &Entity<TextInput>,
    value: impl Into<SharedString>,
    cx: &mut Context<UsageView>,
) {
    input.update(cx, |input, cx| input.set_content(value, cx));
}

fn parse_stream_config(
    this: &UsageView,
    cx: &mut Context<UsageView>,
) -> Result<StreamCheckConfig, String> {
    Ok(StreamCheckConfig {
        timeout_secs: input_value(&this.stream_timeout_secs, cx)
            .parse::<u64>()
            .map_err(|_| raw(k::USAGE_STREAM_ERROR_TIMEOUT).to_string())?,
        max_retries: input_value(&this.stream_max_retries, cx)
            .parse::<u32>()
            .map_err(|_| raw(k::USAGE_STREAM_ERROR_RETRIES).to_string())?,
        degraded_threshold_ms: input_value(&this.stream_degraded_threshold_ms, cx)
            .parse::<u64>()
            .map_err(|_| raw(k::USAGE_STREAM_ERROR_THRESHOLD).to_string())?,
    })
}

/// 表格标题块：卡片顶部的标题 + 说明（配合 `components::card().p_0()` 使用）。
fn table_title(title: &'static str, subtitle: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py_3()
        .child(
            div()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(subtitle.into()),
        )
}

/// 纯文本表格单元格（等宽 grid 轨道内截断）。
fn text_cell(value: impl Into<SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .text_color(theme::text())
        .text_xs()
        .truncate()
        .child(value.into())
}

/// 多选 filter chip：segmented item 观感——未选中 muted 文字 + INSET 底，
/// 选中为 surface + shadow_xs 浮起。行为（点击切换）由调用点接线。
fn filter_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    icon_name: Option<IconName>,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let mut chip = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .aria_selected(selected)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .border_1();
    if selected {
        chip = chip
            .bg(theme::surface())
            .border_color(theme::border())
            .shadow_xs()
            .text_color(theme::text())
            .font_weight(FontWeight::MEDIUM);
    } else {
        chip = chip
            .bg(theme::inset())
            .border_color(theme::border())
            .text_color(theme::muted())
            .hover(|s| s.bg(theme::surface_hover()).text_color(theme::subtext()));
    }
    chip.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .when_some(icon_name, |s, icon_name| {
                s.child(icon(
                    icon_name,
                    if selected {
                        theme::subtext()
                    } else {
                        theme::muted()
                    },
                    13.,
                ))
            })
            .child(label),
    )
}

fn detail_cell(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(div().text_color(theme::muted()).text_xs().child(label))
        .child(
            div()
                .text_color(theme::text())
                .text_xs()
                .truncate()
                .child(value.into()),
        )
}

fn section_label(label: &'static str) -> impl IntoElement {
    div()
        .text_color(theme::text())
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}

fn format_money(value: &str, digits: usize) -> String {
    value
        .parse::<f64>()
        .map(|number| format!("{number:.digits$}"))
        .unwrap_or_else(|_| value.to_string())
}

/// 悬停提示里的时间桶标签：小时桶（RFC3339）显示 “MM-DD HH:00”，天桶显示 “MM-DD”。
fn trend_bucket_label(value: &str) -> String {
    match value.split_once('T') {
        Some((date, time)) if time.len() >= 2 => {
            format!("{} {}:00", short_date_label(date), &time[..2])
        }
        _ => short_date_label(value),
    }
}

fn short_date_label(value: &str) -> String {
    value
        .split('T')
        .next()
        .unwrap_or(value)
        .trim_start_matches("2026-")
        .trim_start_matches("2025-")
        .to_string()
}

fn short_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn full_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn app_label(app_type: &str) -> String {
    match app_type {
        "claude" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        "gemini" => "Gemini".to_string(),
        "opencode" => "OpenCode".to_string(),
        other => other.to_string(),
    }
}

fn app_tone(app_type: &str) -> gpui::Rgba {
    match app_type {
        "claude" => theme::peach(),
        "codex" => theme::text(),
        "gemini" => theme::teal(),
        "opencode" => theme::mauve(),
        _ => theme::accent(),
    }
}

/// The stored `data_source` ids are matched as identifiers; only the label
/// shown next to them is translated, and an unknown id falls back to itself.
fn data_source_label(source: &str) -> String {
    match source {
        "gateway" => raw(k::USAGE_DATA_SOURCE_GATEWAY),
        "proxy" => raw(k::USAGE_DATA_SOURCE_PROXY),
        "session_log" => raw(k::USAGE_DATA_SOURCE_SESSION_LOG),
        "codex_db" => raw(k::USAGE_DATA_SOURCE_CODEX_DB),
        "codex_session" => raw(k::USAGE_DATA_SOURCE_CODEX_SESSION),
        "gemini_session" => raw(k::USAGE_DATA_SOURCE_GEMINI_SESSION),
        "opencode_session" => raw(k::USAGE_DATA_SOURCE_OPENCODE_SESSION),
        other => other,
    }
    .to_string()
}

fn data_source_icon(source: &str) -> IconName {
    match source {
        "gateway" | "proxy" | "codex_db" => IconName::Cloud,
        "session_log" | "codex_session" | "gemini_session" | "opencode_session" => IconName::Folder,
        _ => IconName::Blocks,
    }
}

fn fresh_input_tokens(log: &RequestLogDetail) -> u32 {
    if matches!(log.app_type.as_str(), "codex" | "gemini")
        && log.input_tokens >= log.cache_read_tokens
    {
        log.input_tokens - log.cache_read_tokens
    } else {
        log.input_tokens
    }
}

fn effective_model_label(log: &RequestLogDetail) -> String {
    match (&log.request_model, &log.pricing_model) {
        (Some(request), Some(pricing))
            if !request.is_empty() && !pricing.is_empty() && request != pricing =>
        {
            format!("{request} -> {pricing}")
        }
        (Some(request), _) if !request.is_empty() && request != &log.model => {
            format!("{request} -> {}", log.model)
        }
        (_, Some(pricing)) if !pricing.is_empty() && pricing != &log.model => {
            format!("{} -> {pricing}", log.model)
        }
        _ => log.model.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_local_timestamp, parse_local_timestamp, shifted_year_month};

    #[test]
    fn parses_reference_datetime_formats() {
        for value in [
            "2026/07/21 18:19:00",
            "2026-07-21 18:19:00",
            "2026/07/21 18:19",
            "2026-07-21T18:19",
        ] {
            let timestamp = parse_local_timestamp(value, false).expect("valid local timestamp");
            assert_eq!(
                format_local_timestamp(timestamp, true),
                "2026/07/21 18:19:00"
            );
        }
    }

    #[test]
    fn date_only_values_expand_to_day_boundaries() {
        let start = parse_local_timestamp("2026-07-21", false).expect("valid start date");
        let end = parse_local_timestamp("2026-07-21", true).expect("valid end date");

        assert_eq!(format_local_timestamp(start, true), "2026/07/21 00:00:00");
        assert_eq!(format_local_timestamp(end, true), "2026/07/21 23:59:59");
        assert!(start < end);
    }

    #[test]
    fn rejects_invalid_datetimes() {
        assert!(parse_local_timestamp("2026-02-30 12:00:00", false).is_none());
        assert!(parse_local_timestamp("not a time", false).is_none());
    }

    #[test]
    fn month_navigation_crosses_year_boundaries() {
        assert_eq!(shifted_year_month(2026, 1, -1), (2025, 12));
        assert_eq!(shifted_year_month(2026, 12, 1), (2027, 1));
        assert_eq!(shifted_year_month(2026, 7, 18), (2028, 1));
    }
}

crate::notifications::impl_status_toasts_leveled!(UsageView);
