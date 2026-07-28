//! MCP servers panel. Manages the unified MCP server registry and syncs enabled
//! servers into each supported client configuration.

use std::sync::Arc;

use gpui::{
    Context, Entity, FontWeight, ListAlignment, ListState, ScrollHandle, SharedString, Window, div,
    prelude::*, px,
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
    reload_generation: u64,
    io_busy: bool,
    enabled_apps: Arc<[AppType]>,
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
        Self {
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
            reload_generation: 0,
            io_busy: false,
            enabled_apps: crate::app_meta::enabled_mcp_apps().into(),
        }
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

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.enabled_apps = crate::app_meta::enabled_mcp_apps().into();
        self.reload_generation = self.reload_generation.wrapping_add(1);
        let generation = self.reload_generation;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let servers = cx
                .background_spawn(async move {
                    McpService::get_all_servers(&app)
                        .map(|map| map.into_values().collect::<Vec<_>>())
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.reload_generation {
                    return;
                }
                match servers {
                    Ok(servers) => this.servers = servers,
                    Err(error) => {
                        this.servers.clear();
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::MCP_STATUS_LOAD_FAILED, error = error),
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

    pub fn refresh_apps(&mut self, cx: &mut Context<Self>) {
        self.enabled_apps = crate::app_meta::enabled_mcp_apps().into();
        self.list_state.remeasure();
        cx.notify();
    }

    fn run_io<R, Work, Apply>(&mut self, cx: &mut Context<Self>, work: Work, apply: Apply)
    where
        R: Send + 'static,
        Work: FnOnce() -> R + Send + 'static,
        Apply: FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    {
        if self.io_busy {
            return;
        }
        self.io_busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { work() }).await;
            this.update(cx, |this, cx| {
                this.io_busy = false;
                apply(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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

    fn start_edit_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(server) = self.servers.iter().find(|server| server.id == id).cloned() {
            self.start_edit(server, cx);
        }
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

        let app = self.app.clone();
        let editing = self.form_mode == FormMode::Edit;
        self.run_io(
            cx,
            move || McpService::upsert_server(&app, server).map_err(|error| error.to_string()),
            move |this, result, cx| match result {
                Ok(()) => {
                    let saved = if editing {
                        t(k::MCP_STATUS_SAVED)
                    } else {
                        t(k::MCP_STATUS_CREATED)
                    };
                    this.set_status(NotificationLevel::Success, saved);
                    this.form_mode = FormMode::List;
                    this.clear_form(cx);
                    this.reload(cx);
                }
                Err(error) => this.set_status(
                    NotificationLevel::Error,
                    tf!(k::MCP_STATUS_SAVE_FAILED, error = error),
                ),
            },
        );
    }

    fn do_delete(&mut self, id: String, cx: &mut Context<Self>) {
        let app = self.app.clone();
        self.run_io(
            cx,
            move || McpService::delete_server(&app, &id).map_err(|error| error.to_string()),
            |this, result, cx| {
                match result {
                    Ok(true) => {
                        this.set_status(NotificationLevel::Success, t(k::MCP_STATUS_DELETED))
                    }
                    Ok(false) => {
                        this.set_status(NotificationLevel::Warning, t(k::MCP_STATUS_MISSING))
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::MCP_STATUS_DELETE_FAILED, error = error),
                    ),
                }
                this.reload(cx);
            },
        );
    }

    fn do_sync(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        self.run_io(
            cx,
            move || McpService::sync_all_enabled(&app).map_err(|error| error.to_string()),
            |this, result, _cx| match result {
                Ok(()) => this.set_status(NotificationLevel::Success, t(k::MCP_STATUS_SYNCED)),
                Err(error) => this.set_status(
                    NotificationLevel::Error,
                    tf!(k::MCP_STATUS_SYNC_FAILED, error = error),
                ),
            },
        );
    }

    fn do_toggle_app(&mut self, id: String, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        let state = self.app.clone();
        self.run_io(
            cx,
            move || {
                McpService::toggle_app(&state, &id, app, enabled).map_err(|error| error.to_string())
            },
            |this, result, cx| {
                match result {
                    Ok(()) => {
                        this.set_status(NotificationLevel::Success, t(k::MCP_STATUS_APP_TOGGLED))
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::MCP_STATUS_UPDATE_FAILED, error = error),
                    ),
                }
                this.reload(cx);
            },
        );
    }

    fn do_import_all(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        self.run_io(
            cx,
            move || {
                let imports = [
                    ("Claude", McpService::import_from_claude(&app)),
                    ("Codex", McpService::import_from_codex(&app)),
                    ("Grok Build", McpService::import_from_grokbuild(&app)),
                    ("OpenCode", McpService::import_from_opencode(&app)),
                    ("Hermes", McpService::import_from_hermes(&app)),
                ];
                let mut total = 0usize;
                let mut failures = Vec::new();
                for (label, result) in imports {
                    match result {
                        Ok(count) => total += count,
                        Err(error) => failures.push(format!("{label}: {error}")),
                    }
                }
                (total, failures)
            },
            |this, (total, failures), cx| {
                if failures.is_empty() {
                    this.set_status(
                        NotificationLevel::Success,
                        tf!(k::MCP_STATUS_IMPORTED, count = total),
                    );
                } else {
                    this.set_status(
                        NotificationLevel::Warning,
                        tf!(
                            k::MCP_STATUS_IMPORTED_PARTIAL,
                            count = total,
                            failures = failures.join("; ")
                        ),
                    );
                }
                this.reload(cx);
            },
        );
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
    ) -> impl IntoElement + use<> {
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

    fn render_card(&self, server: &McpServer, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let delete_id = server.id.clone();
        let delete_name = server.name.clone();
        let edit_id = server.id.clone();
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
                                        this.start_edit_by_id(&edit_id, cx);
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
                    self.enabled_apps
                        .iter()
                        .copied()
                        .map(|app| self.render_app_toggle(server, app, cx)),
                ),
            )
    }

    fn render_form_app_toggle(
        &self,
        app: AppType,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
                                self.enabled_apps
                                    .iter()
                                    .copied()
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

        let empty = self.servers.is_empty();
        let row_count = self.servers.len().max(1);
        if self.list_state.item_count() != row_count {
            self.list_state.reset(row_count);
        }

        let list = gpui::list(
            self.list_state.clone(),
            cx.processor(move |this, ix: usize, _window, cx| {
                // 每行自带底部间距（list 不画行间 gap）；pb_3 对齐 content_column 的默认 gap。
                let block = div().w_full().pb_3();
                if empty {
                    block
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
                        .into_any_element()
                } else {
                    match this.servers.get(ix) {
                        Some(server) => {
                            let card = this.render_card(server, cx);
                            block.child(card).into_any_element()
                        }
                        None => gpui::Empty.into_any_element(),
                    }
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
