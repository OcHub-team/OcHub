//! User-facing relay-station manager.
//!
//! A station is presented as one complete commercial relay configuration
//! (New API, Sub2API, or another compatible service). The local gateway,
//! per-CLI keys, route bindings, and protocol conversion remain implementation
//! details and are deliberately hidden from this page.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    div, point, prelude::*, px, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    ScrollHandle, SharedString, Window,
};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{
    Dialect, GatewayChannel, GatewayModelRule, GatewayReasoningConfig, GatewayReasoningMode,
    GatewayRoute,
};
use ochub_core::services::provider::ProviderService;
use ochub_core::{AppState, AppType};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::{icon, IconName};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::text_input::TextInput;
use crate::tf;
use crate::theme;

/// Events the app shell reacts to (navigation requests).
pub enum GatewayEvent {
    /// Open the Providers page for this app (e.g. to switch it off a station).
    OpenProviders(AppType),
}

#[derive(Clone)]
struct RelayStation {
    channel: GatewayChannel,
    route: GatewayRoute,
}

#[derive(Clone)]
struct ImportCandidate {
    app_type: AppType,
    provider_id: String,
    name: String,
    base_url: String,
}

struct ModelRuleEditor {
    id: u64,
    client_model: Entity<TextInput>,
    station_model: Entity<TextInput>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Idle,
    Running,
    Detected(Dialect),
    Failed,
}

struct StationEditor {
    channel_id: String,
    route_id: String,
    created_at: i64,
    name: Entity<TextInput>,
    base_url: Entity<TextInput>,
    api_key: Entity<TextInput>,
    default_model: Entity<TextInput>,
    dialect: Dialect,
    rules: Vec<ModelRuleEditor>,
    reasoning_mode: GatewayReasoningMode,
    low_budget: Entity<TextInput>,
    medium_budget: Entity<TextInput>,
    high_budget: Entity<TextInput>,
    max_budget: Entity<TextInput>,
    enabled: bool,
    probe: ProbeState,
    reveal_key: bool,
    name_error: Option<SharedString>,
    budget_error: Option<SharedString>,
    rules_error: Option<SharedString>,
}

pub struct GatewayView {
    app: Arc<AppState>,
    stations: Vec<RelayStation>,
    import_candidates: Vec<ImportCandidate>,
    active_station_by_app: HashMap<AppType, String>,
    editor: Option<StationEditor>,
    next_rule_id: u64,
    show_imports: bool,
    applying: Option<(String, AppType)>,
    confirm_delete: Option<(String, String)>,
    /// Deleting was refused: (station name, apps still using it).
    delete_blocked: Option<(SharedString, Vec<AppType>)>,
    show_connection: bool,
    connection_loading: bool,
    connection_info: Option<apply::ApplyResult>,
    reveal_connection_key: bool,
    scroll: ScrollHandle,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
}

impl EventEmitter<GatewayEvent> for GatewayView {}

impl GatewayView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, which covers every label the page
    /// draws. It does not reach the open editor: a `TextInput` captures its
    /// placeholder when it is constructed, and the inline field errors were
    /// resolved when a save was refused, so both otherwise survive in the
    /// language they were made in. The station list is a plain scroll body
    /// rather than a virtualized list, so there are no memoized item heights
    /// to invalidate here.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            cx.notify();
            return;
        };

        // Which fields are flagged does not change — only the wording does.
        if editor.name_error.is_some() {
            editor.name_error = Some(t(k::GATEWAY_EDITOR_ERROR_NAME));
        }
        if editor.budget_error.is_some() {
            editor.budget_error = Some(t(k::GATEWAY_EDITOR_ERROR_BUDGET));
        }
        if editor.rules_error.is_some() {
            editor.rules_error = Some(t(k::GATEWAY_EDITOR_ERROR_RULES));
        }

        // The base URL and the four budget inputs hint at a format
        // (`https://api.example.com`, `4096`) rather than at prose, so they
        // read the same in every language and are left alone.
        let mut placeholders: Vec<(Entity<TextInput>, SharedString)> = vec![
            (editor.name.clone(), t(k::GATEWAY_EDITOR_NAME_PLACEHOLDER)),
            (
                editor.api_key.clone(),
                t(k::GATEWAY_EDITOR_API_KEY_PLACEHOLDER),
            ),
            (
                editor.default_model.clone(),
                t(k::GATEWAY_EDITOR_DEFAULT_MODEL_PLACEHOLDER),
            ),
        ];
        for rule in &editor.rules {
            placeholders.push((
                rule.client_model.clone(),
                t(k::GATEWAY_EDITOR_RULE_CLIENT_MODEL_PLACEHOLDER),
            ));
            placeholders.push((
                rule.station_model.clone(),
                t(k::GATEWAY_EDITOR_RULE_STATION_MODEL_PLACEHOLDER),
            ));
        }
        for (input, placeholder) in placeholders {
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        cx.notify();
    }

    pub fn new(app: Arc<AppState>, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            app,
            stations: Vec::new(),
            import_candidates: Vec::new(),
            active_station_by_app: HashMap::new(),
            editor: None,
            next_rule_id: 1,
            show_imports: false,
            applying: None,
            confirm_delete: None,
            delete_blocked: None,
            show_connection: false,
            connection_loading: false,
            connection_info: None,
            reveal_connection_key: false,
            scroll: ScrollHandle::new(),
            status: None,
            status_level: None,
        };
        view.reload(cx);
        view
    }

    pub(crate) fn shortcut_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_delete.is_some() {
            window.play_system_bell();
        } else if self.editor.is_some() {
            self.save_editor(cx);
        } else {
            window.play_system_bell();
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.delete_blocked.take().is_some()
            || self.confirm_delete.take().is_some()
            || self.editor.take().is_some()
        {
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let channels = self.app.db.get_gateway_channels().unwrap_or_default();
        let routes = self.app.db.get_gateway_routes().unwrap_or_default();
        let route_map: HashMap<String, GatewayRoute> = routes
            .into_iter()
            .map(|route| (route.id.clone(), route))
            .collect();

        self.stations = channels
            .into_iter()
            .map(|channel| {
                let route_id = apply::station_route_id(&channel.id);
                let route = route_map
                    .get(&route_id)
                    .cloned()
                    .unwrap_or_else(|| GatewayRoute {
                        id: route_id,
                        name: channel.name.clone(),
                        app_type: None,
                        channel_ids: vec![channel.id.clone()],
                        default_model: None,
                        model_rules: Vec::new(),
                        reasoning: GatewayReasoningConfig::default(),
                        enabled: channel.enabled,
                        created_at: chrono::Utc::now().timestamp(),
                    });
                RelayStation { channel, route }
            })
            .collect();

        let keys = self.app.db.get_gateway_keys().unwrap_or_default();
        self.active_station_by_app.clear();
        for app_type in apply::supported_apps() {
            // Switch-mode apps count as connected when the gateway provider is
            // current; additive apps have no "current", so the managed gateway
            // provider entry existing is the connected signal.
            let connected = if app_type.is_additive_mode() {
                self.app
                    .db
                    .get_provider_by_id(apply::GATEWAY_PROVIDER_ID, app_type.as_str())
                    .is_ok_and(|provider| provider.is_some())
            } else {
                ProviderService::current(&self.app, *app_type)
                    .is_ok_and(|current| current == apply::GATEWAY_PROVIDER_ID)
            };
            if connected {
                if let Some(route_id) = keys
                    .iter()
                    .find(|key| key.name == app_type.as_str() && key.enabled)
                    .and_then(|key| key.route_id.clone())
                {
                    self.active_station_by_app.insert(*app_type, route_id);
                }
            }
        }

        let enabled_apps = crate::app_meta::enabled_app_types();
        let mut candidates = Vec::new();
        let imported_ids: HashSet<String> = self
            .stations
            .iter()
            .map(|station| station.channel.id.clone())
            .collect();
        for app_type in enabled_apps {
            if let Ok(providers) = ProviderService::list(&self.app, app_type) {
                for provider in providers.into_values().filter(|provider| {
                    provider.id != apply::GATEWAY_PROVIDER_ID
                        && provider.category.as_deref() != Some("gateway")
                }) {
                    let channel_id = format!("imported-{}-{}", app_type.as_str(), provider.id);
                    if imported_ids.contains(&channel_id) {
                        continue;
                    }
                    let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
                    if !base_url.trim().is_empty() && !api_key.trim().is_empty() {
                        candidates.push(ImportCandidate {
                            app_type,
                            provider_id: provider.id,
                            name: provider.name,
                            base_url,
                        });
                    }
                }
            }
        }
        self.import_candidates = candidates;
        cx.notify();
    }

    fn open_editor(&mut self, station: Option<&RelayStation>, cx: &mut Context<Self>) {
        let (
            channel_id,
            route_id,
            created_at,
            name,
            base_url,
            api_key,
            default_model,
            dialect,
            rules,
            reasoning,
            enabled,
        ) = match station {
            Some(station) => (
                station.channel.id.clone(),
                station.route.id.clone(),
                station.route.created_at,
                station.channel.name.clone(),
                station.channel.base_url.clone(),
                station.channel.api_key.clone(),
                station.route.default_model.clone().unwrap_or_default(),
                station.channel.dialect,
                station.route.model_rules.clone(),
                station.route.reasoning.clone(),
                station.channel.enabled && station.route.enabled,
            ),
            None => {
                let channel_id = uuid::Uuid::new_v4().to_string();
                (
                    channel_id.clone(),
                    apply::station_route_id(&channel_id),
                    chrono::Utc::now().timestamp(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    Dialect::Messages,
                    Vec::new(),
                    GatewayReasoningConfig::default(),
                    true,
                )
            }
        };

        let mut rule_editors = Vec::new();
        for rule in rules {
            let id = self.next_rule_id;
            self.next_rule_id += 1;
            rule_editors.push(ModelRuleEditor {
                id,
                client_model: cx.new(|cx| {
                    text_input(
                        cx,
                        t(k::GATEWAY_EDITOR_RULE_CLIENT_MODEL_PLACEHOLDER),
                        &rule.model,
                    )
                }),
                station_model: cx.new(|cx| {
                    text_input(
                        cx,
                        t(k::GATEWAY_EDITOR_RULE_STATION_MODEL_PLACEHOLDER),
                        &rule.upstream_model,
                    )
                }),
            });
        }

        self.editor = Some(StationEditor {
            channel_id,
            route_id,
            created_at,
            name: cx.new(|cx| text_input(cx, t(k::GATEWAY_EDITOR_NAME_PLACEHOLDER), &name)),
            base_url: cx.new(|cx| text_input(cx, "https://api.example.com", &base_url)),
            api_key: cx.new(|cx| {
                text_input(cx, t(k::GATEWAY_EDITOR_API_KEY_PLACEHOLDER), &api_key).masked(true)
            }),
            default_model: cx.new(|cx| {
                text_input(
                    cx,
                    t(k::GATEWAY_EDITOR_DEFAULT_MODEL_PLACEHOLDER),
                    &default_model,
                )
            }),
            dialect,
            rules: rule_editors,
            reasoning_mode: reasoning.mode,
            low_budget: cx.new(|cx| text_input(cx, "4096", &reasoning.low_budget.to_string())),
            medium_budget: cx
                .new(|cx| text_input(cx, "10000", &reasoning.medium_budget.to_string())),
            high_budget: cx.new(|cx| text_input(cx, "16000", &reasoning.high_budget.to_string())),
            max_budget: cx.new(|cx| text_input(cx, "32000", &reasoning.max_budget.to_string())),
            enabled,
            probe: ProbeState::Idle,
            reveal_key: false,
            name_error: None,
            budget_error: None,
            rules_error: None,
        });
        // The editor renders pinned above the list; jump there so clicking
        // Edit on a card far down the page visibly responds.
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Every status toast carries its severity explicitly. Guessing it from the
    /// wording mis-reads several of these messages — a relay that is disabled
    /// refuses the apply, it does not succeed — and stops working altogether
    /// once the copy is translated.
    fn set_status(
        &mut self,
        level: NotificationLevel,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(text.into());
        self.status_level = Some(level);
        cx.notify();
    }

    fn add_model_rule(&mut self, cx: &mut Context<Self>) {
        let id = self.next_rule_id;
        self.next_rule_id += 1;
        if let Some(editor) = &mut self.editor {
            editor.rules.push(ModelRuleEditor {
                id,
                client_model: cx.new(|cx| {
                    TextInput::new(cx, t(k::GATEWAY_EDITOR_RULE_CLIENT_MODEL_PLACEHOLDER))
                }),
                station_model: cx.new(|cx| {
                    TextInput::new(cx, t(k::GATEWAY_EDITOR_RULE_STATION_MODEL_PLACEHOLDER))
                }),
            });
            cx.notify();
        }
    }

    fn remove_model_rule(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.rules.retain(|rule| rule.id != id);
            cx.notify();
        }
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let name = input_value(&editor.name, cx);
        let base_url = input_value(&editor.base_url, cx);
        let name_error: Option<SharedString> = if name.is_empty() || base_url.is_empty() {
            Some(t(k::GATEWAY_EDITOR_ERROR_NAME))
        } else {
            None
        };

        let budgets = (
            parse_budget(&editor.low_budget, cx),
            parse_budget(&editor.medium_budget, cx),
            parse_budget(&editor.high_budget, cx),
            parse_budget(&editor.max_budget, cx),
        );
        let mut budget_error: Option<SharedString> = None;
        let (low_budget, medium_budget, high_budget, max_budget) = match budgets {
            (Some(low), Some(medium), Some(high), Some(max))
                if low > 0 && low <= medium && medium <= high && high <= max =>
            {
                (low, medium, high, max)
            }
            _ => {
                budget_error = Some(t(k::GATEWAY_EDITOR_ERROR_BUDGET));
                (0, 0, 0, 0)
            }
        };

        let mut rules_error: Option<SharedString> = None;
        let mut rules = Vec::new();
        for rule in &editor.rules {
            let client_model = input_value(&rule.client_model, cx);
            let station_model = input_value(&rule.station_model, cx);
            if client_model.is_empty() && station_model.is_empty() {
                continue;
            }
            if client_model.is_empty() || station_model.is_empty() {
                rules_error = Some(t(k::GATEWAY_EDITOR_ERROR_RULES));
                break;
            }
            rules.push(GatewayModelRule {
                model: client_model,
                upstream_model: station_model,
                channel_id: Some(editor.channel_id.clone()),
            });
        }

        if name_error.is_some() || budget_error.is_some() || rules_error.is_some() {
            if let Some(editor) = &mut self.editor {
                editor.name_error = name_error;
                editor.budget_error = budget_error;
                editor.rules_error = rules_error;
            }
            cx.notify();
            return;
        }
        if let Some(editor) = &mut self.editor {
            editor.name_error = None;
            editor.budget_error = None;
            editor.rules_error = None;
        }
        let Some(editor) = &self.editor else {
            return;
        };

        let imported_from = self
            .stations
            .iter()
            .find(|station| station.channel.id == editor.channel_id)
            .and_then(|station| station.channel.imported_from.clone());
        let channel = GatewayChannel {
            id: editor.channel_id.clone(),
            name: name.clone(),
            dialect: editor.dialect,
            base_url,
            api_key: input_value(&editor.api_key, cx),
            path_override: None,
            models: Vec::new(),
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: editor.enabled,
            extra_headers: Vec::new(),
            imported_from,
        };
        let route = GatewayRoute {
            id: editor.route_id.clone(),
            name: name.clone(),
            app_type: None,
            channel_ids: vec![editor.channel_id.clone()],
            default_model: nonempty(input_value(&editor.default_model, cx)),
            model_rules: rules,
            reasoning: GatewayReasoningConfig {
                mode: editor.reasoning_mode,
                low_budget,
                medium_budget,
                high_budget,
                max_budget,
            },
            enabled: editor.enabled,
            created_at: editor.created_at,
        };

        let result = self
            .app
            .db
            .upsert_gateway_channel(&channel)
            .and_then(|_| self.app.db.upsert_gateway_route(&route));
        match result {
            Ok(()) => {
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::GATEWAY_STATUS_SAVED, name = name),
                    cx,
                );
                self.editor = None;
                self.reload(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_SAVE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn toggle_station(&mut self, route_id: String, cx: &mut Context<Self>) {
        let Some(station) = self
            .stations
            .iter()
            .find(|station| station.route.id == route_id)
            .cloned()
        else {
            return;
        };
        let enabled = !(station.channel.enabled && station.route.enabled);
        let mut channel = station.channel;
        let mut route = station.route;
        channel.enabled = enabled;
        route.enabled = enabled;
        match self
            .app
            .db
            .upsert_gateway_channel(&channel)
            .and_then(|_| self.app.db.upsert_gateway_route(&route))
        {
            Ok(()) => {
                let message = if enabled {
                    tf!(k::GATEWAY_STATUS_ENABLED, name = channel.name)
                } else {
                    tf!(k::GATEWAY_STATUS_DISABLED, name = channel.name)
                };
                self.set_status(NotificationLevel::Success, message, cx);
                self.reload(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_UPDATE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn request_delete(&mut self, route_id: String, name: String, cx: &mut Context<Self>) {
        let active_apps: Vec<AppType> = self
            .active_station_by_app
            .iter()
            .filter(|(_, active_route)| **active_route == route_id)
            .map(|(app, _)| *app)
            .collect();
        if !active_apps.is_empty() {
            self.delete_blocked = Some((name.into(), active_apps));
            cx.notify();
            return;
        }
        self.confirm_delete = Some((route_id, name));
        cx.notify();
    }

    fn delete_station(&mut self, route_id: String, cx: &mut Context<Self>) {
        let channel_id = self
            .stations
            .iter()
            .find(|station| station.route.id == route_id)
            .map(|station| station.channel.id.clone());
        let result = self.app.db.delete_gateway_route(&route_id).and_then(|_| {
            if let Some(channel_id) = channel_id {
                self.app.db.delete_gateway_channel(&channel_id)?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.set_status(NotificationLevel::Success, t(k::GATEWAY_STATUS_DELETED), cx);
                self.reload(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_DELETE_FAILED, error = err),
                cx,
            ),
        }
    }

    fn import_provider(&mut self, candidate: ImportCandidate, cx: &mut Context<Self>) {
        match apply::import_provider_as_channel(
            &self.app,
            candidate.app_type,
            &candidate.provider_id,
        )
        .and_then(|channel| {
            let route = apply::ensure_station_route(&self.app, &channel)?;
            Ok((channel, route))
        }) {
            Ok((channel, _)) => {
                self.set_status(
                    NotificationLevel::Success,
                    tf!(k::GATEWAY_STATUS_IMPORTED, name = channel.name),
                    cx,
                );
                self.show_imports = false;
                self.reload(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_IMPORT_FAILED, error = err),
                cx,
            ),
        }
    }

    fn apply_station(&mut self, route_id: String, app_type: AppType, cx: &mut Context<Self>) {
        if self.applying.is_some() {
            return;
        }
        let Some(station) = self
            .stations
            .iter()
            .find(|station| station.route.id == route_id)
            .cloned()
        else {
            return;
        };
        if !station.channel.enabled || !station.route.enabled {
            self.set_status(
                NotificationLevel::Warning,
                t(k::GATEWAY_STATUS_DISABLED_BLOCKED),
                cx,
            );
            return;
        }
        if !apply::dialect_compatible(station.channel.dialect, app_type) {
            self.set_status(
                NotificationLevel::Warning,
                tf!(
                    k::GATEWAY_STATUS_DIALECT_UNSUPPORTED,
                    app = crate::app_meta::label(app_type),
                    name = station.channel.name,
                ),
                cx,
            );
            return;
        }
        if let Err(err) = self.app.db.upsert_gateway_route(&station.route) {
            self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_PREPARE_FAILED, error = err),
                cx,
            );
            return;
        }

        self.applying = Some((route_id.clone(), app_type));
        self.set_status(
            NotificationLevel::Info,
            tf!(
                k::GATEWAY_STATUS_APPLYING,
                name = station.channel.name,
                app = crate::app_meta::label(app_type),
            ),
            cx,
        );

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let mut config = app.db.get_gateway_config()?;
                if !config.enabled {
                    config.enabled = true;
                    app.db.set_gateway_config(&config)?;
                }
                let status = app.gateway.start().await?;
                let base_url = status.base_url;
                let route_id_for_apply = route_id.clone();
                let app_for_apply = app.clone();
                cx.background_spawn(async move {
                    apply::apply_station_to_app(
                        &app_for_apply,
                        app_type,
                        &base_url,
                        &route_id_for_apply,
                    )
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.applying = None;
                match result {
                    Ok(applied) => {
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(
                                k::GATEWAY_STATUS_APPLIED,
                                name = applied.route_name,
                                app = crate::app_meta::label(app_type),
                            ),
                            cx,
                        );
                        this.reload(cx);
                    }
                    Err(err) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::GATEWAY_STATUS_APPLY_FAILED, error = err),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_connection_panel(&mut self, cx: &mut Context<Self>) {
        self.show_connection = !self.show_connection;
        if self.show_connection && self.connection_info.is_none() {
            self.load_connection_info(cx);
        }
        cx.notify();
    }

    fn load_connection_info(&mut self, cx: &mut Context<Self>) {
        if self.connection_loading {
            return;
        }
        self.connection_loading = true;
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let mut config = app.db.get_gateway_config()?;
                if !config.enabled {
                    config.enabled = true;
                    app.db.set_gateway_config(&config)?;
                }
                let status = app.gateway.start().await?;
                let base_url = status.base_url;
                let app_for_info = app.clone();
                cx.background_spawn(
                    async move { apply::generic_client_info(&app_for_info, &base_url) },
                )
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.connection_loading = false;
                match result {
                    Ok(info) => {
                        this.connection_info = Some(info);
                    }
                    Err(err) => {
                        this.show_connection = false;
                        this.set_status(
                            NotificationLevel::Error,
                            tf!(k::GATEWAY_STATUS_CONNECTION_INFO_FAILED, error = err),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn detect_dialect(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.probe == ProbeState::Running {
            return;
        }
        let base_url = input_value(&editor.base_url, cx);
        let api_key = input_value(&editor.api_key, cx);
        if let Some(editor) = &mut self.editor {
            if base_url.is_empty() {
                editor.probe = ProbeState::Failed;
                cx.notify();
                return;
            }
            editor.probe = ProbeState::Running;
        }
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let detected = app
                .gateway
                .detect_dialect(base_url, api_key)
                .await
                .ok()
                .flatten();
            this.update(cx, |this, cx| {
                if let Some(editor) = &mut this.editor {
                    match detected {
                        Some(dialect) => {
                            editor.dialect = dialect;
                            editor.probe = ProbeState::Detected(dialect);
                        }
                        None => editor.probe = ProbeState::Failed,
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_reveal_key(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.reveal_key = !editor.reveal_key;
            let masked = !editor.reveal_key;
            editor
                .api_key
                .update(cx, |input, cx| input.set_masked(masked, cx));
            cx.notify();
        }
    }

    /// Disconnect an additive app by removing its managed gateway provider
    /// entry (switch-mode apps instead navigate to the Providers page).
    fn disconnect_app(&mut self, app_type: AppType, cx: &mut Context<Self>) {
        match ProviderService::delete(&self.app, app_type, apply::GATEWAY_PROVIDER_ID) {
            Ok(()) => {
                self.set_status(
                    NotificationLevel::Success,
                    tf!(
                        k::GATEWAY_STATUS_DISCONNECTED,
                        app = crate::app_meta::label(app_type)
                    ),
                    cx,
                );
                self.reload(cx);
            }
            Err(err) => self.set_status(
                NotificationLevel::Error,
                tf!(k::GATEWAY_STATUS_DISCONNECT_FAILED, error = err),
                cx,
            ),
        }
    }

    fn copy_to_clipboard(&mut self, value: String, done: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.set_status(NotificationLevel::Success, done, cx);
    }

    fn render_import_candidate(
        &self,
        candidate: &ImportCandidate,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let candidate_for_import = candidate.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(candidate.name.clone())),
                            )
                            .child(components::badge(
                                BadgeTone::Neutral,
                                crate::app_meta::label(candidate.app_type),
                            )),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .truncate()
                            .child(SharedString::from(candidate.base_url.clone())),
                    ),
            )
            .child(
                components::button(
                    SharedString::from(format!(
                        "station-import-{}-{}",
                        candidate.app_type.as_str(),
                        candidate.provider_id
                    )),
                    t(k::GATEWAY_IMPORT_ACTION),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.import_provider(candidate_for_import.clone(), cx);
                })),
            )
            .into_any_element()
    }

    /// One-line "who is connected to what" summary. Hidden when every app is
    /// on a direct connection.
    fn render_summary_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.active_station_by_app.is_empty() {
            return None;
        }
        let mut apps: Vec<(AppType, String)> = self
            .active_station_by_app
            .iter()
            .map(|(app, route)| (*app, route.clone()))
            .collect();
        apps.sort_by_key(|(app, _)| app.as_str());
        let items: Vec<gpui::AnyElement> = apps
            .into_iter()
            .map(|(app_type, route_id)| {
                let station_name = self
                    .stations
                    .iter()
                    .find(|station| station.route.id == route_id)
                    .map(|station| station.channel.name.clone())
                    .unwrap_or_else(|| raw(k::GATEWAY_SUMMARY_UNKNOWN).to_string());
                let action = if app_type.is_additive_mode() {
                    components::button(
                        SharedString::from(format!("summary-disconnect-{}", app_type.as_str())),
                        t(k::GATEWAY_SUMMARY_DISCONNECT),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.disconnect_app(app_type, cx);
                    }))
                    .into_any_element()
                } else {
                    components::button(
                        SharedString::from(format!("summary-switch-{}", app_type.as_str())),
                        t(k::GATEWAY_ACTION_SWITCH),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                        cx.emit(GatewayEvent::OpenProviders(app_type));
                    }))
                    .into_any_element()
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(crate::app_meta::label(app_type)),
                    )
                    .child(div().text_color(theme::muted()).text_xs().child("→"))
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(station_name)),
                    )
                    .child(action)
                    .into_any_element()
            })
            .collect();

        Some(
            components::card()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_4()
                .child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(t(k::GATEWAY_SUMMARY_TITLE)),
                )
                .children(items)
                .into_any_element(),
        )
    }

    fn render_connection_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let panel = components::card().gap_3().child(section_title(
            t(k::GATEWAY_CONNECTION_TITLE),
            t(k::GATEWAY_CONNECTION_DESCRIPTION),
        ));
        let panel = match self.connection_info.clone() {
            None => panel.child(div().text_color(theme::muted()).text_sm().child(
                if self.connection_loading {
                    t(k::GATEWAY_CONNECTION_LOADING)
                } else {
                    t(k::GATEWAY_CONNECTION_UNAVAILABLE)
                },
            )),
            Some(info) => {
                let url = info.base_url.clone();
                let url_for_copy = url.clone();
                let key = info.key_secret.clone();
                let shown_key = if self.reveal_connection_key {
                    key.clone()
                } else {
                    masked_secret(&key)
                };
                panel
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .w(px(40.))
                                    .child(t(k::GATEWAY_CONNECTION_URL_LABEL)),
                            )
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .child(SharedString::from(url)),
                            )
                            .child(
                                components::button(
                                    "connection-copy-url",
                                    t(k::GATEWAY_ACTION_COPY),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.copy_to_clipboard(
                                            url_for_copy.clone(),
                                            raw(k::GATEWAY_STATUS_URL_COPIED),
                                            cx,
                                        );
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .w(px(40.))
                                    .child(t(k::GATEWAY_CONNECTION_KEY_LABEL)),
                            )
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .child(SharedString::from(shown_key)),
                            )
                            .child(
                                components::button(
                                    "connection-reveal-key",
                                    if self.reveal_connection_key {
                                        t(k::GATEWAY_ACTION_HIDE)
                                    } else {
                                        t(k::GATEWAY_ACTION_SHOW)
                                    },
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.reveal_connection_key = !this.reveal_connection_key;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "connection-copy-key",
                                    t(k::GATEWAY_ACTION_COPY),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.copy_to_clipboard(
                                            key.clone(),
                                            raw(k::GATEWAY_STATUS_KEY_COPIED),
                                            cx,
                                        );
                                    },
                                )),
                            ),
                    )
            }
        };
        panel
            .child(
                div()
                    .text_color(theme::muted())
                    .text_xs()
                    .child(t(k::GATEWAY_CONNECTION_NOTE)),
            )
            .into_any_element()
    }

    fn render_station(&self, station: &RelayStation, cx: &mut Context<Self>) -> gpui::AnyElement {
        let enabled = station.channel.enabled && station.route.enabled;
        let route_id = station.route.id.clone();
        let route_id_for_toggle = route_id.clone();
        let station_for_edit = station.clone();
        let route_id_for_delete = route_id.clone();
        let station_name_for_delete = station.channel.name.clone();
        let reasoning = match station.route.reasoning.mode {
            GatewayReasoningMode::Auto => raw(k::GATEWAY_CARD_REASONING_AUTO),
            GatewayReasoningMode::Passthrough => raw(k::GATEWAY_CARD_REASONING_PASSTHROUGH),
            GatewayReasoningMode::Disabled => raw(k::GATEWAY_CARD_REASONING_DISABLED),
        };
        let model_summary = match (
            station.route.default_model.as_deref(),
            station.route.model_rules.len(),
        ) {
            (Some(model), 0) => tf!(k::GATEWAY_CARD_MODEL_DEFAULT, model = model),
            (Some(model), count) => tf!(
                k::GATEWAY_CARD_MODEL_DEFAULT_WITH_RULES,
                model = model,
                count = count,
            ),
            (None, 0) => raw(k::GATEWAY_CARD_MODEL_PASSTHROUGH).to_string(),
            (None, count) => tf!(k::GATEWAY_CARD_MODEL_RULES, count = count),
        };

        let app_buttons: Vec<gpui::AnyElement> = apply::supported_apps()
            .iter()
            .copied()
            .filter(|app| crate::app_meta::enabled_app_types().contains(app))
            .map(|app_type| {
                let active = self
                    .active_station_by_app
                    .get(&app_type)
                    .is_some_and(|active_route| active_route == &station.route.id);
                let busy = self.applying.is_some();
                let compatible = apply::dialect_compatible(station.channel.dialect, app_type);
                let button = components::button(
                    SharedString::from(format!(
                        "station-apply-{}-{}",
                        station.channel.id,
                        app_type.as_str()
                    )),
                    if active {
                        SharedString::from(tf!(
                            k::GATEWAY_CARD_APPLIED,
                            app = crate::app_meta::label(app_type)
                        ))
                    } else {
                        crate::app_meta::label(app_type)
                    },
                    if active {
                        ButtonTone::Neutral
                    } else {
                        ButtonTone::Primary
                    },
                    ButtonSize::Sm,
                );
                if active || busy || !enabled || !compatible {
                    button
                        .cursor_not_allowed()
                        .opacity(components::DISABLED_OPACITY)
                        .into_any_element()
                } else {
                    let route_id = route_id.clone();
                    button
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.apply_station(route_id.clone(), app_type, cx);
                        }))
                        .into_any_element()
                }
            })
            .collect();

        let editing = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.channel_id == station.channel.id);
        components::card()
            .gap_3()
            .when(editing, |panel| panel.opacity(components::DISABLED_OPACITY))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(260.))
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(icon(IconName::Layers, theme::accent(), 16.))
                                    .child(
                                        div()
                                            .text_color(theme::text())
                                            .text_base()
                                            .font_weight(FontWeight::BOLD)
                                            .child(SharedString::from(
                                                station.channel.name.clone(),
                                            )),
                                    )
                                    .child(components::badge(
                                        BadgeTone::Neutral,
                                        dialect_label(station.channel.dialect),
                                    ))
                                    .when(station.channel.imported_from.is_some(), |row| {
                                        row.child(components::badge(
                                            BadgeTone::Neutral,
                                            t(k::GATEWAY_CARD_BADGE_IMPORTED),
                                        ))
                                    })
                                    .when(editing, |row| {
                                        row.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::GATEWAY_CARD_BADGE_EDITING),
                                        ))
                                    })
                                    .when(!enabled, |row| {
                                        row.child(components::badge(
                                            BadgeTone::Warning,
                                            t(k::GATEWAY_CARD_BADGE_DISABLED),
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .truncate()
                                    .child(SharedString::from(station.channel.base_url.clone())),
                            )
                            .child(div().text_color(theme::subtext()).text_xs().child(
                                SharedString::from(format!("{model_summary} · {reasoning}")),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(
                                layout::toggle(enabled)
                                    .id(SharedString::from(format!(
                                        "station-toggle-{}",
                                        station.channel.id
                                    )))
                                    .role(gpui::Role::Switch)
                                    .aria_label(SharedString::from(tf!(
                                        k::GATEWAY_CARD_TOGGLE_ARIA,
                                        name = station.channel.name
                                    )))
                                    .aria_toggled(if enabled {
                                        gpui::Toggled::True
                                    } else {
                                        gpui::Toggled::False
                                    })
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.toggle_station(route_id_for_toggle.clone(), cx);
                                    })),
                            )
                            .child(
                                components::button(
                                    SharedString::from(format!(
                                        "station-edit-{}",
                                        station.channel.id
                                    )),
                                    t(k::GATEWAY_ACTION_EDIT),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.open_editor(Some(&station_for_edit), cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    SharedString::from(format!(
                                        "station-delete-{}",
                                        station.channel.id
                                    )),
                                    t(k::GATEWAY_ACTION_DELETE),
                                    ButtonTone::Danger,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.request_delete(
                                            route_id_for_delete.clone(),
                                            station_name_for_delete.clone(),
                                            cx,
                                        );
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .mr_1()
                            .child(t(k::GATEWAY_CARD_APPLY_TO)),
                    )
                    .children(app_buttons),
            )
            .into_any_element()
    }

    fn render_editor(&self, editor: &StationEditor, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialect_index = match editor.dialect {
            Dialect::Messages => 0,
            Dialect::Chat => 1,
            Dialect::Responses => 2,
        };
        let on_dialect_select = cx.listener(|this, index: &usize, _window, cx| {
            let dialect = match index {
                1 => Dialect::Chat,
                2 => Dialect::Responses,
                _ => Dialect::Messages,
            };
            if let Some(editor) = &mut this.editor {
                editor.dialect = dialect;
            }
            if dialect == Dialect::Chat {
                this.set_status(
                    NotificationLevel::Warning,
                    t(k::GATEWAY_STATUS_CHAT_DIALECT_CAVEAT),
                    cx,
                );
            } else {
                cx.notify();
            }
        });
        let reasoning_index = match editor.reasoning_mode {
            GatewayReasoningMode::Auto => 0,
            GatewayReasoningMode::Passthrough => 1,
            GatewayReasoningMode::Disabled => 2,
        };
        let on_reasoning_select = cx.listener(|this, index: &usize, _window, cx| {
            if let Some(editor) = &mut this.editor {
                editor.reasoning_mode = match index {
                    1 => GatewayReasoningMode::Passthrough,
                    2 => GatewayReasoningMode::Disabled,
                    _ => GatewayReasoningMode::Auto,
                };
            }
            cx.notify();
        });
        let rule_rows: Vec<gpui::AnyElement> = editor
            .rules
            .iter()
            .map(|rule| {
                let rule_id = rule.id;
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_end()
                    .gap_2()
                    .w_full()
                    .child(div().flex_1().min_w(px(220.)).child(components::field(
                        t(k::GATEWAY_EDITOR_RULE_CLIENT_MODEL_LABEL),
                        false,
                        None,
                        rule.client_model.clone(),
                    )))
                    .child(div().flex_1().min_w(px(220.)).child(components::field(
                        t(k::GATEWAY_EDITOR_RULE_STATION_MODEL_LABEL),
                        false,
                        None,
                        rule.station_model.clone(),
                    )))
                    .child(
                        components::icon_button_tone(
                            SharedString::from(format!("station-rule-delete-{rule_id}")),
                            t(k::GATEWAY_EDITOR_RULE_DELETE),
                            IconName::Trash,
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.remove_model_rule(rule_id, cx);
                            },
                        )),
                    )
                    .into_any_element()
            })
            .collect();

        let probe_help: SharedString = match editor.probe {
            ProbeState::Idle => t(k::GATEWAY_EDITOR_DIALECT_HELP_IDLE),
            ProbeState::Running => t(k::GATEWAY_EDITOR_DIALECT_HELP_RUNNING),
            ProbeState::Detected(dialect) => tf!(
                k::GATEWAY_EDITOR_DIALECT_HELP_DETECTED,
                dialect = dialect_label(dialect)
            )
            .into(),
            ProbeState::Failed => t(k::GATEWAY_EDITOR_DIALECT_HELP_FAILED),
        };
        components::card()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_base()
                            .font_weight(FontWeight::BOLD)
                            .child(
                                if self
                                    .stations
                                    .iter()
                                    .any(|station| station.channel.id == editor.channel_id)
                                {
                                    t(k::GATEWAY_EDITOR_TITLE_EDIT)
                                } else {
                                    t(k::GATEWAY_EDITOR_TITLE_ADD)
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                components::button(
                                    "station-editor-cancel-top",
                                    t(k::GATEWAY_EDITOR_CANCEL),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.editor = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "station-editor-save-top",
                                    t(k::GATEWAY_EDITOR_SAVE),
                                    ButtonTone::Primary,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.save_editor(cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(section_title(
                t(k::GATEWAY_EDITOR_CONNECTION_TITLE),
                t(k::GATEWAY_EDITOR_CONNECTION_DESCRIPTION),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_3()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.))
                            .child(components::field_with_error(
                                t(k::GATEWAY_EDITOR_NAME_LABEL),
                                true,
                                None,
                                editor.name_error.clone(),
                                editor.name.clone(),
                            )),
                    )
                    .child(div().flex_1().min_w(px(280.)).child(components::field(
                        t(k::GATEWAY_EDITOR_BASE_URL_LABEL),
                        true,
                        None,
                        editor.base_url.clone(),
                    )))
                    .child(
                        div().flex_1().min_w(px(220.)).child(components::field(
                            "API Key",
                            false,
                            None,
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w_0().child(editor.api_key.clone()))
                                .child(
                                    components::button(
                                        "station-key-reveal",
                                        if editor.reveal_key {
                                            t(k::GATEWAY_ACTION_HIDE)
                                        } else {
                                            t(k::GATEWAY_ACTION_SHOW)
                                        },
                                        ButtonTone::Ghost,
                                        ButtonSize::Sm,
                                    )
                                    .on_click(cx.listener(
                                        |this, _event, _window, cx| {
                                            this.toggle_reveal_key(cx);
                                        },
                                    )),
                                ),
                        )),
                    ),
            )
            .child(components::field(
                t(k::GATEWAY_EDITOR_DIALECT_LABEL),
                false,
                Some(probe_help),
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(components::segmented(
                        "station-dialect",
                        &["Anthropic Messages", "OpenAI Chat", "OpenAI Responses"],
                        dialect_index,
                        move |index, window, cx| on_dialect_select(&index, window, cx),
                    ))
                    .child(
                        components::button(
                            "station-dialect-detect",
                            if editor.probe == ProbeState::Running {
                                t(k::GATEWAY_EDITOR_DIALECT_DETECTING)
                            } else {
                                t(k::GATEWAY_EDITOR_DIALECT_DETECT)
                            },
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.detect_dialect(cx);
                            },
                        )),
                    ),
            ))
            .child(section_title(
                t(k::GATEWAY_EDITOR_MODELS_TITLE),
                t(k::GATEWAY_EDITOR_MODELS_DESCRIPTION),
            ))
            .child(components::field(
                t(k::GATEWAY_EDITOR_DEFAULT_MODEL_LABEL),
                false,
                Some(t(k::GATEWAY_EDITOR_DEFAULT_MODEL_HELP)),
                editor.default_model.clone(),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::GATEWAY_EDITOR_RULES_TITLE)),
                    )
                    .child(
                        components::button(
                            "station-add-rule",
                            t(k::GATEWAY_EDITOR_RULES_ADD),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.add_model_rule(cx);
                            },
                        )),
                    ),
            )
            .children(rule_rows)
            .when_some(editor.rules_error.clone(), |panel, error| {
                panel.child(div().text_color(theme::red()).text_xs().child(error))
            })
            .when(editor.rules.is_empty(), |panel| {
                panel.child(
                    div()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(t(k::GATEWAY_EDITOR_RULES_EMPTY)),
                )
            })
            .child(section_title(
                t(k::GATEWAY_EDITOR_REASONING_TITLE),
                t(k::GATEWAY_EDITOR_REASONING_DESCRIPTION),
            ))
            .child(components::field(
                t(k::GATEWAY_EDITOR_REASONING_LABEL),
                false,
                None,
                components::segmented(
                    "station-reasoning",
                    &[
                        raw(k::GATEWAY_EDITOR_REASONING_OPTION_AUTO),
                        raw(k::GATEWAY_EDITOR_REASONING_OPTION_PASSTHROUGH),
                        raw(k::GATEWAY_EDITOR_REASONING_OPTION_DISABLED),
                    ],
                    reasoning_index,
                    move |index, window, cx| on_reasoning_select(&index, window, cx),
                ),
            ))
            .when(
                editor.reasoning_mode == GatewayReasoningMode::Auto,
                |panel| {
                    panel.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_2()
                            .w_full()
                            .children(
                                [
                                    ("Low", editor.low_budget.clone()),
                                    ("Medium", editor.medium_budget.clone()),
                                    ("High", editor.high_budget.clone()),
                                    ("Max", editor.max_budget.clone()),
                                ]
                                .into_iter()
                                .map(|(label, input)| {
                                    div().flex_1().min_w(px(150.)).child(components::field(
                                        label,
                                        false,
                                        Some(t(k::GATEWAY_EDITOR_REASONING_BUDGET_HELP)),
                                        input,
                                    ))
                                }),
                            ),
                    )
                },
            )
            .when_some(editor.budget_error.clone(), |panel, error| {
                panel.child(div().text_color(theme::red()).text_xs().child(error))
            })
            .into_any_element()
    }
}

impl Render for GatewayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let import_rows: Vec<gpui::AnyElement> = self
            .import_candidates
            .clone()
            .iter()
            .map(|candidate| self.render_import_candidate(candidate, cx))
            .collect();
        let station_rows: Vec<gpui::AnyElement> = self
            .stations
            .clone()
            .iter()
            .map(|station| self.render_station(station, cx))
            .collect();
        let editor = self.editor.take().map(|editor| {
            let element = self.render_editor(&editor, cx);
            self.editor = Some(editor);
            element
        });
        let connection_panel = self
            .show_connection
            .then(|| self.render_connection_panel(cx));
        let summary_bar = self.render_summary_bar(cx);
        let station_count = self.stations.len();

        let content = layout::wide_column()
            .gap_5()
            .when_some(summary_bar, |column, bar| column.child(bar))
            .when(self.show_imports, |column| {
                column.child(
                    components::card()
                        .gap_3()
                        .child(section_title(
                            t(k::GATEWAY_IMPORT_TITLE),
                            t(k::GATEWAY_IMPORT_DESCRIPTION),
                        ))
                        .when(self.import_candidates.is_empty(), |panel| {
                            panel.child(
                                div()
                                    .text_color(theme::muted())
                                    .text_sm()
                                    .child(t(k::GATEWAY_IMPORT_EMPTY)),
                            )
                        })
                        .when(!import_rows.is_empty(), |panel| {
                            panel.child(layout::group(import_rows))
                        }),
                )
            })
            .when_some(connection_panel, |column, panel| column.child(panel))
            .when_some(editor, |column, editor| column.child(editor))
            .when(station_count == 0 && self.editor.is_none(), |column| {
                column.child(components::empty_state(
                    IconName::Cloud,
                    t(k::GATEWAY_EMPTY_TITLE),
                    t(k::GATEWAY_EMPTY_HINT),
                    Some(
                        components::button(
                            "station-empty-add",
                            t(k::GATEWAY_ACTION_ADD),
                            ButtonTone::Primary,
                            ButtonSize::Md,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.open_editor(None, cx);
                        }))
                        .into_any_element(),
                    ),
                ))
            })
            .children(station_rows);

        layout::page()
            .relative()
            .child(
                layout::page_header(t(k::GATEWAY_PAGE_TITLE), Some(t(k::GATEWAY_PAGE_SUBTITLE)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                components::button(
                                    "station-connection-toggle",
                                    if self.show_connection {
                                        t(k::GATEWAY_PAGE_CONNECTION_HIDE)
                                    } else {
                                        t(k::GATEWAY_PAGE_CONNECTION_SHOW)
                                    },
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.toggle_connection_panel(cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "station-import-toggle",
                                    if self.show_imports {
                                        t(k::GATEWAY_PAGE_IMPORT_HIDE)
                                    } else {
                                        t(k::GATEWAY_PAGE_IMPORT_SHOW)
                                    },
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.show_imports = !this.show_imports;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "station-add",
                                    t(k::GATEWAY_ACTION_ADD),
                                    ButtonTone::Primary,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.open_editor(None, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(layout::scroll_body_tracked(
                "relay-stations-body",
                &self.scroll,
                content,
            ))
            .when_some(self.delete_blocked.clone(), |root, (name, apps)| {
                let labels = apps
                    .iter()
                    .map(|app| crate::app_meta::label(*app).to_string())
                    .collect::<Vec<_>>()
                    .join(raw(k::GATEWAY_DELETE_BLOCKED_SEPARATOR));
                let first_app = apps.first().copied();
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::GATEWAY_DELETE_BLOCKED_TITLE)))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(tf!(
                                        k::GATEWAY_DELETE_BLOCKED_MESSAGE,
                                        apps = labels,
                                        name = name,
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "station-blocked-close",
                                t(k::GATEWAY_DELETE_BLOCKED_ACKNOWLEDGE),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.delete_blocked = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "station-blocked-switch",
                                t(k::GATEWAY_ACTION_SWITCH),
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.delete_blocked = None;
                                if let Some(app) = first_app {
                                    cx.emit(GatewayEvent::OpenProviders(app));
                                }
                                cx.notify();
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
            .when_some(self.confirm_delete.clone(), |root, (route_id, name)| {
                root.child(components::modal_overlay(
                    components::modal_card()
                        .child(components::modal_header(t(k::GATEWAY_CONFIRM_DELETE_TITLE)))
                        .child(
                            components::modal_body().child(
                                div().text_color(theme::subtext()).text_sm().child(
                                    SharedString::from(tf!(
                                        k::GATEWAY_CONFIRM_DELETE_MESSAGE,
                                        name = name
                                    )),
                                ),
                            ),
                        )
                        .child(components::modal_footer(vec![
                            components::button(
                                "station-delete-cancel",
                                t(k::GATEWAY_ACTION_CANCEL),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                            components::button(
                                "station-delete-confirm",
                                t(k::GATEWAY_ACTION_DELETE),
                                ButtonTone::Danger,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.confirm_delete = None;
                                this.delete_station(route_id.clone(), cx);
                            }))
                            .into_any_element(),
                        ])),
                ))
            })
    }
}

fn section_title(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_color(theme::text())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_color(theme::muted())
                .text_xs()
                .child(description.into()),
        )
}

fn masked_secret(secret: &str) -> String {
    let visible = secret.len().min(7);
    format!("{}••••••••", &secret[..visible])
}

fn dialect_label(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Messages => "Anthropic Messages",
        Dialect::Chat => "OpenAI Chat",
        Dialect::Responses => "OpenAI Responses",
    }
}

fn text_input(
    cx: &mut Context<TextInput>,
    placeholder: impl Into<SharedString>,
    value: &str,
) -> TextInput {
    let mut input = TextInput::new(cx, placeholder);
    input.set_content(value.to_string(), cx);
    input
}

fn input_value(input: &Entity<TextInput>, cx: &mut Context<GatewayView>) -> String {
    input.read(cx).content().trim().to_string()
}

fn parse_budget(input: &Entity<TextInput>, cx: &mut Context<GatewayView>) -> Option<u32> {
    input_value(input, cx).parse::<u32>().ok()
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

crate::notifications::impl_status_toasts_leveled!(GatewayView);
