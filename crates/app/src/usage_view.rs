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

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::{icon, IconName};
use crate::layout;
use crate::text_input::TextInput;
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
                errors.push(format!("加载用量失败: {err}"));
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
            errors.push(format!("加载请求日志失败: {err}"));
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
    fn all() -> &'static [(Self, &'static str)] {
        &[
            (Self::Today, "今天"),
            (Self::SevenDays, "最近 1 周"),
            (Self::ThirtyDays, "最近 30 天"),
        ]
    }

    fn label(self) -> &'static str {
        if matches!(self, Self::Custom { .. }) {
            return "自定义";
        }
        Self::all()
            .iter()
            .find_map(|(range, label)| (*range == self).then_some(*label))
            .unwrap_or("今天")
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
    fn all() -> &'static [(Self, &'static str, IconName)] {
        &[
            (Self::Logs, "请求日志", IconName::Message),
            (Self::Providers, "Provider 统计", IconName::Cloud),
            (Self::Models, "模型统计", IconName::Chart),
        ]
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
            log_page_input: cx.new(|cx| text_input(cx, "页码", "").compact()),
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
                self.status = None;
                self.load_error = false;
            }
        } else {
            self.status = Some(SharedString::from(data.errors.join("；")));
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
            self.status = Some(SharedString::from(
                "开始时间格式不正确，请使用 YYYY/MM/DD HH:mm:ss",
            ));
            cx.notify();
            return;
        };
        let Some(end) = parse_local_timestamp(&end_text, true) else {
            self.status = Some(SharedString::from(
                "结束时间格式不正确，请使用 YYYY/MM/DD HH:mm:ss",
            ));
            cx.notify();
            return;
        };
        if start > end {
            self.status = Some(SharedString::from("开始时间不能晚于结束时间"));
            cx.notify();
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
                        this.status = None;
                    }
                    Err(err) => {
                        this.selected_log = None;
                        this.status = Some(SharedString::from(format!("读取请求详情失败: {err}")));
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
        self.status = Some(SharedString::from("正在同步会话…"));
        cx.notify();
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
                this.status = Some(SharedString::from(format!(
                    "会话同步完成: 导入 {} 条，跳过 {} 条，扫描 {} 个文件{}",
                    result.imported,
                    result.skipped,
                    result.files_scanned,
                    if result.errors.is_empty() {
                        String::new()
                    } else {
                        format!("，{} 个错误", result.errors.len())
                    }
                )));
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
                self.status = Some(SharedString::from(format!("已保存模型定价: {model_id}")));
                self.reload(cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("保存模型定价失败: {err}")));
            }
        }
        cx.notify();
    }

    fn delete_pricing(&mut self, model_id: String, cx: &mut Context<Self>) {
        match self.app.db.delete_model_pricing(&model_id) {
            Ok(()) => {
                self.status = Some(SharedString::from(format!("已删除模型定价: {model_id}")));
                self.reload(cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("删除模型定价失败: {err}")));
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
                this.status = Some(SharedString::from(match result {
                    Ok(()) => "计费默认配置已保存".to_string(),
                    Err(err) => format!("保存计费默认配置失败: {err}"),
                }));
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
                self.status = Some(SharedString::from(err));
                cx.notify();
                return;
            }
        };

        match self.app.db.save_stream_check_config(&config) {
            Ok(()) => {
                self.stream_config = config;
                self.status = Some(SharedString::from("模型检测参数已保存"));
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("保存检测参数失败: {err}")));
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
        for weekday in ["日", "一", "二", "三", "四", "五", "六"] {
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
                            .child(SharedString::from(format!(
                                "{}年{:02}月",
                                self.picker_year, self.picker_month
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
                                    "上个月",
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
                                    "下个月",
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
                        calendar_footer_button("usage-picker-clear", "清除").on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.clear_picker_value(endpoint, cx);
                            },
                        )),
                    )
                    .child(
                        calendar_footer_button("usage-picker-today", "今天").on_click(cx.listener(
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
            .track_scroll(&self.picker_hour_scroll);
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
            .track_scroll(&self.picker_minute_scroll);
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
                    .child(time_column_label("时"))
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
                    .child(time_column_label("分"))
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
        const STATUS_FILTERS: [(Option<u16>, &str); 6] = [
            (None, "全部状态"),
            (Some(200), "200"),
            (Some(400), "400"),
            (Some(401), "401"),
            (Some(429), "429"),
            (Some(500), "500"),
        ];

        let time_open = self.open_filter_popover == Some(FilterPopover::Time);
        let mut quick_ranges = div().flex().flex_row().flex_wrap().gap_2();
        for (ix, (range, label)) in UsageRange::all().iter().enumerate() {
            let range = *range;
            quick_ranges = quick_ranges.child(
                quick_range_button(
                    ElementId::Name(format!("usage-quick-range-{ix}").into()),
                    *label,
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
                    "开始时间",
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
                    "结束时间",
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
            .child(filter_section_label("快捷选择"))
            .child(quick_ranges)
            .child(div().w_full().h(px(1.)).bg(theme::border()))
            .child(filter_section_label("自定义范围"))
            .child(start_datetime_control)
            .child(end_datetime_control)
            .child(
                components::button(
                    "usage-apply-range",
                    "确定",
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
            .child(
                dropdown_option(
                    "usage-provider-all",
                    "全部 Provider",
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
                    .child("当前时间范围内暂无 Provider"),
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
                        .unwrap_or_else(|| "全部 Provider".to_string()),
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
            .child(
                dropdown_option("usage-model-all", "全部模型", self.model_filter.is_none())
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
                    .child("当前时间范围内暂无模型"),
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
                        .unwrap_or_else(|| "全部模型".to_string()),
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
        for (ix, (status, label)) in STATUS_FILTERS.iter().enumerate() {
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
        let status_label = STATUS_FILTERS
            .iter()
            .find_map(|(status, label)| (self.status_filter == *status).then_some(*label))
            .unwrap_or("全部状态");
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
                            "重置",
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
        let sources = self
            .data_sources
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
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(format!("{} 次", source.request_count))),
                    )
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
                            .child("数据源"),
                    )
                    .children(sources),
            )
            .child(
                components::icon_button_tone(
                    "usage-sync-sessions",
                    "同步会话",
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
                    "总请求数",
                    summary.total_requests.to_string(),
                    format!("成功率 {:.1}%", summary.success_rate),
                ))
                .child(components::stat_tile(
                    Some(IconName::Diamond),
                    theme::peach(),
                    "总成本",
                    format!("${}", format_money(&summary.total_cost, 6)),
                    format!(
                        "输入 {} / 输出 {}",
                        summary.total_input_tokens, summary.total_output_tokens
                    ),
                ))
                .child(components::stat_tile(
                    Some(IconName::Layers),
                    theme::accent(),
                    "真实 Token",
                    summary.real_total_tokens.to_string(),
                    format!(
                        "缓存创建 {} / 读取 {}",
                        summary.total_cache_creation_tokens, summary.total_cache_read_tokens
                    ),
                ))
                .child(components::stat_tile(
                    Some(IconName::Cloud),
                    theme::teal(),
                    "缓存命中",
                    format!("{cache_hit_rate:.1}%"),
                    "重复输入复用比例".to_string(),
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
                                SharedString::from(format!(
                                    "{} 次 · ${}",
                                    item.summary.total_requests,
                                    format_money(&item.summary.total_cost, 4)
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
                            .child("按应用拆分"),
                    ),
            )
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Layers,
                    "还没有用量",
                    "当前筛选范围内还没有用量。",
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
                SharedString::from(format!(
                    "{} · ${} · {} 次",
                    trend_bucket_label(&stat.date),
                    format_money(&stat.total_cost, 4),
                    stat.request_count
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
                                    .child("使用趋势"),
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
                        .child("当前范围内趋势数据不足（至少需要两个时间桶）。"),
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
                            "累计成本",
                            format_money(&format!("{total_cost}"), 3),
                        ))
                        .child(div().w(px(1.)).h(px(26.)).bg(theme::border()))
                        .child(Self::trend_stat(
                            "单桶峰值",
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
            .map(|(_, label, _)| *label)
            .collect();
        let selected = UsageSection::all()
            .iter()
            .position(|(section, _, _)| *section == self.section)
            .unwrap_or(0);
        let on_select = cx.listener(|this, ix: &usize, _window, cx| {
            if let Some((section, _, _)) = UsageSection::all().get(*ix) {
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
                .aria_label(SharedString::from(format!("筛选 Provider {provider}")))
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
                "Provider 统计",
                "点击行会把整个面板缩小到该 Provider 的统计口径。",
            ))
            .child(components::table_header(&[
                "Provider",
                "请求",
                "Token",
                "成本",
                "成功率",
                "均延迟",
            ]))
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Cloud,
                    "暂无数据",
                    "当前筛选范围内没有 Provider 统计。",
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
                .aria_label(SharedString::from(format!("筛选模型 {model}")))
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
                "模型统计",
                "模型按有效计价模型聚合，价格与请求详情保持同一口径。",
            ))
            .child(components::table_header(&[
                "模型",
                "请求",
                "Token",
                "总成本",
                "均成本",
            ]))
            .when(rows.is_empty(), |s| {
                s.child(components::empty_state(
                    IconName::Chart,
                    "暂无数据",
                    "当前筛选范围内没有模型统计。",
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
                        text_cell(format!(
                            "入 {} / 出 {}",
                            fresh_input_tokens(log),
                            log.output_tokens
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
                .aria_label(SharedString::from(format!(
                    "查看请求详情 {} {}",
                    log.model, log.status_code
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
                        "请求日志",
                        format!(
                            "第 {} / {} 页 · 共 {} 条",
                            page + 1,
                            total_pages,
                            self.log_total
                        ),
                    ))
                    .child(components::table_header(&[
                        "时间 / 来源",
                        "计价模型",
                        "Token",
                        "成本 / 延迟",
                        "状态",
                    ]))
                    .when(rows.is_empty(), |s| {
                        s.child(components::empty_state(
                            IconName::Message,
                            "暂无数据",
                            "当前筛选范围内没有请求日志。",
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
                            .child("请求详情"),
                    ),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(detail_cell("请求 ID", log.request_id.clone()))
                    .child(detail_cell("时间", full_time(log.created_at)))
                    .child(detail_cell("应用", log.app_type.clone()))
                    .child(detail_cell(
                        "Provider",
                        format!("{provider} · {}", log.provider_id.clone()),
                    ))
                    .child(detail_cell("模型", log.model.clone()))
                    .when_some(log.request_model.clone(), |s, value| {
                        s.child(detail_cell("请求模型", value))
                    })
                    .when_some(log.pricing_model.clone(), |s, value| {
                        s.child(detail_cell("计价模型", value))
                    })
                    .child(detail_cell(
                        "输入 Token",
                        fresh_input_tokens(&log).to_string(),
                    ))
                    .child(detail_cell("输出 Token", log.output_tokens.to_string()))
                    .child(detail_cell("缓存读取", log.cache_read_tokens.to_string()))
                    .child(detail_cell(
                        "缓存写入",
                        log.cache_creation_tokens.to_string(),
                    ))
                    .child(detail_cell("输入成本", format!("${}", log.input_cost_usd)))
                    .child(detail_cell("输出成本", format!("${}", log.output_cost_usd)))
                    .child(detail_cell(
                        "缓存读取成本",
                        format!("${}", log.cache_read_cost_usd),
                    ))
                    .child(detail_cell(
                        "缓存写入成本",
                        format!("${}", log.cache_creation_cost_usd),
                    ))
                    .child(detail_cell("总成本", format!("${}", log.total_cost_usd)))
                    .child(detail_cell("成本倍率", format!("×{}", log.cost_multiplier)))
                    .child(detail_cell("延迟", format!("{}ms", log.latency_ms)))
                    .when_some(log.first_token_ms, |s, value| {
                        s.child(detail_cell("首 Token", format!("{value}ms")))
                    })
                    .when_some(log.duration_ms, |s, value| {
                        s.child(detail_cell("持续时间", format!("{value}ms")))
                    })
                    .child(detail_cell("状态", log.status_code.to_string()))
                    .child(detail_cell(
                        "来源",
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
                        .child(SharedString::from(format!("错误信息: {err}"))),
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
            .child(section_label("Provider 快捷筛选"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .children(provider_chips),
            )
            .child(section_label("模型快捷筛选"))
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
                            "编辑",
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
                            "删除",
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
            .child(section_label("计费默认配置"))
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
                        "保存计费默认配置",
                        IconName::Check,
                        ButtonTone::Primary,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.save_pricing_defaults(cx);
                    })),
                ),
            )
            .child(section_label("模型定价表"))
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(components::field(
                        "模型 ID",
                        false,
                        None,
                        self.pricing_model_id.clone(),
                    ))
                    .child(components::field(
                        "显示名称",
                        false,
                        None,
                        self.pricing_display_name.clone(),
                    ))
                    .child(components::field(
                        "输入 / 百万",
                        false,
                        None,
                        self.pricing_input_cost.clone(),
                    ))
                    .child(components::field(
                        "输出 / 百万",
                        false,
                        None,
                        self.pricing_output_cost.clone(),
                    ))
                    .child(components::field(
                        "缓存读 / 百万",
                        false,
                        None,
                        self.pricing_cache_read_cost.clone(),
                    ))
                    .child(components::field(
                        "缓存写 / 百万",
                        false,
                        None,
                        self.pricing_cache_creation_cost.clone(),
                    )),
            )
            .child(
                div().flex().flex_row().child(
                    components::icon_button_tone(
                        "usage-save-pricing",
                        "保存模型定价",
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
                        "已有模型定价",
                        "最多显示前 12 条；可编辑后保存，也可删除。",
                    ))
                    .child(components::table_header(&[
                        "模型 ID",
                        "显示名称",
                        "输入",
                        "输出",
                        "读缓存",
                        "写缓存",
                        "",
                        "",
                    ]))
                    .when(pricing_rows.is_empty(), |s| {
                        s.child(components::empty_state(
                            IconName::Diamond,
                            "暂无数据",
                            "还没有已保存的模型定价。",
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
            .child(header_cell("应用").w(px(100.)))
            .child(header_cell("默认倍率").w(px(96.)))
            .child(header_cell("计价模型来源"))
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
                &["按响应模型计费", "按请求模型计费"],
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
                    .child("连通检测只确认供应商地址可达；收到任意响应即视为可达，不代表鉴权或模型配置一定正确。"),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(3)
                    .gap_3()
                    .child(components::field(
                        "探测超时（秒）",
                        false,
                        None,
                        self.stream_timeout_secs.clone(),
                    ))
                    .child(components::field(
                        "最大重试次数",
                        false,
                        None,
                        self.stream_max_retries.clone(),
                    ))
                    .child(components::field(
                        "降级阈值（毫秒）",
                        false,
                        None,
                        self.stream_degraded_threshold_ms.clone(),
                    )),
            )
            .child(
                div().flex().flex_row().child(
                    components::icon_button_tone(
                        "usage-save-stream-config",
                        "保存检测参数",
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
                        "趋势图",
                        format!("{} 个时间桶 · Token 与成本变化", self.daily.len()),
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
                        "Provider / 模型候选",
                        "从当前范围内真实有数据的条目里快速筛选。",
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
                        "模型定价配置",
                        format!("{} 条定价 · 支持默认倍率和计价模型来源", self.pricing.len()),
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
                        "模型检测参数",
                        format!(
                            "超时 {}s · 重试 {} · 降级阈值 {}ms",
                            self.stream_config.timeout_secs,
                            self.stream_config.max_retries,
                            self.stream_config.degraded_threshold_ms
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
                layout::page_header(
                    "用量",
                    Some("模型、成本、缓存、请求日志、定价与检测配置。".into()),
                )
                .child(
                    components::icon_button_tone(
                        "usage-refresh",
                        "刷新",
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.reload(cx);
                    })),
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
                        .child(components::modal_header("删除模型定价"))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(format!(
                                        "确定删除模型定价「{model_id}」吗？此操作不可撤销。"
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "usage-confirm-delete-cancel",
                                "取消",
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
                                "删除",
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

fn format_local_timestamp(timestamp: i64, with_seconds: bool) -> String {
    let pattern = if with_seconds {
        "%Y/%m/%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format(pattern).to_string())
        .unwrap_or_else(|| "时间无效".to_string())
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
        .aria_label(SharedString::from(format!(
            "{}年{}月{}日",
            date.year(),
            date.month(),
            day
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
            .map_err(|_| "探测超时必须是非负数字".to_string())?,
        max_retries: input_value(&this.stream_max_retries, cx)
            .parse::<u32>()
            .map_err(|_| "最大重试次数必须是非负数字".to_string())?,
        degraded_threshold_ms: input_value(&this.stream_degraded_threshold_ms, cx)
            .parse::<u64>()
            .map_err(|_| "降级阈值必须是非负数字".to_string())?,
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

fn data_source_label(source: &str) -> String {
    match source {
        "gateway" => "转发站请求",
        "proxy" => "旧版本地请求",
        "session_log" => "Claude 会话",
        "codex_db" => "Codex 数据库",
        "codex_session" => "Codex 会话",
        "gemini_session" => "Gemini 会话",
        "opencode_session" => "OpenCode 会话",
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

crate::notifications::impl_status_toasts!(UsageView);
