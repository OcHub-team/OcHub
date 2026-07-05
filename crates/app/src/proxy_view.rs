//! Local proxy panel. Status and Start/Stop are async on `ProxyService`, so all
//! backend calls go through `cx.spawn`: the `Arc<AppState>` is cloned into the
//! async closure, awaited off the render path, then the view is updated and
//! `cx.notify()`'d via the weak handle.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::db::{FailoverQueueItem, HealthStatus, StreamCheckConfig};
use routedeck_core::proxy::{ProxyConfig, ProxyTakeoverStatus};
use routedeck_core::{AppState, AppType, Provider};

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone)]
struct FailoverGroup {
    app_type: AppType,
    label: &'static str,
    proxy_enabled: bool,
    auto_enabled: bool,
    queue: Vec<FailoverQueueItem>,
    available: Vec<Provider>,
}

pub struct ProxyView {
    app: Arc<AppState>,
    running: bool,
    address: String,
    port: u16,
    total_requests: u64,
    success_rate: f32,
    failover_count: u64,
    config: ProxyConfig,
    takeover: ProxyTakeoverStatus,
    listen_address: Entity<TextInput>,
    listen_port: Entity<TextInput>,
    max_retries: Entity<TextInput>,
    streaming_first_byte_timeout: Entity<TextInput>,
    streaming_idle_timeout: Entity<TextInput>,
    non_streaming_timeout: Entity<TextInput>,
    upstream_proxy_url: Entity<TextInput>,
    stream_timeout_secs: Entity<TextInput>,
    stream_max_retries: Entity<TextInput>,
    stream_degraded_threshold_ms: Entity<TextInput>,
    failover_groups: Vec<FailoverGroup>,
    status: Option<SharedString>,
    busy: bool,
    show_network_settings: bool,
    show_failover: bool,
    show_health_checks: bool,
}

impl ProxyView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let config = ProxyConfig::default();
        let listen_address = cx.new(|cx| text_input(cx, "127.0.0.1", &config.listen_address));
        let listen_port = cx.new(|cx| text_input(cx, "15721", &config.listen_port.to_string()));
        let max_retries = cx.new(|cx| text_input(cx, "3", &config.max_retries.to_string()));
        let streaming_first_byte_timeout =
            cx.new(|cx| text_input(cx, "60", &config.streaming_first_byte_timeout.to_string()));
        let streaming_idle_timeout =
            cx.new(|cx| text_input(cx, "120", &config.streaming_idle_timeout.to_string()));
        let non_streaming_timeout =
            cx.new(|cx| text_input(cx, "600", &config.non_streaming_timeout.to_string()));
        let upstream_proxy_url = cx.new(|cx| TextInput::new(cx, "http://127.0.0.1:7890"));
        let stream_config = StreamCheckConfig::default();
        let stream_timeout_secs =
            cx.new(|cx| text_input(cx, "8", &stream_config.timeout_secs.to_string()));
        let stream_max_retries =
            cx.new(|cx| text_input(cx, "1", &stream_config.max_retries.to_string()));
        let stream_degraded_threshold_ms =
            cx.new(|cx| text_input(cx, "6000", &stream_config.degraded_threshold_ms.to_string()));
        let mut this = Self {
            app,
            running: false,
            address: String::new(),
            port: 0,
            total_requests: 0,
            success_rate: 0.0,
            failover_count: 0,
            config,
            takeover: ProxyTakeoverStatus::default(),
            listen_address,
            listen_port,
            max_retries,
            streaming_first_byte_timeout,
            streaming_idle_timeout,
            non_streaming_timeout,
            upstream_proxy_url,
            stream_timeout_secs,
            stream_max_retries,
            stream_degraded_threshold_ms,
            failover_groups: Vec::new(),
            status: None,
            busy: false,
            show_network_settings: false,
            show_failover: false,
            show_health_checks: false,
        };
        this.refresh_status(cx);
        this
    }

    /// Fetch the proxy status asynchronously and fold it into view state.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let status = app.proxy_service.get_status().await;
            let config = app.proxy_service.get_config().await;
            let takeover = app.proxy_service.get_takeover_status().await;
            let upstream = app.db.get_global_proxy_url();
            let stream_config = app.db.get_stream_check_config();
            let failover_groups = load_failover_groups(&app);
            this.update(cx, |this, cx| {
                match status {
                    Ok(status) => {
                        this.running = status.running;
                        this.address = status.address;
                        this.port = status.port;
                        this.total_requests = status.total_requests;
                        this.success_rate = status.success_rate;
                        this.failover_count = status.failover_count;
                        if let Some(err) = status.last_error {
                            this.status = Some(SharedString::from(err));
                        }
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("状态错误: {err}")));
                    }
                }
                match config {
                    Ok(config) => {
                        this.config = config;
                        this.apply_config_to_inputs(cx);
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("配置读取失败: {err}")));
                    }
                }
                match takeover {
                    Ok(takeover) => this.takeover = takeover,
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("接管状态读取失败: {err}")));
                    }
                }
                if let Ok(url) = upstream {
                    set_input(&this.upstream_proxy_url, url.unwrap_or_default(), cx);
                }
                if let Ok(stream_config) = stream_config {
                    set_input(
                        &this.stream_timeout_secs,
                        stream_config.timeout_secs.to_string(),
                        cx,
                    );
                    set_input(
                        &this.stream_max_retries,
                        stream_config.max_retries.to_string(),
                        cx,
                    );
                    set_input(
                        &this.stream_degraded_threshold_ms,
                        stream_config.degraded_threshold_ms.to_string(),
                        cx,
                    );
                }
                this.failover_groups = failover_groups;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_config_to_inputs(&self, cx: &mut Context<Self>) {
        set_input(&self.listen_address, self.config.listen_address.clone(), cx);
        set_input(&self.listen_port, self.config.listen_port.to_string(), cx);
        set_input(&self.max_retries, self.config.max_retries.to_string(), cx);
        set_input(
            &self.streaming_first_byte_timeout,
            self.config.streaming_first_byte_timeout.to_string(),
            cx,
        );
        set_input(
            &self.streaming_idle_timeout,
            self.config.streaming_idle_timeout.to_string(),
            cx,
        );
        set_input(
            &self.non_streaming_timeout,
            self.config.non_streaming_timeout.to_string(),
            cx,
        );
    }

    fn config_from_inputs(&self, cx: &mut Context<Self>) -> Result<ProxyConfig, String> {
        let mut config = self.config.clone();
        let address = input_value(&self.listen_address, cx);
        if address.is_empty() {
            return Err("监听地址不能为空".to_string());
        }
        config.listen_address = if address == "localhost" {
            "127.0.0.1".to_string()
        } else {
            address
        };
        config.listen_port = parse_port(&input_value(&self.listen_port, cx))?;
        config.max_retries = parse_u8(&input_value(&self.max_retries, cx), "最大重试次数")?;
        config.streaming_first_byte_timeout = parse_u64(
            &input_value(&self.streaming_first_byte_timeout, cx),
            "流式首字超时",
        )?;
        config.streaming_idle_timeout = parse_u64(
            &input_value(&self.streaming_idle_timeout, cx),
            "流式静默超时",
        )?;
        config.non_streaming_timeout = parse_u64(
            &input_value(&self.non_streaming_timeout, cx),
            "非流式总超时",
        )?;
        Ok(config)
    }

    fn save_config(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let config = match self.config_from_inputs(cx) {
            Ok(config) => config,
            Err(err) => {
                self.status = Some(SharedString::from(err));
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.status = Some(SharedString::from("正在保存代理配置..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.proxy_service.update_config(&config).await;
            let status = app.proxy_service.get_status().await;
            let saved_config = app.proxy_service.get_config().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(()) => "代理配置已保存".to_string(),
                    Err(err) => format!("保存代理配置失败: {err}"),
                }));
                if let Ok(status) = status {
                    this.running = status.running;
                    this.address = status.address;
                    this.port = status.port;
                    this.total_requests = status.total_requests;
                    this.success_rate = status.success_rate;
                    this.failover_count = status.failover_count;
                }
                if let Ok(config) = saved_config {
                    this.config = config;
                    this.apply_config_to_inputs(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn do_start(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在启动代理..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.proxy_service.start().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(info) => {
                        this.status = Some(SharedString::from(format!(
                            "已启动于 {}:{}",
                            info.address, info.port
                        )));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("启动失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
            // Pull fresh status after the start attempt.
            let status = app.proxy_service.get_status().await;
            this.update(cx, |this, cx| {
                if let Ok(status) = status {
                    this.running = status.running;
                    this.address = status.address;
                    this.port = status.port;
                    this.total_requests = status.total_requests;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn do_start_takeover(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在启动代理并接管工具配置..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.proxy_service.start_with_takeover().await;
            let status = app.proxy_service.get_status().await;
            let takeover = app.proxy_service.get_takeover_status().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(info) => {
                        this.status = Some(SharedString::from(format!(
                            "已启动并接管工具配置：{}:{}",
                            info.address, info.port
                        )));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("启动接管失败: {err}")));
                    }
                }
                if let Ok(status) = status {
                    this.running = status.running;
                    this.address = status.address;
                    this.port = status.port;
                    this.total_requests = status.total_requests;
                    this.success_rate = status.success_rate;
                    this.failover_count = status.failover_count;
                }
                if let Ok(takeover) = takeover {
                    this.takeover = takeover;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn do_stop(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在停止代理..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.proxy_service.stop().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.running = false;
                        this.status = Some(SharedString::from("已停止"));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("停止失败: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn do_stop_restore(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在停止代理并恢复工具配置..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.proxy_service.stop_with_restore().await;
            let status = app.proxy_service.get_status().await;
            let takeover = app.proxy_service.get_takeover_status().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(()) => "已停止代理并恢复工具配置".to_string(),
                    Err(err) => format!("停止恢复失败: {err}"),
                }));
                if let Ok(status) = status {
                    this.running = status.running;
                    this.address = status.address;
                    this.port = status.port;
                    this.total_requests = status.total_requests;
                    this.success_rate = status.success_rate;
                    this.failover_count = status.failover_count;
                } else {
                    this.running = false;
                }
                if let Ok(takeover) = takeover {
                    this.takeover = takeover;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_logging(&mut self, cx: &mut Context<Self>) {
        let mut config = match self.config_from_inputs(cx) {
            Ok(config) => config,
            Err(err) => {
                self.status = Some(SharedString::from(err));
                cx.notify();
                return;
            }
        };
        config.enable_logging = !config.enable_logging;
        self.config = config;
        self.save_config(cx);
    }

    fn toggle_takeover(&mut self, app_type: &'static str, enabled: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from(format!(
            "正在{} {app_type} 接管...",
            if enabled { "启用" } else { "关闭" }
        )));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app
                .proxy_service
                .set_takeover_for_app(app_type, enabled)
                .await;
            let status = app.proxy_service.get_status().await;
            let takeover = app.proxy_service.get_takeover_status().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(()) => format!(
                        "{} {app_type} 接管成功",
                        if enabled { "启用" } else { "关闭" }
                    ),
                    Err(err) => format!("切换 {app_type} 接管失败: {err}"),
                }));
                if let Ok(status) = status {
                    this.running = status.running;
                    this.address = status.address;
                    this.port = status.port;
                    this.total_requests = status.total_requests;
                    this.success_rate = status.success_rate;
                    this.failover_count = status.failover_count;
                }
                if let Ok(takeover) = takeover {
                    this.takeover = takeover;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_upstream_proxy(&mut self, cx: &mut Context<Self>) {
        let raw = input_value(&self.upstream_proxy_url, cx);
        let url = if raw.trim().is_empty() {
            None
        } else {
            Some(raw.trim())
        };
        if let Err(err) = routedeck_core::proxy::http_client::validate_proxy(url) {
            self.status = Some(SharedString::from(format!("上游代理 URL 无效: {err}")));
            cx.notify();
            return;
        }
        match self
            .app
            .db
            .set_global_proxy_url(url)
            .map_err(|err| err.to_string())
            .and_then(|_| routedeck_core::proxy::http_client::apply_proxy(url))
        {
            Ok(()) => self.status = Some(SharedString::from("上游代理设置已保存")),
            Err(err) => self.status = Some(SharedString::from(format!("保存上游代理失败: {err}"))),
        }
        cx.notify();
    }

    fn scan_upstream_proxy(&mut self, cx: &mut Context<Self>) {
        let found = scan_local_proxy_ports();
        if let Some(url) = found.first() {
            set_input(&self.upstream_proxy_url, url.clone(), cx);
            self.status = Some(SharedString::from(format!(
                "发现 {} 个本地代理，已填入 {url}",
                found.len()
            )));
        } else {
            self.status = Some(SharedString::from("未发现常见本地代理端口"));
        }
        cx.notify();
    }

    fn stream_config_from_inputs(
        &self,
        cx: &mut Context<Self>,
    ) -> Result<StreamCheckConfig, String> {
        Ok(StreamCheckConfig {
            timeout_secs: parse_u64(&input_value(&self.stream_timeout_secs, cx), "探测超时")?,
            max_retries: parse_u32(&input_value(&self.stream_max_retries, cx), "探测重试次数")?,
            degraded_threshold_ms: parse_u64(
                &input_value(&self.stream_degraded_threshold_ms, cx),
                "降级阈值",
            )?,
        })
    }

    fn save_stream_config(&mut self, cx: &mut Context<Self>) {
        let config = match self.stream_config_from_inputs(cx) {
            Ok(config) => config,
            Err(err) => {
                self.status = Some(SharedString::from(err));
                cx.notify();
                return;
            }
        };
        match self.app.db.save_stream_check_config(&config) {
            Ok(()) => self.status = Some(SharedString::from("流式检测配置已保存")),
            Err(err) => {
                self.status = Some(SharedString::from(format!("保存流式检测配置失败: {err}")))
            }
        }
        cx.notify();
    }

    fn run_stream_check(
        &mut self,
        app_type: AppType,
        proxy_targets_only: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let config = match self.stream_config_from_inputs(cx) {
            Ok(config) => config,
            Err(err) => {
                self.status = Some(SharedString::from(err));
                cx.notify();
                return;
            }
        };
        let _ = self.app.db.save_stream_check_config(&config);
        self.busy = true;
        self.status = Some(SharedString::from(format!(
            "正在检测 {} 供应商...",
            app_type.as_str()
        )));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = run_stream_check_for_app(app, app_type, config, proxy_targets_only).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(summary) => summary,
                    Err(err) => format!("流式检测失败: {err}"),
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh_failover_groups(&mut self, cx: &mut Context<Self>) {
        self.failover_groups = load_failover_groups(&self.app);
        cx.notify();
    }

    fn toggle_auto_failover(&mut self, app_type: AppType, target: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from(if target {
            "正在启用自动故障转移..."
        } else {
            "正在关闭自动故障转移..."
        }));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = toggle_auto_failover_for_app(app.clone(), app_type, target).await;
            let groups = load_failover_groups(&app);
            this.update(cx, |this, cx| {
                this.busy = false;
                this.failover_groups = groups;
                this.status = Some(SharedString::from(match result {
                    Ok(msg) => msg,
                    Err(err) => format!("切换自动故障转移失败: {err}"),
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_failover_provider(
        &mut self,
        app_type: AppType,
        provider_id: String,
        cx: &mut Context<Self>,
    ) {
        match self
            .app
            .db
            .add_to_failover_queue(app_type.as_str(), &provider_id)
        {
            Ok(()) => {
                self.status = Some(SharedString::from("已加入故障转移队列"));
                self.refresh_failover_groups(cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("加入故障转移队列失败: {err}")));
                cx.notify();
            }
        }
    }

    fn remove_failover_provider(
        &mut self,
        app_type: AppType,
        provider_id: String,
        cx: &mut Context<Self>,
    ) {
        match self
            .app
            .db
            .remove_from_failover_queue(app_type.as_str(), &provider_id)
        {
            Ok(()) => {
                self.status = Some(SharedString::from("已移出故障转移队列"));
                self.refresh_failover_groups(cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("移出故障转移队列失败: {err}")));
                cx.notify();
            }
        }
    }

    fn clear_failover_queue(&mut self, app_type: AppType, cx: &mut Context<Self>) {
        match self.app.db.clear_failover_queue(app_type.as_str()) {
            Ok(()) => {
                self.status = Some(SharedString::from("已清空故障转移队列"));
                self.refresh_failover_groups(cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("清空故障转移队列失败: {err}")));
                cx.notify();
            }
        }
    }

    fn toggle_network_settings(&mut self, cx: &mut Context<Self>) {
        self.show_network_settings = !self.show_network_settings;
        cx.notify();
    }

    fn toggle_failover_panel(&mut self, cx: &mut Context<Self>) {
        self.show_failover = !self.show_failover;
        cx.notify();
    }

    fn toggle_health_checks(&mut self, cx: &mut Context<Self>) {
        self.show_health_checks = !self.show_health_checks;
        cx.notify();
    }

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary).px_4().py_2()
    }

    fn metric_tile(
        label: &'static str,
        value: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        tone: u32,
    ) -> impl IntoElement {
        let value = value.into();
        let detail = detail.into();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(theme::c(tone)))
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    ),
            )
            .child(
                div()
                    .text_color(theme::c(theme::TEXT))
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child(value),
            )
            .child(
                div()
                    .text_color(theme::c(theme::SUBTEXT))
                    .text_xs()
                    .child(detail),
            )
    }

    fn disclosure_row(
        id: &'static str,
        title: &'static str,
        detail: impl Into<SharedString>,
        showing: bool,
        cx: &mut Context<Self>,
        toggle: fn(&mut Self, &mut Context<Self>),
    ) -> impl IntoElement {
        let detail = detail.into();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_4()
            .rounded_md()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child(detail),
                    ),
            )
            .child(
                Self::action_button(id, if showing { "收起" } else { "展开" }, false)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        toggle(this, cx);
                    }))
                    .aria_label(SharedString::from(format!(
                        "{} {title}",
                        if showing { "收起" } else { "展开" }
                    )))
                    .aria_expanded(showing),
            )
    }

    fn render_input_row(label: &'static str, input: Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(label),
            )
            .child(input)
    }

    fn render_takeover_row(
        &self,
        id: &'static str,
        app_type: &'static str,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .role(gpui::Role::Switch)
            .aria_label(label)
            .aria_toggled(if enabled {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_4()
            .py_3()
            .rounded_md()
            .cursor_pointer()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(if enabled { theme::GREEN } else { theme::BORDER }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child(if enabled {
                                "工具配置已指向本地代理，供应商切换会热更新。"
                            } else {
                                "工具配置保持直连，启用后会先备份再接管。"
                            }),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .bg(theme::c(if enabled {
                        theme::GREEN
                    } else {
                        theme::SURFACE_HOVER
                    }))
                    .text_color(theme::c(if enabled {
                        theme::ACCENT_TEXT
                    } else {
                        theme::SUBTEXT
                    }))
                    .text_sm()
                    .child(if enabled { "已接管" } else { "未接管" }),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.toggle_takeover(app_type, !enabled, cx);
            }))
    }

    fn render_failover_queue_row(
        &self,
        app_type: AppType,
        item: &FailoverQueueItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let provider_id = item.provider_id.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme::c(theme::SURFACE_HOVER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(item.provider_name.clone())),
                    )
                    .child(div().text_color(theme::c(theme::MUTED)).text_xs().child(
                        SharedString::from(format!(
                                "{} · 排序 {}",
                                item.provider_id,
                                item.sort_index
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "默认".to_string())
                            )),
                    )),
            )
            .child(
                Self::action_button(
                    format!("failover-remove-{}-{}", app_type.as_str(), provider_id),
                    "移除",
                    false,
                )
                .text_color(theme::c(theme::RED))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.remove_failover_provider(app_type, provider_id.clone(), cx);
                })),
            )
    }

    fn render_failover_available_row(
        &self,
        app_type: AppType,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let provider_id = provider.id.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(provider.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child(SharedString::from(provider.id.clone())),
                    ),
            )
            .child(
                Self::action_button(
                    format!("failover-add-{}-{}", app_type.as_str(), provider_id),
                    "加入",
                    false,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.add_failover_provider(app_type, provider_id.clone(), cx);
                })),
            )
    }

    fn render_failover_group(
        &self,
        group: &FailoverGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app_type = group.app_type;
        let target_auto = !group.auto_enabled;
        let queue_rows: Vec<_> = group
            .queue
            .iter()
            .map(|item| self.render_failover_queue_row(app_type, item, cx))
            .collect();
        let available_rows: Vec<_> = group
            .available
            .iter()
            .map(|provider| self.render_failover_available_row(app_type, provider, cx))
            .collect();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
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
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(group.label),
                            )
                            .child(div().text_color(theme::c(theme::MUTED)).text_xs().child(
                                SharedString::from(format!(
                                    "接管 {} · 自动故障转移 {} · 队列 {} 个",
                                    if group.proxy_enabled {
                                        "已启用"
                                    } else {
                                        "未启用"
                                    },
                                    if group.auto_enabled {
                                        "已开启"
                                    } else {
                                        "已关闭"
                                    },
                                    group.queue.len()
                                )),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Self::action_button(
                                    format!("failover-auto-{}", app_type.as_str()),
                                    if group.auto_enabled {
                                        "关闭自动"
                                    } else {
                                        "开启自动"
                                    },
                                    group.auto_enabled,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.toggle_auto_failover(app_type, target_auto, cx);
                                    },
                                )),
                            )
                            .child(
                                Self::action_button(
                                    format!("failover-clear-{}", app_type.as_str()),
                                    "清空队列",
                                    false,
                                )
                                .text_color(theme::c(theme::RED))
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.clear_failover_queue(app_type, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::c(theme::SUBTEXT))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("故障转移队列"),
                    )
                    .when(group.queue.is_empty(), |s| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::MUTED))
                                .text_xs()
                                .child("队列为空。开启自动时会尝试加入当前供应商作为 P1。"),
                        )
                    })
                    .children(queue_rows),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::c(theme::SUBTEXT))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("可加入供应商"),
                    )
                    .when(group.available.is_empty(), |s| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::MUTED))
                                .text_xs()
                                .child("没有更多可加入的供应商。"),
                        )
                    })
                    .children(available_rows),
            )
    }
}

impl Render for ProxyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running;
        let endpoint = if self.port != 0 {
            format!("{}:{}", self.address, self.port)
        } else {
            "—".to_string()
        };
        let logging_enabled = self.config.enable_logging;
        let takeover_count = [self.takeover.claude, self.takeover.codex]
            .into_iter()
            .filter(|enabled| *enabled)
            .count();
        let failover_groups = self.failover_groups.clone();
        let failover_queue_count: usize =
            failover_groups.iter().map(|group| group.queue.len()).sum();
        let auto_failover_count = failover_groups
            .iter()
            .filter(|group| group.auto_enabled)
            .count();
        let failover_cards: Vec<_> = failover_groups
            .iter()
            .map(|group| self.render_failover_group(group, cx))
            .collect();

        layout::page()
            .child(
                layout::page_header(
                    "本地代理",
                    Some("代理接管、故障转移与流式健康检测。".into()),
                )
                .child(
                    div()
                        .id("proxy-refresh")
                            .role(gpui::Role::Button)
                            .aria_label("刷新代理状态")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::c(theme::SURFACE))
                            .text_color(theme::c(theme::SUBTEXT))
                            .text_sm()
                            .child("刷新")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_status(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("proxy-body")
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .grid()
                            .grid_cols(4)
                            .gap_3()
                            .child(Self::metric_tile(
                                "代理状态",
                                if running { "运行中" } else { "已停止" },
                                endpoint.clone(),
                                if running { theme::GREEN } else { theme::MUTED },
                            ))
                            .child(Self::metric_tile(
                                "请求",
                                self.total_requests.to_string(),
                                format!("成功率 {:.1}%", self.success_rate),
                                theme::ACCENT,
                            ))
                            .child(Self::metric_tile(
                                "接管",
                                format!("{takeover_count}/2"),
                                "Claude / Codex",
                                if takeover_count > 0 {
                                    theme::GREEN
                                } else {
                                    theme::YELLOW
                                },
                            ))
                            .child(Self::metric_tile(
                                "故障转移",
                                failover_queue_count.to_string(),
                                format!("自动 {auto_failover_count}/2 · 触发 {} 次", self.failover_count),
                                theme::MAUVE,
                            )),
                    )
                    .when_some(self.status.clone(), |s, status| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::TEAL))
                                .text_xs()
                                .child(status),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_3()
                            .child(
                                Self::action_button("proxy-start", "启动", true).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.do_start(cx);
                                    }),
                                ),
                            )
                            .child(
                                Self::action_button("proxy-start-takeover", "启动并接管", true)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_start_takeover(cx);
                                    })),
                            )
                            .child(
                                Self::action_button("proxy-stop", "停止", false).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.do_stop(cx);
                                    }),
                                ),
                            )
                            .child(
                                Self::action_button("proxy-stop-restore", "停止并恢复工具配置", false)
                                    .text_color(theme::c(theme::RED))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_stop_restore(cx);
                                    })),
                            ),
                    )
                    .child(Self::disclosure_row(
                        "proxy-network-toggle",
                        "网络设置",
                        format!(
                            "监听 {endpoint} · 接管 {takeover_count}/2 · 日志{}",
                            if logging_enabled { "开启" } else { "关闭" }
                        ),
                        self.show_network_settings,
                        cx,
                        Self::toggle_network_settings,
                    ))
                    .when(self.show_network_settings, |s| {
                        s.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .bg(theme::c(theme::SURFACE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("监听与超时"),
                            )
                            .child(
                                div()
                                    .grid()
                                    .grid_cols(3)
                                    .gap_3()
                                    .child(Self::render_input_row(
                                        "监听地址",
                                        self.listen_address.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "监听端口（0 为随机）",
                                        self.listen_port.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "最大重试次数",
                                        self.max_retries.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "流式首字超时（秒）",
                                        self.streaming_first_byte_timeout.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "流式静默超时（秒）",
                                        self.streaming_idle_timeout.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "非流式总超时（秒）",
                                        self.non_streaming_timeout.clone(),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_between()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .text_color(theme::c(theme::TEXT))
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child("请求日志"),
                                            )
                                            .child(
                                                div()
                                                    .text_color(theme::c(theme::MUTED))
                                                    .text_xs()
                                                    .child("记录代理请求、错误和故障转移链路。"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("proxy-logging")
                                            .role(gpui::Role::Switch)
                                            .aria_label("请求日志")
                                            .aria_toggled(if logging_enabled {
                                                gpui::Toggled::True
                                            } else {
                                                gpui::Toggled::False
                                            })
                                            .px_3()
                                            .py_1p5()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(theme::c(if logging_enabled {
                                                theme::GREEN
                                            } else {
                                                theme::SURFACE_HOVER
                                            }))
                                            .text_color(theme::c(if logging_enabled {
                                                theme::ACCENT_TEXT
                                            } else {
                                                theme::SUBTEXT
                                            }))
                                            .text_sm()
                                            .child(if logging_enabled { "已启用" } else { "已关闭" })
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.toggle_logging(cx);
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .child(
                                        Self::action_button("proxy-save-config", "保存配置", true)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.save_config(cx);
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .bg(theme::c(theme::SURFACE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("上游代理"),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child("配置 RouteDeck 发起外部 HTTP 请求时使用的上游代理；留空表示直连。"),
                            )
                            .child(Self::render_input_row(
                                "代理 URL",
                                self.upstream_proxy_url.clone(),
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button("upstream-save", "保存上游代理", true)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.save_upstream_proxy(cx);
                                                },
                                            )),
                                    )
                                    .child(
                                        Self::action_button("upstream-scan", "扫描本机代理", false)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.scan_upstream_proxy(cx);
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .bg(theme::c(theme::SURFACE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("工具配置接管"),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child("接管会先备份当前配置，再把客户端请求导向本地代理。关闭时会恢复备份。"),
                            )
                            .child(self.render_takeover_row(
                                "proxy-takeover-claude",
                                "claude",
                                "Claude Code",
                                self.takeover.claude,
                                cx,
                            ))
                            .child(self.render_takeover_row(
                                "proxy-takeover-codex",
                                "codex",
                                "Codex",
                                self.takeover.codex,
                                cx,
                            )),
                    )
                    })
                    .child(Self::disclosure_row(
                        "proxy-failover-toggle",
                        "故障转移队列",
                        format!(
                            "{} 个队列项 · 自动 {auto_failover_count}/2",
                            failover_queue_count
                        ),
                        self.show_failover,
                        cx,
                        Self::toggle_failover_panel,
                    ))
                    .when(self.show_failover, |s| {
                        s.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("故障转移队列"),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child("代理模式下按队列顺序重试供应商；自动故障转移会把目标切到队列 P1。"),
                            )
                            .children(failover_cards),
                    )
                    })
                    .child(Self::disclosure_row(
                        "proxy-health-toggle",
                        "流式健康检测",
                        format!(
                            "超时 {}s · 重试 {} · 降级 {}ms",
                            input_value(&self.stream_timeout_secs, cx),
                            input_value(&self.stream_max_retries, cx),
                            input_value(&self.stream_degraded_threshold_ms, cx)
                        ),
                        self.show_health_checks,
                        cx,
                        Self::toggle_health_checks,
                    ))
                    .when(self.show_health_checks, |s| {
                        s.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .bg(theme::c(theme::SURFACE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("流式健康检测"),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child("对供应商进行流式连通性探测，并把结果写入健康检查日志。"),
                            )
                            .child(
                                div()
                                    .grid()
                                    .grid_cols(3)
                                    .gap_3()
                                    .child(Self::render_input_row(
                                        "探测超时（秒）",
                                        self.stream_timeout_secs.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "重试次数",
                                        self.stream_max_retries.clone(),
                                    ))
                                    .child(Self::render_input_row(
                                        "降级阈值（毫秒）",
                                        self.stream_degraded_threshold_ms.clone(),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        Self::action_button("stream-save", "保存检测配置", true)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.save_stream_config(cx);
                                                },
                                            )),
                                    )
                                    .child(
                                        Self::action_button("stream-claude", "检测 Claude", false)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.run_stream_check(AppType::Claude, false, cx);
                                                },
                                            )),
                                    )
                                    .child(
                                        Self::action_button("stream-codex", "检测 Codex", false)
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.run_stream_check(AppType::Codex, false, cx);
                                                },
                                            )),
                                    )
                                    .child(
                                        Self::action_button(
                                            "stream-targets-claude",
                                            "Claude 代理目标",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_stream_check(AppType::Claude, true, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Self::action_button(
                                            "stream-targets-codex",
                                            "Codex 代理目标",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.run_stream_check(AppType::Codex, true, cx);
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    }),
            )
    }
}

fn text_input(cx: &mut Context<TextInput>, placeholder: &str, value: &str) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn input_value(input: &Entity<TextInput>, cx: &mut Context<ProxyView>) -> String {
    input.read(cx).content().trim().to_string()
}

fn set_input(
    input: &Entity<TextInput>,
    value: impl Into<SharedString>,
    cx: &mut Context<ProxyView>,
) {
    input.update(cx, |input, cx| input.set_content(value, cx));
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| "监听端口必须是 0-65535 的数字".to_string())?;
    if port != 0 && port < 1024 {
        return Err("监听端口必须为 0，或位于 1024-65535".to_string());
    }
    Ok(port)
}

fn parse_u8(value: &str, label: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("{label} 必须是 0-255 的数字"))
}

fn parse_u32(value: &str, label: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("{label} 必须是非负数字"))
}

fn parse_u64(value: &str, label: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{label} 必须是非负数字"))
}

fn failover_apps() -> [(AppType, &'static str); 2] {
    [(AppType::Claude, "Claude Code"), (AppType::Codex, "Codex")]
}

fn load_failover_groups(app: &Arc<AppState>) -> Vec<FailoverGroup> {
    failover_apps()
        .into_iter()
        .map(|(app_type, label)| {
            let (proxy_enabled, auto_enabled) = app.db.get_proxy_flags_sync(app_type.as_str());
            let queue = app
                .db
                .get_failover_queue(app_type.as_str())
                .unwrap_or_default();
            let mut available = app
                .db
                .get_available_providers_for_failover(app_type.as_str())
                .unwrap_or_default();
            available.sort_by(|a, b| {
                a.sort_index
                    .unwrap_or(usize::MAX)
                    .cmp(&b.sort_index.unwrap_or(usize::MAX))
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.id.cmp(&b.id))
            });
            FailoverGroup {
                app_type,
                label,
                proxy_enabled,
                auto_enabled,
                queue,
                available,
            }
        })
        .collect()
}

async fn toggle_auto_failover_for_app(
    app: Arc<AppState>,
    app_type: AppType,
    target: bool,
) -> Result<String, String> {
    let app_key = app_type.as_str();
    let mut config = app
        .db
        .get_proxy_config_for_app(app_key)
        .await
        .map_err(|err| err.to_string())?;
    if target && !config.enabled {
        return Err("请先启用该应用的工具配置接管".to_string());
    }

    let mut auto_added_provider_id = None;
    let p1_provider_id = if target {
        let mut queue = app
            .db
            .get_failover_queue(app_key)
            .map_err(|err| err.to_string())?;
        if queue.is_empty() {
            let current_id = routedeck_core::settings::get_effective_current_provider(&app.db, &app_type)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| "队列为空，且当前没有选中的供应商".to_string())?;
            app.db
                .add_to_failover_queue(app_key, &current_id)
                .map_err(|err| err.to_string())?;
            auto_added_provider_id = Some(current_id);
            queue = app
                .db
                .get_failover_queue(app_key)
                .map_err(|err| err.to_string())?;
        }
        queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| "故障转移队列为空".to_string())?
    } else {
        String::new()
    };

    if target {
        if let Err(err) = app
            .proxy_service
            .switch_proxy_target(app_key, &p1_provider_id)
            .await
        {
            if let Some(provider_id) = auto_added_provider_id {
                let _ = app.db.remove_from_failover_queue(app_key, &provider_id);
            }
            return Err(err);
        }
    }

    config.auto_failover_enabled = target;
    app.db
        .update_proxy_config_for_app(config)
        .await
        .map_err(|err| err.to_string())?;

    Ok(if target {
        format!("已启用自动故障转移，P1 为 {p1_provider_id}")
    } else {
        "已关闭自动故障转移".to_string()
    })
}

fn scan_local_proxy_ports() -> Vec<String> {
    const PROXY_PORTS: &[(u16, &str, bool)] = &[
        (7890, "http", true),
        (7891, "socks5", false),
        (1080, "socks5", false),
        (8080, "http", false),
        (8888, "http", false),
        (3128, "http", false),
        (10808, "socks5", false),
        (10809, "http", false),
    ];

    let mut found = Vec::new();
    for &(port, primary_type, is_mixed) in PROXY_PORTS {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        if TcpStream::connect_timeout(&addr.into(), Duration::from_millis(100)).is_ok() {
            found.push(format!("{primary_type}://127.0.0.1:{port}"));
            if is_mixed {
                let alt_type = if primary_type == "http" {
                    "socks5"
                } else {
                    "http"
                };
                found.push(format!("{alt_type}://127.0.0.1:{port}"));
            }
        }
    }
    found
}

async fn run_stream_check_for_app(
    app: Arc<AppState>,
    app_type: AppType,
    config: StreamCheckConfig,
    proxy_targets_only: bool,
) -> Result<String, String> {
    let providers = app
        .db
        .get_all_providers(app_type.as_str())
        .map_err(|err| err.to_string())?;
    let allowed_ids = if proxy_targets_only {
        let mut ids = std::collections::HashSet::new();
        if let Ok(Some(current_id)) = app.db.get_current_provider(app_type.as_str()) {
            ids.insert(current_id);
        }
        if let Ok(queue) = app.db.get_failover_queue(app_type.as_str()) {
            ids.extend(queue.into_iter().map(|item| item.provider_id));
        }
        Some(ids)
    } else {
        None
    };

    let mut operational = 0usize;
    let mut degraded = 0usize;
    let mut failed = 0usize;
    let mut total = 0usize;

    for (id, provider) in providers {
        if allowed_ids
            .as_ref()
            .map(|ids| !ids.contains(&id))
            .unwrap_or(false)
        {
            continue;
        }
        total += 1;
        let result = routedeck_core::services::StreamCheckService::check_with_retry(
            &app_type, &provider, &config, None,
        )
        .await
        .unwrap_or_else(|err| routedeck_core::db::StreamCheckResult {
            status: HealthStatus::Failed,
            success: false,
            message: err.to_string(),
            response_time_ms: None,
            http_status: None,
            model_used: String::new(),
            tested_at: now_unix_seconds(),
            retry_count: 0,
            error_category: None,
        });

        let _ = app
            .db
            .save_stream_check_log(&id, &provider.name, app_type.as_str(), &result);
        match result.status {
            HealthStatus::Operational => operational += 1,
            HealthStatus::Degraded => degraded += 1,
            HealthStatus::Failed => failed += 1,
        }
    }

    if total == 0 {
        return Ok(format!("{} 没有可检测供应商", app_type.as_str()));
    }
    Ok(format!(
        "{} 检测完成：{} 正常，{} 降级，{} 失败（共 {} 个）",
        app_type.as_str(),
        operational,
        degraded,
        failed,
        total
    ))
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
