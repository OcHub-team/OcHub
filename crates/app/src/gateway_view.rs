//! User-facing relay-station manager.
//!
//! A station is presented as one complete commercial relay configuration
//! (New API, Sub2API, or another compatible service) that may expose several
//! API URLs and interfaces. OcHub can detect interfaces and fetch `/v1/models`,
//! while keeping both protocol model lists and aliases editable. The local
//! gateway, per-app keys, and route bindings remain implementation details.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FontWeight, ListAlignment, ListOffset, ListState,
    ScrollHandle, SharedString, Window, div, prelude::*, px,
};
use ochub_core::gateway::apply;
use ochub_core::gateway::types::{
    Dialect, GatewayChannel, GatewayEndpointTestResult, GatewayModelRule, GatewayReasoningConfig,
    GatewayReasoningMode, GatewayRoute,
};
use ochub_core::services::provider::ProviderService;
use ochub_core::{AppState, AppType, ModelProviderImportManifest, prepare_model_provider_import};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
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
    channels: Vec<GatewayChannel>,
    route: GatewayRoute,
}

impl RelayStation {
    fn primary_channel(&self) -> Option<&GatewayChannel> {
        self.channels
            .iter()
            .find(|channel| channel.enabled)
            .or_else(|| self.channels.first())
    }

    fn is_enabled(&self) -> bool {
        self.route.enabled && self.channels.iter().any(|channel| channel.enabled)
    }
}

#[derive(Clone)]
struct ImportCandidate {
    app_type: AppType,
    provider_id: String,
    name: String,
    base_url: String,
}

struct GatewayPageLoad {
    stations: Vec<RelayStation>,
    import_candidates: Vec<ImportCandidate>,
    installed_station_apps: HashSet<(AppType, String)>,
    enabled_apps: Arc<[AppType]>,
}

impl GatewayPageLoad {
    fn load(app: &AppState) -> Self {
        let channels = app.db.get_gateway_channels().unwrap_or_default();
        let routes = app.db.get_gateway_routes().unwrap_or_default();
        let channel_map: HashMap<&str, &GatewayChannel> = channels
            .iter()
            .map(|channel| (channel.id.as_str(), channel))
            .collect();
        let mut referenced_channels = HashSet::new();
        let mut stations = Vec::new();
        for route in routes
            .iter()
            .filter(|route| route.id.starts_with(apply::STATION_ROUTE_PREFIX))
        {
            let grouped: Vec<GatewayChannel> = route
                .channel_ids
                .iter()
                .filter_map(|id| channel_map.get(id.as_str()).copied().cloned())
                .collect();
            if grouped.is_empty() {
                continue;
            }
            referenced_channels.extend(grouped.iter().map(|channel| channel.id.clone()));
            stations.push(RelayStation {
                channels: grouped,
                route: route.clone(),
            });
        }
        // Keep legacy control-API channels visible even when no station route
        // was persisted for them.
        for channel in channels
            .iter()
            .filter(|channel| !referenced_channels.contains(&channel.id))
        {
            stations.push(RelayStation {
                channels: vec![channel.clone()],
                route: GatewayRoute {
                    id: apply::station_route_id(&channel.id),
                    name: channel.name.clone(),
                    website_url: None,
                    app_type: None,
                    channel_ids: vec![channel.id.clone()],
                    default_model: None,
                    model_rules: Vec::new(),
                    reasoning: GatewayReasoningConfig::default(),
                    enabled: channel.enabled,
                    created_at: chrono::Utc::now().timestamp(),
                },
            });
        }

        let keys = app.db.get_gateway_keys().unwrap_or_default();
        let mut installed_station_apps = HashSet::new();
        for app_type in apply::supported_apps() {
            if let Ok(providers) = ProviderService::list(app, *app_type) {
                for provider in providers.values().filter(|provider| {
                    provider.is_local_gateway()
                        || provider
                            .meta
                            .as_ref()
                            .is_some_and(|meta| meta.gateway_route_id.is_some())
                }) {
                    if app_type.is_additive_mode()
                        && provider
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.live_config_managed)
                            == Some(false)
                    {
                        continue;
                    }
                    let route_id = provider
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.gateway_route_id.clone())
                        .or_else(|| {
                            (provider.id == apply::GATEWAY_PROVIDER_ID)
                                .then(|| {
                                    keys.iter()
                                        .find(|key| key.name == app_type.as_str() && key.enabled)
                                        .and_then(|key| key.route_id.clone())
                                })
                                .flatten()
                        });
                    let Some(route_id) = route_id else {
                        continue;
                    };
                    installed_station_apps.insert((*app_type, route_id));
                }
            }
        }

        let imported_ids: HashSet<String> = stations
            .iter()
            .flat_map(|station| station.channels.iter().map(|channel| channel.id.clone()))
            .collect();
        let enabled_apps = crate::app_meta::enabled_app_types();
        let mut import_candidates = Vec::new();
        for app_type in enabled_apps.iter().copied() {
            if let Ok(providers) = ProviderService::list(app, app_type) {
                for provider in providers.into_values().filter(|provider| {
                    provider.id != apply::GATEWAY_PROVIDER_ID
                        && provider.category.as_deref() != Some("gateway")
                        // Station channels point at the local gateway itself;
                        // importing one as an upstream would loop requests.
                        && provider
                            .meta
                            .as_ref()
                            .is_none_or(|meta| meta.gateway_route_id.is_none())
                }) {
                    let channel_id = format!("imported-{}-{}", app_type.as_str(), provider.id);
                    if imported_ids.contains(&channel_id) {
                        continue;
                    }
                    let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
                    if !base_url.trim().is_empty() && !api_key.trim().is_empty() {
                        import_candidates.push(ImportCandidate {
                            app_type,
                            provider_id: provider.id,
                            name: provider.name,
                            base_url,
                        });
                    }
                }
            }
        }

        Self {
            stations,
            import_candidates,
            installed_station_apps,
            enabled_apps: enabled_apps.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GatewayRow {
    Imports,
    Connection,
    Editor,
    Empty,
    Station(usize),
}

struct ModelRuleEditor {
    client_model: Entity<TextInput>,
    station_model: Entity<TextInput>,
    dialect: Option<Dialect>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Idle,
    Running,
    Detected,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
enum ModelFetchState {
    Idle,
    Running,
    Fetched(usize),
    Failed(SharedString),
}

#[derive(Clone, PartialEq, Eq)]
enum EndpointTestState {
    Idle,
    Running,
    Complete(GatewayEndpointTestResult),
    Failed(SharedString),
}

struct EndpointEditor {
    id: String,
    existing_channels: HashMap<Dialect, GatewayChannel>,
    enabled_dialects: HashSet<Dialect>,
    base_url: Entity<TextInput>,
    models: EndpointModelsEditor,
    fetched_models: Vec<String>,
    model_picker_open: bool,
    probe: ProbeState,
    model_fetch: ModelFetchState,
    test: EndpointTestState,
}

/// One model list per endpoint, shared by every interface it exposes.
///
/// The list is not per-interface because nothing can fill three lists apart:
/// discovery is a single OpenAI-style `GET /v1/models` against the endpoint,
/// so a per-dialect split only ever produced three identical copies — and
/// three identical copies make the router's dialect preference try an
/// interface the upstream does not serve the model on before failing over.
struct EndpointModelsEditor {
    selected: Vec<String>,
    manual_input: Entity<TextInput>,
    scroll_handle: ScrollHandle,
}

struct StationEditor {
    route_id: String,
    created_at: i64,
    original_channel_ids: HashSet<String>,
    name: Entity<TextInput>,
    website_url: Entity<TextInput>,
    api_key: Entity<TextInput>,
    endpoints: Vec<EndpointEditor>,
    default_model: Entity<TextInput>,
    rules: Vec<ModelRuleEditor>,
    reasoning_mode: GatewayReasoningMode,
    low_budget: Entity<TextInput>,
    medium_budget: Entity<TextInput>,
    high_budget: Entity<TextInput>,
    max_budget: Entity<TextInput>,
    enabled: bool,
    show_advanced: bool,
    reveal_key: bool,
    name_error: Option<SharedString>,
    dialects_error: Option<SharedString>,
    budget_error: Option<SharedString>,
    rules_error: Option<SharedString>,
    is_deeplink_import: bool,
    import_source: Option<SharedString>,
    import_contains_key: bool,
}

pub struct GatewayView {
    app: Arc<AppState>,
    stations: Vec<RelayStation>,
    import_candidates: Vec<ImportCandidate>,
    installed_station_apps: HashSet<(AppType, String)>,
    editor: Option<StationEditor>,
    show_imports: bool,
    mutation_in_flight: bool,
    confirm_delete: Option<(String, String)>,
    /// Deleting was refused: (station name, apps still using it).
    delete_blocked: Option<(SharedString, Vec<AppType>)>,
    show_connection: bool,
    connection_loading: bool,
    connection_info: Option<apply::ApplyResult>,
    reveal_connection_key: bool,
    rows: Arc<[GatewayRow]>,
    list_state: ListState,
    enabled_apps: Arc<[AppType]>,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
    reload_generation: u64,
}

impl EventEmitter<GatewayEvent> for GatewayView {}

impl GatewayView {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// `refresh_windows` re-runs `render`, which covers every label the page
    /// draws. It does not reach the open editor: a `TextInput` captures its
    /// placeholder when it is constructed, and the inline field errors were
    /// resolved when a save was refused, so both otherwise survive in the
    /// language they were made in. The virtual list also needs its cached item
    /// heights invalidated because translations can wrap differently.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        self.list_state.remeasure();
        let mut placeholders: Vec<(Entity<TextInput>, SharedString)> = Vec::new();
        if let Some(editor) = self.editor.as_mut() {
            // Which fields are flagged does not change — only the wording does.
            if editor.name_error.is_some() {
                editor.name_error = Some(t(k::GATEWAY_EDITOR_ERROR_NAME));
            }
            if editor.dialects_error.is_some() {
                editor.dialects_error = Some(t(k::GATEWAY_EDITOR_ERROR_DIALECTS));
            }
            if editor.budget_error.is_some() {
                editor.budget_error = Some(t(k::GATEWAY_EDITOR_ERROR_BUDGET));
            }
            if editor.rules_error.is_some() {
                editor.rules_error = Some(t(k::GATEWAY_EDITOR_ERROR_RULES));
            }

            placeholders.extend([
                (editor.name.clone(), t(k::GATEWAY_EDITOR_NAME_PLACEHOLDER)),
                (
                    editor.website_url.clone(),
                    t(k::GATEWAY_EDITOR_WEBSITE_PLACEHOLDER),
                ),
                (
                    editor.api_key.clone(),
                    t(k::GATEWAY_EDITOR_API_KEY_PLACEHOLDER),
                ),
                (
                    editor.default_model.clone(),
                    t(k::GATEWAY_EDITOR_DEFAULT_MODEL_PLACEHOLDER),
                ),
            ]);
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
            for endpoint in &editor.endpoints {
                placeholders.push((
                    endpoint.models.manual_input.clone(),
                    t(k::GATEWAY_EDITOR_MODELS_PLACEHOLDER),
                ));
            }
        }
        for (input, placeholder) in placeholders {
            input.update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        cx.notify();
    }

    pub fn new(app: Arc<AppState>, _cx: &mut Context<Self>) -> Self {
        Self {
            app,
            stations: Vec::new(),
            import_candidates: Vec::new(),
            installed_station_apps: HashSet::new(),
            editor: None,
            show_imports: false,
            mutation_in_flight: false,
            confirm_delete: None,
            delete_blocked: None,
            show_connection: false,
            connection_loading: false,
            connection_info: None,
            reveal_connection_key: false,
            rows: Arc::from([]),
            list_state: ListState::new(0, ListAlignment::Top, px(640.)),
            enabled_apps: Arc::from([]),
            status: None,
            status_level: None,
            reload_generation: 0,
        }
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
        let closed_editor = self.editor.take().is_some();
        if self.delete_blocked.take().is_some()
            || self.confirm_delete.take().is_some()
            || closed_editor
        {
            if closed_editor {
                self.rebuild_rows();
            }
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_generation = self.reload_generation.wrapping_add(1);
        let generation = self.reload_generation;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_spawn(async move { GatewayPageLoad::load(&app) })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.reload_generation {
                    return;
                }
                this.stations = data.stations;
                this.import_candidates = data.import_candidates;
                this.installed_station_apps = data.installed_station_apps;
                this.enabled_apps = data.enabled_apps;
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn open_model_provider_import(
        &mut self,
        manifest: ModelProviderImportManifest,
        cx: &mut Context<Self>,
    ) {
        let contains_key = manifest
            .api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty());
        let import_source = manifest
            .source
            .as_ref()
            .and_then(|source| source.website.as_deref().or(Some(source.id.as_str())))
            .or(manifest.website.as_deref())
            .map(SharedString::from);
        let prepared = match prepare_model_provider_import(manifest) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.set_status(
                    NotificationLevel::Error,
                    tf!(k::GATEWAY_STATUS_IMPORT_FAILED, error = error),
                    cx,
                );
                return;
            }
        };
        let station = RelayStation {
            channels: prepared.channels,
            route: prepared.route,
        };
        self.open_editor(Some(&station), cx);
        if let Some(editor) = &mut self.editor {
            editor.is_deeplink_import = true;
            editor.import_source = import_source;
            editor.import_contains_key = contains_key;
            editor.show_advanced = true;
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.stations.len() + 4);
        if self.show_imports {
            rows.push(GatewayRow::Imports);
        }
        if self.show_connection {
            rows.push(GatewayRow::Connection);
        }
        if self.editor.is_some() {
            rows.push(GatewayRow::Editor);
        } else if self.stations.is_empty() {
            rows.push(GatewayRow::Empty);
        }
        rows.extend((0..self.stations.len()).map(GatewayRow::Station));

        let old = self.rows.as_ref();
        let mut prefix = 0;
        while prefix < old.len() && prefix < rows.len() && old[prefix] == rows[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old.len().saturating_sub(prefix)
            && suffix < rows.len().saturating_sub(prefix)
            && old[old.len() - 1 - suffix] == rows[rows.len() - 1 - suffix]
        {
            suffix += 1;
        }
        let old_end = old.len() - suffix;
        let replacement_count = rows.len() - prefix - suffix;
        if prefix == old.len() && prefix == rows.len() {
            // The row identities stayed put, but a reload or editor mutation
            // may have changed their height.
            self.list_state.remeasure();
        } else {
            self.list_state.splice(prefix..old_end, replacement_count);
        }
        self.rows = rows.into();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            self.rebuild_rows();
            cx.notify();
        }
    }

    fn open_editor_by_route_id(&mut self, route_id: &str, cx: &mut Context<Self>) {
        let Some(station) = self
            .stations
            .iter()
            .find(|station| station.route.id == route_id)
            .cloned()
        else {
            return;
        };
        self.open_editor(Some(&station), cx);
    }

    fn open_editor(&mut self, station: Option<&RelayStation>, cx: &mut Context<Self>) {
        let (
            route_id,
            created_at,
            name,
            website_url,
            api_key,
            default_model,
            channels,
            rules,
            reasoning,
            enabled,
        ) = match station {
            Some(station) => {
                let primary = station.primary_channel();
                (
                    station.route.id.clone(),
                    station.route.created_at,
                    station.route.name.clone(),
                    station.route.website_url.clone().unwrap_or_default(),
                    primary
                        .map(|channel| channel.api_key.clone())
                        .unwrap_or_default(),
                    station.route.default_model.clone().unwrap_or_default(),
                    station.channels.clone(),
                    station.route.model_rules.clone(),
                    station.route.reasoning.clone(),
                    station.route.enabled,
                )
            }
            None => (
                format!("{}{}", apply::STATION_ROUTE_PREFIX, uuid::Uuid::new_v4()),
                chrono::Utc::now().timestamp(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                Vec::new(),
                Vec::new(),
                GatewayReasoningConfig::default(),
                true,
            ),
        };

        let channel_dialects: HashMap<String, Dialect> = channels
            .iter()
            .map(|channel| (channel.id.clone(), channel.dialect))
            .collect();
        let mut rule_editors = Vec::new();
        for rule in rules {
            rule_editors.push(ModelRuleEditor {
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
                dialect: rule.dialect.or_else(|| {
                    rule.channel_id
                        .as_ref()
                        .and_then(|channel_id| channel_dialects.get(channel_id).copied())
                }),
            });
        }

        let mut grouped: Vec<(String, Vec<GatewayChannel>)> = Vec::new();
        for channel in channels {
            let grouping_key = channel
                .endpoint_id
                .clone()
                .unwrap_or_else(|| format!("legacy:{}", channel.base_url));
            if let Some((_, group)) = grouped.iter_mut().find(|(key, _)| key == &grouping_key) {
                group.push(channel);
            } else {
                grouped.push((grouping_key, vec![channel]));
            }
        }
        grouped.sort_by_key(|(_, group)| {
            group
                .iter()
                .map(|channel| channel.priority)
                .min()
                .unwrap_or(i32::MAX)
        });
        let mut endpoints = Vec::new();
        for (grouping_key, group) in grouped {
            let id = group
                .iter()
                .find_map(|channel| channel.endpoint_id.clone())
                .unwrap_or_else(|| {
                    grouping_key
                        .strip_prefix("legacy:")
                        .map(|_| uuid::Uuid::new_v4().to_string())
                        .unwrap_or(grouping_key)
                });
            let base_url = group
                .first()
                .map(|channel| channel.base_url.clone())
                .unwrap_or_default();
            let enabled_dialects = group
                .iter()
                .filter(|channel| channel.enabled)
                .map(|channel| channel.dialect)
                .collect();
            let existing_channels: HashMap<Dialect, GatewayChannel> = group
                .into_iter()
                .map(|channel| (channel.dialect, channel))
                .collect();
            endpoints.push(endpoint_editor(
                id,
                base_url,
                existing_channels,
                enabled_dialects,
                cx,
            ));
        }
        if endpoints.is_empty() {
            endpoints.push(endpoint_editor(
                uuid::Uuid::new_v4().to_string(),
                String::new(),
                HashMap::new(),
                HashSet::from([Dialect::Messages]),
                cx,
            ));
        }
        let show_advanced = reasoning != GatewayReasoningConfig::default();
        self.editor = Some(StationEditor {
            route_id,
            created_at,
            original_channel_ids: channel_dialects.keys().cloned().collect(),
            name: cx.new(|cx| text_input(cx, t(k::GATEWAY_EDITOR_NAME_PLACEHOLDER), &name)),
            website_url: cx
                .new(|cx| text_input(cx, t(k::GATEWAY_EDITOR_WEBSITE_PLACEHOLDER), &website_url)),
            api_key: cx.new(|cx| {
                text_input(cx, t(k::GATEWAY_EDITOR_API_KEY_PLACEHOLDER), &api_key).masked(true)
            }),
            endpoints,
            default_model: cx.new(|cx| {
                text_input(
                    cx,
                    t(k::GATEWAY_EDITOR_DEFAULT_MODEL_PLACEHOLDER),
                    &default_model,
                )
            }),
            rules: rule_editors,
            reasoning_mode: reasoning.mode,
            low_budget: cx.new(|cx| text_input(cx, "4096", &reasoning.low_budget.to_string())),
            medium_budget: cx
                .new(|cx| text_input(cx, "10000", &reasoning.medium_budget.to_string())),
            high_budget: cx.new(|cx| text_input(cx, "16000", &reasoning.high_budget.to_string())),
            max_budget: cx.new(|cx| text_input(cx, "32000", &reasoning.max_budget.to_string())),
            enabled,
            show_advanced,
            reveal_key: false,
            name_error: None,
            dialects_error: None,
            budget_error: None,
            rules_error: None,
            is_deeplink_import: false,
            import_source: None,
            import_contains_key: false,
        });
        // The editor renders pinned above the list; jump there so clicking
        // Edit on a card far down the page visibly responds.
        self.rebuild_rows();
        self.list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
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

    fn add_endpoint(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.endpoints.push(endpoint_editor(
            uuid::Uuid::new_v4().to_string(),
            String::new(),
            HashMap::new(),
            HashSet::from([Dialect::Messages]),
            cx,
        ));
        cx.notify();
    }

    fn remove_endpoint(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.endpoints.len() <= 1 {
            return;
        }
        editor
            .endpoints
            .retain(|endpoint| endpoint.id != endpoint_id);
        cx.notify();
    }

    fn toggle_endpoint_dialect(
        &mut self,
        endpoint_id: &str,
        dialect: Dialect,
        cx: &mut Context<Self>,
    ) {
        let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) else {
            return;
        };
        if !endpoint.enabled_dialects.remove(&dialect) {
            endpoint.enabled_dialects.insert(dialect);
        }
        endpoint.probe = ProbeState::Idle;
        cx.notify();
    }

    fn toggle_model_picker(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) else {
            return;
        };
        endpoint.model_picker_open = !endpoint.model_picker_open;
        cx.notify();
    }

    fn toggle_endpoint_model(&mut self, endpoint_id: &str, model: &str, cx: &mut Context<Self>) {
        let Some(models) = self
            .editor
            .as_mut()
            .and_then(|editor| {
                editor
                    .endpoints
                    .iter_mut()
                    .find(|endpoint| endpoint.id == endpoint_id)
            })
            .map(|endpoint| &mut endpoint.models)
        else {
            return;
        };
        if let Some(index) = models
            .selected
            .iter()
            .position(|selected| selected == model)
        {
            models.selected.remove(index);
        } else {
            models.selected.push(model.to_string());
            models.selected.sort();
        }
        cx.notify();
    }

    fn add_manual_endpoint_models(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let Some(input) = self
            .editor
            .as_ref()
            .and_then(|editor| {
                editor
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.id == endpoint_id)
            })
            .map(|endpoint| endpoint.models.manual_input.clone())
        else {
            return;
        };
        let additions = parse_models(&input_value(&input, cx));
        if additions.is_empty() {
            return;
        }
        if let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) {
            let models = &mut endpoint.models;
            models.selected.extend(additions);
            models.selected = normalized_models(std::mem::take(&mut models.selected));
        }
        input.update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.notify();
    }

    fn add_all_fetched_models(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) else {
            return;
        };
        let fetched = endpoint.fetched_models.clone();
        let models = &mut endpoint.models;
        models.selected.extend(fetched);
        models.selected = normalized_models(std::mem::take(&mut models.selected));
        cx.notify();
    }

    fn clear_endpoint_models(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) else {
            return;
        };
        endpoint.models.selected.clear();
        cx.notify();
    }

    fn toggle_editor_advanced(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.editor {
            editor.show_advanced = !editor.show_advanced;
            cx.notify();
        }
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        if self.mutation_in_flight {
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        let name = input_value(&editor.name, cx);
        let name_error: Option<SharedString> = if name.is_empty() {
            Some(t(k::GATEWAY_EDITOR_ERROR_NAME))
        } else {
            None
        };
        let endpoints_valid = !editor.endpoints.is_empty()
            && editor.endpoints.iter().all(|endpoint| {
                !input_value(&endpoint.base_url, cx).is_empty()
                    && !endpoint.enabled_dialects.is_empty()
            });
        let dialects_error: Option<SharedString> =
            (!endpoints_valid).then(|| t(k::GATEWAY_EDITOR_ERROR_DIALECTS));

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
            if client_model.is_empty() && station_model.is_empty() && rule.dialect.is_none() {
                continue;
            }
            if client_model.is_empty()
                || rule.dialect.is_some_and(|dialect| {
                    !editor
                        .endpoints
                        .iter()
                        .any(|endpoint| endpoint.enabled_dialects.contains(&dialect))
                })
            {
                rules_error = Some(t(k::GATEWAY_EDITOR_ERROR_RULES));
                break;
            }
            rules.push(GatewayModelRule {
                model: client_model,
                upstream_model: station_model,
                channel_id: None,
                dialect: rule.dialect,
            });
        }

        if name_error.is_some()
            || dialects_error.is_some()
            || budget_error.is_some()
            || rules_error.is_some()
        {
            if let Some(editor) = &mut self.editor {
                editor.name_error = name_error;
                editor.dialects_error = dialects_error;
                editor.budget_error = budget_error;
                editor.rules_error = rules_error;
            }
            cx.notify();
            return;
        }
        if let Some(editor) = &mut self.editor {
            editor.name_error = None;
            editor.dialects_error = None;
            editor.budget_error = None;
            editor.rules_error = None;
        }
        let Some(editor) = &self.editor else {
            return;
        };

        let api_key = input_value(&editor.api_key, cx);
        let station_id = editor
            .route_id
            .strip_prefix(apply::STATION_ROUTE_PREFIX)
            .unwrap_or(&editor.route_id);
        let mut channels = Vec::new();
        let mut stale_channel_ids = editor.original_channel_ids.clone();
        for (endpoint_index, endpoint) in editor.endpoints.iter().enumerate() {
            let base_url = input_value(&endpoint.base_url, cx);
            for dialect in Dialect::ALL {
                if !endpoint.enabled_dialects.contains(&dialect)
                    && !endpoint.existing_channels.contains_key(&dialect)
                {
                    continue;
                }
                let mut channel = endpoint
                    .existing_channels
                    .get(&dialect)
                    .cloned()
                    .unwrap_or_else(|| GatewayChannel {
                        id: format!(
                            "station-channel:{station_id}:{}:{}",
                            endpoint.id,
                            dialect.as_str()
                        ),
                        endpoint_id: Some(endpoint.id.clone()),
                        name: name.clone(),
                        dialect,
                        base_url: base_url.clone(),
                        api_key: api_key.clone(),
                        path_override: None,
                        models: Vec::new(),
                        model_override: None,
                        priority: endpoint_index as i32 * 10,
                        weight: 1,
                        enabled: true,
                        extra_headers: Vec::new(),
                        imported_from: None,
                    });
                channel.endpoint_id = Some(endpoint.id.clone());
                channel.name = name.clone();
                channel.base_url = base_url.clone();
                channel.api_key = api_key.clone();
                channel.models = endpoint.models.selected.clone();
                channel.priority = endpoint_index as i32 * 10;
                channel.enabled = endpoint.enabled_dialects.contains(&dialect);
                stale_channel_ids.remove(&channel.id);
                channels.push(channel);
            }
        }
        let route = GatewayRoute {
            id: editor.route_id.clone(),
            name: name.clone(),
            website_url: nonempty(input_value(&editor.website_url, cx)),
            app_type: None,
            channel_ids: channels.iter().map(|channel| channel.id.clone()).collect(),
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
        let is_deeplink_import = editor.is_deeplink_import;
        let stale_channel_ids = stale_channel_ids.into_iter().collect::<Vec<_>>();

        self.mutation_in_flight = true;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    app.db
                        .save_gateway_station(&channels, &route, &stale_channel_ids)
                        .map(|_| name)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.mutation_in_flight = false;
                match result {
                    Ok(name) => {
                        this.set_status(
                            NotificationLevel::Success,
                            if is_deeplink_import {
                                tf!(k::GATEWAY_STATUS_IMPORTED, name = name)
                            } else {
                                tf!(k::GATEWAY_STATUS_SAVED, name = name)
                            },
                            cx,
                        );
                        this.editor = None;
                        this.rebuild_rows();
                        this.reload(cx);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::GATEWAY_STATUS_SAVE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_station(&mut self, route_id: String, cx: &mut Context<Self>) {
        if self.mutation_in_flight {
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
        let enabled = !station.is_enabled();
        let mut route = station.route;
        route.enabled = enabled;
        self.mutation_in_flight = true;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    app.db
                        .upsert_gateway_route(&route)
                        .map(|_| route.name)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.mutation_in_flight = false;
                match result {
                    Ok(name) => {
                        let message = if enabled {
                            tf!(k::GATEWAY_STATUS_ENABLED, name = name)
                        } else {
                            tf!(k::GATEWAY_STATUS_DISABLED, name = name)
                        };
                        this.set_status(NotificationLevel::Success, message, cx);
                        this.reload(cx);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::GATEWAY_STATUS_UPDATE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn request_delete(&mut self, route_id: String, name: String, cx: &mut Context<Self>) {
        let active_apps: Vec<AppType> = self
            .installed_station_apps
            .iter()
            .filter(|(_, installed_route)| installed_route == &route_id)
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
        if self.mutation_in_flight {
            return;
        }
        let channel_ids = self
            .stations
            .iter()
            .find(|station| station.route.id == route_id)
            .map(|station| {
                station
                    .channels
                    .iter()
                    .map(|channel| channel.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.mutation_in_flight = true;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    app.db
                        .delete_gateway_route(&route_id)
                        .and_then(|_| {
                            for channel_id in channel_ids {
                                app.db.delete_gateway_channel(&channel_id)?;
                            }
                            Ok(())
                        })
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.mutation_in_flight = false;
                match result {
                    Ok(()) => {
                        this.set_status(
                            NotificationLevel::Success,
                            t(k::GATEWAY_STATUS_DELETED),
                            cx,
                        );
                        this.reload(cx);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::GATEWAY_STATUS_DELETE_FAILED, error = error),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn import_provider(&mut self, app_type: AppType, provider_id: String, cx: &mut Context<Self>) {
        if self.mutation_in_flight {
            return;
        }
        self.mutation_in_flight = true;
        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    apply::import_provider_as_channel(&app, app_type, &provider_id)
                        .and_then(|channel| {
                            let route = apply::ensure_station_route(&app, &channel)?;
                            Ok((channel, route))
                        })
                        .map(|(channel, _)| channel.name)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.mutation_in_flight = false;
                match result {
                    Ok(name) => {
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(k::GATEWAY_STATUS_IMPORTED, name = name),
                            cx,
                        );
                        this.show_imports = false;
                        this.rebuild_rows();
                        this.reload(cx);
                    }
                    Err(error) => this.set_status(
                        NotificationLevel::Error,
                        tf!(k::GATEWAY_STATUS_IMPORT_FAILED, error = error),
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
        self.rebuild_rows();
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
            let prepare_app = app.clone();
            let prepare = cx
                .background_spawn(async move {
                    let mut config = prepare_app
                        .db
                        .get_gateway_config()
                        .map_err(|error| error.to_string())?;
                    if !config.enabled {
                        config.enabled = true;
                        prepare_app
                            .db
                            .set_gateway_config(&config)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok::<(), String>(())
                })
                .await;
            let result = async {
                prepare?;
                let status = app
                    .gateway
                    .start()
                    .await
                    .map_err(|error| error.to_string())?;
                let base_url = status.base_url;
                let app_for_info = app.clone();
                cx.background_spawn(
                    async move { apply::generic_client_info(&app_for_info, &base_url) },
                )
                .await
                .map_err(|error| error.to_string())
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
                        this.rebuild_rows();
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

    fn detect_dialects(&mut self, endpoint_id: String, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let Some(endpoint) = editor
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == endpoint_id)
        else {
            return;
        };
        if endpoint.probe == ProbeState::Running {
            return;
        }
        let base_url = input_value(&endpoint.base_url, cx);
        let api_key = input_value(&editor.api_key, cx);
        if let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) {
            if base_url.is_empty() {
                endpoint.probe = ProbeState::Failed;
                cx.notify();
                return;
            }
            endpoint.probe = ProbeState::Running;
        }
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let detected = app
                .gateway
                .detect_dialects(base_url, api_key)
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                if let Some(endpoint) = this.editor.as_mut().and_then(|editor| {
                    editor
                        .endpoints
                        .iter_mut()
                        .find(|endpoint| endpoint.id == endpoint_id)
                }) {
                    if detected.is_empty() {
                        endpoint.probe = ProbeState::Failed;
                    } else {
                        endpoint.enabled_dialects = detected.iter().copied().collect();
                        endpoint.probe = ProbeState::Detected;
                        if let Some(editor) = &mut this.editor {
                            editor.dialects_error = None;
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn fetch_endpoint_models(&mut self, endpoint_id: String, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let Some(endpoint) = editor
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == endpoint_id)
        else {
            return;
        };
        if endpoint.model_fetch == ModelFetchState::Running {
            return;
        }
        let base_url = input_value(&endpoint.base_url, cx);
        let api_key = input_value(&editor.api_key, cx);
        if let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) {
            if base_url.is_empty() {
                endpoint.model_fetch =
                    ModelFetchState::Failed(t(k::GATEWAY_EDITOR_ENDPOINT_URL_REQUIRED));
                cx.notify();
                return;
            }
            endpoint.model_fetch = ModelFetchState::Running;
        }
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.gateway.fetch_models(base_url, api_key).await;
            this.update(cx, |this, cx| {
                let Some(endpoint) = this.editor.as_mut().and_then(|editor| {
                    editor
                        .endpoints
                        .iter_mut()
                        .find(|endpoint| endpoint.id == endpoint_id)
                }) else {
                    return;
                };
                match result {
                    Ok(fetched) => {
                        let fetched = normalized_models(fetched);
                        let fetched_count = fetched.len();
                        endpoint.fetched_models =
                            merged_model_options(&fetched, &endpoint.models.selected);
                        endpoint.model_fetch = ModelFetchState::Fetched(fetched_count);
                    }
                    Err(error) => {
                        endpoint.model_fetch =
                            ModelFetchState::Failed(SharedString::from(error.to_string()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn test_endpoint(&mut self, endpoint_id: String, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let Some(endpoint) = editor
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == endpoint_id)
        else {
            return;
        };
        if endpoint.test == EndpointTestState::Running {
            return;
        }
        let base_url = input_value(&endpoint.base_url, cx);
        let api_key = input_value(&editor.api_key, cx);
        if let Some(endpoint) = self.editor.as_mut().and_then(|editor| {
            editor
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.id == endpoint_id)
        }) {
            if base_url.is_empty() {
                endpoint.test =
                    EndpointTestState::Failed(t(k::GATEWAY_EDITOR_ENDPOINT_URL_REQUIRED));
                cx.notify();
                return;
            }
            endpoint.test = EndpointTestState::Running;
        }
        cx.notify();

        let app = self.app.clone();
        cx.spawn(async move |this, cx| {
            let result = app.gateway.test_endpoint(base_url, api_key).await;
            this.update(cx, |this, cx| {
                let Some(endpoint) = this.editor.as_mut().and_then(|editor| {
                    editor
                        .endpoints
                        .iter_mut()
                        .find(|endpoint| endpoint.id == endpoint_id)
                }) else {
                    return;
                };
                endpoint.test = match result {
                    Ok(result) => EndpointTestState::Complete(result),
                    Err(error) => EndpointTestState::Failed(error.to_string().into()),
                };
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

    fn copy_to_clipboard(&mut self, value: String, done: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.set_status(NotificationLevel::Success, done, cx);
    }

    fn render_import_candidate(
        &self,
        candidate: &ImportCandidate,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let app_type = candidate.app_type;
        let provider_id = candidate.provider_id.clone();
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
                    this.import_provider(app_type, provider_id.clone(), cx);
                })),
            )
            .into_any_element()
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
        let enabled = station.is_enabled();
        let route_id = station.route.id.clone();
        let route_id_for_toggle = route_id.clone();
        let route_id_for_edit = route_id.clone();
        let route_id_for_delete = route_id.clone();
        let station_name = station.route.name.clone();
        let station_name_for_delete = station_name.clone();
        let base_url = station
            .primary_channel()
            .map(|channel| channel.base_url.clone())
            .unwrap_or_default();
        let endpoint_count = station
            .channels
            .iter()
            .map(|channel| {
                channel
                    .endpoint_id
                    .clone()
                    .unwrap_or_else(|| channel.base_url.clone())
            })
            .collect::<HashSet<_>>()
            .len();
        let mut model_summary = tf!(
            k::GATEWAY_CARD_MODELS,
            count = apply::station_models(&station.route, &station.channels).len(),
        );
        match station.route.reasoning.mode {
            GatewayReasoningMode::Auto => {}
            GatewayReasoningMode::Passthrough => {
                model_summary.push_str(" · ");
                model_summary.push_str(raw(k::GATEWAY_CARD_REASONING_PASSTHROUGH));
            }
            GatewayReasoningMode::Disabled => {
                model_summary.push_str(" · ");
                model_summary.push_str(raw(k::GATEWAY_CARD_REASONING_DISABLED));
            }
        }
        let dialect_badges: Vec<gpui::AnyElement> = Dialect::ALL
            .into_iter()
            .filter(|dialect| {
                station
                    .channels
                    .iter()
                    .any(|channel| channel.enabled && channel.dialect == *dialect)
            })
            .map(|dialect| {
                components::badge(dialect_badge_tone(dialect), dialect_label(dialect))
                    .into_any_element()
            })
            .collect();
        let imported = station
            .channels
            .iter()
            .any(|channel| channel.imported_from.is_some());

        let editing = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.route_id == station.route.id);
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
                                            .child(SharedString::from(station_name.clone())),
                                    )
                                    .children(dialect_badges)
                                    .when(imported, |row| {
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
                                    .child(SharedString::from(base_url)),
                            )
                            .when(endpoint_count > 1, |column| {
                                column.child(div().text_color(theme::muted()).text_xs().child(
                                    SharedString::from(tf!(
                                        k::GATEWAY_CARD_ENDPOINTS,
                                        count = endpoint_count
                                    )),
                                ))
                            })
                            .when_some(station.route.website_url.clone(), |column, website| {
                                column.child(
                                    div()
                                        .text_color(theme::muted())
                                        .text_xs()
                                        .truncate()
                                        .child(SharedString::from(website)),
                                )
                            })
                            .child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_xs()
                                    .child(SharedString::from(model_summary)),
                            ),
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
                                        station.route.id
                                    )))
                                    .role(gpui::Role::Switch)
                                    .aria_label(SharedString::from(tf!(
                                        k::GATEWAY_CARD_TOGGLE_ARIA,
                                        name = station_name
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
                                        station.route.id
                                    )),
                                    t(k::GATEWAY_ACTION_EDIT),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.open_editor_by_route_id(&route_id_for_edit, cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    SharedString::from(format!(
                                        "station-delete-{}",
                                        station.route.id
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
            .into_any_element()
    }

    fn render_endpoint_model_picker(
        &self,
        endpoint: &EndpointEditor,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let models = &endpoint.models;
        let open = endpoint.model_picker_open;
        let options = merged_model_options(&endpoint.fetched_models, &models.selected);
        let selected_count = models.selected.len();
        let summary: SharedString = if selected_count == 0 {
            t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_UNRESTRICTED)
        } else {
            tf!(
                k::GATEWAY_EDITOR_ENDPOINT_MODELS_SELECTED,
                count = selected_count
            )
            .into()
        };
        let endpoint_id_for_toggle = endpoint.id.clone();
        let trigger = div()
            .id(SharedString::from(format!(
                "station-model-picker-{}",
                endpoint.id
            )))
            .role(gpui::Role::Button)
            .aria_label(summary.clone())
            .aria_expanded(open)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .h(px(38.))
            .px_3()
            .rounded_lg()
            .border_1()
            .border_color(if open {
                theme::accent()
            } else {
                theme::border_strong()
            })
            .bg(theme::surface())
            .cursor_pointer()
            .text_sm()
            .text_color(theme::text())
            .hover(|style| style.border_color(theme::accent()).bg(theme::panel()))
            .child(div().flex_1().min_w_0().truncate().child(summary))
            .when(!endpoint.fetched_models.is_empty(), |row| {
                row.child(components::badge(
                    BadgeTone::Neutral,
                    SharedString::from(tf!(
                        k::GATEWAY_EDITOR_ENDPOINT_MODELS_AVAILABLE,
                        count = endpoint.fetched_models.len()
                    )),
                ))
            })
            .child(icon(IconName::ChevronDown, theme::muted(), 13.))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.toggle_model_picker(&endpoint_id_for_toggle, cx);
            }));

        let endpoint_id_for_all = endpoint.id.clone();
        let endpoint_id_for_clear = endpoint.id.clone();
        let mut picker = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme::border())
            .bg(theme::panel())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::subtext())
                            .text_xs()
                            .child(t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_PICKER_TITLE)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(if endpoint.fetched_models.is_empty() {
                                components::disabled_button(
                                    SharedString::from(format!(
                                        "station-models-all-{}",
                                        endpoint.id
                                    )),
                                    t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_ADD_ALL),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                    true,
                                )
                            } else {
                                components::button(
                                    SharedString::from(format!(
                                        "station-models-all-{}",
                                        endpoint.id
                                    )),
                                    t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_ADD_ALL),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.add_all_fetched_models(&endpoint_id_for_all, cx);
                                    },
                                ))
                            })
                            .child(if selected_count == 0 {
                                components::disabled_button(
                                    SharedString::from(format!(
                                        "station-models-clear-{}",
                                        endpoint.id
                                    )),
                                    t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_CLEAR),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                    true,
                                )
                            } else {
                                components::button(
                                    SharedString::from(format!(
                                        "station-models-clear-{}",
                                        endpoint.id
                                    )),
                                    t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_CLEAR),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.clear_endpoint_models(&endpoint_id_for_clear, cx);
                                    },
                                ))
                            }),
                    ),
            );

        if options.is_empty() {
            picker = picker.child(
                div()
                    .px_3()
                    .py_3()
                    .rounded_md()
                    .bg(theme::inset())
                    .text_color(theme::muted())
                    .text_xs()
                    .child(t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_PICKER_EMPTY)),
            );
        } else {
            // Seven 34px rows exceed the 224px viewport. Determine this from
            // the data rather than from `ScrollHandle::max_offset()`, which is
            // unavailable until after the first layout and would let the
            // virtualized page consume the first gesture after opening.
            let options_scrollable = options.len() > 6;
            let contained_scroll = models.scroll_handle.clone();
            let mut option_list = div()
                .id(SharedString::from(format!(
                    "station-model-options-{}",
                    endpoint.id
                )))
                .flex()
                .flex_col()
                .max_h(px(224.))
                .overflow_y_scroll()
                .track_scroll(&models.scroll_handle)
                .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(contained_scroll))
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .bg(theme::surface());
            for (option_index, model) in options.into_iter().enumerate() {
                let selected = models.selected.contains(&model);
                let endpoint_id = endpoint.id.clone();
                let model_for_click = model.clone();
                let check = div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .flex_none()
                    .rounded_sm()
                    .border_1()
                    .border_color(if selected {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .bg(if selected {
                        theme::accent_soft()
                    } else {
                        theme::surface()
                    })
                    .when(selected, |box_| {
                        box_.child(icon(IconName::Check, theme::accent(), 11.))
                    });
                option_list = option_list.child(
                    div()
                        .id(SharedString::from(format!(
                            "station-model-option-{}-{option_index}",
                            endpoint.id
                        )))
                        .role(gpui::Role::Button)
                        .aria_label(SharedString::from(model.clone()))
                        .aria_selected(selected)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .min_h(px(34.))
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(if selected {
                            theme::text()
                        } else {
                            theme::subtext()
                        })
                        .bg(if selected {
                            theme::accent_soft().alpha(0.45)
                        } else {
                            theme::surface()
                        })
                        .hover(|style| style.bg(theme::surface_hover()))
                        .child(check)
                        .child(div().min_w_0().flex_1().truncate().child(model))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.toggle_endpoint_model(&endpoint_id, &model_for_click, cx);
                        })),
                );
            }
            picker = picker.child(
                div()
                    .relative()
                    // The station editor lives inside a virtualized page list.
                    // That ancestor registers its wheel hitbox after its
                    // children, so propagation alone cannot keep a nested
                    // gesture local. This wrapper is painted before the inner
                    // scroller and removes the ancestor from wheel hit testing.
                    .when(options_scrollable, |container| container.occlude())
                    .child(option_list),
            );
        }

        let endpoint_id_for_manual = endpoint.id.clone();
        picker = picker.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(220.))
                        .child(models.manual_input.clone()),
                )
                .child(
                    components::button(
                        SharedString::from(format!("station-models-manual-add-{}", endpoint.id)),
                        t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_ADD),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.add_manual_endpoint_models(&endpoint_id_for_manual, cx);
                    })),
                ),
        );

        components::field(
            t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_LABEL),
            false,
            Some(t(k::GATEWAY_EDITOR_ENDPOINT_MODELS_HELP)),
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(trigger)
                .when(open, |control| control.child(picker)),
        )
        .into_any_element()
    }

    fn render_endpoint_editor(
        &self,
        endpoint: &EndpointEditor,
        index: usize,
        can_remove: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let endpoint_id_for_remove = endpoint.id.clone();
        let endpoint_id_for_detect = endpoint.id.clone();
        let endpoint_id_for_fetch = endpoint.id.clone();
        let endpoint_id_for_test = endpoint.id.clone();
        let dialect_controls: Vec<gpui::AnyElement> = Dialect::ALL
            .into_iter()
            .map(|dialect| {
                let selected = endpoint.enabled_dialects.contains(&dialect);
                let endpoint_id = endpoint.id.clone();
                let control = div()
                    .id(SharedString::from(format!(
                        "station-interface-{}-{}",
                        endpoint.id,
                        dialect.as_str()
                    )))
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(dialect_label(dialect)))
                    .aria_selected(selected)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .cursor_pointer()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(components::status_dot_sized(
                        if selected {
                            theme::accent()
                        } else {
                            theme::muted()
                        },
                        7.,
                    ))
                    .child(dialect_label(dialect))
                    .hover(|style| style.border_color(theme::accent().alpha(0.5)));
                let control = if selected {
                    control
                        .bg(theme::accent_soft())
                        .border_color(theme::accent().alpha(0.35))
                        .text_color(theme::accent())
                } else {
                    control
                        .bg(theme::surface())
                        .border_color(theme::border())
                        .text_color(theme::subtext())
                };
                control
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.toggle_endpoint_dialect(&endpoint_id, dialect, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        let model_field = self.render_endpoint_model_picker(endpoint, cx);

        let probe_help: SharedString = match endpoint.probe {
            ProbeState::Idle => t(k::GATEWAY_EDITOR_DIALECT_HELP_IDLE),
            ProbeState::Running => t(k::GATEWAY_EDITOR_DIALECT_HELP_RUNNING),
            ProbeState::Detected => {
                let dialects = Dialect::ALL
                    .into_iter()
                    .filter(|dialect| endpoint.enabled_dialects.contains(dialect))
                    .map(dialect_label)
                    .collect::<Vec<_>>()
                    .join(" · ");
                tf!(k::GATEWAY_EDITOR_DIALECT_HELP_DETECTED, dialect = dialects).into()
            }
            ProbeState::Failed => t(k::GATEWAY_EDITOR_DIALECT_HELP_FAILED),
        };
        let fetch_help: Option<(SharedString, gpui::Rgba)> = match &endpoint.model_fetch {
            ModelFetchState::Idle => None,
            ModelFetchState::Running => {
                Some((t(k::GATEWAY_EDITOR_MODELS_FETCHING), theme::muted()))
            }
            ModelFetchState::Fetched(count) => Some((
                tf!(k::GATEWAY_EDITOR_MODELS_FETCHED, count = count).into(),
                theme::accent(),
            )),
            ModelFetchState::Failed(error) => Some((error.clone(), theme::red())),
        };
        let test_help: Option<(SharedString, gpui::Rgba)> = match &endpoint.test {
            EndpointTestState::Idle => None,
            EndpointTestState::Running => {
                Some((t(k::GATEWAY_EDITOR_ENDPOINT_TESTING), theme::muted()))
            }
            EndpointTestState::Complete(result) => Some((
                tf!(
                    k::GATEWAY_EDITOR_ENDPOINT_TEST_RESULT,
                    status = result.status,
                    latency = result.latency_ms
                )
                .into(),
                if (200..300).contains(&result.status) {
                    theme::accent()
                } else if result.reachable {
                    theme::yellow()
                } else {
                    theme::red()
                },
            )),
            EndpointTestState::Failed(error) => Some((error.clone(), theme::red())),
        };

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(theme::border())
            .bg(theme::inset())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(tf!(
                                k::GATEWAY_EDITOR_ENDPOINT_NUMBER,
                                number = index + 1
                            ))),
                    )
                    .when(can_remove, |row| {
                        row.child(
                            components::icon_button_tone(
                                SharedString::from(format!(
                                    "station-endpoint-remove-{}",
                                    endpoint.id
                                )),
                                t(k::GATEWAY_EDITOR_ENDPOINT_REMOVE),
                                IconName::Trash,
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.remove_endpoint(&endpoint_id_for_remove, cx);
                                },
                            )),
                        )
                    }),
            )
            .child(components::field(
                t(k::GATEWAY_EDITOR_BASE_URL_LABEL),
                true,
                Some(t(k::GATEWAY_EDITOR_ENDPOINT_URL_HELP)),
                endpoint.base_url.clone(),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        components::button(
                            SharedString::from(format!("station-endpoint-test-{}", endpoint.id)),
                            if endpoint.test == EndpointTestState::Running {
                                t(k::GATEWAY_EDITOR_ENDPOINT_TESTING)
                            } else {
                                t(k::GATEWAY_EDITOR_ENDPOINT_TEST)
                            },
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.test_endpoint(endpoint_id_for_test.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("station-dialect-detect-{}", endpoint.id)),
                            if endpoint.probe == ProbeState::Running {
                                t(k::GATEWAY_EDITOR_DIALECT_DETECTING)
                            } else {
                                t(k::GATEWAY_EDITOR_DIALECT_DETECT)
                            },
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.detect_dialects(endpoint_id_for_detect.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        components::button(
                            SharedString::from(format!("station-models-fetch-{}", endpoint.id)),
                            if endpoint.model_fetch == ModelFetchState::Running {
                                t(k::GATEWAY_EDITOR_MODELS_FETCHING)
                            } else {
                                t(k::GATEWAY_EDITOR_MODELS_FETCH)
                            },
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.fetch_endpoint_models(endpoint_id_for_fetch.clone(), cx);
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(theme::muted()).text_xs().child(probe_help))
                    .when_some(test_help, |status, (text, color)| {
                        status.child(div().text_color(color).text_xs().child(text))
                    })
                    .when_some(fetch_help, |status, (text, color)| {
                        status.child(div().text_color(color).text_xs().child(text))
                    }),
            )
            .child(components::field(
                t(k::GATEWAY_EDITOR_DIALECT_LABEL),
                true,
                None,
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .children(dialect_controls),
            ))
            .child(model_field)
            .into_any_element()
    }

    fn render_editor(&self, editor: &StationEditor, cx: &mut Context<Self>) -> gpui::AnyElement {
        let reasoning_index = match editor.reasoning_mode {
            GatewayReasoningMode::Passthrough => 0,
            GatewayReasoningMode::Auto => 1,
            GatewayReasoningMode::Disabled => 2,
        };
        let on_reasoning_select = cx.listener(|this, index: &usize, _window, cx| {
            if let Some(editor) = &mut this.editor {
                editor.reasoning_mode = match index {
                    1 => GatewayReasoningMode::Auto,
                    2 => GatewayReasoningMode::Disabled,
                    _ => GatewayReasoningMode::Passthrough,
                };
            }
            this.list_state.remeasure();
            cx.notify();
        });
        let endpoint_rows: Vec<gpui::AnyElement> = editor
            .endpoints
            .iter()
            .enumerate()
            .map(|(index, endpoint)| {
                self.render_endpoint_editor(endpoint, index, editor.endpoints.len() > 1, cx)
            })
            .collect();
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
                                    .any(|station| station.route.id == editor.route_id)
                                {
                                    t(k::GATEWAY_EDITOR_TITLE_EDIT)
                                } else if editor.is_deeplink_import {
                                    t(k::GATEWAY_DEEPLINK_TITLE)
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
                                        this.close_editor(cx);
                                    },
                                )),
                            )
                            .child(
                                components::button(
                                    "station-editor-save-top",
                                    if editor.is_deeplink_import {
                                        t(k::GATEWAY_DEEPLINK_ACTION_IMPORT)
                                    } else {
                                        t(k::GATEWAY_EDITOR_SAVE)
                                    },
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
            .when(editor.is_deeplink_import, |panel| {
                panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .rounded_lg()
                        .bg(theme::accent_soft())
                        .child(
                            div()
                                .text_color(theme::accent())
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(t(k::GATEWAY_DEEPLINK_DESCRIPTION)),
                        )
                        .when_some(editor.import_source.clone(), |notice, source| {
                            notice.child(div().text_color(theme::subtext()).text_xs().child(
                                SharedString::from(tf!(
                                    k::GATEWAY_DEEPLINK_SOURCE,
                                    source = source
                                )),
                            ))
                        })
                        .when(editor.import_contains_key, |notice| {
                            notice.child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_xs()
                                    .child(t(k::GATEWAY_DEEPLINK_KEY_NOTE)),
                            )
                        }),
                )
            })
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
                    .child(div().flex_1().min_w(px(240.)).child(components::field(
                        t(k::GATEWAY_EDITOR_WEBSITE_LABEL),
                        false,
                        None,
                        editor.website_url.clone(),
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
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section_title(
                        t(k::GATEWAY_EDITOR_ENDPOINTS_TITLE),
                        t(k::GATEWAY_EDITOR_ENDPOINTS_DESCRIPTION),
                    ))
                    .child(
                        components::button(
                            "station-endpoint-add",
                            t(k::GATEWAY_EDITOR_ENDPOINT_ADD),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.add_endpoint(cx);
                            },
                        )),
                    ),
            )
            .children(endpoint_rows)
            .when_some(editor.dialects_error.clone(), |panel, error| {
                panel.child(div().text_color(theme::red()).text_xs().child(error))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme::accent_soft())
                    .child(icon(IconName::Check, theme::accent(), 14.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_color(theme::accent())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t(k::GATEWAY_EDITOR_ROUTING_AUTO_TITLE)),
                            )
                            .child(
                                div()
                                    .text_color(theme::subtext())
                                    .text_xs()
                                    .child(t(k::GATEWAY_EDITOR_ROUTING_AUTO_DESCRIPTION)),
                            ),
                    ),
            )
            .child(
                components::disclosure(
                    "station-advanced",
                    t(k::GATEWAY_EDITOR_ADVANCED_TITLE),
                    t(k::GATEWAY_EDITOR_ADVANCED_DESCRIPTION),
                    editor.show_advanced,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_editor_advanced(cx);
                })),
            )
            .when(editor.show_advanced, |panel| {
                panel
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
                                raw(k::GATEWAY_EDITOR_REASONING_OPTION_PASSTHROUGH),
                                raw(k::GATEWAY_EDITOR_REASONING_OPTION_AUTO),
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
                                        .map(
                                            |(label, input)| {
                                                div().flex_1().min_w(px(150.)).child(
                                                    components::field(
                                                        label,
                                                        false,
                                                        Some(t(
                                                            k::GATEWAY_EDITOR_REASONING_BUDGET_HELP,
                                                        )),
                                                        input,
                                                    ),
                                                )
                                            },
                                        ),
                                    ),
                            )
                        },
                    )
                    .when_some(editor.budget_error.clone(), |panel, error| {
                        panel.child(div().text_color(theme::red()).text_xs().child(error))
                    })
            })
            .into_any_element()
    }

    fn render_import_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let import_rows: Vec<gpui::AnyElement> = self
            .import_candidates
            .iter()
            .map(|candidate| self.render_import_candidate(candidate, cx))
            .collect();
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
            })
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        components::empty_state(
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
        )
        .into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let block = div().w_full().pb_5();
        match self.rows.get(index).copied() {
            Some(GatewayRow::Imports) => {
                block.child(self.render_import_panel(cx)).into_any_element()
            }
            Some(GatewayRow::Connection) => block
                .child(self.render_connection_panel(cx))
                .into_any_element(),
            Some(GatewayRow::Editor) => {
                // Keep the editor outside `self` while its element tree is
                // built, matching the previous borrow-safe rendering path.
                let editor = self.editor.take();
                let element = editor.as_ref().map(|editor| self.render_editor(editor, cx));
                self.editor = editor;
                if let Some(editor) = element {
                    block.child(editor).into_any_element()
                } else {
                    block.into_any_element()
                }
            }
            Some(GatewayRow::Empty) => block.child(self.render_empty_state(cx)).into_any_element(),
            Some(GatewayRow::Station(station_index)) => {
                if let Some(station) = self.stations.get(station_index) {
                    block
                        .child(self.render_station(station, cx))
                        .into_any_element()
                } else {
                    block.into_any_element()
                }
            }
            None => block.into_any_element(),
        }
    }
}

impl Render for GatewayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                                        this.rebuild_rows();
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
            .child(layout::wide_virtual_body(
                "relay-stations-body",
                gpui::list(
                    self.list_state.clone(),
                    cx.processor(|this, index, window, cx| this.render_row(index, window, cx)),
                ),
                &self.list_state,
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

fn dialect_badge_tone(dialect: Dialect) -> BadgeTone {
    match dialect {
        Dialect::Messages => BadgeTone::Mauve,
        Dialect::Responses => BadgeTone::Teal,
        Dialect::Chat => BadgeTone::Neutral,
    }
}

fn endpoint_editor(
    id: String,
    base_url: String,
    existing_channels: HashMap<Dialect, GatewayChannel>,
    enabled_dialects: HashSet<Dialect>,
    cx: &mut Context<GatewayView>,
) -> EndpointEditor {
    let known_models = normalized_models(
        existing_channels
            .values()
            .flat_map(|channel| channel.models.iter().cloned())
            .collect(),
    );
    let models = EndpointModelsEditor {
        selected: collapse_channel_models(&existing_channels),
        manual_input: cx.new(|cx| TextInput::new(cx, t(k::GATEWAY_EDITOR_MODELS_PLACEHOLDER))),
        scroll_handle: ScrollHandle::new(),
    };
    EndpointEditor {
        id,
        existing_channels,
        enabled_dialects,
        base_url: cx.new(|cx| text_input(cx, "https://api.example.com", &base_url)),
        models,
        fetched_models: known_models,
        model_picker_open: false,
        probe: ProbeState::Idle,
        model_fetch: ModelFetchState::Idle,
        test: EndpointTestState::Idle,
    }
}

fn normalized_models(models: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for model in models {
        let model = model.trim();
        if !model.is_empty() && !normalized.iter().any(|existing| existing == model) {
            normalized.push(model.to_string());
        }
    }
    normalized.sort();
    normalized
}

/// Collapse the per-interface model lists a station was stored with into the
/// single list the editor now edits.
///
/// Configs written before the lists merged — and imports that fill only one
/// interface — can disagree between dialects. An empty list means "no
/// restriction", so one unrestricted interface leaves the whole endpoint
/// unrestricted; otherwise the union keeps every model that routed before.
/// Widening is the safe direction here: the alternative silently stops routing
/// a model the station was serving. Disabled interfaces are ignored because
/// their lists do not reach the router, unless that would leave nothing to
/// collapse.
fn collapse_channel_models(channels: &HashMap<Dialect, GatewayChannel>) -> Vec<String> {
    let mut considered: Vec<&GatewayChannel> = channels
        .values()
        .filter(|channel| channel.enabled)
        .collect();
    if considered.is_empty() {
        considered = channels.values().collect();
    }
    if considered.iter().any(|channel| channel.models.is_empty()) {
        return Vec::new();
    }
    normalized_models(
        considered
            .into_iter()
            .flat_map(|channel| channel.models.iter().cloned())
            .collect(),
    )
}

fn merged_model_options(fetched: &[String], selected: &[String]) -> Vec<String> {
    normalized_models(fetched.iter().chain(selected).cloned().collect::<Vec<_>>())
}

fn parse_models(raw: &str) -> Vec<String> {
    let mut models = Vec::new();
    for model in raw.split(['\n', ',']) {
        let model = model.trim();
        if !model.is_empty() && !models.iter().any(|existing| existing == model) {
            models.push(model.to_string());
        }
    }
    models
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

#[cfg(test)]
mod tests {
    use super::{
        Dialect, GatewayChannel, HashMap, collapse_channel_models, merged_model_options,
        normalized_models, parse_models,
    };

    fn channel(dialect: Dialect, enabled: bool, models: &[&str]) -> (Dialect, GatewayChannel) {
        (
            dialect,
            GatewayChannel {
                id: format!("channel-{}", dialect.as_str()),
                endpoint_id: Some("endpoint".into()),
                name: "station".into(),
                dialect,
                base_url: "https://api.example.com".into(),
                api_key: "sk-test".into(),
                path_override: None,
                models: models.iter().map(|model| model.to_string()).collect(),
                model_override: None,
                priority: 0,
                weight: 1,
                enabled,
                extra_headers: Vec::new(),
                imported_from: None,
            },
        )
    }

    #[test]
    fn manual_model_list_accepts_lines_and_commas_without_duplicates() {
        assert_eq!(
            parse_models(" model-b\nmodel-a, model-b\n\n"),
            vec!["model-b".to_string(), "model-a".to_string()]
        );
    }

    #[test]
    fn fetched_models_are_trimmed_deduplicated_and_sorted() {
        assert_eq!(
            normalized_models(vec![
                " model-b ".to_string(),
                "model-a".to_string(),
                "model-b".to_string(),
                String::new(),
            ]),
            vec!["model-a".to_string(), "model-b".to_string()]
        );
    }

    #[test]
    fn picker_keeps_saved_models_that_are_missing_from_latest_fetch() {
        assert_eq!(
            merged_model_options(
                &["model-b".to_string()],
                &["custom-model".to_string(), "model-b".to_string()],
            ),
            vec!["custom-model".to_string(), "model-b".to_string()]
        );
    }

    #[test]
    fn per_dialect_lists_collapse_to_their_union() {
        let channels: HashMap<Dialect, GatewayChannel> = [
            channel(Dialect::Messages, true, &["claude-sonnet-4-6"]),
            channel(Dialect::Chat, true, &["gpt-5.5", "claude-sonnet-4-6"]),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            collapse_channel_models(&channels),
            vec!["claude-sonnet-4-6".to_string(), "gpt-5.5".to_string()]
        );
    }

    #[test]
    fn one_unrestricted_interface_leaves_the_endpoint_unrestricted() {
        let channels: HashMap<Dialect, GatewayChannel> = [
            channel(Dialect::Messages, true, &["claude-sonnet-4-6"]),
            channel(Dialect::Chat, true, &[]),
        ]
        .into_iter()
        .collect();
        assert!(collapse_channel_models(&channels).is_empty());
    }

    #[test]
    fn a_disabled_interface_does_not_widen_the_collapsed_list() {
        let channels: HashMap<Dialect, GatewayChannel> = [
            channel(Dialect::Messages, true, &["claude-sonnet-4-6"]),
            channel(Dialect::Chat, false, &[]),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            collapse_channel_models(&channels),
            vec!["claude-sonnet-4-6".to_string()]
        );
    }
}
