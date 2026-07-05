//! MCP servers panel. Manages the unified MCP server registry and syncs enabled
//! servers into each supported client configuration.

use std::sync::Arc;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::db::legacy_json::{McpApps, McpServer};
use routedeck_core::services::McpService;
use routedeck_core::{AppState, AppType};

use crate::components::{self, ButtonTone, ConfirmModal};
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
    /// Pending delete awaiting confirmation: (server id, display name).
    pending_delete: Option<(String, String)>,
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
            pending_delete: None,
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

    fn mcp_apps() -> [AppType; 4] {
        [
            AppType::Claude,
            AppType::Codex,
            AppType::OpenCode,
            AppType::Hermes,
        ]
    }

    fn app_label(app: AppType) -> &'static str {
        match app {
            AppType::Claude => "Claude",
            AppType::Codex => "Codex",
            AppType::OpenCode => "OpenCode",
            AppType::Hermes => "Hermes",
            AppType::ClaudeDesktop => "Claude Desktop",
            AppType::OpenClaw => "OpenClaw",
        }
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

    fn request_delete(&mut self, id: String, name: String, cx: &mut Context<Self>) {
        self.pending_delete = Some((id, name));
        cx.notify();
    }

    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.pending_delete = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some((id, _)) = self.pending_delete.take() else {
            return;
        };
        self.do_delete(id, cx);
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

    fn render_app_toggle(
        &self,
        server: &McpServer,
        app: AppType,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let enabled = server.apps.is_enabled_for(&app);
        let id = server.id.clone();
        div()
            .id(SharedString::from(format!(
                "mcp-toggle-{}-{}",
                server.id,
                app.as_str()
            )))
            .role(gpui::Role::Switch)
            .aria_label(SharedString::from(format!(
                "为 {} 启用 MCP 服务器",
                Self::app_label(app)
            )))
            .aria_toggled(if enabled {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
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
            .text_xs()
            .child(Self::app_label(app))
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

        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .p_4()
            .rounded_lg()
            .bg(theme::c(theme::SURFACE))
            .border_1()
            .border_color(theme::c(theme::BORDER))
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
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(name)),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child(SharedString::from(endpoint)),
                            )
                            .when_some(desc, |s, d| {
                                s.child(
                                    div()
                                        .text_color(theme::c(theme::SUBTEXT))
                                        .text_xs()
                                        .child(SharedString::from(d)),
                                )
                            })
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEAL))
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
                                div()
                                    .id(SharedString::from(format!("mcp-edit-{}", server.id)))
                                    .role(gpui::Role::Button)
                                    .aria_label("编辑 MCP 服务器")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::c(theme::SURFACE_HOVER))
                                    .text_color(theme::c(theme::SUBTEXT))
                                    .text_sm()
                                    .child("编辑")
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.start_edit(edit_server.clone(), cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("mcp-delete-{}", server.id)))
                                    .role(gpui::Role::Button)
                                    .aria_label("删除 MCP 服务器")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::c(theme::SURFACE_HOVER))
                                    .text_color(theme::c(theme::RED))
                                    .text_sm()
                                    .child("删除")
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.request_delete(
                                            delete_id.clone(),
                                            delete_name.clone(),
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .children(Self::mcp_apps().map(|app| self.render_app_toggle(server, app, cx))),
            )
    }

    fn render_field(&self, label: &str, input: &Entity<TextInput>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .text_color(theme::c(theme::SUBTEXT))
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(label.to_string())),
            )
            .child(input.clone())
    }

    fn render_form_app_pill(&self, app: AppType, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = self.apps.is_enabled_for(&app);
        div()
            .id(SharedString::from(format!("mcp-form-app-{}", app.as_str())))
            .role(gpui::Role::Switch)
            .aria_label(SharedString::from(format!(
                "表单中启用 {}",
                Self::app_label(app)
            )))
            .aria_toggled(if enabled {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .bg(theme::c(if enabled {
                theme::ACCENT
            } else {
                theme::SURFACE
            }))
            .text_color(theme::c(if enabled {
                theme::ACCENT_TEXT
            } else {
                theme::SUBTEXT
            }))
            .text_sm()
            .child(Self::app_label(app))
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

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(theme::c(theme::BG))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(theme::c(theme::BORDER))
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .id("mcp-form-back")
                            .role(gpui::Role::Button)
                            .aria_label("返回 MCP 列表")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme::c(theme::SURFACE))
                            .text_color(theme::c(theme::SUBTEXT))
                            .text_sm()
                            .child("返回列表")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.cancel_form(cx);
                            })),
                    ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::c(theme::TEAL))
                        .text_xs()
                        .child(status),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .max_w(px(760.))
                    .child(self.render_field("名称", &self.name))
                    .child(self.render_field("描述", &self.description))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::c(theme::SUBTEXT))
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("启用到应用"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .children(
                                        Self::mcp_apps()
                                            .map(|app| self.render_form_app_pill(app, cx)),
                                    ),
                            ),
                    )
                    .child(self.render_field("服务器 JSON", &self.spec_json))
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .line_height(px(18.))
                            .child(
                                r#"示例：{"type":"stdio","command":"npx","args":["-y","@modelcontextprotocol/server-filesystem"]} 或 {"type":"sse","url":"https://example.com/sse"}"#,
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .child(
                                div()
                                    .id("mcp-form-save")
                                    .role(gpui::Role::Button)
                                    .aria_label("保存 MCP 服务器")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::c(theme::ACCENT))
                                    .text_color(theme::c(theme::ACCENT_TEXT))
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("保存")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.do_save(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("mcp-form-cancel")
                                    .role(gpui::Role::Button)
                                    .aria_label("取消编辑 MCP 服务器")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(theme::c(theme::SURFACE))
                                    .text_color(theme::c(theme::SUBTEXT))
                                    .text_sm()
                                    .child("取消")
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.cancel_form(cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl McpView {
    fn render_with_confirm(
        &self,
        base: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((_, name)) = self.pending_delete.clone() else {
            return base;
        };
        let modal = ConfirmModal::delete(
            "删除 MCP 服务器",
            format!("确定要删除 MCP 服务器“{name}”吗？此操作无法撤销。"),
        );
        let confirm =
            components::action_button_tone("mcp-delete-confirm", "删除", ButtonTone::Danger)
                .on_click(cx.listener(|this, _event, _window, cx| this.confirm_delete(cx)));
        let cancel = components::action_button("mcp-delete-cancel", "取消", false)
            .on_click(cx.listener(|this, _event, _window, cx| this.cancel_delete(cx)));
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(base)
            .child(components::confirm_overlay(&modal, confirm, cancel))
            .into_any_element()
    }
}

impl Render for McpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_mode != FormMode::List {
            let base = self.render_form(cx).into_any_element();
            return self.render_with_confirm(base, cx);
        }

        let cards: Vec<_> = self
            .servers
            .iter()
            .map(|s| self.render_card(s, cx))
            .collect();
        let is_empty = cards.is_empty();

        let base = layout::page()
            .child(
                layout::page_header(
                    "MCP 服务器",
                    Some(
                        "统一管理 MCP，并同步到 Claude、Codex、OpenCode 和 Hermes。".into(),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .id("mcp-add")
                                .role(gpui::Role::Button)
                                .aria_label("新增 MCP 服务器")
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(theme::c(theme::ACCENT))
                                .text_color(theme::c(theme::ACCENT_TEXT))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("新增")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.start_add(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("mcp-import-all")
                                .role(gpui::Role::Button)
                                .aria_label("从应用导入 MCP 服务器")
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(theme::c(theme::SURFACE))
                                .text_color(theme::c(theme::SUBTEXT))
                                .text_sm()
                                .child("从应用导入")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.do_import_all(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("mcp-sync")
                                .role(gpui::Role::Button)
                                .aria_label("同步 MCP 到应用")
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(theme::c(theme::SURFACE))
                                .text_color(theme::c(theme::SUBTEXT))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("同步到应用")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.do_sync(cx);
                                })),
                        ),
                ),
            )
            .when_some(self.status.clone(), |s, status| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::c(theme::TEAL))
                        .text_xs()
                        .child(status),
                )
            })
            .child(layout::scroll_body(
                "mcp-list",
                layout::content_column()
                    .when(is_empty, |s| {
                        s.child(
                            div()
                                .text_color(theme::c(theme::MUTED))
                                .child("还没有配置 MCP 服务器。"),
                        )
                    })
                    .children(cards),
            ))
            .into_any_element();
        self.render_with_confirm(base, cx)
    }
}
