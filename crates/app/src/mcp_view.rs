//! MCP servers panel. Manages the unified MCP server registry and syncs enabled
//! servers into each supported client configuration.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, Context, Entity, FontWeight, ListAlignment, ListState, ScrollHandle,
    SharedString, Window,
};
use ochub_core::db::legacy_json::{McpApps, McpServer};
use ochub_core::services::McpService;
use ochub_core::{AppState, AppType};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
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
    status_level: Option<NotificationLevel>,
    form_mode: FormMode,
    editing_id: Option<String>,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    spec_json: Entity<TextInput>,
    apps: McpApps,
    /// 待确认删除的服务器（id, 名称）；`Some` 时展示确认模态。
    confirm_delete: Option<(String, String)>,
    list_state: ListState,
    form_scroll_handle: ScrollHandle,
}

/// Row plan for the virtualized list. Rebuilt每帧并被 list 的 processor 捕获，
/// 保证一帧内索引与内容一致。`Card` 存 `servers` 的下标。
#[derive(Clone, Copy)]
enum McpRow {
    EmptyState,
    Card(usize),
}

impl McpView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, but gpui's virtualized lists cache
    /// measured item heights and invalidate them only on a width change, so a
    /// translation that changes a row's height would otherwise leave the list
    /// scrolled to stale offsets.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        // Placeholders are captured when the input is constructed, and this
        // view outlives a locale switch, so they need pushing in by hand. The
        // spec field's placeholder is a JSON sample and stays as written.
        self.name.update(cx, |input, cx| {
            input.set_placeholder(t(k::MCP_FORM_NAME_PLACEHOLDER), cx)
        });
        self.description.update(cx, |input, cx| {
            input.set_placeholder(t(k::MCP_FORM_DESCRIPTION_PLACEHOLDER), cx)
        });
        self.list_state.remeasure();
        cx.notify();
    }

    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() || self.form_mode == FormMode::List {
            window.play_system_bell();
        } else {
            self.do_save(cx);
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.take().is_some() {
            cx.notify();
        } else if self.form_mode == FormMode::List {
            window.play_system_bell();
        } else {
            self.cancel_form(cx);
        }
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, t(k::MCP_FORM_NAME_PLACEHOLDER)));
        let description = cx.new(|cx| TextInput::new(cx, t(k::MCP_FORM_DESCRIPTION_PLACEHOLDER)));
        let spec_json = cx
            .new(|cx| TextInput::new(cx, r#"{"type":"stdio","command":"","args":[]}"#).code(true));
        let mut this = Self {
            app,
            servers: Vec::new(),
            status: None,
            status_level: None,
            form_mode: FormMode::List,
            editing_id: None,
            name,
            description,
            spec_json,
            apps: McpApps::default(),
            confirm_delete: None,
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            form_scroll_handle: ScrollHandle::new(),
        };
        this.reload();
        this
    }

    /// Queue a toast with an explicit severity. Callers keep their own
    /// `cx.notify()` so the status can also be set from `reload`, which has no
    /// context. Never leave the level unset: `None` falls back to guessing the
    /// severity from the message text.
    fn set_status(&mut self, level: NotificationLevel, message: impl Into<SharedString>) {
        self.status = Some(message.into());
        self.status_level = Some(level);
    }

    fn clear_status(&mut self) {
        self.status = None;
        self.status_level = None;
    }

    pub fn reload(&mut self) {
        match McpService::get_all_servers(&self.app) {
            Ok(map) => self.servers = map.into_values().collect(),
            Err(err) => {
                self.servers = Vec::new();
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::MCP_STATUS_LOAD_FAILED, error = err),
                );
            }
        }
        // 行数变化由 render 里的 reset 处理；这里只失效高度缓存。
        self.list_state.remeasure();
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
            raw(k::MCP_CARD_APPS_NONE).to_string()
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
        self.clear_status();
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
        self.clear_status();
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
            self.set_status(NotificationLevel::Error, t(k::MCP_STATUS_NAME_REQUIRED));
            cx.notify();
            return;
        }

        let spec_raw = self.spec_json.read(cx).content().to_string();
        let spec = match serde_json::from_str::<serde_json::Value>(&spec_raw) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                self.set_status(NotificationLevel::Error, t(k::MCP_STATUS_SPEC_NOT_OBJECT));
                cx.notify();
                return;
            }
            Err(err) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::MCP_STATUS_JSON_INVALID, error = err),
                );
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
                let saved = if self.form_mode == FormMode::Edit {
                    t(k::MCP_STATUS_SAVED)
                } else {
                    t(k::MCP_STATUS_CREATED)
                };
                self.set_status(NotificationLevel::Success, saved);
                self.form_mode = FormMode::List;
                self.clear_form(cx);
                self.reload();
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::MCP_STATUS_SAVE_FAILED, error = err),
            ),
        }
        cx.notify();
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        match McpService::delete_server(&self.app, &id) {
            Ok(true) => self.set_status(NotificationLevel::Success, t(k::MCP_STATUS_DELETED)),
            Ok(false) => self.set_status(NotificationLevel::Warning, t(k::MCP_STATUS_MISSING)),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::MCP_STATUS_DELETE_FAILED, error = err),
            ),
        }
        self.reload();
        cx.notify();
    }

    fn do_sync(&mut self, cx: &mut Context<Self>) {
        match McpService::sync_all_enabled(&self.app) {
            Ok(()) => self.set_status(NotificationLevel::Success, t(k::MCP_STATUS_SYNCED)),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::MCP_STATUS_SYNC_FAILED, error = err),
            ),
        }
        cx.notify();
    }

    fn do_toggle_app(&mut self, id: String, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        match McpService::toggle_app(&self.app, &id, app, enabled) {
            Ok(()) => self.set_status(NotificationLevel::Success, t(k::MCP_STATUS_APP_TOGGLED)),
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::MCP_STATUS_UPDATE_FAILED, error = err),
            ),
        }
        self.reload();
        cx.notify();
    }

    fn do_import_all(&mut self, cx: &mut Context<Self>) {
        let imports = [
            ("Claude", McpService::import_from_claude(&self.app)),
            ("Codex", McpService::import_from_codex(&self.app)),
            ("Grok Build", McpService::import_from_grokbuild(&self.app)),
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

        // 部分应用导入失败时仍算完成，但降级为警告，避免看起来像全量成功。
        if failures.is_empty() {
            self.set_status(
                NotificationLevel::Success,
                tf!(k::MCP_STATUS_IMPORTED, count = total),
            );
        } else {
            self.set_status(
                NotificationLevel::Warning,
                tf!(
                    k::MCP_STATUS_IMPORTED_PARTIAL,
                    count = total,
                    failures = failures.join("; ")
                ),
            );
        }
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
            tf!(k::MCP_CARD_APP_TOGGLE_ARIA, app = Self::app_label(app)),
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
                            .child(div().text_color(theme::teal()).text_xs().child(
                                SharedString::from(tf!(k::MCP_CARD_APPS_LABEL, apps = apps)),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                components::button(
                                    format!("mcp-edit-{}", server.id),
                                    t(k::MCP_CARD_EDIT),
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
                                    t(k::MCP_CARD_DELETE),
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
            tf!(k::MCP_FORM_APP_TOGGLE_ARIA, app = Self::app_label(app)),
            enabled,
            Self::app_label(app),
        )
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.set_form_app(app, !enabled, cx);
        }))
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.form_mode == FormMode::Edit {
            t(k::MCP_FORM_TITLE_EDIT)
        } else {
            t(k::MCP_FORM_TITLE_ADD)
        };

        layout::page()
            .child(
                layout::page_header(title, None).child(
                    components::button(
                        "mcp-form-back",
                        t(k::MCP_FORM_BACK),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.cancel_form(cx);
                    })),
                ),
            )
            .child(layout::scroll_body(
                "mcp-form-body",
                &self.form_scroll_handle,
                layout::content_column().child(
                    components::card()
                        .gap_4()
                        .child(components::field(
                            t(k::MCP_FORM_NAME_LABEL),
                            false,
                            None,
                            self.name.clone(),
                        ))
                        .child(components::field(
                            t(k::MCP_FORM_DESCRIPTION_LABEL),
                            false,
                            None,
                            self.description.clone(),
                        ))
                        .child(components::field(
                            t(k::MCP_FORM_APPS_LABEL),
                            false,
                            None,
                            div().flex().flex_row().flex_wrap().gap_3().children(
                                Self::mcp_apps()
                                    .into_iter()
                                    .map(|app| self.render_form_app_toggle(app, cx)),
                            ),
                        ))
                        .child(components::field(
                            t(k::MCP_FORM_SPEC_LABEL),
                            false,
                            Some(t(k::MCP_FORM_SPEC_HELP)),
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
                                        t(k::MCP_FORM_SAVE),
                                        ButtonTone::Primary,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.do_save(cx);
                                        },
                                    )),
                                )
                                .child(
                                    components::button(
                                        "mcp-form-cancel",
                                        t(k::MCP_FORM_CANCEL),
                                        ButtonTone::Neutral,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.cancel_form(cx);
                                        },
                                    )),
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

        let mut plan: Vec<McpRow> = Vec::new();
        if self.servers.is_empty() {
            plan.push(McpRow::EmptyState);
        } else {
            plan.extend((0..self.servers.len()).map(McpRow::Card));
        }
        if self.list_state.item_count() != plan.len() {
            self.list_state.reset(plan.len());
        }

        let list = gpui::list(
            self.list_state.clone(),
            cx.processor(move |this, ix: usize, _window, cx| {
                // 每行自带底部间距（list 不画行间 gap）；pb_3 对齐 content_column 的默认 gap。
                let block = div().w_full().pb_3();
                match plan.get(ix).copied() {
                    Some(McpRow::EmptyState) => block
                        .child(components::empty_state(
                            IconName::Blocks,
                            t(k::MCP_EMPTY_TITLE),
                            t(k::MCP_EMPTY_HINT),
                            Some(
                                components::button(
                                    "mcp-add-empty",
                                    t(k::MCP_EMPTY_ACTION),
                                    ButtonTone::Primary,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.start_add(cx);
                                }))
                                .into_any_element(),
                            ),
                        ))
                        .into_any_element(),
                    Some(McpRow::Card(pix)) => match this.servers.get(pix) {
                        Some(server) => {
                            let card = this.render_card(server, cx);
                            block.child(card).into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    },
                    None => gpui::Empty.into_any_element(),
                }
            }),
        );

        layout::page()
            .relative()
            .child(
                layout::page_header(t(k::MCP_PAGE_TITLE), Some(t(k::MCP_PAGE_SUBTITLE))).child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            components::icon_button_tone(
                                "mcp-add",
                                t(k::MCP_ACTION_ADD),
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
                                t(k::MCP_ACTION_IMPORT),
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
                                t(k::MCP_ACTION_SYNC),
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
            .child(layout::virtual_body(
                "mcp-list-body",
                list,
                &self.list_state,
            ))
            .when_some(self.confirm_delete.clone(), |root, target| {
                let (delete_id, name) = target;
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::MCP_DELETE_TITLE)))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(tf!(k::MCP_DELETE_MESSAGE, name = name)),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "mcp-confirm-delete-cancel",
                                t(k::MCP_DELETE_CANCEL),
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
                                t(k::MCP_DELETE_CONFIRM),
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

crate::notifications::impl_status_toasts_leveled!(McpView);
