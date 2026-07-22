//! 中转网关面板：网关启停与配置、上游渠道管理（方言/模型匹配/权重/优先级/健康）、
//! 本地 API key 管理、以及“一键配置”把各应用指向本地网关。
//!
//! 后端调用模式与 `proxy_view` 一致：`Arc<AppState>` clone 进 `cx.spawn`，
//! await 后经 weak handle 更新视图并 `cx.notify()`。

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, prelude::*, px, ClipboardItem, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{
    ChannelHealth, Dialect, GatewayChannel, GatewayConfig, GatewayKey,
};
use ochub_core::{AppState, AppType};

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

/// 渠道编辑表单（新建与编辑共用）。
struct ChannelEditor {
    /// 编辑中的渠道 id（新建时为空）。
    id: String,
    dialect: Dialect,
    name: Entity<TextInput>,
    base_url: Entity<TextInput>,
    api_key: Entity<TextInput>,
    models: Entity<TextInput>,
    model_override: Entity<TextInput>,
    priority: Entity<TextInput>,
    weight: Entity<TextInput>,
    enabled: bool,
}

pub struct GatewayView {
    app: Arc<AppState>,
    running: bool,
    base_url: String,
    config: GatewayConfig,
    channels: Vec<GatewayChannel>,
    health: HashMap<String, ChannelHealth>,
    keys: Vec<GatewayKey>,
    port_input: Entity<TextInput>,
    health_interval_input: Entity<TextInput>,
    new_key_name: Entity<TextInput>,
    editor: Option<ChannelEditor>,
    status: Option<SharedString>,
    busy: bool,
}

impl GatewayView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let config = GatewayConfig::default();
        let port_input = cx.new(|cx| text_input(cx, "4180", &config.port.to_string()));
        let health_interval_input =
            cx.new(|cx| text_input(cx, "300", &config.health_interval_secs.to_string()));
        let new_key_name = cx.new(|cx| TextInput::new(cx, "key 名称（如 cherry-studio）"));
        let mut this = Self {
            app,
            running: false,
            base_url: String::new(),
            config,
            channels: Vec::new(),
            health: HashMap::new(),
            keys: Vec::new(),
            port_input,
            health_interval_input,
            new_key_name,
            editor: None,
            status: None,
            busy: false,
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let status = app.gateway.status().await;
            let health = app.gateway.health_snapshot().await;
            let config = app.db.get_gateway_config();
            let channels = app.db.get_gateway_channels();
            let keys = app.db.get_gateway_keys();
            this.update(cx, |this, cx| {
                this.running = status.running;
                this.base_url = status.base_url;
                this.health = health;
                if let Ok(config) = config {
                    set_input(&this.port_input, config.port.to_string(), cx);
                    set_input(
                        &this.health_interval_input,
                        config.health_interval_secs.to_string(),
                        cx,
                    );
                    this.config = config;
                }
                if let Ok(channels) = channels {
                    this.channels = channels;
                }
                if let Ok(keys) = keys {
                    this.keys = keys;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- 生命周期 -----------------------------------------------------------

    fn do_start(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.save_config_silent(cx);
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.gateway.start().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(status) => {
                        this.running = true;
                        this.base_url = status.base_url.clone();
                        this.status = Some(SharedString::from(format!(
                            "网关已启动：{}",
                            status.base_url
                        )));
                    }
                    Err(err) => {
                        this.status = Some(SharedString::from(format!("启动失败: {err}")));
                    }
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
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.gateway.stop().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.running = false;
                        this.base_url = String::new();
                        this.status = Some(SharedString::from("网关已停止"));
                    }
                    Err(err) => this.status = Some(SharedString::from(format!("停止失败: {err}"))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- 配置 ---------------------------------------------------------------

    fn config_from_inputs(&self, cx: &mut Context<Self>) -> Result<GatewayConfig, String> {
        let port = input_value(&self.port_input, cx)
            .parse::<u16>()
            .map_err(|_| "端口必须是 1-65535 的数字".to_string())?;
        if port < 1024 {
            return Err("端口必须位于 1024-65535".to_string());
        }
        let health_interval_secs = input_value(&self.health_interval_input, cx)
            .parse::<u64>()
            .map_err(|_| "健康探测间隔必须是非负数字（秒，0 关闭）".to_string())?;
        Ok(GatewayConfig {
            enabled: self.config.enabled,
            port,
            require_key: self.config.require_key,
            health_interval_secs,
        })
    }

    fn save_config_silent(&mut self, cx: &mut Context<Self>) {
        if let Ok(config) = self.config_from_inputs(cx) {
            let _ = self.app.db.set_gateway_config(&config);
            self.config = config;
        }
    }

    fn save_config(&mut self, cx: &mut Context<Self>) {
        match self.config_from_inputs(cx) {
            Ok(config) => match self.app.db.set_gateway_config(&config) {
                Ok(()) => {
                    self.config = config;
                    self.status = Some(SharedString::from(
                        "网关配置已保存（端口变更需重启网关生效）",
                    ));
                    let app = self.app.clone();
                    cx.spawn(async move |_this, _cx| {
                        let _ = app.gateway.reload_config().await;
                    })
                    .detach();
                }
                Err(err) => self.status = Some(SharedString::from(format!("保存失败: {err}"))),
            },
            Err(err) => self.status = Some(SharedString::from(err)),
        }
        cx.notify();
    }

    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        self.config.enabled = !self.config.enabled;
        self.save_config(cx);
    }

    fn toggle_require_key(&mut self, cx: &mut Context<Self>) {
        self.config.require_key = !self.config.require_key;
        self.save_config(cx);
    }

    // -- 渠道 ---------------------------------------------------------------

    fn open_editor(&mut self, channel: Option<&GatewayChannel>, cx: &mut Context<Self>) {
        let (
            id,
            dialect,
            name,
            base_url,
            api_key,
            models,
            model_override,
            priority,
            weight,
            enabled,
        ) = match channel {
            Some(c) => (
                c.id.clone(),
                c.dialect,
                c.name.clone(),
                c.base_url.clone(),
                c.api_key.clone(),
                c.models.join(", "),
                c.model_override.clone().unwrap_or_default(),
                c.priority.to_string(),
                c.weight.to_string(),
                c.enabled,
            ),
            None => (
                String::new(),
                Dialect::Messages,
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                "0".to_string(),
                "1".to_string(),
                true,
            ),
        };
        self.editor = Some(ChannelEditor {
            id,
            dialect,
            name: cx.new(|cx| text_input(cx, "渠道名称", &name)),
            base_url: cx.new(|cx| text_input(cx, "https://api.example.com", &base_url)),
            api_key: cx.new(|cx| text_input(cx, "上游 API Key", &api_key)),
            models: cx
                .new(|cx| text_input(cx, "模型匹配，逗号分隔，支持 *（留空匹配所有）", &models)),
            model_override: cx
                .new(|cx| text_input(cx, "上游模型重写（留空透传）", &model_override)),
            priority: cx.new(|cx| text_input(cx, "0", &priority)),
            weight: cx.new(|cx| text_input(cx, "1", &weight)),
            enabled,
        });
        cx.notify();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let name = input_value(&editor.name, cx);
        let base_url = input_value(&editor.base_url, cx);
        if name.is_empty() || base_url.is_empty() {
            self.status = Some(SharedString::from("渠道名称与 Base URL 不能为空"));
            cx.notify();
            return;
        }
        let priority = input_value(&editor.priority, cx)
            .parse::<i32>()
            .unwrap_or(0);
        let weight = input_value(&editor.weight, cx)
            .parse::<u32>()
            .unwrap_or(1)
            .max(1);
        let models: Vec<String> = input_value(&editor.models, cx)
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let model_override = {
            let v = input_value(&editor.model_override, cx);
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        };
        let channel = GatewayChannel {
            id: if editor.id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                editor.id.clone()
            },
            name,
            dialect: editor.dialect,
            base_url,
            api_key: input_value(&editor.api_key, cx),
            path_override: None,
            models,
            model_override,
            priority,
            weight,
            enabled: editor.enabled,
            extra_headers: vec![],
        };
        match self.app.db.upsert_gateway_channel(&channel) {
            Ok(()) => {
                self.status = Some(SharedString::from(format!("渠道 {} 已保存", channel.name)));
                self.editor = None;
                self.reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("保存渠道失败: {err}"))),
        }
        cx.notify();
    }

    fn delete_channel(&mut self, id: String, cx: &mut Context<Self>) {
        match self.app.db.delete_gateway_channel(&id) {
            Ok(_) => {
                self.status = Some(SharedString::from("渠道已删除"));
                self.reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("删除渠道失败: {err}"))),
        }
        cx.notify();
    }

    fn toggle_channel_enabled(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(mut channel) = self.channels.iter().find(|c| c.id == id).cloned() {
            channel.enabled = !channel.enabled;
            if let Err(err) = self.app.db.upsert_gateway_channel(&channel) {
                self.status = Some(SharedString::from(format!("更新渠道失败: {err}")));
            }
            self.reload(cx);
        }
    }

    fn probe_now(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from("正在探测渠道健康..."));
        cx.notify();
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            app.gateway.probe_now().await;
            let health = app.gateway.health_snapshot().await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.health = health;
                this.status = Some(SharedString::from("健康探测完成"));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- keys ---------------------------------------------------------------

    fn create_key(&mut self, cx: &mut Context<Self>) {
        let name = input_value(&self.new_key_name, cx);
        if name.is_empty() {
            self.status = Some(SharedString::from("请先输入 key 名称"));
            cx.notify();
            return;
        }
        let key = GatewayKey {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            key: ochub_core::gateway::generate_key_secret(),
            enabled: true,
            created_at: chrono::Utc::now().timestamp(),
        };
        match self.app.db.upsert_gateway_key(&key) {
            Ok(()) => {
                self.status = Some(SharedString::from(format!("已创建 key「{}」", key.name)));
                set_input(&self.new_key_name, "", cx);
                self.reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("创建 key 失败: {err}"))),
        }
        cx.notify();
    }

    fn delete_key(&mut self, id: String, cx: &mut Context<Self>) {
        match self.app.db.delete_gateway_key(&id) {
            Ok(_) => {
                self.status = Some(SharedString::from("key 已删除"));
                self.reload(cx);
            }
            Err(err) => self.status = Some(SharedString::from(format!("删除 key 失败: {err}"))),
        }
        cx.notify();
    }

    fn copy_text(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = Some(SharedString::from(format!("{label}已复制到剪贴板")));
        cx.notify();
    }

    // -- 一键配置 -----------------------------------------------------------

    fn apply_to_app(&mut self, app_type: AppType, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !self.running {
            self.status = Some(SharedString::from("请先启动网关，再进行一键配置"));
            cx.notify();
            return;
        }
        self.busy = true;
        self.status = Some(SharedString::from(format!(
            "正在配置 {}...",
            crate::app_meta::label(app_type)
        )));
        cx.notify();
        let app = self.app.clone();
        let base_url = self.base_url.clone();
        cx.spawn(async move |this, cx| {
            let result = {
                let app = app.clone();
                let base_url = base_url.clone();
                cx.background_spawn(async move { apply::apply_to_app(&app, app_type, &base_url) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.busy = false;
                this.status = Some(SharedString::from(match result {
                    Ok(r) => format!(
                        "{} 已指向本地网关（key: {}）",
                        crate::app_meta::label(app_type),
                        r.key_name
                    ),
                    Err(err) => format!("一键配置失败: {err}"),
                }));
                this.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn copy_generic_info(&mut self, cx: &mut Context<Self>) {
        if !self.running {
            self.status = Some(SharedString::from("请先启动网关"));
            cx.notify();
            return;
        }
        match apply::generic_client_info(&self.app, &self.base_url) {
            Ok(info) => {
                let text = format!("base_url: {}\napi_key: {}", info.base_url, info.key_secret);
                self.copy_text("通用客户端连接信息", text, cx);
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("获取连接信息失败: {err}")));
                cx.notify();
            }
        }
    }

    // -- 渲染 ---------------------------------------------------------------

    fn action_button(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        components::action_button(id, label, primary).px_4().py_2()
    }

    fn dialect_label(dialect: Dialect) -> &'static str {
        match dialect {
            Dialect::Messages => "messages",
            Dialect::Chat => "chat",
            Dialect::Responses => "responses",
        }
    }

    fn toggle_row(
        id: &'static str,
        title: &'static str,
        detail: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        toggle: fn(&mut Self, &mut Context<Self>),
    ) -> impl IntoElement {
        div()
            .id(id)
            .role(gpui::Role::Switch)
            .aria_label(title)
            .aria_toggled(if enabled {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_4()
            .py_3()
            .rounded_md()
            .cursor_pointer()
            .bg(theme::surface())
            .border_1()
            .border_color(if enabled {
                theme::green()
            } else {
                theme::border()
            })
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
                            .child(title),
                    )
                    .child(div().text_color(theme::muted()).text_xs().child(detail)),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .bg(if enabled {
                        theme::green()
                    } else {
                        theme::surface_hover()
                    })
                    .text_color(if enabled {
                        theme::accent_text()
                    } else {
                        theme::subtext()
                    })
                    .text_sm()
                    .child(if enabled { "开启" } else { "关闭" }),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| toggle(this, cx)))
    }

    fn input_row(label: &'static str, input: Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_color(theme::muted()).text_xs().child(label))
            .child(input)
    }

    fn health_dot(&self, channel_id: &str) -> (gpui::Rgba, &'static str) {
        match self.health.get(channel_id) {
            Some(ChannelHealth::Healthy) => (theme::green(), "健康"),
            Some(ChannelHealth::Unhealthy(_)) => (theme::red(), "异常"),
            _ => (theme::muted(), "未探测"),
        }
    }

    fn render_channel_row(
        &self,
        channel: &GatewayChannel,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (dot, health_label) = self.health_dot(&channel.id);
        let id = channel.id.clone();
        let id_for_toggle = id.clone();
        let id_for_delete = id.clone();
        let channel_for_edit = channel.clone();
        let models_desc = if channel.models.is_empty() {
            "匹配所有模型".to_string()
        } else {
            channel.models.join(", ")
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .rounded_md()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(dot))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(channel.name.clone())),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(theme::surface_hover())
                                    .text_color(theme::subtext())
                                    .text_xs()
                                    .child(Self::dialect_label(channel.dialect)),
                            )
                            .when(!channel.enabled, |d| {
                                d.child(div().text_color(theme::yellow()).text_xs().child("已停用"))
                            }),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(format!(
                                "{} · {} · 优先级 {} · 权重 {} · {health_label}",
                                channel.base_url, models_desc, channel.priority, channel.weight
                            ))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Self::action_button(
                            SharedString::from(format!("gw-ch-toggle-{id}")),
                            if channel.enabled { "停用" } else { "启用" },
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.toggle_channel_enabled(id_for_toggle.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            SharedString::from(format!("gw-ch-edit-{id}")),
                            "编辑",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_editor(Some(&channel_for_edit.clone()), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            SharedString::from(format!("gw-ch-del-{id}")),
                            "删除",
                            false,
                        )
                        .text_color(theme::red())
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_channel(id_for_delete.clone(), cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_editor(&self, editor: &ChannelEditor, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialect_buttons = [Dialect::Messages, Dialect::Chat, Dialect::Responses]
            .into_iter()
            .map(|d| {
                let selected = editor.dialect == d;
                Self::action_button(
                    SharedString::from(format!("gw-ed-dialect-{}", Self::dialect_label(d))),
                    Self::dialect_label(d),
                    selected,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    if let Some(editor) = &mut this.editor {
                        editor.dialect = d;
                    }
                    cx.notify();
                }))
                .into_any_element()
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
            .border_color(theme::accent())
            .child(
                div()
                    .text_color(theme::text())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if editor.id.is_empty() {
                        "新建渠道"
                    } else {
                        "编辑渠道"
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(theme::muted()).text_xs().child("上游方言"))
                    .children(dialect_buttons),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_3()
                    .child(Self::input_row("渠道名称", editor.name.clone()))
                    .child(Self::input_row("Base URL", editor.base_url.clone()))
                    .child(Self::input_row("API Key", editor.api_key.clone()))
                    .child(Self::input_row(
                        "模型匹配（逗号分隔，支持 *）",
                        editor.models.clone(),
                    ))
                    .child(Self::input_row(
                        "上游模型重写",
                        editor.model_override.clone(),
                    ))
                    .child(
                        div()
                            .grid()
                            .grid_cols(2)
                            .gap_3()
                            .child(Self::input_row(
                                "优先级（小者优先）",
                                editor.priority.clone(),
                            ))
                            .child(Self::input_row("权重", editor.weight.clone())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        Self::action_button("gw-ed-save", "保存渠道", true).on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.save_editor(cx);
                            },
                        )),
                    )
                    .child(Self::action_button("gw-ed-cancel", "取消", false).on_click(
                        cx.listener(|this, _event, _window, cx| {
                            this.editor = None;
                            cx.notify();
                        }),
                    )),
            )
            .into_any_element()
    }

    fn render_key_row(&self, key: &GatewayKey, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = key.id.clone();
        let secret = key.key.clone();
        let masked = if key.key.len() > 10 {
            format!("{}…{}", &key.key[..7], &key.key[key.key.len() - 4..])
        } else {
            key.key.clone()
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .rounded_md()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
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
                            .child(SharedString::from(key.name.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(masked)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Self::action_button(
                            SharedString::from(format!("gw-key-copy-{id}")),
                            "复制",
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.copy_text("API key ", secret.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        Self::action_button(
                            SharedString::from(format!("gw-key-del-{id}")),
                            "删除",
                            false,
                        )
                        .text_color(theme::red())
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.delete_key(id.clone(), cx);
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn one_click_buttons(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        apply::supported_apps()
            .iter()
            .filter(|a| crate::app_meta::enabled_app_types().contains(a))
            .map(|&app_type| {
                Self::action_button(
                    SharedString::from(format!("gw-apply-{}", app_type.as_str())),
                    format!("配置 {}", crate::app_meta::label(app_type)),
                    true,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.apply_to_app(app_type, cx);
                }))
                .into_any_element()
            })
            .collect()
    }
}

impl Render for GatewayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running;
        let endpoint = if running {
            self.base_url.clone()
        } else {
            "未运行".to_string()
        };
        let channel_count = self.channels.len();
        let enabled_count = self.channels.iter().filter(|c| c.enabled).count();
        let key_count = self.keys.len();
        let channel_rows: Vec<gpui::AnyElement> = self
            .channels
            .clone()
            .iter()
            .map(|c| self.render_channel_row(c, cx))
            .collect();
        let key_rows: Vec<gpui::AnyElement> = self
            .keys
            .clone()
            .iter()
            .map(|k| self.render_key_row(k, cx))
            .collect();
        let editor_el = self.editor.take().map(|editor| {
            let el = self.render_editor(&editor, cx);
            self.editor = Some(editor);
            el
        });

        layout::page()
            .child(
                layout::page_header(
                    "中转网关",
                    Some("本地 relay：多方言端点 + 渠道路由 + 一键配置应用。".into()),
                )
                .child(
                    div()
                        .id("gw-refresh")
                        .role(gpui::Role::Button)
                        .aria_label("刷新网关状态")
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme::surface())
                        .text_color(theme::subtext())
                        .text_sm()
                        .child("刷新")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.reload(cx);
                        })),
                ),
            )
            .child(
                div()
                    .id("gw-body")
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .overflow_y_scroll()
                    // 状态行 + 启停
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(10.))
                                    .h(px(10.))
                                    .rounded_full()
                                    .bg(if running { theme::green() } else { theme::muted() }),
                            )
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(endpoint.clone())),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(SharedString::from(format!(
                                        "渠道 {enabled_count}/{channel_count} · key {key_count}"
                                    ))),
                            ),
                    )
                    .when_some(self.status.clone(), |s, status| {
                        s.child(div().text_color(theme::teal()).text_xs().child(status))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_3()
                            .child(
                                Self::action_button("gw-start", "启动网关", true).on_click(
                                    cx.listener(|this, _event, _window, cx| this.do_start(cx)),
                                ),
                            )
                            .child(
                                Self::action_button("gw-stop", "停止", false).on_click(
                                    cx.listener(|this, _event, _window, cx| this.do_stop(cx)),
                                ),
                            )
                            .child(
                                Self::action_button("gw-probe", "探测渠道健康", false).on_click(
                                    cx.listener(|this, _event, _window, cx| this.probe_now(cx)),
                                ),
                            ),
                    )
                    // 基础配置
                    .child(
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
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("网关设置"),
                            )
                            .child(
                                div()
                                    .grid()
                                    .grid_cols(3)
                                    .gap_3()
                                    .child(Self::input_row("监听端口", self.port_input.clone()))
                                    .child(Self::input_row(
                                        "健康探测间隔（秒，0 关闭）",
                                        self.health_interval_input.clone(),
                                    ))
                                    .child(
                                        div().flex().flex_col().gap_1().child(
                                            Self::action_button("gw-save-config", "保存设置", false)
                                                .on_click(cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.save_config(cx);
                                                    },
                                                )),
                                        ),
                                    ),
                            )
                            .child(Self::toggle_row(
                                "gw-autostart",
                                "随应用自动启动",
                                "启动 RouteDeck 时自动拉起网关（端点地址保持稳定）。",
                                self.config.enabled,
                                cx,
                                Self::toggle_autostart,
                            ))
                            .child(Self::toggle_row(
                                "gw-require-key",
                                "要求 API key",
                                "推理端点要求本地 key（Bearer / x-api-key），用量按 key 归因。",
                                self.config.require_key,
                                cx,
                                Self::toggle_require_key,
                            )),
                    )
                    // 渠道
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("上游渠道"),
                            )
                            .child(
                                Self::action_button("gw-ch-add", "新建渠道", true).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.open_editor(None, cx);
                                    }),
                                ),
                            ),
                    )
                    .when_some(editor_el, |s, el| s.child(el))
                    .child(div().flex().flex_col().gap_2().children(channel_rows))
                    .when(channel_count == 0, |s| {
                        s.child(
                            div()
                                .text_color(theme::muted())
                                .text_sm()
                                .child("尚无渠道。新建渠道并选择其上游方言（messages / chat / responses）。"),
                        )
                    })
                    // keys
                    .child(
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
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("本地 API keys"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_3()
                                    .child(div().flex_1().child(self.new_key_name.clone()))
                                    .child(
                                        Self::action_button("gw-key-create", "创建 key", true)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.create_key(cx);
                                            })),
                                    ),
                            )
                            .child(div().flex().flex_col().gap_2().children(key_rows)),
                    )
                    // 一键配置
                    .child(
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
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("一键配置应用"),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child("为应用写入指向本地网关的供应商条目并切换；每个应用使用独立 key，用量单独归因。"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_3()
                                    .children(self.one_click_buttons(cx))
                                    .child(
                                        Self::action_button("gw-generic-info", "复制通用客户端信息", false)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.copy_generic_info(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}

fn text_input(cx: &mut Context<TextInput>, placeholder: &str, value: &str) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn input_value(input: &Entity<TextInput>, cx: &mut Context<GatewayView>) -> String {
    input.read(cx).content().trim().to_string()
}

fn set_input(
    input: &Entity<TextInput>,
    value: impl Into<SharedString>,
    cx: &mut Context<GatewayView>,
) {
    input.update(cx, |input, cx| input.set_content(value, cx));
}
