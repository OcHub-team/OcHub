//! Usage statistics workbench. Mirrors the reference cc-switch dashboard while
//! staying native GPUI: scoped filters, trends, provider/model tables, request
//! detail, pricing configuration, and stream-check parameters.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Datelike, Duration, Local, TimeZone};
use gpui::{
    div, ease_out_quint, prelude::*, px, Animation, AnimationExt, Context, ElementId, Entity,
    FontWeight, SharedString, Window,
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

const LOG_PAGE_SIZE: u32 = 20;
const PRICING_APPS: [&str; 3] = ["claude", "codex", "gemini"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageRange {
    Today,
    OneDay,
    SevenDays,
    FourteenDays,
    ThirtyDays,
}

impl UsageRange {
    fn all() -> &'static [(Self, &'static str)] {
        &[
            (Self::Today, "今天"),
            (Self::OneDay, "24 小时"),
            (Self::SevenDays, "7 天"),
            (Self::FourteenDays, "14 天"),
            (Self::ThirtyDays, "30 天"),
        ]
    }

    fn label(self) -> &'static str {
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
            Self::OneDay => (now - Duration::hours(24)).timestamp(),
            Self::SevenDays => (now - Duration::days(7)).timestamp(),
            Self::FourteenDays => (now - Duration::days(14)).timestamp(),
            Self::ThirtyDays => (now - Duration::days(30)).timestamp(),
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

impl UsageSection {
    fn all() -> &'static [(Self, &'static str, IconName)] {
        &[
            (Self::Logs, "请求日志", IconName::Message),
            (Self::Providers, "Provider 统计", IconName::Proxy),
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
    models: Vec<ModelStats>,
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
    selected_log: Option<RequestLogDetail>,
    show_trend: bool,
    show_scope_options: bool,
    show_pricing: bool,
    show_stream_config: bool,
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
    multiplier_gemini: Entity<TextInput>,
    stream_timeout_secs: Entity<TextInput>,
    stream_max_retries: Entity<TextInput>,
    stream_degraded_threshold_ms: Entity<TextInput>,
}

impl UsageView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            app,
            summary: None,
            summary_by_app: Vec::new(),
            daily: Vec::new(),
            providers: Vec::new(),
            models: Vec::new(),
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
            selected_log: None,
            show_trend: true,
            show_scope_options: false,
            show_pricing: false,
            show_stream_config: false,
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
            multiplier_gemini: cx.new(|cx| text_input(cx, "1", "1")),
            stream_timeout_secs: cx.new(|cx| text_input(cx, "8", "8")),
            stream_max_retries: cx.new(|cx| text_input(cx, "1", "1")),
            stream_degraded_threshold_ms: cx.new(|cx| text_input(cx, "6000", "6000")),
        };
        this.reload();
        this.load_config_forms(cx);
        this
    }

    pub fn reload(&mut self) {
        self.status = None;
        let (start, end) = self.range.bounds();
        let app_type = self.app_filter.as_deref();
        let provider_name = self.provider_filter.as_deref();
        let model = self.model_filter.as_deref();

        match self
            .app
            .db
            .get_usage_summary(start, end, app_type, provider_name, model)
        {
            Ok(s) => self.summary = Some(s),
            Err(err) => {
                self.summary = None;
                self.status = Some(SharedString::from(format!("加载用量失败: {err}")));
            }
        }

        self.summary_by_app = self
            .app
            .db
            .get_usage_summary_by_app(start, end, provider_name, model)
            .unwrap_or_default();
        self.daily = self
            .app
            .db
            .get_daily_trends(start, end, app_type, provider_name, model)
            .unwrap_or_default();
        self.providers = self
            .app
            .db
            .get_provider_stats(start, end, app_type, provider_name, model)
            .unwrap_or_default();
        self.models = self
            .app
            .db
            .get_model_stats(start, end, app_type, provider_name, model)
            .unwrap_or_default();

        let filters = LogFilters {
            app_type: self.app_filter.clone(),
            provider_name: self.provider_filter.clone(),
            model: self.model_filter.clone(),
            status_code: self.status_filter,
            start_date: start,
            end_date: end,
        };
        match self
            .app
            .db
            .get_request_logs(&filters, self.log_page, LOG_PAGE_SIZE)
        {
            Ok(page) => {
                self.logs = page.data;
                self.log_total = page.total;
            }
            Err(err) => {
                self.logs.clear();
                self.log_total = 0;
                self.status = Some(SharedString::from(format!("加载请求日志失败: {err}")));
            }
        }

        self.data_sources = get_data_source_breakdown(&self.app.db).unwrap_or_default();
        self.pricing = self.app.db.get_model_pricing().unwrap_or_default();
        self.stream_config = self.app.db.get_stream_check_config().unwrap_or_default();
    }

    fn load_config_forms(&mut self, cx: &mut Context<Self>) {
        if let Ok(config) = self.app.db.get_stream_check_config() {
            self.stream_config = config.clone();
            set_input(
                &self.stream_timeout_secs,
                config.timeout_secs.to_string(),
                cx,
            );
            set_input(&self.stream_max_retries, config.max_retries.to_string(), cx);
            set_input(
                &self.stream_degraded_threshold_ms,
                config.degraded_threshold_ms.to_string(),
                cx,
            );
        }

        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            self.status = Some(SharedString::from(
                "无法加载计费默认配置: runtime 初始化失败",
            ));
            return;
        };

        for app in PRICING_APPS {
            let multiplier = runtime
                .block_on(self.app.db.get_default_cost_multiplier(app))
                .unwrap_or_else(|_| "1".to_string());
            let source = runtime
                .block_on(self.app.db.get_pricing_model_source(app))
                .unwrap_or_else(|_| "response".to_string());
            self.pricing_sources.insert(app.to_string(), source);
            match app {
                "claude" => set_input(&self.multiplier_claude, multiplier, cx),
                "codex" => set_input(&self.multiplier_codex, multiplier, cx),
                "gemini" => set_input(&self.multiplier_gemini, multiplier, cx),
                _ => {}
            }
        }
    }

    fn reset_log_page(&mut self) {
        self.log_page = 0;
        self.selected_log = None;
    }

    fn set_range(&mut self, range: UsageRange, cx: &mut Context<Self>) {
        self.range = range;
        self.reset_log_page();
        self.reload();
        cx.notify();
    }

    fn set_app_filter(&mut self, app_type: Option<String>, cx: &mut Context<Self>) {
        if self.app_filter != app_type {
            self.provider_filter = None;
            self.model_filter = None;
        }
        self.app_filter = app_type;
        self.reset_log_page();
        self.reload();
        cx.notify();
    }

    fn set_provider_filter(&mut self, provider_name: Option<String>, cx: &mut Context<Self>) {
        if self.provider_filter != provider_name {
            self.model_filter = None;
        }
        self.provider_filter = provider_name;
        self.reset_log_page();
        self.reload();
        cx.notify();
    }

    fn set_model_filter(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        self.model_filter = model;
        self.reset_log_page();
        self.reload();
        cx.notify();
    }

    fn set_status_filter(&mut self, status: Option<u16>, cx: &mut Context<Self>) {
        self.status_filter = status;
        self.reset_log_page();
        self.reload();
        cx.notify();
    }

    fn set_section(&mut self, section: UsageSection, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    fn set_log_page(&mut self, page: u32, cx: &mut Context<Self>) {
        self.log_page = page;
        self.selected_log = None;
        self.reload();
        cx.notify();
    }

    fn select_log(&mut self, request_id: String, cx: &mut Context<Self>) {
        match self.app.db.get_request_detail(&request_id) {
            Ok(detail) => {
                self.selected_log = detail;
                self.status = None;
            }
            Err(err) => {
                self.selected_log = None;
                self.status = Some(SharedString::from(format!("读取请求详情失败: {err}")));
            }
        }
        cx.notify();
    }

    fn sync_sessions(&mut self, cx: &mut Context<Self>) {
        let mut result = match sync_claude_session_logs(&self.app.db) {
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
                services::session_usage_codex::sync_codex_usage(&self.app.db),
            ),
            (
                "Gemini",
                services::session_usage_gemini::sync_gemini_usage(&self.app.db),
            ),
            (
                "OpenCode",
                services::session_usage_opencode::sync_opencode_usage(&self.app.db),
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

        self.status = Some(SharedString::from(format!(
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
        self.reload();
        cx.notify();
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
                self.reload();
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
                self.reload();
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
            (
                "gemini",
                input_value(&self.multiplier_gemini, cx),
                self.pricing_sources
                    .get("gemini")
                    .cloned()
                    .unwrap_or_else(|| "response".to_string()),
            ),
        ];

        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            self.status = Some(SharedString::from(
                "保存计费默认配置失败: runtime 初始化失败",
            ));
            cx.notify();
            return;
        };

        let result = runtime.block_on(async {
            for (app, multiplier, source) in configs {
                self.app
                    .db
                    .set_default_cost_multiplier(app, multiplier.trim())
                    .await?;
                self.app
                    .db
                    .set_pricing_model_source(app, source.trim())
                    .await?;
            }
            Ok::<(), ochub_core::AppError>(())
        });

        self.status = Some(SharedString::from(match result {
            Ok(()) => "计费默认配置已保存".to_string(),
            Err(err) => format!("保存计费默认配置失败: {err}"),
        }));
        cx.notify();
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

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 单选筛选组统一用 segmented（范围 / 应用 / 状态）。
        let range_labels: Vec<&str> = UsageRange::all().iter().map(|(_, label)| *label).collect();
        let range_selected = UsageRange::all()
            .iter()
            .position(|(range, _)| *range == self.range)
            .unwrap_or(0);
        let on_range = cx.listener(|this, ix: &usize, _window, cx| {
            if let Some((range, _)) = UsageRange::all().get(*ix) {
                this.set_range(*range, cx);
            }
        });
        let range_segmented = components::segmented(
            "usage-range",
            &range_labels,
            range_selected,
            move |ix, window, cx| on_range(&ix, window, cx),
        );

        const APP_FILTERS: [(Option<&str>, &str); 5] = [
            (None, "全部"),
            (Some("claude"), "Claude"),
            (Some("codex"), "Codex"),
            (Some("gemini"), "Gemini"),
            (Some("opencode"), "OpenCode"),
        ];
        let app_labels: Vec<&str> = APP_FILTERS.iter().map(|(_, label)| *label).collect();
        let app_selected = APP_FILTERS
            .iter()
            .position(|(app, _)| self.app_filter.as_deref() == *app)
            .unwrap_or(0);
        let on_app = cx.listener(move |this, ix: &usize, _window, cx| {
            if let Some((app, _)) = APP_FILTERS.get(*ix) {
                this.set_app_filter(app.map(str::to_string), cx);
            }
        });
        let app_segmented = components::segmented(
            "usage-app",
            &app_labels,
            app_selected,
            move |ix, window, cx| on_app(&ix, window, cx),
        );

        const STATUS_FILTERS: [(Option<u16>, &str); 6] = [
            (None, "全部状态"),
            (Some(200), "200"),
            (Some(400), "400"),
            (Some(401), "401"),
            (Some(429), "429"),
            (Some(500), "500"),
        ];
        let status_labels: Vec<&str> = STATUS_FILTERS.iter().map(|(_, label)| *label).collect();
        let status_selected = STATUS_FILTERS
            .iter()
            .position(|(status, _)| self.status_filter == *status)
            .unwrap_or(0);
        let on_status = cx.listener(move |this, ix: &usize, _window, cx| {
            if let Some((status, _)) = STATUS_FILTERS.get(*ix) {
                this.set_status_filter(*status, cx);
            }
        });
        let status_segmented = components::segmented(
            "usage-status",
            &status_labels,
            status_selected,
            move |ix, window, cx| on_status(&ix, window, cx),
        );

        components::card()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("范围与口径"),
                    )
                    .child(
                        components::button(
                            "usage-clear-filters",
                            "重置",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.app_filter = None;
                                this.provider_filter = None;
                                this.model_filter = None;
                                this.status_filter = None;
                                this.reset_log_page();
                                this.reload();
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(range_segmented)
            .child(app_segmented)
            .child(status_segmented)
            .when(
                self.provider_filter.is_some() || self.model_filter.is_some(),
                |s| {
                    s.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_2()
                            .when_some(self.provider_filter.clone(), |s, provider| {
                                let clear_label = format!("Provider: {provider} ×");
                                s.child(
                                    filter_chip(
                                        "usage-active-provider",
                                        clear_label,
                                        true,
                                        Some(IconName::Proxy),
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.set_provider_filter(None, cx);
                                        },
                                    )),
                                )
                            })
                            .when_some(self.model_filter.clone(), |s, model| {
                                let clear_label = format!("模型: {model} ×");
                                s.child(
                                    filter_chip(
                                        "usage-active-model",
                                        clear_label,
                                        true,
                                        Some(IconName::Layers),
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.set_model_filter(None, cx);
                                        },
                                    )),
                                )
                            }),
                    )
                },
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
                let width = ((item.summary.real_total_tokens as f64 / max_tokens as f64) * 240.0)
                    .max(8.0) as f32;
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
                                    .w(px(width))
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

    fn render_trend(&self) -> impl IntoElement {
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
                    crate::chart::AreaChart::new(values)
                        .height(176.)
                        .with_animation(
                            anim_id,
                            Animation::new(std::time::Duration::from_millis(720))
                                .with_easing(ease_out_quint()),
                            |chart, delta| chart.progress(delta),
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
                    IconName::Proxy,
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
        let total_pages = self.log_total.div_ceil(LOG_PAGE_SIZE).max(1);
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

        let prev = components::button(
            "usage-prev-page",
            "上一页",
            ButtonTone::Neutral,
            ButtonSize::Sm,
        )
        .on_click(cx.listener(move |this, _event, _window, cx| {
            let next = this.log_page.saturating_sub(1);
            this.set_log_page(next, cx);
        }));
        let next = components::button(
            "usage-next-page",
            "下一页",
            ButtonTone::Neutral,
            ButtonSize::Sm,
        )
        .on_click(cx.listener(move |this, _event, _window, cx| {
            let total_pages = this.log_total.div_ceil(LOG_PAGE_SIZE).max(1);
            let next = (this.log_page + 1).min(total_pages - 1);
            this.set_log_page(next, cx);
        }));

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
            .child(components::pagination(
                prev.into_any_element(),
                format!("第 {} / {} 页", page + 1, total_pages),
                next.into_any_element(),
            ))
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
                        .bg(theme::c(0xffeef0))
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
                    Some(IconName::Proxy),
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
            .children(
                PRICING_APPS
                    .iter()
                    .map(|app| self.render_pricing_default_row(app, cx)),
            )
            .child(
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
            "gemini" => self.multiplier_gemini.clone(),
            _ => self.multiplier_claude.clone(),
        };
        let on_source = cx.listener(move |this, ix: &usize, _window, cx| {
            this.set_pricing_source(app, if *ix == 0 { "response" } else { "request" }, cx);
        });

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
                    .text_xs()
                    .truncate()
                    .child(SharedString::from(app_label(app))),
            )
            .child(components::field("默认倍率", false, None, input))
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
            )
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
                        this.reload();
                        cx.notify();
                    })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "usage-body",
                layout::wide_column()
                    .gap_4()
                    .child(self.render_filters(cx))
                    .child(self.render_data_sources(cx))
                    .child(self.render_summary())
                    .child(
                        components::disclosure(
                            "usage-trend-toggle",
                            "趋势图",
                            format!("{} 个时间桶 · Token 与成本变化", self.daily.len()),
                            self.show_trend,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.show_trend = !this.show_trend;
                                cx.notify();
                            },
                        )),
                    )
                    .when(self.show_trend, |s| s.child(self.render_trend()))
                    .child(
                        components::disclosure(
                            "usage-scope-toggle",
                            "Provider / 模型候选",
                            "从当前范围内真实有数据的条目里快速筛选。",
                            self.show_scope_options,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.show_scope_options = !this.show_scope_options;
                                cx.notify();
                            },
                        )),
                    )
                    .when(self.show_scope_options, |s| {
                        s.child(self.render_scope_options(cx))
                    })
                    .child(self.render_section_tabs(cx))
                    .child(self.render_active_section(cx))
                    .child(
                        components::disclosure(
                            "usage-pricing-toggle",
                            "模型定价配置",
                            format!("{} 条定价 · 支持默认倍率和计价模型来源", self.pricing.len()),
                            self.show_pricing,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.show_pricing = !this.show_pricing;
                                cx.notify();
                            },
                        )),
                    )
                    .when(self.show_pricing, |s| {
                        s.child(self.render_pricing_config(cx))
                    })
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
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.show_stream_config = !this.show_stream_config;
                                cx.notify();
                            },
                        )),
                    )
                    .when(self.show_stream_config, |s| {
                        s.child(self.render_stream_config(cx))
                    }),
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
        "proxy" => "代理请求",
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
        "proxy" | "codex_db" => IconName::Cloud,
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
