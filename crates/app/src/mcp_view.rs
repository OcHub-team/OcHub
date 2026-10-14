//! MCP servers panel. Manages the unified MCP server registry and syncs enabled
//! servers into each supported client configuration.

use std::future::Future;
use std::sync::Arc;

use gpui::{
    Context, Entity, ListAlignment, ListState, ScrollHandle, SharedString, Window, div, prelude::*,
    px,
};
use ochub_core::db::legacy_json::{McpApps, McpServer};
use ochub_core::{AppState, AppType};

use crate::components::{self, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::product_ui::tools as tool_screens;
use crate::remote::WorkspaceBackend;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormMode {
    List,
    Add,
    Edit,
}

/// Keep the last successful data while a refresh is pending or has failed.
#[derive(Default)]
struct ServerLoad {
    servers: Vec<McpServer>,
    loading: bool,
    error: Option<SharedString>,
}

impl ServerLoad {
    fn begin(&mut self) {
        self.loading = true;
        self.error = None;
    }

    fn finish(&mut self, result: Result<Vec<McpServer>, SharedString>) {
        self.loading = false;
        match result {
            Ok(servers) => {
                self.servers = servers;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn needs_status_row(&self) -> bool {
        self.loading || self.error.is_some() || self.servers.is_empty()
    }
}

impl gpui::EventEmitter<crate::interaction::DiscardForm> for McpView {}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::interaction::LeaveState;

    #[gpui::test]
    fn draft_survives_disconnect_and_clean_cancel_needs_no_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        let app = Arc::new(AppState::new(Arc::new(
            ochub_core::Database::memory().unwrap(),
        )));
        let view = cx.new(|cx| McpView::new(app, cx));
        view.update(cx, |view, cx| {
            view.start_add(cx);
            assert_eq!(view.leave_state(cx), LeaveState::Clean);
            view.name
                .update(cx, |input, cx| input.set_content("unsaved server", cx));
            assert_eq!(view.leave_state(cx), LeaveState::Dirty);
            view.set_workspace_unavailable(Vec::new(), cx);
            assert_eq!(view.name.read(cx).content().as_ref(), "unsaved server");
            assert_eq!(view.leave_state(cx), LeaveState::Dirty);
            view.discard_changes(cx);
            assert_eq!(view.leave_state(cx), LeaveState::Clean);
        });
    }
}

pub struct McpView {
    backend: WorkspaceBackend,
    server_load: ServerLoad,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    form_mode: FormMode,
    expanded_server: Option<String>,
    maintenance_open: bool,
    form_baseline: Vec<String>,
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
    workspace_generation: u64,
    io_busy: bool,
    workspace_available: bool,
    enabled_apps: Arc<[AppType]>,
}

impl McpView {
    fn form_values(&self, cx: &gpui::App) -> Vec<String> {
        vec![
            self.name.read(cx).content().to_string(),
            self.description.read(cx).content().to_string(),
            self.spec_json.read(cx).content().to_string(),
            serde_json::to_string(&self.apps).expect("MCP app flags serialize"),
        ]
    }

    pub(crate) fn leave_state(&self, cx: &gpui::App) -> crate::interaction::LeaveState {
        crate::interaction::LeaveState::new(
            self.form_mode != FormMode::List && self.form_values(cx) != self.form_baseline,
            self.io_busy,
        )
    }

    pub(crate) fn discard_changes(&mut self, cx: &mut Context<Self>) {
        self.form_mode = FormMode::List;
        self.confirm_delete = None;
        self.clear_form(cx);
        cx.notify();
    }

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
            backend: WorkspaceBackend::local(app.clone()),
            server_load: ServerLoad::default(),
            status: None,
            status_level: None,
            form_mode: FormMode::List,
            expanded_server: None,
            maintenance_open: false,
            form_baseline: Vec::new(),
            editing_id: None,
            name,
            description,
            spec_json,
            apps: McpApps::default(),
            confirm_delete: None,
            list_state: ListState::new(0, ListAlignment::Top, px(512.)),
            form_scroll_handle: ScrollHandle::new(),
            reload_generation: 0,
            workspace_generation: 0,
            io_busy: false,
            workspace_available: true,
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
        self.reload_generation = self.reload_generation.wrapping_add(1);
        if !self.workspace_available {
            return;
        }
        let generation = self.reload_generation;
        let backend = self.backend.clone();
        self.server_load.begin();
        self.list_state.remeasure();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let servers = crate::core_async::run(async move {
                backend
                    .list_mcp_servers()
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                if generation != this.reload_generation {
                    return;
                }
                this.server_load.finish(servers.map_err(|error| {
                    SharedString::from(tf!(k::MCP_STATUS_LOAD_FAILED, error = error))
                }));
                this.list_state.remeasure();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn set_workspace(
        &mut self,
        backend: WorkspaceBackend,
        enabled_apps: Vec<AppType>,
        cx: &mut Context<Self>,
    ) {
        if self.backend.same_workspace(&backend) {
            if !self.backend.same_target(&backend) {
                self.workspace_generation = self.workspace_generation.wrapping_add(1);
            }
            self.backend = backend;
            self.workspace_available = true;
            self.enabled_apps = enabled_apps.into();
            self.reload(cx);
            return;
        }
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.backend = backend;
        self.workspace_available = true;
        self.io_busy = false;
        self.enabled_apps = enabled_apps.into();
        self.server_load = ServerLoad::default();
        self.form_mode = FormMode::List;
        self.expanded_server = None;
        self.maintenance_open = false;
        self.confirm_delete = None;
        self.list_state.remeasure();
        self.reload(cx);
    }

    pub(crate) fn set_workspace_unavailable(
        &mut self,
        enabled_apps: Vec<AppType>,
        cx: &mut Context<Self>,
    ) {
        self.workspace_generation = self.workspace_generation.wrapping_add(1);
        self.workspace_available = false;
        self.reload_generation = self.reload_generation.wrapping_add(1);
        self.io_busy = false;
        self.enabled_apps = enabled_apps.into();
        self.server_load = ServerLoad::default();
        self.server_load.finish(Err(t(k::MCP_LOAD_UNAVAILABLE)));
        self.confirm_delete = None;
        self.list_state.remeasure();
        cx.notify();
    }

    pub fn refresh_apps(&mut self, cx: &mut Context<Self>) {
        self.enabled_apps = crate::app_meta::enabled_mcp_apps().into();
        self.list_state.remeasure();
        cx.notify();
    }

    fn run_backend<R, Fut, Work, Apply>(&mut self, cx: &mut Context<Self>, work: Work, apply: Apply)
    where
        R: Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        Work: FnOnce() -> Fut + Send + 'static,
        Apply: FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    {
        if self.io_busy || !self.workspace_available {
            return;
        }
        self.io_busy = true;
        let generation = self.workspace_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(work()).await;
            this.update(cx, |this, cx| {
                if generation != this.workspace_generation {
                    return;
                }
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
        if !self.workspace_available {
            return;
        }
        self.clear_form(cx);
        self.form_mode = FormMode::Add;
        self.form_baseline = self.form_values(cx);
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
        self.form_baseline = self.form_values(cx);
        self.clear_status();
        cx.notify();
    }

    fn start_edit_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(server) = self
            .server_load
            .servers
            .iter()
            .find(|server| server.id == id)
            .cloned()
        {
            self.start_edit(server, cx);
        }
    }

    fn cancel_form(&mut self, cx: &mut Context<Self>) {
        match self.leave_state(cx) {
            crate::interaction::LeaveState::Busy => {}
            crate::interaction::LeaveState::Dirty => cx.emit(crate::interaction::DiscardForm(
                crate::interaction::FormOwner::Mcp,
            )),
            crate::interaction::LeaveState::Clean => self.discard_changes(cx),
        }
    }

    fn set_form_app(&mut self, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        self.apps.set_enabled_for(&app, enabled);
        cx.notify();
    }

    fn do_save(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_available {
            self.set_status(
                NotificationLevel::Error,
                tf!(
                    k::MCP_STATUS_SAVE_FAILED,
                    error = "remote workspace is not connected"
                ),
            );
            cx.notify();
            return;
        }
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

        let backend = self.backend.clone();
        let original_id = self.editing_id.clone();
        let editing = self.form_mode == FormMode::Edit;
        self.run_backend(
            cx,
            move || async move {
                backend
                    .upsert_mcp_server(original_id.as_deref(), server)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
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
        let backend = self.backend.clone();
        self.run_backend(
            cx,
            move || async move {
                backend
                    .delete_mcp_server(&id)
                    .await
                    .map_err(|error| error.to_string())
            },
            |this, result, cx| {
                match result {
                    Ok(()) => this.set_status(NotificationLevel::Success, t(k::MCP_STATUS_DELETED)),
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
        let backend = self.backend.clone();
        self.run_backend(
            cx,
            move || async move {
                backend
                    .sync_all_mcp_servers()
                    .await
                    .map_err(|error| error.to_string())
            },
            |this, result, _cx| match result {
                Ok(_) => this.set_status(NotificationLevel::Success, t(k::MCP_STATUS_SYNCED)),
                Err(error) => this.set_status(
                    NotificationLevel::Error,
                    tf!(k::MCP_STATUS_SYNC_FAILED, error = error),
                ),
            },
        );
    }

    fn do_toggle_app(&mut self, id: String, app: AppType, enabled: bool, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.run_backend(
            cx,
            move || async move {
                backend
                    .set_mcp_app(&id, &app.app_id(), enabled)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
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
        let backend = self.backend.clone();
        let apps = self.enabled_apps.clone();
        self.run_backend(
            cx,
            move || async move {
                let mut total = 0usize;
                let mut failures = Vec::new();
                for app in apps.iter().copied() {
                    let label = Self::app_label(app).to_string();
                    let result = backend.import_mcp_from_app(&app.app_id()).await;
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
        ochub_ui::keyboard::interactive(
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
                .child(div().text_color(theme::subtext()).text_xs().child(label)),
        )
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
        let expanded = self.expanded_server.as_deref() == Some(server.id.as_str());
        let disclosure_id = server.id.clone();

        tool_screens::mcp_card(tool_screens::McpCard {
            name: SharedString::from(name),
            endpoint: SharedString::from(endpoint),
            description: desc.map(SharedString::from),
            apps_label: SharedString::from(tf!(k::MCP_CARD_APPS_LABEL, apps = apps)),
            expanded,
            disclosure: components::icon_only_button_tone(
                format!("mcp-expand-{}", server.id),
                tf!(k::MCP_CARD_MANAGE_ARIA, name = server.name),
                if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .aria_expanded(expanded)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.expanded_server = if expanded {
                    None
                } else {
                    Some(disclosure_id.clone())
                };
                this.list_state.remeasure();
                cx.notify();
            }))
            .into_any_element(),
            actions: vec![
                components::button(
                    format!("mcp-edit-{}", server.id),
                    t(k::MCP_CARD_EDIT),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.start_edit_by_id(&edit_id, cx);
                }))
                .into_any_element(),
                components::button(
                    format!("mcp-delete-{}", server.id),
                    t(k::MCP_CARD_DELETE),
                    ButtonTone::Danger,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.confirm_delete = Some((delete_id.clone(), delete_name.clone()));
                    cx.notify();
                }))
                .into_any_element(),
            ],
            app_toggles: self
                .enabled_apps
                .iter()
                .copied()
                .map(|app| self.render_app_toggle(server, app, cx).into_any_element())
                .collect(),
        })
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

    fn render_load_state(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.server_load.loading {
            return components::card()
                .flex_row()
                .items_center()
                .gap_2()
                .child(components::spinner(theme::muted(), 16.))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::muted())
                        .child(t(k::MCP_LOAD_LOADING)),
                )
                .into_any_element();
        }
        if let Some(error) = &self.server_load.error {
            return components::empty_state(
                IconName::Refresh,
                t(k::MCP_LOAD_FAILED_TITLE),
                error.clone(),
                self.workspace_available.then(|| {
                    components::button(
                        "mcp-retry",
                        t(k::MCP_LOAD_RETRY),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| this.reload(cx)))
                    .into_any_element()
                }),
            )
            .into_any_element();
        }
        components::empty_state(
            IconName::Blocks,
            t(k::MCP_EMPTY_TITLE),
            t(k::MCP_EMPTY_HINT),
            Some(
                components::button(
                    "mcp-import-empty",
                    t(k::MCP_ACTION_IMPORT),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.do_import_all(cx)))
                .into_any_element(),
            ),
        )
        .into_any_element()
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

        let has_status_row = self.server_load.needs_status_row();
        let row_count = self.server_load.servers.len() + usize::from(has_status_row);
        if self.list_state.item_count() != row_count {
            self.list_state.reset(row_count);
        }

        let list = gpui::list(
            self.list_state.clone(),
            cx.processor(move |this, ix: usize, _window, cx| {
                // 每行自带底部间距（list 不画行间 gap）；pb_3 对齐 content_column 的默认 gap。
                let block = div().w_full().pb_3();
                if has_status_row && ix == 0 {
                    block.child(this.render_load_state(cx)).into_any_element()
                } else {
                    match this
                        .server_load
                        .servers
                        .get(ix - usize::from(has_status_row))
                    {
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
                layout::page_header(t(k::MCP_PAGE_TITLE), None).child(
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
                            components::icon_only_button_tone(
                                "mcp-maintenance",
                                t(k::MCP_ACTION_MORE),
                                IconName::Settings,
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .aria_expanded(self.maintenance_open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.maintenance_open = !this.maintenance_open;
                                cx.notify();
                            })),
                        ),
                ),
            )
            .when(self.maintenance_open, |page| {
                page.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .px_6()
                        .pb_3()
                        .child(
                            components::button(
                                "mcp-import-all",
                                t(k::MCP_ACTION_IMPORT),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.do_import_all(cx))),
                        )
                        .child(
                            components::button(
                                "mcp-sync",
                                t(k::MCP_ACTION_SYNC),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.do_sync(cx))),
                        ),
                )
            })
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

#[cfg(test)]
mod tests {
    use super::{McpApps, McpServer, ServerLoad};

    fn server(id: &str) -> McpServer {
        McpServer {
            id: id.into(),
            name: id.into(),
            server: serde_json::json!({"type": "stdio", "command": "synthetic"}),
            apps: McpApps::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn failed_refresh_keeps_servers_until_an_authoritative_success() {
        let mut load = ServerLoad::default();
        load.finish(Ok(vec![server("existing")]));
        assert!(!load.needs_status_row());
        load.begin();
        assert!(load.loading);
        assert_eq!(load.servers[0].id, "existing");
        load.finish(Err("offline".into()));
        assert!(!load.loading);
        assert!(load.needs_status_row());
        assert_eq!(load.error.as_deref(), Some("offline"));
        assert_eq!(load.servers[0].id, "existing");
        load.begin();
        load.finish(Ok(Vec::new()));
        assert!(load.servers.is_empty());
        assert!(load.error.is_none());
        assert!(load.needs_status_row());
    }

    #[test]
    fn first_load_failure_remains_distinct_from_a_successful_empty_list() {
        let mut load = ServerLoad::default();
        load.begin();
        assert!(load.loading && load.servers.is_empty());
        load.finish(Err("unavailable".into()));
        assert!(load.error.is_some());
        assert!(load.servers.is_empty());
        load.begin();
        assert!(load.loading && load.error.is_none());
        load.finish(Ok(Vec::new()));
        assert!(!load.loading && load.error.is_none());
    }
}
