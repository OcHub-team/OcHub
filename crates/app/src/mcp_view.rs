//! MCP servers panel. Manages the unified MCP server registry and syncs enabled
//! servers into each supported client configuration.

use std::sync::Arc;

use gpui::{div, prelude::*, Context, Entity, FontWeight, SharedString, Window};
use ochub_core::db::legacy_json::{McpApps, McpServer};
use ochub_core::services::McpService;
use ochub_core::{AppState, AppType};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::icons::IconName;
use crate::layout;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormMode {
    List,
    Add,
    Edit,
}

pub struct McpView {
    app: Arc<AppState>,
    servers: Vec<McpServer>,
    status: Option<SharedString>,
    form_mode: FormMode,
    editing_id: Option<String>,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    spec_json: Entity<TextInput>,
    apps: McpApps,
    /// 待确认删除的服务器（id, 名称）；`Some` 时展示确认模态。
    confirm_delete: Option<(String, String)>,
}

impl McpView {
    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "服务器名称"));
        let description = cx.new(|cx| TextInput::new(cx, "描述（可选）"));
        let spec_json = cx
            .new(|cx| TextInput::new(cx, r#"{"type":"stdio","command":"","args":[]}"#).code(true));
        let mut this = Self {
            app,
            servers: Vec::new(),
            status: None,
            form_mode: FormMode::List,
            editing_id: None,
            name,
            description,
            spec_json,
            apps: McpApps::default(),
            confirm_delete: None,
        };
        this.reload();
        this
    }

    pub fn reload(&mut self) {
        match McpService::get_all_servers(&self.app) {
            Ok(map) => self.servers = map.into_values().collect(),
            Err(err) => {
                self.servers = Vec::new();
                self.status = Some(SharedString::from(format!("加载服务器失败: {err}")));
            }
        }
    }

    fn mcp_apps() -> Vec<AppType> {
        crate::app_meta::enabled_mcp_apps()
    }

    fn app_label(app: AppType) -> SharedString {
        crate::app_meta::label(app)
    }

    fn default_spec() -> &'static str {
        r#"{
  "type": "stdio",
  "command": "",
  "args": []
}"#
    }

    fn endpoint(server: &McpServer) -> String {
        if let Some(cmd) = server.server.get("command").and_then(|v| v.as_str()) {
            let args = server
                .server
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if args.is_empty() {
                cmd.to_string()
            } else {
                format!("{cmd} {args}")
            }
        } else if let Some(url) = server.server.get("url").and_then(|v| v.as_str()) {
            url.to_string()
        } else if let Some(t) = server.server.get("type").and_then(|v| v.as_str()) {
            t.to_string()
        } else {
            "—".to_string()
        }
    }

    fn enabled_apps_label(server: &McpServer) -> String {
        let apps = server.apps.enabled_apps();
        if apps.is_empty() {
            "未启用应用".to_string()
        } else {
            apps.iter()
                .map(|a| Self::app_label(*a).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    fn clear_form(&mut self, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.apps = McpApps::default();
        self.name.update(cx, |input, cx| input.set_content("", cx));
        self.description
            .update(cx, |input, cx| input.set_content("", cx));
        self.spec_json
            .update(cx, |input, cx| input.set_content(Self::default_spec(), cx));
    }

    fn start_add(&mut self, cx: &mut Context<Self>) {
        self.clear_form(cx);
        self.form_mode = FormMode::Add;
        self.status = None;
        cx.notify();
    }

    fn start_edit(&mut self, server: McpServer, cx: &mut Context<Self>) {
        self.form_mode = FormMode::Edit;
        self.editing_id = Some(server.id.clone());
        self.apps = server.apps.clone();
        self.name
            .update(cx, |input, cx| input.set_content(server.name, cx));
        self.description.update(cx, |input, cx| {
            input.set_content(server.description.unwrap_or_default(), cx)
        });
        let spec = serde_json::to_string_pretty(&server.server)
            .unwrap_or_else(|_| server.server.to_string());
        self.spec_json
            .update(cx, |input, cx| input.set_content(spec, cx));
        self.status = None;
        cx.notify();
    }

    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        self.form_mode = FormMode::List;
        self.clear_form(cx);
        cx.notify();
    }

    fn set_form_app(&mut self, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        self.apps.set_enabled_for(&app, enabled);
        cx.notify();
    }

    fn do_save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.status = Some(SharedString::from("名称不能为空"));
            cx.notify();
            return;
        }

        let spec_raw = self.spec_json.read(cx).content().to_string();
        let spec = match serde_json::from_str::<serde_json::Value>(&spec_raw) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                self.status = Some(SharedString::from("服务器配置必须是 JSON 对象"));
                cx.notify();
                return;
            }
            Err(err) => {
                self.status = Some(SharedString::from(format!("JSON 解析失败: {err}")));
                cx.notify();
                return;
            }
        };

        let description = self.description.read(cx).content().trim().to_string();
        let id = self
            .editing_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let server = McpServer {
            id: id.clone(),
            name,
            server: spec,
            apps: self.apps.clone(),
            description: (!description.is_empty()).then_some(description),
            homepage: None,
            docs: None,
            tags: Vec::new(),
        };

        match McpService::upsert_server(&self.app, server) {
            Ok(()) => {
                self.status = Some(SharedString::from(if self.form_mode == FormMode::Edit {
                    "服务器已保存"
                } else {
                    "服务器已创建"
                }));
                self.form_mode = FormMode::List;
                self.clear_form(cx);
                self.reload();
            }
            Err(err) => self.status = Some(SharedString::from(format!("保存失败: {err}"))),
        }
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match McpService::delete_server(&self.app, &id) {
            Ok(true) => self.status = Some(SharedString::from("服务器已删除")),
            Ok(false) => self.status = Some(SharedString::from("服务器不存在")),
            Err(err) => self.status = Some(SharedString::from(format!("删除失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    fn do_sync(&mut self, cx: &mut Context<Self>) {
        match McpService::sync_all_enabled(&self.app) {
            Ok(()) => self.status = Some(SharedString::from("已同步启用的服务器到应用")),
            Err(err) => self.status = Some(SharedString::from(format!("同步失败: {err}"))),
        }
        cx.notify();
    }

    fn do_toggle_app(&mut self, id: String, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        match McpService::toggle_app(&self.app, &id, app, enabled) {
            Ok(()) => self.status = Some(SharedString::from("应用启用状态已更新")),
            Err(err) => self.status = Some(SharedString::from(format!("更新失败: {err}"))),
        }
        self.reload();
        cx.notify();
    }

    fn do_import_all(&mut self, cx: &mut Context<Self>) {
        let imports = [
            ("Claude", McpService::import_from_claude(&self.app)),
            ("Codex", McpService::import_from_codex(&self.app)),
            ("Gemini", McpService::import_from_gemini(&self.app)),
            ("OpenCode", McpService::import_from_opencode(&self.app)),
            ("Hermes", McpService::import_from_hermes(&self.app)),
        ];
        let mut total = 0usize;
        let mut failures = Vec::new();
        for (label, result) in imports {
            match result {
                Ok(count) => total += count,
                Err(err) => failures.push(format!("{label}: {err}")),
            }
        }

        self.status = if failures.is_empty() {
            Some(SharedString::from(format!(
                "已从应用导入 {total} 个新服务器"
            )))
        } else {
            Some(SharedString::from(format!(
                "导入完成，新增 {total} 个；失败：{}",
                failures.join("; ")
            )))
        };
        self.reload();
        cx.notify();
    }

    /// 「toggle + app 名」小组件：整个 chip 可点，语义与开关一致。
    fn toggle_chip(
        element_id: String,
        aria: String,
        enabled: bool,
        label: SharedString,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(element_id))
            .role(gpui::Role::Switch)
            .aria_label(SharedString::from(aria))
            .aria_toggled(if enabled {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .child(layout::toggle(enabled))
            .child(div().text_color(theme::subtext()).text_xs().child(label))
    }

    fn render_app_toggle(
        &self,
        server: &McpServer,
        app: AppType,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let enabled = server.apps.is_enabled_for(&app);
        let id = server.id.clone();
        Self::toggle_chip(
            format!("mcp-toggle-{}-{}", server.id, app.as_str()),
            format!("为 {} 启用 MCP 服务器", Self::app_label(app)),
            enabled,
            Self::app_label(app),
        )
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.do_toggle_app(id.clone(), app, !enabled, cx);
        }))
    }

    fn render_card(&self, server: &McpServer, cx: &mut Context<Self>) -> impl IntoElement {
        let delete_id = server.id.clone();
        let delete_name = server.name.clone();
        let edit_server = server.clone();
        let endpoint = Self::endpoint(server);
        let apps = Self::enabled_apps_label(server);
        let name = server.name.clone();
        let desc = server.description.clone();

        components::card()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(name)),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(SharedString::from(endpoint)),
                            )
                            .when_some(desc, |s, d| {
                                s.child(
                                    div()
                                        .text_color(theme::subtext())
                                        .text_xs()
                                        .child(SharedString::from(d)),
                                )
                            })
                            .child(
                                div()
                                    .text_color(theme::teal())
                                    .text_xs()
                                    .child(SharedString::from(format!("应用：{apps}"))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                components::button(
                                    format!("mcp-edit-{}", server.id),
                                    "编辑",
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.start_edit(edit_server.clone(), cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    format!("mcp-delete-{}", server.id),
                                    "删除",
                                    ButtonTone::Danger,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.confirm_delete =
                                            Some((delete_id.clone(), delete_name.clone()));
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div().flex().flex_row().flex_wrap().gap_3().children(
                    Self::mcp_apps()
                        .into_iter()
                        .map(|app| self.render_app_toggle(server, app, cx)),
                ),
            )
    }

    fn render_form_app_toggle(&self, app: AppType, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.apps.is_enabled_for(&app);
        Self::toggle_chip(
            format!("mcp-form-app-{}", app.as_str()),
            format!("表单中启用 {}", Self::app_label(app)),
            enabled,
            Self::app_label(app),
        )
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.set_form_app(app, !enabled, cx);
        }))
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.form_mode == FormMode::Edit {
            "编辑 MCP 服务器"
        } else {
            "新增 MCP 服务器"
        };

        layout::page()
            .child(
                layout::page_header(title, None).child(
                    components::button(
                        "mcp-form-back",
                        "← 返回",
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.cancel_form(cx);
                    })),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "mcp-form-body",
                layout::content_column().child(
                    components::card()
                        .gap_4()
                        .child(components::field("名称", false, None, self.name.clone()))
                        .child(components::field(
                            "描述",
                            false,
                            None,
                            self.description.clone(),
                        ))
                        .child(components::field(
                            "启用到应用",
                            false,
                            None,
                            div().flex().flex_row().flex_wrap().gap_3().children(
                                Self::mcp_apps()
                                    .into_iter()
                                    .map(|app| self.render_form_app_toggle(app, cx)),
                            ),
                        ))
                        .child(components::field(
                            "服务器 JSON",
                            false,
                            Some(SharedString::from(
                                r#"示例：{"type":"stdio","command":"npx","args":["-y","@modelcontextprotocol/server-filesystem"]} 或 {"type":"sse","url":"https://example.com/sse"}"#,
                            )),
                            self.spec_json.clone(),
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_3()
                                .child(
                                    components::button(
                                        "mcp-form-save",
                                        "保存",
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_save(cx);
                                    })),
                                )
                                .child(
                                    components::button(
                                        "mcp-form-cancel",
                                        "取消",
                                        ButtonTone::Neutral,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_form(cx);
                                    })),
                                ),
                        ),
                ),
            ))
    }
}

impl Render for McpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_mode != FormMode::List {
            return self.render_form(cx).into_any_element();
        }

        let cards: Vec<_> = self
            .servers
            .iter()
            .map(|s| self.render_card(s, cx))
            .collect();
        let is_empty = cards.is_empty();

        layout::page()
            .relative()
            .child(
                layout::page_header(
                    "MCP 服务器",
                    Some(
                        "统一管理 MCP，并同步到 Claude、Codex、Gemini、OpenCode 和 Hermes。".into(),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            components::icon_button_tone(
                                "mcp-add",
                                "新增",
                                IconName::Add,
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.start_add(cx);
                                },
                            )),
                        )
                        .child(
                            components::icon_button_tone(
                                "mcp-import-all",
                                "从应用导入",
                                IconName::Archive,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.do_import_all(cx);
                                },
                            )),
                        )
                        .child(
                            components::icon_button_tone(
                                "mcp-sync",
                                "同步到应用",
                                IconName::Refresh,
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.do_sync(cx);
                                },
                            )),
                        ),
                ),
            )
            .child(components::status_footer(self.status.clone()))
            .child(layout::scroll_body(
                "mcp-list",
                layout::content_column()
                    .when(is_empty, |s| {
                        s.child(components::empty_state(
                            IconName::Blocks,
                            "还没有配置 MCP 服务器",
                            "新增服务器，或从各应用现有配置一键导入。",
                            Some(
                                components::button(
                                    "mcp-add-empty",
                                    "新增服务器",
                                    ButtonTone::Primary,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.start_add(cx);
                                }))
                                .into_any_element(),
                            ),
                        ))
                    })
                    .children(cards),
            ))
            .when_some(self.confirm_delete.clone(), |root, target| {
                let (delete_id, name) = target;
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header("删除 MCP 服务器"))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(format!(
                                        "确定删除服务器「{name}」吗？此操作不可撤销。"
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "mcp-confirm-delete-cancel",
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
                                "mcp-confirm-delete-ok",
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.do_delete(delete_id.clone(), cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .into_any_element()
    }
}
