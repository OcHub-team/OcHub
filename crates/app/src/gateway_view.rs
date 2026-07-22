//! 中转网关面板：网关启停与配置、上游渠道管理（方言/模型匹配/权重/优先级/健康）、
//! 本地 API key 管理、以及“一键配置”把各应用指向本地网关。
//!
//! 后端调用模式与 `proxy_view` 一致：`Arc<AppState>` clone 进 `cx.spawn`，
//! await 后经 weak handle 更新视图并 `cx.notify()`。

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, prelude::*, ClipboardItem, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{
    ChannelHealth, Dialect, GatewayChannel, GatewayConfig, GatewayKey,
};
use ochub_core::{AppState, AppType};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::icons::IconName;
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

/// 删除确认目标（渠道或本地 API key），携带 id 与展示名称。
#[derive(Clone)]
enum ConfirmTarget {
    Channel(String, String),
    Key(String, String),
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
    /// 待确认的删除目标；`Some` 时展示确认模态。
    confirm_delete: Option<ConfirmTarget>,
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
            confirm_delete: None,
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

    fn dialect_label(dialect: Dialect) -> &'static str {
        match dialect {
            Dialect::Messages => "messages",
            Dialect::Chat => "chat",
            Dialect::Responses => "responses",
        }
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
        let channel_for_edit = channel.clone();
        let delete_target = ConfirmTarget::Channel(channel.id.clone(), channel.name.clone());
        let models_desc = if channel.models.is_empty() {
            "匹配所有模型".to_string()
        } else {
            channel.models.join(", ")
        };
        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
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
                            .child(components::status_dot(dot))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(channel.name.clone())),
                            )
                            .child(components::badge(
                                BadgeTone::Neutral,
                                Self::dialect_label(channel.dialect),
                            ))
                            .when(!channel.enabled, |d| {
                                d.child(components::badge(BadgeTone::Warning, "已停用"))
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
                    .items_center()
                    .gap_2()
                    .child(
                        layout::toggle(channel.enabled)
                            .id(SharedString::from(format!("gw-ch-toggle-{id}")))
                            .role(gpui::Role::Switch)
                            .aria_label(SharedString::from(format!("启停渠道 {}", channel.name)))
                            .aria_toggled(if channel.enabled {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.toggle_channel_enabled(id_for_toggle.clone(), cx);
                            })),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("gw-ch-edit-{id}")),
                            "编辑",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.open_editor(Some(&channel_for_edit.clone()), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("gw-ch-del-{id}")),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(delete_target.clone());
                                cx.notify();
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_editor(&self, editor: &ChannelEditor, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialect_ix = match editor.dialect {
            Dialect::Messages => 0,
            Dialect::Chat => 1,
            Dialect::Responses => 2,
        };
        let on_select = cx.listener(|this, ix: &usize, _w, cx| {
            if let Some(editor) = &mut this.editor {
                editor.dialect = match ix {
                    1 => Dialect::Chat,
                    2 => Dialect::Responses,
                    _ => Dialect::Messages,
                };
            }
            cx.notify();
        });
        components::card()
            .gap_3()
            .border_color(theme::accent())
            .child(
                div()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if editor.id.is_empty() {
                        "新建渠道"
                    } else {
                        "编辑渠道"
                    }),
            )
            .child(components::field(
                "上游方言",
                false,
                None,
                components::segmented(
                    "gw-dialect",
                    &["messages", "chat", "responses"],
                    dialect_ix,
                    move |ix, window, cx| on_select(&ix, window, cx),
                ),
            ))
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_3()
                    .child(components::field(
                        "渠道名称",
                        false,
                        None,
                        editor.name.clone(),
                    ))
                    .child(components::field(
                        "Base URL",
                        false,
                        None,
                        editor.base_url.clone(),
                    ))
                    .child(components::field(
                        "API Key",
                        false,
                        None,
                        editor.api_key.clone(),
                    ))
                    .child(components::field(
                        "模型匹配（逗号分隔，支持 *）",
                        false,
                        None,
                        editor.models.clone(),
                    ))
                    .child(components::field(
                        "上游模型重写",
                        false,
                        None,
                        editor.model_override.clone(),
                    ))
                    .child(
                        div()
                            .grid()
                            .grid_cols(2)
                            .gap_3()
                            .child(components::field(
                                "优先级（小者优先）",
                                false,
                                None,
                                editor.priority.clone(),
                            ))
                            .child(components::field(
                                "权重",
                                false,
                                None,
                                editor.weight.clone(),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        components::button(
                            "gw-ed-save",
                            "保存渠道",
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.save_editor(cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            "gw-ed-cancel",
                            "取消",
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.editor = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_key_row(&self, key: &GatewayKey, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = key.id.clone();
        let secret = key.key.clone();
        let delete_target = ConfirmTarget::Key(key.id.clone(), key.name.clone());
        let masked = if key.key.len() > 10 {
            format!("{}…{}", &key.key[..7], &key.key[key.key.len() - 4..])
        } else {
            key.key.clone()
        };
        components::card()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
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
                    .items_center()
                    .gap_2()
                    .child(
                        components::icon_button_tone(
                            SharedString::from(format!("gw-key-copy-{id}")),
                            "复制",
                            IconName::Copy,
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.copy_text("API key ", secret.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("gw-key-del-{id}")),
                            "删除",
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.confirm_delete = Some(delete_target.clone());
                                cx.notify();
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
                components::button(
                    SharedString::from(format!("gw-apply-{}", app_type.as_str())),
                    format!("配置 {}", crate::app_meta::label(app_type)),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
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
            .relative()
            .child(
                layout::page_header(
                    "中转网关",
                    Some("本地 relay：多方言端点 + 渠道路由 + 一键配置应用。".into()),
                )
                .child(
                    components::icon_button_tone(
                        "gw-refresh",
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
            .child(components::status_footer(self.status.clone()))
            .child(
                layout::scroll_body(
                    "gateway-body",
                    layout::wide_column()
                        .gap_4()
                        // 状态仪表盘
                        .child(
                            div()
                                .grid()
                                .grid_cols(3)
                                .gap_3()
                                .w_full()
                                .child(components::stat_tile(
                                    None,
                                    if running { theme::green() } else { theme::muted() },
                                    "运行状态",
                                    if running { "运行中" } else { "未运行" },
                                    if running {
                                        self.base_url.clone()
                                    } else {
                                        "网关未启动".to_string()
                                    },
                                ))
                                .child(components::stat_tile(
                                    Some(IconName::Cloud),
                                    theme::accent(),
                                    "渠道",
                                    format!("{enabled_count}/{channel_count}"),
                                    "上游渠道",
                                ))
                                .child(components::stat_tile(
                                    Some(IconName::Key),
                                    theme::teal(),
                                    "API keys",
                                    key_count.to_string(),
                                    "本地推理密钥",
                                )),
                        )
                        // 启停与探测
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap_3()
                                .child(
                                    components::button(
                                        "gw-start",
                                        "启动网关",
                                        ButtonTone::Primary,
                                        ButtonSize::Md,
                                    )
                                    .on_click(
                                        cx.listener(|this, _event, _window, cx| this.do_start(cx)),
                                    ),
                                )
                                .child(
                                    components::button(
                                        "gw-stop",
                                        "停止",
                                        ButtonTone::Neutral,
                                        ButtonSize::Md,
                                    )
                                    .on_click(
                                        cx.listener(|this, _event, _window, cx| this.do_stop(cx)),
                                    ),
                                )
                                .child(
                                    components::button(
                                        "gw-probe",
                                        "探测渠道健康",
                                        ButtonTone::Neutral,
                                        ButtonSize::Md,
                                    )
                                    .on_click(
                                        cx.listener(|this, _event, _window, cx| this.probe_now(cx)),
                                    ),
                                ),
                        )
                        // 网关设置
                        .child(
                            components::card()
                                .gap_3()
                                .child(card_title(
                                    "网关设置",
                                    "本地 relay 的监听端口与上游健康探测间隔；端口变更需重启网关生效。",
                                ))
                                .child(
                                    div()
                                        .grid()
                                        .grid_cols(2)
                                        .gap_3()
                                        .child(components::field(
                                            "监听端口",
                                            false,
                                            None,
                                            self.port_input.clone(),
                                        ))
                                        .child(components::field(
                                            "健康探测间隔（秒，0 关闭）",
                                            false,
                                            None,
                                            self.health_interval_input.clone(),
                                        )),
                                )
                                .child(
                                    div().flex().flex_row().justify_end().child(
                                        components::button(
                                            "gw-save-config",
                                            "保存设置",
                                            ButtonTone::Neutral,
                                            ButtonSize::Sm,
                                        )
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.save_config(cx);
                                        })),
                                    ),
                                ),
                        )
                        // 开关行
                        .child(layout::group(vec![
                            components::field_row(
                                "随应用自动启动",
                                "启动 RouteDeck 时自动拉起网关（端点地址保持稳定）。",
                                layout::toggle(self.config.enabled),
                            )
                            .id("gw-autostart")
                            .role(gpui::Role::Switch)
                            .aria_label("随应用自动启动")
                            .aria_toggled(if self.config.enabled {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::inset()))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_autostart(cx);
                            }))
                            .into_any_element(),
                            components::field_row(
                                "要求 API key",
                                "推理端点要求本地 key（Bearer / x-api-key），用量按 key 归因。",
                                layout::toggle(self.config.require_key),
                            )
                            .id("gw-require-key")
                            .role(gpui::Role::Switch)
                            .aria_label("要求 API key")
                            .aria_toggled(if self.config.require_key {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::inset()))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_require_key(cx);
                            }))
                            .into_any_element(),
                        ]))
                        // 渠道
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .w_full()
                                .child(card_title(
                                    "上游渠道",
                                    "按模型匹配、优先级与权重把请求路由到上游方言端点。",
                                ))
                                .child(
                                    components::button(
                                        "gw-ch-add",
                                        "新建渠道",
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_editor(None, cx);
                                    })),
                                ),
                        )
                        .when_some(editor_el, |s, el| s.child(el))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .w_full()
                                .children(channel_rows),
                        )
                        .when(channel_count == 0, |s| {
                            s.child(components::empty_state(
                                IconName::Cloud,
                                "尚无渠道",
                                "新建渠道并选择其上游方言（messages / chat / responses）。",
                                Some(
                                    components::button(
                                        "gw-ch-add-empty",
                                        "新建渠道",
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.open_editor(None, cx);
                                    }))
                                    .into_any_element(),
                                ),
                            ))
                        })
                        // keys
                        .child(
                            components::card()
                                .gap_3()
                                .child(card_title(
                                    "本地 API keys",
                                    "推理端点的本地密钥（Bearer / x-api-key），用量按 key 归因。",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_3()
                                        .child(div().flex_1().child(self.new_key_name.clone()))
                                        .child(
                                            components::button(
                                                "gw-key-create",
                                                "创建 key",
                                                ButtonTone::Primary,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.create_key(cx);
                                                },
                                            )),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .w_full()
                                        .children(key_rows),
                                ),
                        )
                        // 一键配置
                        .child(
                            components::card()
                                .gap_3()
                                .child(card_title(
                                    "一键配置应用",
                                    "为应用写入指向本地网关的供应商条目并切换；每个应用使用独立 key，用量单独归因。",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .gap_3()
                                        .children(self.one_click_buttons(cx))
                                        .child(
                                            components::button(
                                                "gw-generic-info",
                                                "复制通用客户端信息",
                                                ButtonTone::Neutral,
                                                ButtonSize::Sm,
                                            )
                                            .on_click(cx.listener(
                                                |this, _event, _window, cx| {
                                                    this.copy_generic_info(cx);
                                                },
                                            )),
                                        ),
                                ),
                        ),
                ),
            )
            .when_some(self.confirm_delete.clone(), |root, target| {
                let (title, message, delete_id, is_channel) = match &target {
                    ConfirmTarget::Channel(id, name) => (
                        "删除渠道",
                        format!("确定删除渠道「{name}」吗？此操作不可撤销。"),
                        id.clone(),
                        true,
                    ),
                    ConfirmTarget::Key(id, name) => (
                        "删除 API key",
                        format!("确定删除 key「{name}」吗？使用它的应用将无法再通过本地网关推理。"),
                        id.clone(),
                        false,
                    ),
                };
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(title))
                        .child(
                            components::modal_body().child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_sm()
                                    .child(SharedString::from(message)),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "gw-confirm-delete-cancel",
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
                                "gw-confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                if is_channel {
                                    this.delete_channel(delete_id.clone(), cx);
                                } else {
                                    this.delete_key(delete_id.clone(), cx);
                                }
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

/// 卡内/区块标题：`layout::section_header` 风格（SEMIBOLD 标题 + MUTED 说明），
/// 不带额外上间距，可直接放在卡片内或区块标题行内。
fn card_title(title: &'static str, description: &'static str) -> gpui::Div {
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
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(description),
        )
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
