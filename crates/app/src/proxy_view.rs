//! Local proxy panel. Status and Start/Stop are async on `ProxyService`, so all
//! backend calls go through `cx.spawn`: the `Arc<AppState>` is cloned into the
//! async closure, awaited off the render path, then the view is updated and
//! `cx.notify()`'d via the weak handle.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::db::{HealthStatus, StreamCheckConfig};
use ochub_core::proxy::{ProxyConfig, ProxyTakeoverStatus};
use ochub_core::{AppState, AppType};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

pub struct ProxyView {
    app: Arc<AppState>,
    running: bool,
    address: String,
    port: u16,
    total_requests: u64,
    success_rate: f32,
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
    status: Option<SharedString>,
    busy: bool,
    show_network_settings: bool,
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
            status: None,
            busy: false,
            show_network_settings: false,
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
            this.update(cx, |this, cx| {
                match status {
                    Ok(status) => {
                        this.running = status.running;
                        this.address = status.address;
                        this.port = status.port;
                        this.total_requests = status.total_requests;
                        this.success_rate = status.success_rate;
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
        if let Err(err) = ochub_core::proxy::http_client::validate_proxy(url) {
            self.status = Some(SharedString::from(format!("上游代理 URL 无效: {err}")));
            cx.notify();
            return;
        }
        match self
            .app
            .db
            .set_global_proxy_url(url)
            .map_err(|err| err.to_string())
            .and_then(|_| ochub_core::proxy::http_client::apply_proxy(url))
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

    fn toggle_network_settings(&mut self, cx: &mut Context<Self>) {
        self.show_network_settings = !self.show_network_settings;
        cx.notify();
    }

    fn toggle_health_checks(&mut self, cx: &mut Context<Self>) {
        self.show_health_checks = !self.show_health_checks;
        cx.notify();
    }

    /// Stream-check buttons for every enabled proxy-capable app.
    fn stream_check_buttons(cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut buttons = Vec::new();
        for app in crate::app_meta::enabled_app_types() {
            let Some(plugin) = ochub_core::plugin::get_plugin(&app.app_id()) else {
                continue;
            };
            if plugin.proxy().is_none() {
                continue;
            }
            let label = crate::app_meta::label(app);
            buttons.push(
                components::button(
                    SharedString::from(format!("stream-{}", app.as_str())),
                    format!("检测 {label}"),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.run_stream_check(app, false, cx);
                }))
                .into_any_element(),
            );
            buttons.push(
                components::button(
                    SharedString::from(format!("stream-targets-{}", app.as_str())),
                    format!("{label} 代理目标"),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.run_stream_check(app, true, cx);
                }))
                .into_any_element(),
            );
        }
        buttons
    }

    /// Takeover toggle rows for every enabled takeover-capable app.
    fn takeover_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        crate::app_meta::enabled_app_types()
            .into_iter()
            .filter_map(|app| {
                let plugin = ochub_core::plugin::get_plugin(&app.app_id())?;
                if !plugin.proxy()?.supports_takeover {
                    return None;
                }
                let enabled = match app {
                    AppType::Claude => self.takeover.claude,
                    AppType::Codex => self.takeover.codex,
                    AppType::Gemini => self.takeover.gemini,
                    _ => return None,
                };
                Some(
                    self.render_takeover_row(
                        SharedString::from(format!("proxy-takeover-{}", app.as_str())),
                        app.as_str(),
                        crate::app_meta::label(app),
                        enabled,
                        cx,
                    )
                    .into_any_element(),
                )
            })
            .collect()
    }

    fn render_takeover_row(
        &self,
        id: impl Into<gpui::ElementId>,
        app_type: &'static str,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = label.into();
        components::field_row(
            label.clone(),
            if enabled {
                "工具配置已指向本地代理，供应商切换会热更新。"
            } else {
                "工具配置保持直连，启用后会先备份再接管。"
            },
            layout::toggle(enabled),
        )
        .id(id)
        .role(gpui::Role::Switch)
        .aria_label(label)
        .aria_toggled(if enabled {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .cursor_pointer()
        .hover(|s| s.bg(theme::inset()))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.toggle_takeover(app_type, !enabled, cx);
        }))
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
        let takeover_count = [
            self.takeover.claude,
            self.takeover.codex,
            self.takeover.gemini,
        ]
        .into_iter()
        .filter(|enabled| *enabled)
        .count();

        layout::page()
            .child(
                layout::page_header("代理", Some("代理接管与流式健康检测。".into())).child(
                    components::icon_button_tone(
                        "proxy-refresh",
                        "刷新",
                        IconName::Refresh,
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.refresh_status(cx);
                    })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "proxy-body",
                layout::wide_column()
                    .gap_4()
                    .child(
                        div()
                            .grid()
                            .grid_cols(4)
                            .gap_3()
                            .w_full()
                            .child(components::stat_tile(
                                None,
                                if running {
                                    theme::green()
                                } else {
                                    theme::muted()
                                },
                                "代理状态",
                                if running { "运行中" } else { "已停止" },
                                endpoint.clone(),
                            ))
                            .child(components::stat_tile(
                                None,
                                theme::accent(),
                                "请求",
                                self.total_requests.to_string(),
                                format!("成功率 {:.1}%", self.success_rate),
                            ))
                            .child(components::stat_tile(
                                None,
                                if takeover_count > 0 {
                                    theme::green()
                                } else {
                                    theme::yellow()
                                },
                                "接管",
                                format!("{takeover_count}/3"),
                                "Claude / Codex / Gemini",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_3()
                            .child(
                                components::button(
                                    "proxy-start",
                                    "启动",
                                    ButtonTone::Primary,
                                    ButtonSize::Md,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.do_start(cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "proxy-start-takeover",
                                    "启动并接管",
                                    ButtonTone::Primary,
                                    ButtonSize::Md,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.do_start_takeover(cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "proxy-stop",
                                    "停止",
                                    ButtonTone::Neutral,
                                    ButtonSize::Md,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.do_stop(cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "proxy-stop-restore",
                                    "停止并恢复工具配置",
                                    ButtonTone::Danger,
                                    ButtonSize::Md,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.do_stop_restore(cx);
                                    },
                                )),
                            ),
                    )
                    .child(
                        components::disclosure(
                            "proxy-network-toggle",
                            "网络设置",
                            format!(
                                "监听 {endpoint} · 接管 {takeover_count}/3 · 日志{}",
                                if logging_enabled { "开启" } else { "关闭" }
                            ),
                            self.show_network_settings,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.toggle_network_settings(cx);
                            },
                        )),
                    )
                    .when(self.show_network_settings, |s| {
                        s.child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("监听与超时"),
                                )
                                .child(
                                    div()
                                        .grid()
                                        .grid_cols(3)
                                        .gap_3()
                                        .child(components::field(
                                            "监听地址",
                                            false,
                                            None,
                                            self.listen_address.clone(),
                                        ))
                                        .child(components::field(
                                            "监听端口（0 为随机）",
                                            false,
                                            None,
                                            self.listen_port.clone(),
                                        ))
                                        .child(components::field(
                                            "最大重试次数",
                                            false,
                                            None,
                                            self.max_retries.clone(),
                                        ))
                                        .child(components::field(
                                            "流式首字超时（秒）",
                                            false,
                                            None,
                                            self.streaming_first_byte_timeout.clone(),
                                        ))
                                        .child(components::field(
                                            "流式静默超时（秒）",
                                            false,
                                            None,
                                            self.streaming_idle_timeout.clone(),
                                        ))
                                        .child(components::field(
                                            "非流式总超时（秒）",
                                            false,
                                            None,
                                            self.non_streaming_timeout.clone(),
                                        )),
                                )
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "proxy-save-config",
                                            "保存配置",
                                            ButtonTone::Primary,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.save_config(cx);
                                            }),
                                        ),
                                    ),
                                ),
                        )
                        .child(layout::group(vec![components::field_row(
                            "请求日志",
                            "记录代理请求、错误和故障转移链路。",
                            layout::toggle(logging_enabled),
                        )
                        .id("proxy-logging")
                        .role(gpui::Role::Switch)
                        .aria_label("请求日志")
                        .aria_toggled(if logging_enabled {
                            gpui::Toggled::True
                        } else {
                            gpui::Toggled::False
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::inset()))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.toggle_logging(cx);
                        }))
                        .into_any_element()]))
                        .child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("上游代理"),
                                )
                                .child(div().text_color(theme::muted()).text_xs().child(
                                    "配置 OCHUB 发起外部 HTTP 请求时使用的上游代理；留空表示直连。",
                                ))
                                .child(components::field(
                                    "代理 URL",
                                    false,
                                    None,
                                    self.upstream_proxy_url.clone(),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "upstream-save",
                                                "保存上游代理",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.save_upstream_proxy(cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            components::button(
                                                "upstream-scan",
                                                "扫描本机代理",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.scan_upstream_proxy(cx);
                                                }),
                                            ),
                                        ),
                                ),
                        )
                        .child(layout::section_header(
                            "工具配置接管",
                            "接管会先备份当前配置，再把客户端请求导向本地代理。关闭时会恢复备份。",
                        ))
                        .child(layout::group(self.takeover_rows(cx)))
                    })
                    .child(
                        components::disclosure(
                            "proxy-health-toggle",
                            "流式健康检测",
                            format!(
                                "超时 {}s · 重试 {} · 降级 {}ms",
                                input_value(&self.stream_timeout_secs, cx),
                                input_value(&self.stream_max_retries, cx),
                                input_value(&self.stream_degraded_threshold_ms, cx)
                            ),
                            self.show_health_checks,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.toggle_health_checks(cx);
                            },
                        )),
                    )
                    .when(self.show_health_checks, |s| {
                        s.child(
                            components::card()
                                .gap_3()
                                .child(
                                    div()
                                        .text_color(theme::text())
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("流式健康检测"),
                                )
                                .child(div().text_color(theme::muted()).text_xs().child(
                                    "对供应商进行流式连通性探测，并把结果写入健康检查日志。",
                                ))
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
                                            "重试次数",
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
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(
                                            components::button(
                                                "stream-save",
                                                "保存检测配置",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.save_stream_config(cx);
                                                }),
                                            ),
                                        )
                                        .children(Self::stream_check_buttons(cx)),
                                ),
                        )
                    }),
            ))
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
        let result = ochub_core::services::StreamCheckService::check_with_retry(
            &app_type, &provider, &config, None,
        )
        .await
        .unwrap_or_else(|err| ochub_core::db::StreamCheckResult {
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
