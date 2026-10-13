//! Schema-driven provider add/edit form.
//!
//! Instead of one generic name/baseURL/key/model form, this renders whatever
//! [`ochub_core::provider_config::AppConfig`] the selected app exposes: typed field
//! widgets (text / secret / select / toggle / key-value / model-grid) grouped in
//! sections, a live preview of the exact file(s) the app will receive, and a
//! validation strip. Saving encodes the edited values back into both
//! `settingsConfig` *and* `meta`.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::product_ui::provider_editor as editor_screen;
use gpui::{
    Anchor, Context, Entity, Focusable, FontWeight, HighlightStyle, ListAlignment, ListOffset,
    ListState, MouseButton, Pixels, SharedString, StyledText, Task, Window, anchored, deferred,
    div, point, prelude::*, px, uniform_list,
};
use ochub_core::application::{
    DraftApplyState, EditorCommonConfig, EditorDraft, EditorDraftApplyResult, EditorDraftInput,
    ProviderSwitchPolicy, provider_revision_hash,
};
use ochub_core::gateway::apply;
use ochub_core::plugin::{AppMode, AppPlugin};
use ochub_core::provider_config::{
    self, AppConfig, ConfigIssue, FieldKind, FormField, FormSection, FormValues, GridCellKind,
    Language, Severity, StationCapabilities, bool_val, str_val,
};
use ochub_core::services::ConfigService;
use ochub_core::{AppId, AppState, AppType, Provider, ProviderMeta, UsageResult};
use serde_json::{Map, Value, json};

use crate::code_editor::CodeEditor;
use crate::components;
use crate::components::{BadgeTone, ButtonSize, ButtonTone};
use crate::fold::{FoldRegion, fold_regions};
use crate::highlight::{self, Lang};
use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::product_ui::shell::content_viewport;
use crate::remote::WorkspaceBackend;
use crate::text_input::{TextInput, TextInputEvent};
use crate::tf;
use crate::theme;
use ochub_ui::modal::{self, ModalFocusState};

/// Outcome of the editor, observed by the host view via `cx.subscribe`.
pub enum EditorEvent {
    Saved { leave: bool },
    Applied { leave: bool },
    Cancelled,
}

impl gpui::EventEmitter<EditorEvent> for ProviderEditor {}

struct KvRow {
    id: usize,
    key: Entity<TextInput>,
    value: Entity<TextInput>,
}

struct GridRow {
    id: usize,
    cells: HashMap<String, Entity<TextInput>>,
    toggles: HashMap<String, bool>,
    /// Native row fields that this schema does not render still belong to it.
    extras: Map<String, Value>,
}

/// Active "edit this file directly" modal over the preview.
struct RawEdit {
    file_index: usize,
    filename: SharedString,
    input: Entity<CodeEditor>,
    error: Option<SharedString>,
    original_content: String,
}

#[derive(Clone)]
struct PreviewDocument {
    filename: SharedString,
    language_label: &'static str,
    lang: Lang,
    content: Arc<str>,
    line_ranges: Arc<Vec<Range<usize>>>,
    regions: Arc<Vec<FoldRegion>>,
    region_headers: Arc<HashSet<usize>>,
    visible_rows: Arc<Vec<usize>>,
    max_line_cells: usize,
}

impl PreviewDocument {
    fn line(&self, index: usize) -> &str {
        self.line_ranges
            .get(index)
            .and_then(|range| self.content.get(range.clone()))
            .unwrap_or("")
    }

    fn line_count(&self) -> usize {
        self.line_ranges.len()
    }
}

#[derive(Clone, Default)]
struct PreviewCache {
    files: Vec<PreviewDocument>,
    issues: Vec<ConfigIssue>,
    error_count: usize,
    warning_count: usize,
}

struct PreviewBuild {
    cache: PreviewCache,
    total_lines: usize,
    total_bytes: usize,
    elapsed: Duration,
}

const PREVIEW_REFRESH_DELAY: Duration = Duration::from_millis(140);
const PREVIEW_FOLD_BACKGROUND_LINE_THRESHOLD: usize = 4_096;
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorTab {
    Basic,
    Advanced,
    Files,
}

#[derive(Clone)]
struct PendingApply {
    draft: EditorDraft,
    snapshot: Value,
}

/// Where a channel gets its endpoint from: typed in by hand, or supplied by a
/// configured relay station through the local gateway.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderSource {
    Direct,
    Station,
}

/// Gateway coordinates for the selected station, resolved in the background
/// so the live preview can show the exact endpoint + key that a save would
/// embed. Keyed by route id so a station change invalidates it.
#[derive(Clone)]
struct StationGatewayInfo {
    route_id: String,
    origin: String,
    key: String,
}

/// Which model-suggestion popover is open. Suggestions only exist in station
/// mode, where the station declares the exact upstream models it serves.
#[derive(Clone, PartialEq)]
enum ModelSuggestionTarget {
    /// A schema text field (`model` or Grok Build's `upstream_model`).
    Field(String),
    /// The `model` cell of one grid row: (grid field id, row id).
    GridCell(String, usize),
}

impl ModelSuggestionTarget {
    fn element_id(&self) -> String {
        match self {
            Self::Field(id) => format!("model-suggest-field-{id}"),
            Self::GridCell(field, row) => format!("model-suggest-grid-{field}-{row}"),
        }
    }
}

pub struct ProviderEditor {
    app: Arc<AppState>,
    backend: WorkspaceBackend,
    backend_available: bool,
    app_id: AppId,
    app_type: Option<AppType>,
    plugin: Arc<dyn AppPlugin>,
    plugin_revision: Option<String>,
    codec: Box<dyn AppConfig>,
    /// Shared so each frame's render iterates it without a deep clone.
    schema: Arc<Vec<FormSection>>,
    /// Working form values; text/kv/grid inputs are pulled into this on demand.
    values: FormValues,
    /// The authoritative working document (`settingsConfig`) that form values
    /// are merged ONTO. Starts as the stored config; a direct file edit
    /// replaces it wholesale, so keys the form doesn't model survive editing,
    /// preview, and save.
    working_base: Value,
    original_id: Option<String>,
    original_provider: Option<Provider>,
    provider_id: Entity<TextInput>,
    name: Entity<TextInput>,
    website_url: Entity<TextInput>,
    category: Entity<TextInput>,
    notes: Entity<TextInput>,
    text_inputs: HashMap<String, Entity<TextInput>>,
    revealed_secrets: HashSet<String>,
    kv_rows: HashMap<String, Vec<KvRow>>,
    grid_rows: HashMap<String, Vec<GridRow>>,
    next_row_id: usize,
    /// Index of the last applied preset (drives the preset segmented control's
    /// selection highlight only; applying a preset behaves exactly as before).
    selected_preset: Option<usize>,
    /// Codec presets, resolved once. Building one runs a full `decode`, and
    /// `render_form_intro` runs every frame — this must not be recomputed
    /// there.
    presets: Arc<Vec<provider_config::Preset>>,
    /// Field id of the one schema select whose dropdown is currently open.
    open_select_field: Option<String>,
    /// Collapsed fold regions in the preview pane: (file index, header line).
    preview_collapsed: HashSet<(usize, usize)>,
    /// Only the selected file is mounted; this keeps multi-file previews from
    /// stacking several independent documents into one enormous page.
    preview_active_file: usize,
    preview_cache: PreviewCache,
    preview_dirty: bool,
    /// Invalidates an in-flight preview build as soon as newer form input
    /// arrives. Expensive line/fold indexing runs off the UI thread.
    preview_generation: u64,
    preview_applied_generation: u64,
    preview_refresh_task: Option<Task<()>>,
    /// When `Some`, a modal code editor for one preview file is open.
    raw_edit: Option<RawEdit>,
    raw_focus: ModalFocusState,
    convert_focus: ModalFocusState,
    common_config_supported: bool,
    common_config_enabled: bool,
    common_snippet: Entity<TextInput>,
    original_snippet: String,
    original_snippet_present: bool,
    common_base_pinned: bool,
    convert_open: bool,
    /// Prevent duplicate submissions while provider/config writes run off the
    /// UI thread.
    saving: bool,
    error: Option<SharedString>,
    status: Option<SharedString>,
    /// Channel source; `Station` hides endpoint/credential fields and embeds
    /// the local gateway's coordinates instead.
    source: ProviderSource,
    /// Stations compatible with this app, loaded from core in the background.
    station_options: Vec<apply::StationChannelOption>,
    station_options_loaded: bool,
    /// Selected station route id while `source == Station`.
    selected_station: Option<String>,
    station_dropdown_open: bool,
    /// Gateway origin + shared key for the selected station (preview only;
    /// the save path re-resolves both against the running gateway).
    station_gateway: Option<StationGatewayInfo>,
    open_model_suggestion: Option<ModelSuggestionTarget>,
    /// Severity of whichever toast `take_toast` will hand over next. Always
    /// set alongside `error`/`status` so the host never has to guess it from
    /// the wording.
    status_level: Option<NotificationLevel>,
    form_list_state: ListState,
    form_stack_grid: bool,
    form_official_login: bool,
    show_advanced_fields: bool,
    tab: EditorTab,
    draft_id: String,
    draft_revision: Option<u64>,
    stable_provider_id: String,
    base_provider_hash: Option<String>,
    saved_snapshot: Value,
    raw_documents: Option<Vec<String>>,
    direct_values: Option<FormValues>,
    station_values: Option<FormValues>,
    official_values: Option<FormValues>,
    checked_snapshot: Option<Value>,
    applied: bool,
    pending_apply: Option<PendingApply>,
    receipt_id: Option<String>,
    undo_provider: Option<Option<Provider>>,
    undo_draft: Option<EditorDraft>,
    field_errors: HashMap<String, SharedString>,
    operation_note: Option<SharedString>,
    operation_failed: bool,
}

impl ProviderEditor {
    /// Re-apply the current locale to state that a repaint cannot reach.
    ///
    /// A `TextInput` captures its placeholder when it is constructed, and this
    /// editor outlives a language change while it is open, so the placeholders
    /// have to be pushed in by hand. The form is a virtualized list whose item
    /// heights are memoized until the width changes, so a translation that
    /// makes a row taller or shorter also needs an explicit remeasure.
    ///
    /// Re-read translated schema metadata without replacing input entities or
    /// their drafts, selection, focus or undo history.
    pub fn relocalize(&mut self, cx: &mut Context<Self>) {
        self.provider_id.update(cx, |input, cx| {
            input.set_placeholder(t(k::PROVIDER_EDITOR_IDENTITY_ID_PLACEHOLDER), cx)
        });
        self.name.update(cx, |input, cx| {
            input.set_placeholder(t(k::PROVIDER_EDITOR_IDENTITY_NAME_PLACEHOLDER), cx)
        });
        self.notes.update(cx, |input, cx| {
            input.set_placeholder(t(k::PROVIDER_EDITOR_IDENTITY_NOTES_PLACEHOLDER), cx)
        });
        self.common_snippet.update(cx, |input, cx| {
            input.set_placeholder(t(k::PROVIDER_EDITOR_COMMON_CONFIG_SNIPPET_PLACEHOLDER), cx)
        });
        self.schema = Arc::new(self.codec.schema());
        self.presets = Arc::new(self.codec.presets());
        for field in self.schema.iter().flat_map(|section| &section.fields) {
            match &field.kind {
                FieldKind::Text { placeholder } | FieldKind::Secret { placeholder } => {
                    if let Some(input) = self.text_inputs.get(&field.id) {
                        input.update(cx, |input, cx| {
                            input.set_placeholder(placeholder.clone(), cx)
                        });
                    }
                }
                FieldKind::KeyValue {
                    key_placeholder,
                    value_placeholder,
                } => {
                    for row in self.kv_rows.get(&field.id).into_iter().flatten() {
                        row.key.update(cx, |input, cx| {
                            input.set_placeholder(key_placeholder.clone(), cx)
                        });
                        row.value.update(cx, |input, cx| {
                            input.set_placeholder(value_placeholder.clone(), cx)
                        });
                    }
                }
                FieldKind::ModelGrid { columns } => {
                    for row in self.grid_rows.get(&field.id).into_iter().flatten() {
                        for column in columns {
                            if let GridCellKind::Text { placeholder } = &column.kind
                                && let Some(input) = row.cells.get(&column.key)
                            {
                                input.update(cx, |input, cx| {
                                    input.set_placeholder(placeholder.clone(), cx)
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
        cx.notify();
    }

    pub(crate) fn shortcut_save(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.convert_open {
            return;
        }
        self.submit_draft(false, false, cx);
    }

    pub(crate) fn shortcut_cancel(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.convert_open {
            self.close_convert(cx);
        } else if self.raw_edit.is_some() {
            self.close_raw_edit(cx);
        } else if self.open_select_field.take().is_some()
            || self.open_model_suggestion.take().is_some()
            || std::mem::take(&mut self.station_dropdown_open)
        {
            cx.notify();
        } else {
            cx.emit(EditorEvent::Cancelled);
        }
    }

    pub fn new_add(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_id: AppId,
        plugin: Arc<dyn AppPlugin>,
        codec: Box<dyn AppConfig>,
        cx: &mut Context<Self>,
    ) -> Self {
        let schema = codec.schema();
        let values = codec.decode(&Value::Null, None);
        let mut this = Self::base(
            app, backend, app_id, plugin, codec, schema, values, None, None, cx,
        );
        Self::observe_preview_input(&this.category, cx);
        this.build_inputs(cx);
        this.load_station_options(cx);
        this.start_preview_build(cx);
        this.saved_snapshot = this.editor_snapshot(cx);
        this
    }

    pub fn new_edit(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_id: AppId,
        plugin: Arc<dyn AppPlugin>,
        codec: Box<dyn AppConfig>,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> Self {
        let app_type = AppType::from_app_id(&app_id);
        let schema = codec.schema();
        let values = codec.decode(&provider.settings_config, provider.meta.as_ref());
        let station_route = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.gateway_route_id.clone())
            .filter(|_| app_type.is_some_and(provider_config::station_source_supported));
        let mut this = Self::base(
            app,
            backend,
            app_id,
            plugin,
            codec,
            schema,
            values,
            Some(provider.id.clone()),
            Some(provider.clone()),
            cx,
        );
        if let Some(route_id) = station_route {
            this.source = ProviderSource::Station;
            this.selected_station = Some(route_id);
        }
        this.set_identity(provider, cx);
        Self::observe_preview_input(&this.category, cx);
        this.build_inputs(cx);
        this.load_station_options(cx);
        this.start_preview_build(cx);
        this.saved_snapshot = this.editor_snapshot(cx);
        this
    }

    #[allow(clippy::too_many_arguments)]
    fn base(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_id: AppId,
        plugin: Arc<dyn AppPlugin>,
        codec: Box<dyn AppConfig>,
        schema: Vec<FormSection>,
        values: FormValues,
        original_id: Option<String>,
        original_provider: Option<Provider>,
        cx: &mut Context<Self>,
    ) -> Self {
        let working_base = original_provider
            .as_ref()
            .map(|p| p.settings_config.clone())
            .unwrap_or(Value::Null);
        let app_type = AppType::from_app_id(&app_id);
        let common_config_supported = app_type.is_some_and(common_config_supported);
        let presets = Arc::new(codec.presets());
        // A preset is selected only when the user actually applies it. The
        // presence or category of a connection does not establish its origin,
        // and restoring a draft must never silently apply template defaults.
        let selected_preset = None;
        let common_config_enabled = original_provider
            .as_ref()
            .and_then(|provider| provider.meta.as_ref())
            .and_then(|meta| meta.common_config_enabled)
            .unwrap_or(false);
        let original_snippet = String::new();
        let form_item_count = schema.len() + 3;
        let common_snippet = cx.new(|cx| {
            let mut input =
                TextInput::new(cx, t(k::PROVIDER_EDITOR_COMMON_CONFIG_SNIPPET_PLACEHOLDER))
                    .code(true)
                    .multiline(true);
            input.set_content("", cx);
            input
        });
        let stable_provider_id = original_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let base_provider_hash = original_provider.as_ref().map(provider_revision_hash);
        let plugin_revision = plugin.configuration_revision().map(str::to_owned);
        let mut this = Self {
            app,
            backend,
            backend_available: true,
            app_id,
            app_type,
            plugin,
            plugin_revision,
            codec,
            schema: Arc::new(schema),
            values,
            working_base,
            original_id,
            original_provider,
            provider_id: cx
                .new(|cx| TextInput::new(cx, t(k::PROVIDER_EDITOR_IDENTITY_ID_PLACEHOLDER))),
            name: cx.new(|cx| TextInput::new(cx, t(k::PROVIDER_EDITOR_IDENTITY_NAME_PLACEHOLDER))),
            website_url: cx.new(|cx| TextInput::new(cx, "https://example.com")),
            category: cx.new(|cx| TextInput::new(cx, "aggregator / official / third_party")),
            notes: cx.new(|cx| {
                TextInput::new(cx, t(k::PROVIDER_EDITOR_IDENTITY_NOTES_PLACEHOLDER)).multiline(true)
            }),
            text_inputs: HashMap::new(),
            revealed_secrets: HashSet::new(),
            kv_rows: HashMap::new(),
            grid_rows: HashMap::new(),
            next_row_id: 0,
            selected_preset,
            presets,
            open_select_field: None,
            preview_collapsed: HashSet::new(),
            preview_active_file: 0,
            preview_cache: PreviewCache::default(),
            preview_dirty: true,
            preview_generation: 1,
            preview_applied_generation: 0,
            preview_refresh_task: None,
            raw_edit: None,
            raw_focus: ModalFocusState::new(cx),
            convert_focus: ModalFocusState::new(cx),
            common_config_supported,
            common_config_enabled,
            common_snippet,
            original_snippet,
            original_snippet_present: false,
            common_base_pinned: false,
            convert_open: false,
            saving: false,
            error: None,
            status: None,
            status_level: None,
            source: ProviderSource::Direct,
            station_options: Vec::new(),
            station_options_loaded: false,
            selected_station: None,
            station_dropdown_open: false,
            station_gateway: None,
            open_model_suggestion: None,
            form_list_state: ListState::new(form_item_count, ListAlignment::Top, px(720.)),
            form_stack_grid: false,
            form_official_login: false,
            show_advanced_fields: false,
            tab: EditorTab::Basic,
            draft_id: uuid::Uuid::new_v4().to_string(),
            draft_revision: None,
            stable_provider_id,
            base_provider_hash,
            saved_snapshot: Value::Null,
            raw_documents: None,
            direct_values: None,
            station_values: None,
            official_values: None,
            checked_snapshot: None,
            applied: false,
            pending_apply: None,
            receipt_id: None,
            undo_provider: None,
            undo_draft: None,
            field_errors: HashMap::new(),
            operation_note: None,
            operation_failed: false,
        };
        for input in [
            &this.name,
            &this.provider_id,
            &this.website_url,
            &this.category,
            &this.notes,
            &this.common_snippet,
        ] {
            Self::observe_preview_input(input, cx);
        }
        this.load_common_snippet(cx);
        this
    }

    fn load_common_snippet(&mut self, cx: &mut Context<Self>) {
        if !self.common_config_supported {
            return;
        }
        let backend = self.backend.clone();
        let app_id = self.app_id.clone();
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(async move { backend.common_config(&app_id).await }).await;
            this.update(cx, |this, cx| {
                if this.common_base_pinned {
                    return;
                }
                let snippet = match result {
                    Ok(snippet) => snippet,
                    Err(error) => {
                        log::warn!(
                            "failed to load common config snippet for {}: {}",
                            this.app_id.as_str(),
                            error
                        );
                        None
                    }
                };
                // A fast typist may have changed the empty editor before the
                // database result returned. Keep their text, while still
                // recording the stored baseline for the save comparison.
                let was_clean = !this.is_dirty(cx);
                let untouched = this.draft_revision.is_none()
                    && this.common_snippet.read(cx).content().is_empty();
                this.original_snippet_present = snippet.is_some();
                let snippet = snippet.unwrap_or_default();
                this.original_snippet = snippet.clone();
                this.common_base_pinned = true;
                if untouched {
                    this.common_snippet
                        .update(cx, |input, cx| input.set_content(snippet, cx));
                }
                if was_clean {
                    this.saved_snapshot = this.editor_snapshot(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_identity(&mut self, provider: &Provider, cx: &mut Context<Self>) {
        self.provider_id
            .update(cx, |i, cx| i.set_content(provider.id.clone(), cx));
        self.name
            .update(cx, |i, cx| i.set_content(provider.name.clone(), cx));
        self.website_url.update(cx, |i, cx| {
            i.set_content(provider.website_url.clone().unwrap_or_default(), cx)
        });
        self.category.update(cx, |i, cx| {
            i.set_content(provider.category.clone().unwrap_or_default(), cx)
        });
        self.notes.update(cx, |i, cx| {
            i.set_content(provider.notes.clone().unwrap_or_default(), cx)
        });
    }

    // ---- relay-station source -------------------------------------------------

    /// Load the stations this app could draw from. Only runs for apps whose
    /// codec supports the station source; everyone else never sees the toggle.
    fn load_station_options(&mut self, cx: &mut Context<Self>) {
        if !self
            .app_type
            .is_some_and(provider_config::station_source_supported)
        {
            self.station_options_loaded = true;
            return;
        }
        let backend = self.backend.clone();
        let Some(app_type) = self.app_type else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(
                    async move { backend.station_channel_options(app_type).await },
                )
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(options) => this.station_options = options,
                    Err(error) => {
                        log::warn!("failed to load relay stations: {error}");
                    }
                }
                this.station_options_loaded = true;
                // Capabilities ride along with the options, so what the
                // station can back is only known now.
                this.clamp_station_fields();
                this.refresh_station_gateway(cx);
                this.invalidate_preview(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Resolve the gateway origin + shared station key for the picker preview.
    /// The save path re-does this against the *running* gateway; here the
    /// configured port is close enough and starting the service just to paint
    /// a preview would be wrong.
    fn refresh_station_gateway(&mut self, cx: &mut Context<Self>) {
        let Some(route_id) = self.selected_station.clone() else {
            self.station_gateway = None;
            return;
        };
        if self
            .station_gateway
            .as_ref()
            .is_some_and(|info| info.route_id == route_id)
        {
            return;
        }
        let backend = self.backend.clone();
        let Some(app_type) = self.app_type else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .station_gateway_preview(&route_id, app_type)
                    .await
                    .map(|(origin, key)| (route_id, origin, key))
            })
            .await;
            this.update(cx, |this, cx| {
                let Ok((route_id, origin, key)) = result else {
                    return;
                };
                // A station change raced us; that selection has its own task.
                if this.selected_station.as_deref() != Some(route_id.as_str()) {
                    return;
                }
                this.station_gateway = Some(StationGatewayInfo {
                    route_id,
                    origin,
                    key,
                });
                this.invalidate_preview(cx);
            })
            .ok();
        })
        .detach();
    }

    fn set_source(&mut self, source: ProviderSource, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        match self.source {
            ProviderSource::Direct => self.direct_values = Some(self.current_values(cx)),
            ProviderSource::Station => self.station_values = Some(self.current_values(cx)),
        }
        self.source = source;
        let restored = match source {
            ProviderSource::Direct => self.direct_values.clone(),
            ProviderSource::Station => self.station_values.clone(),
        };
        if let Some(values) = restored {
            self.apply_values(values, cx);
        }
        if self.app_type == Some(AppType::Codex) && str_val(&self.values, "auth_mode") != "api_key"
        {
            provider_config::set_str(
                &mut self.values,
                "auth_mode",
                if source == ProviderSource::Station {
                    "api_key"
                } else {
                    "openai_login_with_api_key"
                },
            );
        }
        self.open_model_suggestion = None;
        self.station_dropdown_open = false;
        if source == ProviderSource::Station {
            self.clamp_station_fields();
            self.refresh_station_gateway(cx);
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn select_station(&mut self, route_id: String, cx: &mut Context<Self>) {
        if self.selected_station.as_deref() == Some(route_id.as_str()) {
            self.station_dropdown_open = false;
            cx.notify();
            return;
        }
        self.selected_station = Some(route_id.clone());
        self.station_dropdown_open = false;
        self.station_gateway = None;
        // Seed the channel name from the station when the user has not typed
        // one yet; a name they wrote is never overwritten.
        if self.name.read(cx).content().trim().is_empty()
            && let Some(option) = self
                .station_options
                .iter()
                .find(|option| option.route_id == route_id)
        {
            let name = option.name.clone();
            self.name
                .update(cx, |input, cx| input.set_content(name, cx));
        }
        self.clamp_station_fields();
        self.refresh_station_gateway(cx);
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    /// What the selected station can back. An unknown station (options still
    /// loading, or one that vanished) reads as "nothing extra", which only
    /// ever narrows the form.
    fn station_capabilities(&self) -> StationCapabilities {
        self.selected_station
            .as_deref()
            .and_then(|route_id| {
                self.station_options
                    .iter()
                    .find(|option| option.route_id == route_id)
            })
            .map(|option| option.capabilities)
            .unwrap_or_default()
    }

    /// Pull the station-constrained fields back into range for the selected
    /// station, so the form shows the same choice a save would write.
    fn clamp_station_fields(&mut self) {
        if self.source != ProviderSource::Station {
            return;
        }
        let caps = self.station_capabilities();
        if let Some(app_type) = self.app_type {
            provider_config::clamp_station_fields(&mut self.values, app_type, caps);
        }
    }

    /// Models the selected station declares, for suggestion popovers.
    fn station_models(&self) -> &[String] {
        self.selected_station
            .as_deref()
            .and_then(|route_id| {
                self.station_options
                    .iter()
                    .find(|option| option.route_id == route_id)
            })
            .map(|option| option.models.as_slice())
            .unwrap_or(&[])
    }

    /// The text currently sitting in the input a suggestion popover hangs off.
    /// The popover filters against it live, so a half-typed name narrows the
    /// station catalog instead of making the user scroll it.
    fn model_query(&self, target: &ModelSuggestionTarget, cx: &Context<Self>) -> String {
        let input = match target {
            ModelSuggestionTarget::Field(id) => self.text_inputs.get(id),
            ModelSuggestionTarget::GridCell(field, row) => self
                .grid_rows
                .get(field)
                .and_then(|rows| rows.iter().find(|r| r.id == *row))
                .and_then(|row| row.cells.get("model")),
        };
        input
            .map(|input| input.read(cx).content().trim().to_string())
            .unwrap_or_default()
    }

    /// Open the popover for `target` (or close whatever is open when `None`).
    /// The popover floats, so this never changes form layout — no remeasure.
    fn set_model_suggestion(
        &mut self,
        target: Option<ModelSuggestionTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.open_model_suggestion == target {
            return;
        }
        self.open_model_suggestion = target;
        cx.notify();
    }

    fn apply_model_suggestion(
        &mut self,
        target: ModelSuggestionTarget,
        model: String,
        cx: &mut Context<Self>,
    ) {
        let input = match &target {
            ModelSuggestionTarget::Field(id) => self.text_inputs.get(id).cloned(),
            ModelSuggestionTarget::GridCell(field, row) => self
                .grid_rows
                .get(field)
                .and_then(|rows| rows.iter().find(|r| r.id == *row))
                .and_then(|row| row.cells.get("model"))
                .cloned(),
        };
        if let Some(input) = input {
            input.update(cx, |input, cx| input.set_content(model, cx));
        }
        self.open_model_suggestion = None;
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    /// Build text/kv/grid input entities from the current `values`.
    fn build_inputs(&mut self, cx: &mut Context<Self>) {
        let fields: Vec<FormField> = self
            .schema
            .iter()
            .flat_map(|s| s.fields.iter().cloned())
            .collect();
        for field in fields {
            match &field.kind {
                FieldKind::Text { placeholder } | FieldKind::Secret { placeholder } => {
                    let masked = matches!(field.kind, FieldKind::Secret { .. })
                        && !self.revealed_secrets.contains(&field.id);
                    let content = str_val(&self.values, &field.id).to_string();
                    let placeholder = placeholder.clone();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, placeholder).masked(masked);
                        input.set_content(content, cx);
                        input
                    });
                    Self::observe_preview_input(&input, cx);
                    self.text_inputs.insert(field.id.clone(), input);
                }
                FieldKind::KeyValue { .. } => {
                    let mut rows = Vec::new();
                    if let Some(obj) = self.values.get(&field.id).and_then(Value::as_object) {
                        for (k, v) in obj.clone() {
                            let id = self.next_row_id;
                            self.next_row_id += 1;
                            let v = v.as_str().unwrap_or_default().to_string();
                            let key = cx.new(|cx| {
                                let mut input = TextInput::new(cx, "key");
                                input.set_content(k.clone(), cx);
                                input
                            });
                            let value = cx.new(|cx| {
                                let mut input = TextInput::new(cx, "value");
                                input.set_content(v, cx);
                                input
                            });
                            Self::observe_preview_input(&key, cx);
                            Self::observe_preview_input(&value, cx);
                            rows.push(KvRow { id, key, value });
                        }
                    }
                    self.kv_rows.insert(field.id.clone(), rows);
                }
                FieldKind::ModelGrid { columns } => {
                    let columns = columns.clone();
                    let mut rows = Vec::new();
                    if let Some(arr) = self
                        .values
                        .get(&field.id)
                        .and_then(Value::as_array)
                        .cloned()
                    {
                        for row in arr {
                            let id = self.next_row_id;
                            self.next_row_id += 1;
                            let mut cells = HashMap::new();
                            let mut toggles = HashMap::new();
                            let extras = row.as_object().cloned().unwrap_or_default();
                            for col in &columns {
                                match &col.kind {
                                    GridCellKind::Text { placeholder } => {
                                        let content = grid_text_display(row.get(&col.key));
                                        let placeholder = placeholder.clone();
                                        let input = cx.new(|cx| {
                                            let mut input = TextInput::new(cx, placeholder);
                                            input.set_content(content, cx);
                                            input
                                        });
                                        Self::observe_preview_input(&input, cx);
                                        cells.insert(col.key.clone(), input);
                                    }
                                    GridCellKind::Toggle => {
                                        let on = row
                                            .get(&col.key)
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                        toggles.insert(col.key.clone(), on);
                                    }
                                }
                            }
                            rows.push(GridRow {
                                id,
                                cells,
                                toggles,
                                extras,
                            });
                        }
                    }
                    self.grid_rows.insert(field.id.clone(), rows);
                }
                FieldKind::Select { .. } | FieldKind::Toggle => {}
            }
        }
        self.form_list_state.remeasure();
    }

    fn observe_preview_input(input: &Entity<TextInput>, cx: &mut Context<Self>) {
        cx.subscribe(input, |this, _input, _: &TextInputEvent, cx| {
            this.field_errors.clear();
            // An open model popover filters against the live input text, so a
            // keystroke has to repaint the form, not just the input itself.
            if this.open_model_suggestion.is_some() {
                cx.notify();
            }
            this.schedule_preview_refresh(cx);
            cx.notify();
        })
        .detach();
    }

    /// Text fields can emit several changes in one typing burst. Keep the form
    /// responsive immediately and rebuild the potentially multi-megabyte native
    /// document only after the user pauses briefly.
    fn schedule_preview_refresh(&mut self, cx: &mut Context<Self>) {
        if self.checked_snapshot.take().is_some() {
            self.operation_note = None;
        }
        self.preview_dirty = true;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_refresh_task.take();
        self.preview_refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_REFRESH_DELAY).await;
            this.update(cx, |this, cx| {
                if this.preview_dirty {
                    this.start_preview_build(cx);
                }
            })
            .ok();
        }));
    }

    fn invalidate_preview(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview_refresh(cx);
        cx.notify();
    }

    fn start_preview_build(&mut self, cx: &mut Context<Self>) {
        if !self.preview_dirty {
            return;
        }
        self.pull_values(cx);
        let generation = self.preview_generation;
        let plugin = self.plugin.clone();
        let values = self.values.clone();
        let working_base = self.working_base.clone();
        let category = self.category.read(cx).content().trim().to_string();
        let collapsed = self.preview_collapsed.clone();
        let raw_documents = self.raw_documents.clone();
        self.preview_dirty = false;
        self.preview_refresh_task = None;

        cx.spawn(async move |this, cx| {
            let build = cx
                .background_spawn(async move {
                    let codec = plugin.provider_config()?;
                    Some(build_preview_cache(
                        codec.as_ref(),
                        &values,
                        &working_base,
                        (!category.is_empty()).then_some(category.as_str()),
                        &collapsed,
                        raw_documents.as_deref(),
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.preview_generation {
                    return;
                }
                if let Some(build) = build {
                    this.apply_preview_build(build);
                }
                this.preview_applied_generation = generation;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_preview_current(&mut self, cx: &Context<Self>) {
        if self.preview_applied_generation == self.preview_generation {
            return;
        }
        self.pull_values(cx);
        let category = self.category.read(cx).content().trim().to_string();
        let build = build_preview_cache(
            self.codec.as_ref(),
            &self.values,
            &self.working_base,
            (!category.is_empty()).then_some(category.as_str()),
            &self.preview_collapsed,
            self.raw_documents.as_deref(),
        );
        self.apply_preview_build(build);
        self.preview_applied_generation = self.preview_generation;
    }

    fn apply_preview_build(&mut self, build: PreviewBuild) {
        self.preview_cache = build.cache;
        self.preview_active_file = self
            .preview_active_file
            .min(self.preview_cache.files.len().saturating_sub(1));
        self.preview_dirty = false;
        log::debug!(
            "provider preview cache rebuilt: {} files, {} lines, {} bytes in {:?}",
            self.preview_cache.files.len(),
            build.total_lines,
            build.total_bytes,
            build.elapsed
        );
    }

    /// Replace the working values and rebuild every input widget from them.
    fn apply_values(&mut self, values: FormValues, cx: &mut Context<Self>) {
        self.values = values;
        self.text_inputs.clear();
        self.kv_rows.clear();
        self.grid_rows.clear();
        self.build_inputs(cx);
        self.open_select_field = None;
        self.invalidate_preview(cx);
        self.form_list_state.remeasure();
    }

    /// Apply a built-in preset: replace values and rebuild inputs.
    fn apply_preset(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(preset) = self.presets.get(index) else {
            return;
        };
        let values = preset.values.clone();
        if let Some(category) = preset.category.clone() {
            self.category
                .update(cx, |input, cx| input.set_content(category, cx));
        }
        if let Some(display_name) = preset.display_name.clone() {
            let current = self.name.read(cx).content().trim().to_string();
            if current.is_empty() && !display_name.is_empty() {
                self.name
                    .update(cx, |input, cx| input.set_content(display_name, cx));
            }
        }
        if let Some(website_url) = preset.website_url.clone() {
            let current = self.website_url.read(cx).content().trim().to_string();
            if current.is_empty() && !website_url.is_empty() {
                self.website_url
                    .update(cx, |input, cx| input.set_content(website_url, cx));
            }
        }
        self.apply_values(values, cx);
        self.selected_preset = Some(index);
        self.form_official_login = self.uses_official_login(cx);
        self.form_list_state.remeasure();
        cx.notify();
    }

    /// Open the modal code editor for preview file `index`.
    fn open_raw_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.ensure_preview_current(cx);
        if let Some(file) = self.preview_cache.files.get(index) {
            let original_content = self
                .raw_documents
                .as_ref()
                .and_then(|files| files.get(index))
                .cloned()
                .unwrap_or_else(|| file.content.to_string());
            let content = SharedString::from(original_content.clone());
            let filename = file.filename.clone();
            let lang = file.lang;
            let input = cx.new(|cx| {
                let mut input = CodeEditor::new(cx, lang, "");
                input.set_content(content, cx);
                input
            });
            cx.observe(&input, |_this, _input, cx| cx.notify()).detach();
            self.raw_edit = Some(RawEdit {
                file_index: index,
                filename,
                input,
                error: None,
                original_content,
            });
            cx.notify();
        }
    }

    fn close_raw_edit(&mut self, cx: &mut Context<Self>) {
        self.raw_documents = self.current_raw_documents(cx);
        self.raw_edit = None;
        self.invalidate_preview(cx);
        cx.notify();
    }

    /// Switching back to structured fields must never silently ignore a newer
    /// native document. Invalid text remains editable/savable on the file tab.
    fn merge_pending_raw(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(documents) = self.raw_documents.clone() else {
            return true;
        };
        match self.codec.parse_files(&documents) {
            Ok(mut settings) => {
                if self.plugin.is_user_manifest() {
                    settings = replace_document_stores(&self.working_base, settings);
                }
                self.pull_values(cx);
                let prior_meta = self
                    .original_provider
                    .as_ref()
                    .and_then(|provider| provider.meta.as_ref());
                let current_meta = self
                    .codec
                    .encode(&self.values, &self.working_base, prior_meta)
                    .meta;
                self.values = self.codec.decode(&settings, current_meta.as_ref());
                self.working_base = settings;
                self.raw_documents = None;
                self.text_inputs.clear();
                self.kv_rows.clear();
                self.grid_rows.clear();
                self.build_inputs(cx);
                self.invalidate_preview(cx);
                true
            }
            Err(error) => {
                self.tab = EditorTab::Files;
                self.operation_note = Some(error.into());
                self.operation_failed = true;
                cx.notify();
                false
            }
        }
    }

    /// Parse the edited file back into values (and re-derive the form), preserving
    /// the current `meta` (which lives outside the files).
    fn apply_raw_edit(&mut self, cx: &mut Context<Self>) {
        let (idx, edited) = match self.raw_edit.as_ref() {
            Some(raw) => (raw.file_index, raw.input.read(cx).content().to_string()),
            None => return,
        };
        self.ensure_preview_current(cx);
        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let cur_meta = self
            .codec
            .encode(&self.values, &self.working_base, prior_meta.as_ref())
            .meta;
        let mut contents: Vec<String> = self.current_raw_documents(cx).unwrap_or_else(|| {
            self.preview_cache
                .files
                .iter()
                .map(|file| file.content.to_string())
                .collect()
        });
        if idx < contents.len() {
            contents[idx] = edited;
        }
        match self.codec.parse_files(&contents) {
            Ok(mut settings) => {
                if self.plugin.is_user_manifest() {
                    settings = replace_document_stores(&self.working_base, settings);
                }
                // The edited document becomes the new authoritative base, so
                // keys the form doesn't model are preserved through save.
                self.working_base = settings.clone();
                self.values = self.codec.decode(&settings, cur_meta.as_ref());
                self.text_inputs.clear();
                self.kv_rows.clear();
                self.grid_rows.clear();
                self.build_inputs(cx);
                self.raw_edit = None;
                self.raw_documents = None;
                self.operation_note = Some(t(k::PROVIDER_EDITOR_RAW_APPLIED));
                self.operation_failed = false;
                self.invalidate_preview(cx);
            }
            Err(e) => {
                if let Some(raw) = self.raw_edit.as_mut() {
                    raw.error = Some(SharedString::from(e));
                }
                cx.notify();
            }
        }
    }

    /// Pull all text/kv/grid input contents into `self.values`.
    fn pull_values(&mut self, cx: &Context<Self>) {
        for (id, input) in &self.text_inputs {
            self.values.insert(
                id.clone(),
                Value::String(input.read(cx).content().to_string()),
            );
        }
        for (id, rows) in &self.kv_rows {
            let mut obj = Map::new();
            for row in rows {
                let k = row.key.read(cx).content().trim().to_string();
                if k.is_empty() {
                    continue;
                }
                obj.insert(k, Value::String(row.value.read(cx).content().to_string()));
            }
            self.values.insert(id.clone(), Value::Object(obj));
        }
        for (id, rows) in &self.grid_rows {
            let mut arr = Vec::new();
            for row in rows {
                let mut obj = row.extras.clone();
                for (k, input) in &row.cells {
                    obj.insert(
                        k.clone(),
                        grid_text_value(row.extras.get(k), input.read(cx).content().as_ref()),
                    );
                }
                for (k, on) in &row.toggles {
                    obj.insert(k.clone(), Value::Bool(*on));
                }
                arr.push(Value::Object(obj));
            }
            self.values.insert(id.clone(), Value::Array(arr));
        }
        // Station mode: the hidden endpoint fields always track the selected
        // station, so the preview (and any validation derived from it) shows
        // the same gateway coordinates a save would embed. Until the gateway
        // info arrives the stale decoded values stay — a transient state the
        // arrival's `invalidate_preview` corrects.
        if self.source == ProviderSource::Station
            && let Some(info) = &self.station_gateway
            && let Some(app_type) = self.app_type
        {
            let caps = self.station_capabilities();
            provider_config::inject_station_endpoint(
                &mut self.values,
                app_type,
                &info.origin,
                &info.key,
                caps,
            );
        }
    }

    fn is_editing(&self) -> bool {
        self.original_id.is_some()
    }

    // ---- toasts -------------------------------------------------------------

    /// Post a status toast with its severity stated outright. Guessing the
    /// severity from the wording mis-reads several of these messages (an empty
    /// model list is not a failure) and stops working altogether once the copy
    /// is translated. Callers still drive the redraw, exactly as before.
    fn set_status(&mut self, level: NotificationLevel, text: impl Into<SharedString>) {
        self.status = Some(text.into());
        self.status_level = Some(level);
    }

    /// The error channel outranks `status` in [`crate::notifications::ToastSource`],
    /// and everything on it is a failed or refused action — always an error toast.
    fn set_error(&mut self, text: impl Into<SharedString>) {
        self.error = Some(text.into());
        self.status_level = Some(NotificationLevel::Error);
    }

    // ---- mutation handlers --------------------------------------------------

    fn set_select(&mut self, field_id: String, value: String, cx: &mut Context<Self>) {
        self.open_select_field = None;
        self.values.insert(field_id, Value::String(value));
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn toggle_secret_reveal(&mut self, field_id: &str, cx: &mut Context<Self>) {
        let revealed = if self.revealed_secrets.contains(field_id) {
            self.revealed_secrets.remove(field_id);
            false
        } else {
            self.revealed_secrets.insert(field_id.to_string());
            true
        };
        if let Some(input) = self.text_inputs.get(field_id).cloned() {
            input.update(cx, |input, cx| input.set_masked(!revealed, cx));
        }
        cx.notify();
    }

    fn toggle_bool(&mut self, field_id: String, cx: &mut Context<Self>) {
        let cur = bool_val(&self.values, &field_id);
        self.values.insert(field_id, Value::Bool(!cur));
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn kv_add(&mut self, field_id: String, cx: &mut Context<Self>) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let key = cx.new(|cx| TextInput::new(cx, "key"));
        let value = cx.new(|cx| TextInput::new(cx, "value"));
        self.kv_rows
            .entry(field_id.clone())
            .or_default()
            .push(KvRow { id, key, value });
        if let Some(rows) = self.kv_rows.get(&field_id)
            && let Some(row) = rows.last()
        {
            Self::observe_preview_input(&row.key, cx);
            Self::observe_preview_input(&row.value, cx);
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn kv_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.kv_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn grid_add(&mut self, field_id: String, cx: &mut Context<Self>) {
        let columns = self.columns_for(&field_id);
        let id = self.next_row_id;
        self.next_row_id += 1;
        let mut cells = HashMap::new();
        let mut toggles = HashMap::new();
        for col in &columns {
            match &col.kind {
                GridCellKind::Text { placeholder } => {
                    let placeholder = placeholder.clone();
                    cells.insert(
                        col.key.clone(),
                        cx.new(|cx| TextInput::new(cx, placeholder)),
                    );
                }
                GridCellKind::Toggle => {
                    toggles.insert(col.key.clone(), false);
                }
            }
        }
        self.grid_rows
            .entry(field_id.clone())
            .or_default()
            .push(GridRow {
                id,
                cells,
                toggles,
                extras: Map::new(),
            });
        if let Some(rows) = self.grid_rows.get(&field_id)
            && let Some(row) = rows.last()
        {
            for input in row.cells.values() {
                Self::observe_preview_input(input, cx);
            }
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn grid_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.grid_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
        self.form_list_state.remeasure();
        self.invalidate_preview(cx);
    }

    fn grid_toggle(
        &mut self,
        field_id: String,
        row_id: usize,
        col: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(rows) = self.grid_rows.get_mut(&field_id)
            && let Some(row) = rows.iter_mut().find(|r| r.id == row_id)
        {
            let cur = row.toggles.get(&col).copied().unwrap_or(false);
            row.toggles.insert(col, !cur);
        }
        self.invalidate_preview(cx);
    }

    fn columns_for(&self, field_id: &str) -> Vec<provider_config::GridColumn> {
        for section in self.schema.iter() {
            for field in &section.fields {
                if field.id == field_id
                    && let FieldKind::ModelGrid { columns } = &field.kind
                {
                    return columns.clone();
                }
            }
        }
        Vec::new()
    }

    /// The common-config snippet when the user changed it this session. Both
    /// save paths persist it ahead of the provider write.
    fn common_snippet_update(&self, cx: &Context<Self>) -> Option<String> {
        if !self.common_config_supported {
            return None;
        }
        let snippet = self.common_snippet.read(cx).content().to_string();
        (snippet != self.original_snippet).then_some(snippet)
    }

    /// The host uses the same snapshot for every navigation and close path.
    /// Preview cache invalidation and visual tab state are intentionally absent.
    pub(crate) fn is_dirty(&self, cx: &gpui::App) -> bool {
        self.pending_apply.is_some() || self.editor_snapshot(cx) != self.saved_snapshot
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.saving
    }

    pub(crate) fn has_pending_apply(&self) -> bool {
        self.pending_apply.is_some()
    }

    pub(crate) fn can_save_draft(&self) -> bool {
        !self.saving
            && self.pending_apply.is_none()
            && self.backend_available
            && self.backend.supports_editor_drafts()
    }

    pub(crate) fn set_workspace_backend(
        &mut self,
        backend: Option<WorkspaceBackend>,
        cx: &mut Context<Self>,
    ) {
        self.backend_available = backend.is_some();
        if let Some(backend) = backend {
            self.backend = backend;
        }
        if !self.backend_available {
            self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_OFFLINE));
            self.operation_failed = true;
        } else {
            self.operation_note = self
                .pending_apply
                .as_ref()
                .map(|_| t(k::PROVIDER_EDITOR_WORKBENCH_PENDING_REQUIRED));
            self.operation_failed = self.pending_apply.is_some();
        }
        cx.notify();
    }

    pub(crate) fn request_save_and_leave(&mut self, cx: &mut Context<Self>) {
        self.submit_draft(false, true, cx);
    }

    fn current_values(&self, cx: &gpui::App) -> FormValues {
        let mut values = self.values.clone();
        for (id, input) in &self.text_inputs {
            values.insert(
                id.clone(),
                Value::String(input.read(cx).content().to_string()),
            );
        }
        for (id, rows) in &self.kv_rows {
            let object = rows
                .iter()
                .filter_map(|row| {
                    let key = row.key.read(cx).content().trim().to_string();
                    (!key.is_empty())
                        .then(|| (key, Value::String(row.value.read(cx).content().to_string())))
                })
                .collect();
            values.insert(id.clone(), Value::Object(object));
        }
        for (id, rows) in &self.grid_rows {
            values.insert(
                id.clone(),
                Value::Array(
                    rows.iter()
                        .map(|row| {
                            let mut object = row.extras.clone();
                            for (key, input) in &row.cells {
                                object.insert(
                                    key.clone(),
                                    grid_text_value(
                                        row.extras.get(key),
                                        input.read(cx).content().as_ref(),
                                    ),
                                );
                            }
                            for (key, value) in &row.toggles {
                                object.insert(key.clone(), Value::Bool(*value));
                            }
                            Value::Object(object)
                        })
                        .collect(),
                ),
            );
        }
        values
    }

    fn current_raw_documents(&self, cx: &gpui::App) -> Option<Vec<String>> {
        let mut documents = self.raw_documents.clone();
        if let Some(raw) = &self.raw_edit
            && (documents.is_some()
                || raw.input.read(cx).content().as_ref() != raw.original_content)
        {
            let files = documents.get_or_insert_with(|| {
                self.preview_cache
                    .files
                    .iter()
                    .map(|file| file.content.to_string())
                    .collect()
            });
            if let Some(file) = files.get_mut(raw.file_index) {
                *file = raw.input.read(cx).content().to_string();
            }
        }
        documents
    }

    fn editor_snapshot(&self, cx: &gpui::App) -> Value {
        let mut snapshot = json!({
            "name": self.name.read(cx).content().to_string(),
            "providerId": self.provider_id.read(cx).content().to_string(),
            "website": self.website_url.read(cx).content().to_string(),
            "category": self.category.read(cx).content().to_string(),
            "notes": self.notes.read(cx).content().to_string(),
            "values": self.current_values(cx), "workingBase": self.working_base,
            "kvRows": self.kv_rows.iter().map(|(id, rows)| (id.clone(), rows.iter().map(|row| json!({
                "key": row.key.read(cx).content().to_string(), "value": row.value.read(cx).content().to_string()
            })).collect::<Vec<_>>())).collect::<HashMap<_, _>>(),
            "source": if self.source == ProviderSource::Station { "station" } else { "direct" },
            "station": self.selected_station,
            "directValues": self.direct_values, "stationValues": self.station_values, "officialValues": self.official_values,
            "commonEnabled": self.common_config_enabled,
            "commonSnippet": self.common_snippet.read(cx).content().to_string(),
            "commonBaseline": self.original_snippet_present.then(|| self.original_snippet.clone()),
            "rawDocuments": self.current_raw_documents(cx),
        });
        if let Some(revision) = &self.plugin_revision {
            snapshot["pluginRevision"] = json!(revision);
        }
        snapshot
    }

    pub fn new_draft(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_id: AppId,
        plugin: Arc<dyn AppPlugin>,
        codec: Box<dyn AppConfig>,
        draft: EditorDraft,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new_edit(
            app,
            backend,
            app_id,
            plugin,
            codec,
            &draft.input.provider,
            cx,
        );
        this.original_id = draft.input.original_provider_id.clone();
        this.base_provider_hash = draft.input.base_provider_hash.clone();
        this.stable_provider_id = draft.input.provider.id.clone();
        this.draft_id = draft.id;
        this.draft_revision = Some(draft.revision);
        let state = &draft.input.editor_state;
        if let Some(revision) = state.get("pluginRevision").and_then(Value::as_str) {
            this.plugin_revision = Some(revision.to_owned());
        }
        for (key, input) in [
            ("name", &this.name),
            ("providerId", &this.provider_id),
            ("website", &this.website_url),
            ("category", &this.category),
            ("notes", &this.notes),
            ("commonSnippet", &this.common_snippet),
        ] {
            if let Some(value) = state.get(key).and_then(Value::as_str) {
                input.update(cx, |input, cx| input.set_content(value.to_string(), cx));
            }
        }
        if let Some(values) = state
            .get("values")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            this.values = values;
        }
        if let Some(base) = state.get("workingBase") {
            this.working_base = base.clone();
        }
        if let Some(source) = state.get("source").and_then(Value::as_str) {
            this.source = if source == "station" {
                ProviderSource::Station
            } else {
                ProviderSource::Direct
            };
        }
        if let Some(station) = state.get("station") {
            this.selected_station = station.as_str().map(str::to_owned);
        }
        this.direct_values = state
            .get("directValues")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        this.station_values = state
            .get("stationValues")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        this.official_values = state
            .get("officialValues")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        this.common_config_enabled = state
            .get("commonEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(this.common_config_enabled);
        if let Some(common) = &draft.input.common_config {
            this.original_snippet = common.base_snippet.clone().unwrap_or_default();
            this.original_snippet_present = common.base_snippet.is_some();
            this.common_base_pinned = true;
        } else if let Some(baseline) = state.get("commonBaseline") {
            this.original_snippet = baseline.as_str().unwrap_or_default().to_owned();
            this.original_snippet_present = baseline.is_string();
            this.common_base_pinned = true;
        }
        this.raw_documents = draft.input.raw_documents.or_else(|| {
            state
                .get("rawDocuments")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
        });
        if this.raw_documents.is_some() {
            this.tab = EditorTab::Files;
        }
        this.text_inputs.clear();
        this.kv_rows.clear();
        this.grid_rows.clear();
        this.build_inputs(cx);
        if let Some(fields) = state.get("kvRows").and_then(Value::as_object) {
            for (field, rows) in fields {
                let Some(rows) = rows.as_array() else {
                    continue;
                };
                let mut restored = Vec::new();
                for row in rows {
                    let id = this.next_row_id;
                    this.next_row_id += 1;
                    let key = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "key");
                        input.set_content(row["key"].as_str().unwrap_or("").to_owned(), cx);
                        input
                    });
                    let value = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "value");
                        input.set_content(row["value"].as_str().unwrap_or("").to_owned(), cx);
                        input
                    });
                    Self::observe_preview_input(&key, cx);
                    Self::observe_preview_input(&value, cx);
                    restored.push(KvRow { id, key, value });
                }
                this.kv_rows.insert(field.clone(), restored);
            }
        }
        this.saved_snapshot = this.editor_snapshot(cx);
        this.invalidate_preview(cx);
        this
    }

    fn draft_input(&mut self, cx: &mut Context<Self>) -> EditorDraftInput {
        self.pull_values(cx);
        let prior_meta = self
            .original_provider
            .as_ref()
            .and_then(|p| p.meta.as_ref());
        let encoded = self
            .codec
            .encode(&self.values, &self.working_base, prior_meta);
        let typed_id = self.provider_id.read(cx).content().trim().to_string();
        let id = if typed_id.is_empty() {
            self.stable_provider_id.clone()
        } else {
            typed_id
        };
        let mut provider = self
            .original_provider
            .clone()
            .unwrap_or_else(|| Provider::with_id(id.clone(), String::new(), Value::Null, None));
        provider.id = id;
        provider.name = self.name.read(cx).content().trim().to_string();
        provider.settings_config = encoded.settings_config;
        provider.meta = encoded.meta;
        provider.website_url = nonempty(self.website_url.read(cx).content().trim().to_string());
        provider.category = nonempty(self.category.read(cx).content().trim().to_string());
        provider.notes = nonempty(self.notes.read(cx).content().trim().to_string());
        if provider.created_at.is_none() {
            provider.created_at = Some(chrono_now_millis());
        }
        if self.common_config_supported {
            provider
                .meta
                .get_or_insert_with(ProviderMeta::default)
                .common_config_enabled = self.common_config_enabled.then_some(true);
        }
        if self.source == ProviderSource::Station {
            provider
                .meta
                .get_or_insert_with(ProviderMeta::default)
                .gateway_route_id = self.selected_station.clone();
        } else if let Some(meta) = provider.meta.as_mut() {
            meta.gateway_route_id = None;
        }
        EditorDraftInput {
            provider,
            original_provider_id: self.original_id.clone(),
            base_provider_hash: self.base_provider_hash.clone(),
            editor_state: self.editor_snapshot(cx),
            raw_documents: self.current_raw_documents(cx),
            common_config: self
                .common_snippet_update(cx)
                .map(|snippet| EditorCommonConfig {
                    snippet,
                    base_snippet: self
                        .original_snippet_present
                        .then(|| self.original_snippet.clone()),
                }),
        }
    }

    fn check_configuration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_apply.is_some() {
            self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_PENDING_REQUIRED));
            self.operation_failed = true;
            cx.notify();
            return;
        }
        self.pull_values(cx);
        self.field_errors.clear();
        if self.name.read(cx).content().trim().is_empty() {
            let message = t(k::PROVIDER_EDITOR_IDENTITY_NAME_REQUIRED);
            self.field_errors.insert("$name".into(), message.clone());
            self.tab = EditorTab::Basic;
            self.show_advanced_fields = false;
            self.form_list_state.remeasure();
            self.form_list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
            self.name.read(cx).focus_handle(cx).focus(window, cx);
            self.operation_note = Some(message.clone());
            self.operation_failed = true;
            cx.notify();
            return;
        }
        let mut values = self.values.clone();
        if let Some(documents) = self.current_raw_documents(cx) {
            match self.codec.parse_files(&documents) {
                Ok(settings) => {
                    values = self.codec.decode(
                        &settings,
                        self.original_provider
                            .as_ref()
                            .and_then(|p| p.meta.as_ref()),
                    )
                }
                Err(error) => {
                    self.tab = EditorTab::Files;
                    self.operation_note = Some(error.clone().into());
                    self.operation_failed = true;
                    cx.notify();
                    return;
                }
            }
        }
        let category = nonempty(self.category.read(cx).content().to_string());
        let issues = self
            .codec
            .validate_for_category(&values, category.as_deref());
        for issue in &issues {
            if issue.severity == Severity::Error
                && let Some(field) = &issue.field
            {
                self.field_errors
                    .insert(field.clone(), issue.message.clone().into());
            }
        }
        if let Some(issue) = issues.iter().find(|i| i.severity == Severity::Error) {
            if let Some(field) = &issue.field {
                let advanced = self.schema.iter().any(|s| {
                    self.section_is_advanced(s) && s.fields.iter().any(|f| &f.id == field)
                });
                self.tab = if advanced {
                    EditorTab::Advanced
                } else {
                    EditorTab::Basic
                };
                self.show_advanced_fields = advanced;
                self.form_list_state.remeasure();
                if let Some(index) = self.schema.iter().position(|section| {
                    section
                        .fields
                        .iter()
                        .any(|candidate| &candidate.id == field)
                }) {
                    self.form_list_state.scroll_to(ListOffset {
                        item_ix: index + 1,
                        offset_in_item: px(0.),
                    });
                }
                if let Some(input) = self.text_inputs.get(field) {
                    input.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            self.operation_note = Some(issue.message.clone().into());
            self.operation_failed = true;
        } else {
            self.checked_snapshot = Some(self.editor_snapshot(cx));
            self.operation_note = Some(t(k::PROVIDER_EDITOR_CODEX_CHECKED));
            self.operation_failed = false;
        }
        cx.notify();
    }

    /// Persist a revision first. Applying never reads a newer unconfirmed draft.
    fn submit_draft(&mut self, apply_to_target: bool, leave: bool, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.pending_apply.is_some() {
            self.set_error(t(k::PROVIDER_EDITOR_WORKBENCH_PENDING_REQUIRED));
            cx.notify();
            return;
        }
        if !self.backend_available {
            self.set_error(t(k::PROVIDER_EDITOR_WORKBENCH_OFFLINE));
            cx.notify();
            return;
        }
        if !self.backend.supports_editor_drafts() {
            self.set_error(t(k::PROVIDER_EDITOR_WORKBENCH_UNSUPPORTED));
            cx.notify();
            return;
        }
        if !apply_to_target
            && !self.is_dirty(cx)
            && self.draft_revision.is_none()
            && self.original_id.is_some()
        {
            self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_UNCHANGED));
            cx.emit(EditorEvent::Saved { leave });
            cx.notify();
            return;
        }
        let input = self.draft_input(cx);
        let snapshot = input.editor_state.clone();
        let backend = self.backend.clone();
        let plugin = self.plugin.clone();
        let app_id = self.app_id.clone();
        let draft_id = self.draft_id.clone();
        let expected_revision = self.draft_revision;
        let station = if apply_to_target && self.source == ProviderSource::Station {
            match (self.app_type, self.selected_station.clone()) {
                (Some(app_type), Some(route)) => Some((
                    app_type,
                    route,
                    self.values.clone(),
                    self.working_base.clone(),
                    self.station_capabilities(),
                    self.station_models().to_vec(),
                )),
                _ => {
                    self.set_error(t(k::PROVIDER_EDITOR_STATION_REQUIRED));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.saving = true;
        self.error = None;
        self.operation_note = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let mut draft = backend
                    .upsert_editor_draft(&app_id, &draft_id, expected_revision, input)
                    .await
                    .map_err(|error| (None, error.to_string(), false))?;
                if !apply_to_target {
                    return Ok((draft, None));
                }
                if let Some((app_type, route, mut values, mut base, caps, models)) = station {
                    let prepared = async {
                        {
                            let codec = plugin.provider_config().ok_or_else(|| {
                                "The selected app no longer has a configuration codec".to_string()
                            })?;
                            if let Some(documents) = &draft.input.raw_documents {
                                base = codec.parse_files(documents)?;
                                values = codec.decode(&base, draft.input.provider.meta.as_ref());
                            }
                            if draft.input.provider.name.trim().is_empty() {
                                return Err(
                                    t(k::PROVIDER_EDITOR_IDENTITY_NAME_REQUIRED).to_string()
                                );
                            }
                            if let Some(issue) = codec
                                .validate_for_category(
                                    &values,
                                    draft.input.provider.category.as_deref(),
                                )
                                .into_iter()
                                .find(|issue| issue.severity == Severity::Error)
                            {
                                return Err(issue.message);
                            }
                        }
                        backend
                            .set_gateway_running(true)
                            .await
                            .map_err(|e| e.to_string())?;
                        let (origin, key) = backend
                            .station_gateway_preview(&route, app_type)
                            .await
                            .map_err(|e| e.to_string())?;
                        let original = &draft.input.provider;
                        let identity = apply::StationChannelIdentity {
                            id: original.id.clone(),
                            name: original.name.clone(),
                            website_url: original.website_url.clone(),
                            category: original.category.clone(),
                            notes: original.notes.clone(),
                        };
                        let mut provider = apply::build_station_channel_with_endpoint(
                            app_type,
                            &route,
                            &values,
                            identity,
                            &origin,
                            &key,
                            caps,
                            &models,
                            &base,
                            original.meta.as_ref(),
                        )
                        .map_err(|e| e.to_string())?;
                        provider.created_at = original.created_at;
                        provider.sort_index = original.sort_index;
                        provider.icon.clone_from(&original.icon);
                        provider.icon_color.clone_from(&original.icon_color);
                        let mut next = draft.input.clone();
                        {
                            let codec = plugin.provider_config().ok_or_else(|| {
                                "The selected app no longer has a configuration codec".to_string()
                            })?;
                            next.editor_state["values"] = serde_json::to_value(
                                codec.decode(&provider.settings_config, provider.meta.as_ref()),
                            )
                            .map_err(|error| error.to_string())?;
                            next.editor_state["workingBase"] = provider.settings_config.clone();
                        }
                        next.provider = provider;
                        // The snapshot keeps the user's raw text for a failed
                        // apply/undo; core must apply the prepared station data.
                        next.raw_documents = None;
                        backend
                            .upsert_editor_draft(&app_id, &draft_id, Some(draft.revision), next)
                            .await
                            .map_err(|e| e.to_string())
                    }
                    .await;
                    match prepared {
                        Ok(prepared) => draft = prepared,
                        Err(error) => return Err((Some(draft), error, false)),
                    }
                }
                let outcome = backend
                    .apply_editor_draft(
                        &app_id,
                        &draft_id,
                        draft.revision,
                        ProviderSwitchPolicy::Abort,
                    )
                    .await
                    .map_err(|error| {
                        (Some(draft.clone()), error.to_string(), backend.is_remote())
                    })?;
                if outcome.state == DraftApplyState::ExternalConfirmation {
                    if backend.is_remote() {
                        return Err((
                            Some(draft),
                            t(k::SHELL_PROVIDER_IMPORT_REMOTE_UNSUPPORTED).to_string(),
                            false,
                        ));
                    }
                    let provider = outcome.provider.as_ref().ok_or_else(|| {
                        (
                            Some(draft.clone()),
                            "Import request has no prepared provider".to_string(),
                            false,
                        )
                    })?;
                    let deeplink = ochub_core::apps::cherry_studio::build_provider_import_deeplink(
                        &provider.provider,
                    )
                    .map_err(|error| (Some(draft.clone()), error.to_string(), false))?;
                    crate::app_ui::open_cherry_studio_deeplink(&deeplink)
                        .map_err(|error| (Some(draft.clone()), error, false))?;
                }
                Ok((draft, Some(outcome)))
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok((draft, outcome)) => {
                        this.accept_submission(draft, outcome, snapshot, leave, cx)
                    }
                    Err((saved, error, application_unknown)) => {
                        if application_unknown && let Some(draft) = saved.clone() {
                            this.pending_apply = Some(PendingApply { draft, snapshot });
                            this.operation_note = Some(
                                tf!(k::PROVIDER_EDITOR_WORKBENCH_RESULT_UNKNOWN, error = error)
                                    .into(),
                            );
                            this.operation_failed = true;
                            cx.notify();
                            return;
                        }
                        if let Some(draft) = saved {
                            this.draft_revision = Some(draft.revision);
                            this.saved_snapshot = snapshot;
                            cx.emit(EditorEvent::Saved { leave: false });
                        }
                        this.operation_note = Some(error.clone().into());
                        this.operation_failed = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn accept_submission(
        &mut self,
        draft: EditorDraft,
        outcome: Option<EditorDraftApplyResult>,
        snapshot: Value,
        leave: bool,
        cx: &mut Context<Self>,
    ) {
        self.pending_apply = None;

        let unchanged_in_flight = self.editor_snapshot(cx) == snapshot;
        self.draft_revision = Some(draft.revision);
        self.saved_snapshot = snapshot;
        if let Some(outcome) = outcome {
            match outcome.state {
                DraftApplyState::Applied => {
                    self.applied = true;
                    self.undo_draft = outcome.undo_available.then(|| draft.clone());
                    self.undo_provider = outcome
                        .undo_available
                        .then(|| self.original_provider.clone());
                    self.receipt_id = if outcome.undo_available {
                        outcome.receipt_id
                    } else {
                        None
                    };
                    if let Some(details) = outcome.provider {
                        self.original_id = Some(details.provider.id.clone());
                        self.base_provider_hash = Some(provider_revision_hash(&details.provider));
                        if unchanged_in_flight {
                            self.working_base = details.provider.settings_config.clone();
                            self.values = self
                                .codec
                                .decode(&self.working_base, details.provider.meta.as_ref());
                            self.raw_documents = None;
                            self.raw_edit = None;
                            self.text_inputs.clear();
                            self.kv_rows.clear();
                            self.grid_rows.clear();
                            self.build_inputs(cx);
                            self.set_identity(&details.provider, cx);
                        }
                        self.original_provider = Some(details.provider);
                    }
                    if let Some(common) = draft.input.common_config {
                        self.original_snippet = common.snippet;
                        self.original_snippet_present = true;
                    }
                    if unchanged_in_flight {
                        self.saved_snapshot = self.editor_snapshot(cx);
                        self.invalidate_preview(cx);
                    }
                    self.draft_id = uuid::Uuid::new_v4().to_string();
                    self.draft_revision = None;
                    self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_APPLIED));
                    self.operation_failed = false;
                    if !outcome.warnings.is_empty() {
                        self.operation_note = Some(
                            format!(
                                "{} {}",
                                t(k::PROVIDER_EDITOR_WORKBENCH_APPLIED),
                                outcome.warnings.join(" ")
                            )
                            .into(),
                        );
                    }
                    cx.emit(EditorEvent::Applied {
                        leave: leave && unchanged_in_flight,
                    });
                }
                DraftApplyState::ExternalConfirmation => {
                    self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_EXTERNAL));
                    self.operation_failed = false;
                    cx.emit(EditorEvent::Saved { leave: false });
                }
                DraftApplyState::Failed => {
                    let detail = outcome.failure.map(|f| f.message).unwrap_or_default();
                    self.operation_note =
                        Some(tf!(k::PROVIDER_EDITOR_WORKBENCH_APPLY_FAILED, error = detail).into());
                    self.operation_failed = true;
                    cx.emit(EditorEvent::Saved { leave: false });
                }
            }
        } else {
            self.applied = false;
            self.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_SAVED));
            self.operation_failed = false;
            cx.emit(EditorEvent::Saved {
                leave: leave && unchanged_in_flight,
            });
        }
    }

    /// Resolve the exact previously submitted revision. In particular, do not
    /// upsert first: a successful remote commit has already consumed its draft.
    fn reconcile_pending_apply(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if !self.backend_available {
            self.set_error(t(k::PROVIDER_EDITOR_WORKBENCH_OFFLINE));
            cx.notify();
            return;
        }
        let Some(pending) = self.pending_apply.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let app_id = self.app_id.clone();
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let draft = pending.draft.clone();
            let result = crate::core_async::run(async move {
                backend
                    .apply_editor_draft(
                        &app_id,
                        &draft.id,
                        draft.revision,
                        ProviderSwitchPolicy::Abort,
                    )
                    .await
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(outcome) if outcome.state == DraftApplyState::ExternalConfirmation => {
                        // Pending operations are remote. Never launch a local
                        // application as a fallback for a remote import.
                        this.pending_apply = None;
                        this.draft_revision = Some(pending.draft.revision);
                        this.saved_snapshot = pending.snapshot;
                        this.operation_note = Some(t(k::SHELL_PROVIDER_IMPORT_REMOTE_UNSUPPORTED));
                        this.operation_failed = true;
                        cx.emit(EditorEvent::Saved { leave: false });
                    }
                    Ok(outcome) => this.accept_submission(
                        pending.draft,
                        Some(outcome),
                        pending.snapshot,
                        false,
                        cx,
                    ),
                    Err(error) => {
                        this.operation_note = Some(
                            tf!(k::PROVIDER_EDITOR_WORKBENCH_RESULT_UNKNOWN, error = error).into(),
                        );
                        this.operation_failed = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn undo_apply(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.backend_available || self.pending_apply.is_some() {
            return;
        }
        let Some(receipt) = self.receipt_id.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let app_id = self.app_id.clone();
        let undo_draft_id = self.undo_draft.as_ref().map(|draft| draft.id.clone());
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend.undo_config_apply(&receipt).await?;
                match undo_draft_id {
                    Some(id) => backend.get_editor_draft(&app_id, &id).await,
                    None => Ok(None),
                }
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(restored_draft) => {
                        this.receipt_id = None;
                        this.applied = false;
                        if let Some(before) = this.undo_provider.take() {
                            this.original_id = before.as_ref().map(|provider| provider.id.clone());
                            this.original_provider = before;
                        }
                        let fallback = this.undo_draft.take();
                        if let Some(draft) = restored_draft {
                            this.draft_id = draft.id;
                            this.draft_revision = Some(draft.revision);
                            this.original_id = draft.input.original_provider_id;
                            this.base_provider_hash = draft.input.base_provider_hash;
                            if let Some(common) = draft.input.common_config {
                                this.original_snippet_present = common.base_snippet.is_some();
                                this.original_snippet = common.base_snippet.unwrap_or_default();
                            }
                            this.saved_snapshot = draft.input.editor_state;
                        } else {
                            if let Some(draft) = fallback {
                                this.original_id = draft.input.original_provider_id;
                                this.base_provider_hash = draft.input.base_provider_hash;
                            }
                            this.draft_id = uuid::Uuid::new_v4().to_string();
                            this.draft_revision = None;
                            this.saved_snapshot = Value::Null;
                        }
                        this.operation_note = Some(t(k::PROVIDER_EDITOR_WORKBENCH_UNDONE));
                        this.operation_failed = false;
                        cx.emit(EditorEvent::Applied { leave: false });
                    }
                    Err(error) => {
                        this.operation_note = Some(error.to_string().into());
                        this.operation_failed = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn field_text(&self, id: &str, cx: &Context<Self>) -> String {
        self.text_inputs
            .get(id)
            .map(|i| i.read(cx).content().trim().to_string())
            .unwrap_or_default()
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) {
        if let Some(provider_id) = self.original_id.clone() {
            let backend = self.backend.clone();
            let app_id = self.app_id.clone();
            self.error = None;
            self.set_status(
                NotificationLevel::Info,
                t(k::PROVIDER_EDITOR_MODELS_FETCHING),
            );
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = crate::core_async::run(async move {
                    backend
                        .provider_network_operation(
                            ochub_protocol::methods::PROVIDER_MODELS,
                            &app_id,
                            &provider_id,
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| error.to_string())
                        })
                })
                .await;
                this.update(cx, |this, cx| {
                    this.finish_models(result);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let base_url = self.field_text("base_url", cx);
        let api_key = self.field_text("api_key", cx);
        if base_url.is_empty() || api_key.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_MODELS_NEEDS_CREDENTIALS));
            cx.notify();
            return;
        }
        self.error = None;
        self.set_status(
            NotificationLevel::Info,
            t(k::PROVIDER_EDITOR_MODELS_FETCHING),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                ochub_core::services::model_fetch::fetch_models(
                    &base_url, &api_key, false, None, None,
                )
                .await
            })
            .await;
            this.update(cx, |this, cx| {
                this.finish_models(result.map_err(|error| error.to_string()));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_models(
        &mut self,
        result: Result<Vec<ochub_core::services::model_fetch::FetchedModel>, String>,
    ) {
        match result {
            Ok(models) => {
                let preview = models
                    .iter()
                    .take(6)
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if preview.is_empty() {
                    self.set_status(
                        NotificationLevel::Warning,
                        t(k::PROVIDER_EDITOR_MODELS_NONE),
                    );
                } else {
                    self.set_status(
                        NotificationLevel::Success,
                        tf!(
                            k::PROVIDER_EDITOR_MODELS_FETCHED,
                            count = models.len(),
                            models = preview
                        ),
                    );
                }
            }
            Err(error) => {
                self.set_error(tf!(k::PROVIDER_EDITOR_MODELS_FETCH_FAILED, error = error));
                self.status = None;
            }
        }
    }

    fn speedtest_base_url(&mut self, cx: &mut Context<Self>) {
        if let Some(provider_id) = self.original_id.clone() {
            let backend = self.backend.clone();
            let app_id = self.app_id.clone();
            self.error = None;
            self.set_status(
                NotificationLevel::Info,
                t(k::PROVIDER_EDITOR_SPEEDTEST_TESTING),
            );
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = crate::core_async::run(async move {
                    backend
                        .provider_network_operation(
                            ochub_protocol::methods::PROVIDER_SPEED_TEST,
                            &app_id,
                            &provider_id,
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| error.to_string())
                        })
                })
                .await;
                this.update(cx, |this, cx| {
                    this.finish_speedtest(result);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let base_url = self.field_text("base_url", cx);
        if base_url.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_SPEEDTEST_NEEDS_BASE_URL));
            cx.notify();
            return;
        }
        self.error = None;
        self.set_status(
            NotificationLevel::Info,
            t(k::PROVIDER_EDITOR_SPEEDTEST_TESTING),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                ochub_core::services::SpeedtestService::test_endpoints(vec![base_url], Some(8))
                    .await
            })
            .await;
            this.update(cx, |this, cx| {
                this.finish_speedtest(result.map_err(|error| error.to_string()));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_speedtest(
        &mut self,
        result: Result<Vec<ochub_core::services::speedtest::EndpointLatency>, String>,
    ) {
        match result {
            Ok(results) => {
                let (level, message) = results
                    .first()
                    .map(|item| {
                        if let Some(error) = &item.error {
                            (
                                NotificationLevel::Error,
                                tf!(k::PROVIDER_EDITOR_SPEEDTEST_FAILED, error = error),
                            )
                        } else {
                            let unknown = raw(k::PROVIDER_EDITOR_COMMON_UNKNOWN);
                            (
                                NotificationLevel::Success,
                                tf!(
                                    k::PROVIDER_EDITOR_SPEEDTEST_OK,
                                    status = item
                                        .status
                                        .map(|status| status.to_string())
                                        .unwrap_or_else(|| unknown.to_string()),
                                    latency = item
                                        .latency
                                        .map(|latency| latency.to_string())
                                        .unwrap_or_else(|| unknown.to_string())
                                ),
                            )
                        }
                    })
                    .unwrap_or_else(|| {
                        (
                            NotificationLevel::Warning,
                            raw(k::PROVIDER_EDITOR_SPEEDTEST_NO_RESULT).to_string(),
                        )
                    });
                self.set_status(level, message);
            }
            Err(error) => {
                self.set_error(tf!(k::PROVIDER_EDITOR_SPEEDTEST_FAILED, error = error));
                self.status = None;
            }
        }
    }

    fn query_balance(&mut self, cx: &mut Context<Self>) {
        if let Some(provider_id) = self.original_id.clone() {
            let backend = self.backend.clone();
            let app_id = self.app_id.clone();
            self.error = None;
            self.set_status(
                NotificationLevel::Info,
                t(k::PROVIDER_EDITOR_BALANCE_QUERYING),
            );
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = crate::core_async::run(async move {
                    backend
                        .provider_network_operation(
                            ochub_protocol::methods::PROVIDER_BALANCE,
                            &app_id,
                            &provider_id,
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| error.to_string())
                        })
                })
                .await;
                this.update(cx, |this, cx| {
                    this.finish_balance(result);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let base_url = self.field_text("base_url", cx);
        let api_key = self.field_text("api_key", cx);
        if base_url.is_empty() || api_key.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_BALANCE_NEEDS_CREDENTIALS));
            cx.notify();
            return;
        }
        self.error = None;
        self.set_status(
            NotificationLevel::Info,
            t(k::PROVIDER_EDITOR_BALANCE_QUERYING),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                ochub_core::services::balance::get_balance(&base_url, &api_key).await
            })
            .await;
            this.update(cx, |this, cx| {
                this.finish_balance(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_balance(&mut self, result: Result<UsageResult, String>) {
        match result {
            Ok(result) => {
                let (level, text) = format_usage_result(&result);
                self.set_status(level, text);
            }
            Err(error) => {
                self.set_error(tf!(k::PROVIDER_EDITOR_BALANCE_FAILED, error = error));
                self.status = None;
            }
        }
    }

    fn toggle_common_config(&mut self, cx: &mut Context<Self>) {
        self.common_config_enabled = !self.common_config_enabled;
        self.invalidate_preview(cx);
    }

    fn extract_common_config(&mut self, cx: &mut Context<Self>) {
        if self.backend.is_remote() {
            let backend = self.backend.clone();
            let app_id = self.app_id.clone();
            cx.spawn(async move |this, cx| {
                let result =
                    crate::core_async::run(
                        async move { backend.extract_common_config(&app_id).await },
                    )
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(snippet) => {
                            this.common_snippet
                                .update(cx, |input, cx| input.set_content(snippet, cx));
                            this.set_status(
                                NotificationLevel::Success,
                                t(k::PROVIDER_EDITOR_COMMON_CONFIG_EXTRACTED),
                            );
                            this.error = None;
                        }
                        Err(err) => {
                            this.set_error(tf!(
                                k::PROVIDER_EDITOR_COMMON_CONFIG_EXTRACT_FAILED,
                                error = err
                            ));
                            this.status = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        self.pull_values(cx);
        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let settings = self
            .codec
            .encode(&self.values, &self.working_base, prior_meta.as_ref())
            .settings_config;
        let Some(app_type) = self.app_type else {
            return;
        };
        match ConfigService::extract_common_config_snippet(&self.app, app_type, Some(&settings)) {
            Ok(snippet) => {
                self.common_snippet
                    .update(cx, |input, cx| input.set_content(snippet, cx));
                self.set_status(
                    NotificationLevel::Success,
                    t(k::PROVIDER_EDITOR_COMMON_CONFIG_EXTRACTED),
                );
                self.error = None;
            }
            Err(err) => {
                self.set_error(tf!(
                    k::PROVIDER_EDITOR_COMMON_CONFIG_EXTRACT_FAILED,
                    error = err
                ));
                self.status = None;
            }
        }
        cx.notify();
    }

    fn open_convert(&mut self, cx: &mut Context<Self>) {
        self.convert_open = true;
        cx.notify();
    }

    fn close_convert(&mut self, cx: &mut Context<Self>) {
        self.convert_open = false;
        cx.notify();
    }

    fn convert_to(&mut self, target: AppType, cx: &mut Context<Self>) {
        if self.saving || !self.backend_available || self.pending_apply.is_some() {
            return;
        }
        self.pull_values(cx);
        let Some(target_codec) = provider_config::config_for(target) else {
            return;
        };
        let encoded = target_codec.encode(&self.values, &Value::Null, None);
        let base_name = self.name.read(cx).content().trim().to_string();
        let source_label = self.plugin.display_name();
        let name = if base_name.is_empty() {
            tf!(k::PROVIDER_EDITOR_CONVERT_COPY_NAME, app = source_label)
        } else {
            tf!(
                k::PROVIDER_EDITOR_CONVERT_COPY_NAME_SUFFIXED,
                name = base_name,
                app = source_label
            )
        };
        let website_url = nonempty(self.website_url.read(cx).content().trim().to_string());
        let mut provider = Provider::with_id(
            uuid::Uuid::new_v4().to_string(),
            name,
            encoded.settings_config,
            website_url,
        );
        provider.meta = encoded.meta;
        provider.category = nonempty(self.category.read(cx).content().trim().to_string());
        provider.notes = nonempty(self.notes.read(cx).content().trim().to_string());
        provider.created_at = Some(chrono_now_millis());
        self.saving = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .upsert_editor_draft(
                        &target.app_id(),
                        &uuid::Uuid::new_v4().to_string(),
                        None,
                        EditorDraftInput {
                            provider,
                            original_provider_id: None,
                            base_provider_hash: None,
                            editor_state: Value::Null,
                            raw_documents: None,
                            common_config: None,
                        },
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.convert_open = false;
                        this.set_status(
                            NotificationLevel::Success,
                            tf!(
                                k::PROVIDER_EDITOR_CONVERT_COPIED,
                                app = crate::app_meta::label(target)
                            ),
                        );
                        this.error = None;
                    }
                    Err(error) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_CONVERT_FAILED, error = error));
                        this.status = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- rendering ----------------------------------------------------------

    fn uses_official_login(&self, cx: &Context<Self>) -> bool {
        self.source == ProviderSource::Direct
            && matches!(
                self.app_type,
                Some(
                    AppType::Claude
                        | AppType::ClaudeDesktop
                        | AppType::Codex
                        | AppType::KimiCode
                        | AppType::GrokBuild
                )
            )
            && self.category.read(cx).content().trim() == "official"
    }

    fn render_official_auth_section(&self) -> gpui::AnyElement {
        let app_label = self.plugin.display_name();
        let cli_hint = match self.app_type {
            Some(AppType::Claude) => Some("`claude /login`"),
            Some(AppType::KimiCode) => Some("`kimi login`"),
            _ => None,
        };
        let mut card = components::card()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .child(
                div()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(tf!(
                        k::PROVIDER_EDITOR_OFFICIAL_TITLE,
                        app = app_label
                    ))),
            );
        if let Some(command) = cli_hint {
            card = card.child(div().text_color(theme::muted()).text_sm().child(
                SharedString::from(tf!(k::PROVIDER_EDITOR_OFFICIAL_CLI_HINT, command = command)),
            ));
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(layout::section_header(
                t(k::PROVIDER_EDITOR_OFFICIAL_SECTION_TITLE),
                None,
            ))
            .child(card)
            .into_any_element()
    }

    fn render_field(
        &self,
        field: &FormField,
        stack_grid: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.app_type == Some(AppType::Codex) && field.id == "auth_mode" {
            let login = str_val(&self.values, "auth_mode") != "api_key";
            let on_select = cx.listener(|this, index: &usize, _, cx| {
                let mode = if *index == 0 {
                    "api_key"
                } else if this
                    .text_inputs
                    .get("base_url")
                    .is_none_or(|i| i.read(cx).content().trim().is_empty())
                {
                    "openai_login"
                } else {
                    "openai_login_with_api_key"
                };
                this.set_select("auth_mode".into(), mode.into(), cx);
            });
            return components::field(
                t(k::PROVIDER_EDITOR_CODEX_ACCOUNT),
                false,
                None,
                components::segmented(
                    "codex-account",
                    &[raw(k::PROVIDER_EDITOR_CODEX_NO_ACCOUNT), "ChatGPT"],
                    usize::from(login),
                    move |index, window, cx| on_select(&index, window, cx),
                ),
            )
            .into_any_element();
        }
        let body = match &field.kind {
            FieldKind::Text { .. } => {
                let input = self
                    .text_inputs
                    .get(&field.id)
                    .map(|i| i.clone().into_any_element())
                    .unwrap_or_else(|| div().into_any_element());
                if self.source == ProviderSource::Station
                    && matches!(field.id.as_str(), "model" | "upstream_model")
                {
                    self.with_model_suggestions(
                        ModelSuggestionTarget::Field(field.id.clone()),
                        input,
                        cx,
                    )
                } else {
                    input
                }
            }
            FieldKind::Secret { .. } => {
                let revealed = self.revealed_secrets.contains(&field.id);
                let input = self
                    .text_inputs
                    .get(&field.id)
                    .map(|i| i.clone().into_any_element())
                    .unwrap_or_else(|| div().into_any_element());
                let field_id = field.id.clone();
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(div().flex_1().min_w(px(220.)).child(input))
                    .child(
                        components::icon_only_button_tone(
                            SharedString::from(format!("secret-reveal-{}", field.id)),
                            if revealed {
                                t(k::GATEWAY_ACTION_HIDE)
                            } else {
                                t(k::GATEWAY_ACTION_SHOW)
                            },
                            if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                this.toggle_secret_reveal(&field_id, cx);
                            },
                        )),
                    )
                    .into_any_element()
            }
            FieldKind::Select { options } => {
                // A station channel reaches the gateway with the gateway's own
                // key, so options that would leave it credential-less are not
                // offered while that source is active.
                let hidden: &[&str] = if self.source == ProviderSource::Station {
                    self.app_type
                        .map(|app| provider_config::station_hidden_options(app, &field.id))
                        .unwrap_or(&[])
                } else {
                    &[]
                };
                let options: Vec<&provider_config::SelectOption> = options
                    .iter()
                    .filter(|option| !hidden.contains(&option.value.as_str()))
                    .collect();
                let current = str_val(&self.values, &field.id).to_string();
                let selected = options.iter().position(|o| o.value == current).unwrap_or(0);
                let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
                let values: Vec<String> = options.iter().map(|o| o.value.clone()).collect();
                let selector = if components::select_prefers_dropdown(&labels) {
                    let fid = field.id.clone();
                    let open = self.open_select_field.as_deref() == Some(field.id.as_str());
                    let on_event = cx.listener(
                        move |this, event: &components::SelectDropdownEvent, _window, cx| {
                            match *event {
                                components::SelectDropdownEvent::Open(open) => {
                                    this.open_select_field =
                                        if open { Some(fid.clone()) } else { None };
                                    cx.notify();
                                }
                                components::SelectDropdownEvent::Select(index) => {
                                    if let Some(value) = values.get(index).cloned() {
                                        this.set_select(fid.clone(), value, cx);
                                    }
                                }
                            }
                        },
                    );
                    components::select_dropdown(
                        SharedString::from(format!("select-{}", field.id)),
                        &labels,
                        selected,
                        open,
                        move |event, window, cx| on_event(&event, window, cx),
                    )
                    .into_any_element()
                } else {
                    let fid = field.id.clone();
                    let on_select = cx.listener(move |this, ix: &usize, _window, cx| {
                        if let Some(value) = values.get(*ix).cloned() {
                            this.set_select(fid.clone(), value, cx);
                        }
                    });
                    components::segmented(
                        SharedString::from(format!("select-{}", field.id)),
                        &labels,
                        selected,
                        move |ix, window, cx| on_select(&ix, window, cx),
                    )
                    .into_any_element()
                };
                let control = div().w_full().min_w_0().child(selector);
                control.into_any_element()
            }
            FieldKind::Toggle => {
                let on = bool_val(&self.values, &field.id);
                let disabled = self.station_toggle_disabled(field);
                let fid = field.id.clone();
                let control = div()
                    .id(SharedString::from(format!("tog-{}", field.id)))
                    .child(layout::toggle(on))
                    .role(gpui::Role::Switch)
                    .aria_label(SharedString::from(field.label.clone()))
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    });
                if disabled {
                    control
                        .cursor_not_allowed()
                        .opacity(components::DISABLED_OPACITY)
                        .into_any_element()
                } else {
                    ochub_ui::keyboard::interactive(control)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.toggle_bool(fid.clone(), cx);
                        }))
                        .into_any_element()
                }
            }
            FieldKind::KeyValue { .. } => self.render_kv(&field.id, cx).into_any_element(),
            FieldKind::ModelGrid { columns } => self
                .render_grid(&field.id, columns, stack_grid, cx)
                .into_any_element(),
        };

        components::field(field.label.clone(), field.required, None, body)
            .when_some(
                self.field_errors.get(&field.id).cloned(),
                |field, message| {
                    field.child(div().text_xs().text_color(theme::red()).child(message))
                },
            )
            .into_any_element()
    }

    /// A toggle the selected station leaves no room for: the transport it
    /// declares (`supports_websockets`), or a feature it has no upstream for.
    /// Disabled capabilities stay visible without explanatory sublabels.
    fn station_toggle_disabled(&self, field: &FormField) -> bool {
        if self.source != ProviderSource::Station || self.app_type != Some(AppType::Codex) {
            return false;
        }
        match field.id.as_str() {
            "supports_websockets" => true,
            "remote_compaction" => !self.station_capabilities().remote_compaction,
            _ => false,
        }
    }

    /// Wrap a model text input with a popover listing the selected station's
    /// declared models. Typing stays free-form; the popover only fills the
    /// input, so names outside the station catalog remain possible.
    ///
    /// The list floats above the form (deferred + anchored) rather than sitting
    /// in the column: opening it must not reflow the fields below it, and its
    /// own scrolling must not drag the form along with it.
    ///
    /// The wrapper stays a plain block div on purpose. Taffy only tracks the
    /// static position of an absolutely positioned child in block layout; in a
    /// flex container it falls back to the container's alignment, which would
    /// pin the popover to the top edge — on top of the input.
    fn with_model_suggestions(
        &self,
        target: ModelSuggestionTarget,
        input: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open = self.open_model_suggestion.as_ref() == Some(&target);
        let toggle_target = target.clone();
        let focus_target = target.clone();
        let mut column = div().relative().w_full().min_w_0();
        column = column.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        // Clicking into the field opens the list so typing
                        // narrows it right away. The popover's dismiss handler
                        // runs first (capture phase), so this re-opens rather
                        // than fighting it.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.set_model_suggestion(Some(focus_target.clone()), cx);
                            }),
                        )
                        .child(input),
                )
                .child(
                    div()
                        .id(SharedString::from(target.element_id()))
                        .role(gpui::Role::Button)
                        .aria_label(t(k::PROVIDER_EDITOR_MODEL_SUGGESTIONS))
                        .aria_expanded(open)
                        .flex_none()
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(if open {
                            theme::accent()
                        } else {
                            theme::muted()
                        })
                        .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()))
                        .child(icon(
                            IconName::ChevronDown,
                            if open {
                                theme::accent()
                            } else {
                                theme::muted()
                            },
                            12.,
                        ))
                        // Mouse-down, not click: the popover dismisses itself on
                        // a capture-phase mouse-down, so a mouse-up toggle would
                        // only ever see the closed state and re-open.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.set_model_suggestion(
                                    (!open).then(|| toggle_target.clone()),
                                    cx,
                                );
                            }),
                        ),
                ),
        );
        if open {
            let catalog = self.station_models();
            let query = self.model_query(&target, cx).to_lowercase();
            let models: Vec<&String> = catalog
                .iter()
                .filter(|model| query.is_empty() || model.to_lowercase().contains(query.as_str()))
                .collect();
            let empty_key = if catalog.is_empty() {
                k::PROVIDER_EDITOR_MODEL_SUGGESTIONS_EMPTY
            } else {
                k::PROVIDER_EDITOR_MODEL_SUGGESTIONS_NO_MATCH
            };
            let mut list = div()
                .id(SharedString::from(format!("{}-list", target.element_id())))
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .min_w(px(220.))
                .max_h(px(240.))
                .overflow_y_scroll()
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(theme::border())
                .bg(theme::overlay())
                .shadow(theme::shadow_popover())
                .occlude();
            if models.is_empty() {
                list = list.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(t(empty_key)),
                );
            }
            for model in models {
                let pick = model.clone();
                let pick_target = target.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "{}-option-{}",
                            target.element_id(),
                            model
                        )))
                        .role(gpui::Role::ListBoxOption)
                        .aria_label(SharedString::from(model.clone()))
                        .flex()
                        .flex_row()
                        .items_center()
                        .w_full()
                        .min_h(px(28.))
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(theme::subtext())
                        .hover(|style| style.bg(theme::surface_hover()).text_color(theme::text()))
                        .child(div().min_w_0().flex_1().truncate().child(model.clone()))
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.apply_model_suggestion(pick_target.clone(), pick.clone(), cx);
                        })),
                );
            }
            let dismiss_target = target.clone();
            let list = list.on_mouse_down_out(cx.listener(move |this, _event, _window, cx| {
                if this.open_model_suggestion.as_ref() == Some(&dismiss_target) {
                    this.set_model_suggestion(None, cx);
                }
            }));
            column = column.child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(4.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(list),
                )
                .priority(30),
            );
        }
        column.into_any_element()
    }

    fn render_kv(&self, field_id: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut col = div().flex().flex_col().gap_2().w_full().min_w_0();
        if let Some(rows) = self.kv_rows.get(field_id) {
            for row in rows {
                let fid = field_id.to_string();
                let rid = row.id;
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(row.key.clone()))
                        .child(div().flex_1().min_w_0().child(row.value.clone()))
                        .child(
                            div().flex_none().child(
                                components::icon_button_tone(
                                    SharedString::from(format!("kv-del-{field_id}-{rid}")),
                                    t(k::PROVIDER_EDITOR_ROW_DELETE),
                                    IconName::Trash,
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(
                                    move |this, _e, _w, cx| {
                                        this.kv_remove(fid.clone(), rid, cx);
                                    },
                                )),
                            ),
                        ),
                );
            }
        }
        let fid = field_id.to_string();
        col.child(
            components::button(
                SharedString::from(format!("kv-add-{field_id}")),
                t(k::PROVIDER_EDITOR_KV_ADD),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.kv_add(fid.clone(), cx);
            })),
        )
    }

    fn render_grid(
        &self,
        field_id: &str,
        columns: &[provider_config::GridColumn],
        stack_grid: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Row-card list (docs/ui-overhaul.md §7.2): one card per mapping row
        // instead of a truncated table. Column slots: the first text column
        // (e.g. 角色) is fixed-width, the rest flex, toggles are pinned, and a
        // ghost icon button deletes the row.
        let mut col = div().flex().flex_col().gap_2().w_full().min_w_0();

        if stack_grid {
            if let Some(rows) = self.grid_rows.get(field_id) {
                for row in rows {
                    let rid = row.id;
                    let mut card = components::card()
                        .flex_col()
                        .items_stretch()
                        .min_w_0()
                        .gap_3()
                        .px_3()
                        .py_3();
                    for column in columns {
                        let label = SharedString::from(column.label.clone());
                        match &column.kind {
                            GridCellKind::Text { .. } => {
                                let cell = row
                                    .cells
                                    .get(&column.key)
                                    .map(|input| input.clone().into_any_element())
                                    .unwrap_or_else(|| div().into_any_element());
                                let cell = if self.source == ProviderSource::Station
                                    && column.key == "model"
                                {
                                    self.with_model_suggestions(
                                        ModelSuggestionTarget::GridCell(field_id.to_string(), rid),
                                        cell,
                                        cx,
                                    )
                                } else {
                                    cell
                                };
                                card = card.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            div().text_color(theme::muted()).text_xs().child(label),
                                        )
                                        .child(cell),
                                );
                            }
                            GridCellKind::Toggle => {
                                let on = row.toggles.get(&column.key).copied().unwrap_or(false);
                                let fid = field_id.to_string();
                                let key = column.key.clone();
                                card = card.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .gap_3()
                                        .child(
                                            div()
                                                .text_color(theme::subtext())
                                                .text_xs()
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "grid-tog-{field_id}-{rid}-{}",
                                                    column.key
                                                )))
                                                .role(gpui::Role::Switch)
                                                .aria_label(SharedString::from(
                                                    column.label.clone(),
                                                ))
                                                .aria_toggled(if on {
                                                    gpui::Toggled::True
                                                } else {
                                                    gpui::Toggled::False
                                                })
                                                .focusable()
                                                .tab_stop(true)
                                                .cursor_pointer()
                                                .child(layout::toggle(on))
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        this.grid_toggle(
                                                            fid.clone(),
                                                            rid,
                                                            key.clone(),
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        ),
                                );
                            }
                        }
                    }
                    let fid = field_id.to_string();
                    card = card.child(
                        div().flex().flex_row().justify_end().child(
                            components::icon_button_tone(
                                SharedString::from(format!("grid-del-{field_id}-{rid}")),
                                t(k::PROVIDER_EDITOR_ROW_DELETE),
                                IconName::Trash,
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.grid_remove(fid.clone(), rid, cx);
                                },
                            )),
                        ),
                    );
                    col = col.child(card);
                }
            }
            let fid = field_id.to_string();
            return col.child(
                components::button(
                    SharedString::from(format!("grid-add-{field_id}")),
                    t(k::PROVIDER_EDITOR_GRID_ADD_MODEL),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .w_full()
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.grid_add(fid.clone(), cx);
                })),
            );
        }

        // Caption header aligned with the row cards below.
        let mut header = div().flex().flex_row().items_center().gap_2().px_3();
        let mut first_text = true;
        for c in columns {
            let label = div()
                .text_color(theme::muted())
                .text_xs()
                .child(SharedString::from(c.label.clone()));
            header = header.child(match &c.kind {
                GridCellKind::Text { .. } if first_text => {
                    first_text = false;
                    div().w(px(96.)).flex_none().overflow_hidden().child(label)
                }
                GridCellKind::Text { .. } => {
                    div().flex_1().min_w_0().overflow_hidden().child(label)
                }
                GridCellKind::Toggle => div().w(px(64.)).flex_none().child(label),
            });
        }
        header = header.child(div().w(px(32.)).flex_none());
        col = col.child(header);

        if let Some(rows) = self.grid_rows.get(field_id) {
            for row in rows {
                let rid = row.id;
                let mut card = components::card()
                    .flex_row()
                    .items_center()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_2()
                    .px_3()
                    .py_2();
                let mut first_text = true;
                for c in columns {
                    match &c.kind {
                        GridCellKind::Text { .. } => {
                            let cell = row
                                .cells
                                .get(&c.key)
                                .map(|i| i.clone().into_any_element())
                                .unwrap_or_else(|| div().into_any_element());
                            let cell = if self.source == ProviderSource::Station && c.key == "model"
                            {
                                self.with_model_suggestions(
                                    ModelSuggestionTarget::GridCell(field_id.to_string(), rid),
                                    cell,
                                    cx,
                                )
                            } else {
                                cell
                            };
                            let slot = if first_text {
                                first_text = false;
                                div().w(px(96.)).flex_none().overflow_hidden()
                            } else {
                                div().flex_1().min_w_0().overflow_hidden()
                            };
                            card = card.child(slot.child(cell));
                        }
                        GridCellKind::Toggle => {
                            let on = row.toggles.get(&c.key).copied().unwrap_or(false);
                            let fid = field_id.to_string();
                            let key = c.key.clone();
                            card = card.child(
                                div().w(px(64.)).flex_none().flex().justify_center().child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "grid-tog-{field_id}-{rid}-{}",
                                            c.key
                                        )))
                                        .role(gpui::Role::Switch)
                                        .aria_label(SharedString::from(c.label.clone()))
                                        .aria_toggled(if on {
                                            gpui::Toggled::True
                                        } else {
                                            gpui::Toggled::False
                                        })
                                        .focusable()
                                        .tab_stop(true)
                                        .cursor_pointer()
                                        .child(layout::toggle(on))
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            this.grid_toggle(fid.clone(), rid, key.clone(), cx);
                                        })),
                                ),
                            );
                        }
                    }
                }
                let fid = field_id.to_string();
                card = card.child(
                    div().w(px(32.)).flex_none().child(
                        components::icon_only_button_tone(
                            SharedString::from(format!("grid-del-{field_id}-{rid}")),
                            t(k::PROVIDER_EDITOR_ROW_DELETE),
                            IconName::Trash,
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.grid_remove(fid.clone(), rid, cx);
                        })),
                    ),
                );
                col = col.child(card);
            }
        }
        let fid = field_id.to_string();
        col.child(
            components::button(
                SharedString::from(format!("grid-add-{field_id}")),
                t(k::PROVIDER_EDITOR_GRID_ADD_MODEL),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .w_full()
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.grid_add(fid.clone(), cx);
            })),
        )
    }

    fn select_preview_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.preview_cache.files.len() {
            self.preview_active_file = index;
            cx.notify();
        }
    }

    fn toggle_preview_fold(&mut self, file_index: usize, header: usize, cx: &mut Context<Self>) {
        let key = (file_index, header);
        if !self.preview_collapsed.remove(&key) {
            self.preview_collapsed.insert(key);
        }

        let previous_generation = self.preview_generation;
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let generation = self.preview_generation;
        if self.preview_dirty || self.preview_applied_generation != previous_generation {
            self.preview_dirty = true;
            self.start_preview_build(cx);
            cx.notify();
            return;
        }

        let Some(document) = self.preview_cache.files.get(file_index) else {
            self.preview_dirty = true;
            self.start_preview_build(cx);
            cx.notify();
            return;
        };
        let collapsed = self
            .preview_collapsed
            .iter()
            .filter_map(|(index, header)| (*index == file_index).then_some(*header))
            .collect::<HashSet<_>>();
        if document.line_count() <= PREVIEW_FOLD_BACKGROUND_LINE_THRESHOLD {
            let rows = preview_visible_rows(document.line_count(), &document.regions, &collapsed);
            if let Some(document) = self.preview_cache.files.get_mut(file_index) {
                document.visible_rows = Arc::new(rows);
            }
            self.preview_applied_generation = generation;
            cx.notify();
            return;
        }

        let line_count = document.line_count();
        let regions = document.regions.clone();
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_spawn(
                    async move { preview_visible_rows(line_count, &regions, &collapsed) },
                )
                .await;
            this.update(cx, |this, cx| {
                if generation != this.preview_generation {
                    return;
                }
                if let Some(document) = this.preview_cache.files.get_mut(file_index) {
                    document.visible_rows = Arc::new(rows);
                    this.preview_applied_generation = generation;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_preview_rows(
        &mut self,
        file_index: usize,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(document) = self.preview_cache.files.get(file_index).cloned() else {
            return Vec::new();
        };

        let mut rows = Vec::with_capacity(range.end.saturating_sub(range.start));
        for visible_index in range {
            let Some(&line_index) = document.visible_rows.get(visible_index) else {
                continue;
            };
            let line = document.line(line_index);
            let folded = document.region_headers.contains(&line_index)
                && self.preview_collapsed.contains(&(file_index, line_index));
            let display = if folded {
                SharedString::from(format!("{line} ⋯"))
            } else {
                SharedString::from(line.to_string())
            };

            let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
            let mut offset = 0usize;
            for (len, token) in highlight::line_spans(document.lang, line) {
                if len > 0 && token != crate::highlight::Token::Plain {
                    highlights.push((
                        offset..offset + len,
                        HighlightStyle {
                            color: Some(token.color().into()),
                            ..Default::default()
                        },
                    ));
                }
                offset += len;
            }

            let chevron: gpui::AnyElement = if document.region_headers.contains(&line_index) {
                div()
                    .w(px(14.))
                    .flex_none()
                    .cursor_pointer()
                    .text_color(theme::muted())
                    .child(if folded { "▸" } else { "▾" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_preview_fold(file_index, line_index, cx);
                        }),
                    )
                    .into_any_element()
            } else {
                div().w(px(14.)).flex_none().into_any_element()
            };

            rows.push(
                div()
                    .id(SharedString::from(format!(
                        "preview-row-{file_index}-{line_index}"
                    )))
                    .flex()
                    .flex_row()
                    .flex_none()
                    .items_center()
                    .min_w_full()
                    .min_w(px(document.max_line_cells as f32 * 8. + 64.))
                    .h(px(18.))
                    .px_4()
                    .cursor_pointer()
                    .child(chevron)
                    .child(
                        div()
                            .flex_none()
                            .child(StyledText::new(display).with_highlights(highlights)),
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_raw_edit(file_index, cx);
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    fn render_preview(
        &self,
        _compact: bool,
        _expanded_height: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let file_count = self.preview_cache.files.len();
        let file_index = self.preview_active_file.min(file_count.saturating_sub(1));
        let document = self.preview_cache.files.get(file_index).cloned();

        // Keep file tabs short; the selected target's complete path has its
        // own horizontally scrollable line below this compact action bar.
        let mut header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .flex_none()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text())
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t(k::PROVIDER_EDITOR_PREVIEW_TITLE)),
            );
        if file_count > 1 {
            for (index, file) in self.preview_cache.files.iter().enumerate() {
                header = header.child(
                    components::button(
                        SharedString::from(format!("preview-tab-{index}")),
                        preview_file_basename(&file.filename),
                        if index == file_index {
                            ButtonTone::Neutral
                        } else {
                            ButtonTone::Ghost
                        },
                        ButtonSize::Sm,
                    )
                    .flex_none()
                    .max_w(px(180.))
                    .overflow_hidden()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.select_preview_file(index, cx);
                    })),
                );
            }
        } else if let Some(file) = &document {
            header = header.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme::subtext())
                    .text_xs()
                    .font_family(ochub_ui::assets::monospace_font_family())
                    .child(preview_file_basename(&file.filename)),
            );
        }
        header = header.child(div().flex_1());
        if let Some(file) = &document {
            let metadata = SharedString::from(tf!(
                k::PROVIDER_EDITOR_PREVIEW_METADATA,
                lines = file.line_count(),
                size = format_preview_bytes(file.content.len())
            ));
            header = header
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(metadata),
                )
                .child(components::badge(BadgeTone::Neutral, file.language_label))
                .child(
                    components::button(
                        SharedString::from(format!("preview-edit-{file_index}")),
                        t(k::PROVIDER_EDITOR_PREVIEW_EDIT),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_raw_edit(file_index, cx);
                    })),
                );
        }
        header = header.child(
            components::button(
                "editor-refresh-preview",
                t(k::PROVIDER_EDITOR_PREVIEW_REFRESH),
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.invalidate_preview(cx);
            })),
        );
        let mut pane = components::card()
            .p_0()
            .min_h_0()
            .min_w_0()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            .child(header)
            .when_some(document.as_ref(), |pane, file| {
                pane.child(preview_path_row(
                    "preview-selected-path",
                    file.filename.clone(),
                ))
            })
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::PROVIDER_EDITOR_WORKBENCH_CANDIDATE)),
            );

        // Issues sit directly under the header: they are the actionable part
        // of this pane and must never be pushed below the fold by the code.
        if let Some(issues) = self.render_preview_issues() {
            pane = pane.child(issues);
        }

        let Some(document) = document else {
            return pane
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .px_6()
                        .text_color(theme::muted())
                        .text_sm()
                        .child(t(k::PROVIDER_EDITOR_PREVIEW_EMPTY)),
                )
                .into_any_element();
        };

        let visible_count = document.visible_rows.len();
        let preview_list = uniform_list(
            SharedString::from(format!("preview-lines-{file_index}")),
            visible_count,
            cx.processor(move |this, range, _window, cx| {
                this.render_preview_rows(file_index, range, cx)
            }),
        )
        .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
        .flex_1()
        .min_h_0()
        .w_full();

        pane.child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_hidden()
                .text_xs()
                .font_family(ochub_ui::assets::monospace_font_family())
                .text_color(theme::text())
                .child(preview_list),
        )
        .into_any_element()
    }

    /// Keep validation visible without consuming the entire preview. Long
    /// issue lists scroll within a bounded strip below the header.
    fn render_preview_issues(&self) -> Option<gpui::AnyElement> {
        if self.preview_cache.issues.is_empty() {
            return None;
        }
        Some(
            div()
                .id("preview-validation-issues")
                .flex()
                .flex_col()
                .flex_none()
                .max_h(px(128.))
                .overflow_y_scroll()
                .gap_1()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme::border())
                .children(self.preview_cache.issues.iter().map(|issue| {
                    let (bg, fg, tag) = match issue.severity {
                        Severity::Error => (
                            theme::red_soft(),
                            theme::red(),
                            raw(k::PROVIDER_EDITOR_ISSUE_ERROR_TAG),
                        ),
                        Severity::Warning => (
                            theme::yellow_soft(),
                            theme::yellow(),
                            raw(k::PROVIDER_EDITOR_ISSUE_WARNING_TAG),
                        ),
                        Severity::Info => (
                            theme::inset(),
                            theme::subtext(),
                            raw(k::PROVIDER_EDITOR_ISSUE_INFO_TAG),
                        ),
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .min_w_0()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(bg)
                        .child(
                            div()
                                .flex_none()
                                .pt(px(1.))
                                .text_color(fg)
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child(tag),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .text_color(theme::subtext())
                                .text_sm()
                                .child(SharedString::from(issue.message.clone())),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_raw_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let raw = self.raw_edit.as_ref()?;
        let available = content_viewport(window);
        let save = if !self.can_save_draft() {
            components::disabled_button(
                "raw-save-draft",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Neutral,
                ButtonSize::Sm,
                true,
            )
        } else {
            components::button(
                "raw-save-draft",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| this.submit_draft(false, false, cx)))
        };
        let card = modal::card_in(available, px(760.))
            .h((available.height - px(32.)).min(px(640.)))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_4()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(preview_file_basename(&raw.filename)),
                    )
                    .child(
                        components::icon_button_tone(
                            "raw-close",
                            t(k::PROVIDER_EDITOR_RAW_CLOSE),
                            IconName::Close,
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .flex_none()
                        .on_click(cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx))),
                    ),
            )
            .child(preview_path_row("raw-selected-path", raw.filename.clone()))
            .child(
                components::modal_body()
                    .flex_1()
                    .min_h_0()
                    .when_some(raw.error.clone(), |s, err| {
                        s.child(div().text_color(theme::red()).text_xs().child(err))
                    })
                    // CodeEditor scrolls (and frames) itself; a second scroll
                    // container here made wheel gestures move both layers.
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h(px(0.))
                            .w_full()
                            .child(raw.input.clone()),
                    ),
            )
            .child(components::modal_footer(vec![
                components::button(
                    "raw-cancel",
                    t(k::PROVIDER_EDITOR_RAW_CANCEL),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx)))
                .into_any_element(),
                save.into_any_element(),
                components::button(
                    "raw-apply",
                    t(k::PROVIDER_EDITOR_RAW_APPLY),
                    ButtonTone::Primary,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _e, _w, cx| this.apply_raw_edit(cx)))
                .into_any_element(),
            ]));
        Some(
            // The overlay occludes the page (the card occludes the overlay), so
            // the editor behind neither scrolls nor reacts; clicking the scrim
            // dismisses the modal.
            modal::overlay(
                "raw-config-modal",
                raw.filename.clone(),
                &self.raw_focus,
                card,
            )
            .on_action(cx.listener(|this, _: &modal::Dismiss, _, cx| this.close_raw_edit(cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx)),
            )
            .into_any_element(),
        )
    }

    fn render_common_config(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.tab != EditorTab::Advanced {
            return gpui::Empty.into_any_element();
        }
        let enabled = self.common_config_enabled;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(layout::section_header(
                t(k::PROVIDER_EDITOR_COMMON_CONFIG_SECTION_TITLE),
                None,
            ))
            .child({
                let on_change = cx.listener(|this, _: &(), _, cx| this.toggle_common_config(cx));
                components::checkbox(
                    "common-config-toggle",
                    t(k::PROVIDER_EDITOR_COMMON_CONFIG_TOGGLE_LABEL),
                    enabled,
                    self.saving,
                    move |window, cx| on_change(&(), window, cx),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::PROVIDER_EDITOR_WORKBENCH_SHARED)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        components::button(
                            "common-config-extract",
                            t(k::PROVIDER_EDITOR_COMMON_CONFIG_EXTRACT),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.extract_common_config(cx);
                            },
                        )),
                    ),
            )
            // The code-mode TextInput brings its own frame, fixed height and
            // internal scroll (with wheel containment) — wrapping it in another
            // scroll container stacked three scroll regions on this page.
            .child(self.common_snippet.clone())
            .into_any_element()
    }

    fn render_convert_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.convert_open {
            return None;
        }
        let mut targets = div().flex().flex_col().gap_2().w_full();
        for app in crate::app_meta::enabled_app_types() {
            if Some(app) == self.app_type {
                continue;
            }
            targets = targets.child(
                div()
                    .id(SharedString::from(format!(
                        "convert-target-{}",
                        app.as_str()
                    )))
                    .role(gpui::Role::Button)
                    .focusable()
                    .tab_stop(true)
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme::inset())
                    .text_color(theme::text())
                    .text_sm()
                    .hover(|style| style.bg(theme::accent_soft()).text_color(theme::accent()))
                    .child(crate::app_meta::label(app))
                    .child(div().text_color(theme::muted()).text_xs().child("→"))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.convert_to(app, cx);
                    })),
            );
        }
        Some(
            modal::overlay(
                "convert-modal",
                t(k::PROVIDER_EDITOR_CONVERT_TITLE),
                &self.convert_focus,
                modal::card_in(content_viewport(window), px(520.))
                    .child(components::modal_header(t(
                        k::PROVIDER_EDITOR_CONVERT_TITLE,
                    )))
                    .child(modal::body("convert-targets").child(targets))
                    .child(components::modal_footer(vec![
                        components::button(
                            "convert-cancel",
                            t(k::PROVIDER_EDITOR_CONVERT_CANCEL),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.close_convert(cx);
                        }))
                        .into_any_element(),
                    ])),
            )
            .on_action(cx.listener(|this, _: &modal::Dismiss, _, cx| this.close_convert(cx)))
            .into_any_element(),
        )
    }

    fn render_identity(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .when(
                self.tab == EditorTab::Basic
                    && self
                        .app_type
                        .is_some_and(provider_config::station_source_supported),
                |column| {
                    column.child(components::field(
                        if self.app_type == Some(AppType::Codex) {
                            t(k::PROVIDER_EDITOR_CODEX_MODEL_SOURCE)
                        } else {
                            t(k::PROVIDER_EDITOR_SOURCE_LABEL)
                        },
                        false,
                        None,
                        self.render_source_selector(cx),
                    ))
                },
            )
            .when(self.tab == EditorTab::Basic, |column| {
                column.child(
                    components::field(
                        t(k::PROVIDER_EDITOR_IDENTITY_NAME_LABEL),
                        true,
                        None,
                        self.name.clone(),
                    )
                    .when_some(
                        self.field_errors.get("$name").cloned(),
                        |field, message| {
                            field.child(div().text_xs().text_color(theme::red()).child(message))
                        },
                    ),
                )
            })
            .when(self.tab == EditorTab::Advanced, |column| {
                column
                    .when(!self.is_editing(), |s| {
                        s.child(components::field(
                            t(k::PROVIDER_EDITOR_IDENTITY_ID_LABEL),
                            false,
                            None,
                            self.provider_id.clone(),
                        ))
                    })
                    .child(components::field(
                        t(k::PROVIDER_EDITOR_IDENTITY_WEBSITE_LABEL),
                        false,
                        None,
                        self.website_url.clone(),
                    ))
                    .child(components::field(
                        t(k::PROVIDER_EDITOR_IDENTITY_CATEGORY_LABEL),
                        false,
                        None,
                        self.category.clone(),
                    ))
                    .child(components::field(
                        t(k::PROVIDER_EDITOR_IDENTITY_NOTES_LABEL),
                        false,
                        None,
                        self.notes.clone(),
                    ))
            })
    }

    /// Direct-connection vs. relay-station toggle, plus the station picker
    /// when the station source is active.
    fn render_source_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let codex = self.app_type == Some(AppType::Codex);
        let official = codex
            && self.source == ProviderSource::Direct
            && str_val(&self.values, "auth_mode") == "openai_login"
            && self
                .text_inputs
                .get("base_url")
                .is_none_or(|i| i.read(cx).content().trim().is_empty());
        let labels = if codex {
            vec![
                t(k::PROVIDER_EDITOR_CODEX_OFFICIAL),
                t(k::PROVIDER_EDITOR_SOURCE_STATION),
                t(k::PROVIDER_EDITOR_SOURCE_DIRECT),
            ]
        } else {
            vec![
                t(k::PROVIDER_EDITOR_SOURCE_DIRECT),
                t(k::PROVIDER_EDITOR_SOURCE_STATION),
            ]
        };
        let label_refs: Vec<&str> = labels.iter().map(SharedString::as_ref).collect();
        let selected = if self.source == ProviderSource::Station {
            1
        } else if codex && !official {
            2
        } else {
            0
        };
        let on_select = cx.listener(move |this, index: &usize, _window, cx| {
            this.set_source(
                if *index == 1 {
                    ProviderSource::Station
                } else {
                    ProviderSource::Direct
                },
                cx,
            );
            if codex && *index == 0 {
                if str_val(&this.values, "auth_mode") != "openai_login" {
                    this.official_values = Some(this.current_values(cx));
                }
                provider_config::set_str(&mut this.values, "auth_mode", "openai_login");
                if let Some(input) = this.text_inputs.get("base_url") {
                    input.update(cx, |i, cx| i.set_content("", cx));
                }
                if let Some(input) = this.text_inputs.get("api_key") {
                    input.update(cx, |i, cx| i.set_content("", cx));
                }
                provider_config::set_str(&mut this.values, "base_url", "");
                provider_config::set_str(&mut this.values, "api_key", "");
            } else if codex && *index == 2 && str_val(&this.values, "auth_mode") == "openai_login" {
                if let Some(values) = this.official_values.take() {
                    this.apply_values(values, cx);
                }
                provider_config::set_str(
                    &mut this.values,
                    "auth_mode",
                    "openai_login_with_api_key",
                );
            }
            this.form_list_state.remeasure();
            this.invalidate_preview(cx);
        });
        let mut column =
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(components::segmented(
                    "provider-source",
                    &label_refs,
                    selected,
                    move |index, window, cx| on_select(&index, window, cx),
                ));
        if self.source == ProviderSource::Station {
            column = column.child(components::field(
                t(k::PROVIDER_EDITOR_SOURCE_STATION),
                true,
                None,
                self.render_station_picker(cx),
            ));
        }
        column.into_any_element()
    }

    fn render_station_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.station_options.is_empty() {
            let message = if self.station_options_loaded {
                t(k::PROVIDER_EDITOR_STATION_EMPTY)
            } else {
                SharedString::new_static("…")
            };
            return div()
                .text_sm()
                .text_color(theme::muted())
                .child(message)
                .into_any_element();
        }
        let labels: Vec<String> = self
            .station_options
            .iter()
            .map(|option| {
                tf!(
                    k::PROVIDER_EDITOR_STATION_OPTION,
                    name = option.name,
                    count = option.models.len()
                )
            })
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected = self
            .selected_station
            .as_deref()
            .and_then(|route_id| {
                self.station_options
                    .iter()
                    .position(|option| option.route_id == route_id)
            })
            .unwrap_or(usize::MAX);
        let route_ids: Vec<String> = self
            .station_options
            .iter()
            .map(|option| option.route_id.clone())
            .collect();
        let open = self.station_dropdown_open;
        let on_event = cx.listener(
            move |this, event: &components::SelectDropdownEvent, _window, cx| match *event {
                components::SelectDropdownEvent::Open(open) => {
                    this.station_dropdown_open = open;
                    this.form_list_state.remeasure();
                    cx.notify();
                }
                components::SelectDropdownEvent::Select(index) => {
                    if let Some(route_id) = route_ids.get(index).cloned() {
                        this.select_station(route_id, cx);
                    }
                }
            },
        );
        let mut column = div().flex().flex_col().gap_1().w_full().min_w_0().child(
            components::select_dropdown_with_placeholder(
                "provider-station",
                &label_refs,
                selected,
                open,
                t(k::PROVIDER_EDITOR_STATION_PLACEHOLDER),
                move |event, window, cx| on_event(&event, window, cx),
            ),
        );
        // Editing a channel whose station was deleted: say so instead of
        // showing a blank dropdown selection.
        if selected == usize::MAX && self.selected_station.is_some() {
            column = column.child(
                div()
                    .text_xs()
                    .text_color(theme::red())
                    .child(t(k::PROVIDER_EDITOR_STATION_MISSING)),
            );
        }
        column.into_any_element()
    }

    fn render_form_intro(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut intro = div().flex().flex_col().gap_5().w_full().min_w_0();
        // Station mode owns the endpoint, so the preset picker does not apply
        // there.
        if self.tab == EditorTab::Basic
            && self.source == ProviderSource::Direct
            && !self.presets.is_empty()
        {
            let names: Vec<&str> = self
                .presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect();
            let selected = self.selected_preset.unwrap_or(usize::MAX);
            let on_select = cx.listener(move |this, position: &usize, _window, cx| {
                this.apply_preset(*position, cx);
            });
            intro = intro.child(components::field(
                t(k::PROVIDER_EDITOR_FORM_PRESETS_LABEL),
                false,
                None,
                components::segmented(
                    "editor-presets",
                    &names,
                    selected,
                    move |position, window, cx| on_select(&position, window, cx),
                ),
            ));
        }
        intro.child(self.render_identity(cx)).into_any_element()
    }

    fn section_is_advanced(&self, section: &FormSection) -> bool {
        section.advanced || (self.app_type == Some(AppType::Codex) && section.id == "provider")
    }

    fn render_form_section(
        &self,
        section_index: usize,
        stack_grid: bool,
        official_login: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(section) = self.schema.get(section_index) else {
            return gpui::Empty.into_any_element();
        };
        if self.section_is_advanced(section) != (self.tab == EditorTab::Advanced) {
            return gpui::Empty.into_any_element();
        }
        if official_login
            && self.app_type != Some(AppType::Codex)
            && (section.id == "endpoint_auth"
                || (self.app_type == Some(AppType::KimiCode) && section_index == 0))
        {
            return self.render_official_auth_section();
        }
        if official_login && self.app_type == Some(AppType::KimiCode) && section_index > 0 {
            return gpui::Empty.into_any_element();
        }

        // In station mode the station supplies endpoint/credentials, so the
        // managed fields leave the form entirely; sections they fully occupy
        // collapse away.
        let station_mode = self.source == ProviderSource::Station;
        let fields: Vec<&FormField> = section
            .fields
            .iter()
            .filter(|field| field.is_visible(&self.values))
            .filter(|field| {
                !(self.app_type == Some(AppType::Codex)
                    && !self.show_advanced_fields
                    && matches!(
                        field.id.as_str(),
                        "name" | "provider_id" | "remote_compaction"
                    ))
            })
            .filter(|field| {
                !(self.app_type == Some(AppType::Codex)
                    && self.source == ProviderSource::Direct
                    && str_val(&self.values, "auth_mode") == "openai_login"
                    && self
                        .text_inputs
                        .get("base_url")
                        .is_none_or(|i| i.read(cx).content().trim().is_empty())
                    && matches!(
                        field.id.as_str(),
                        "base_url"
                            | "api_key"
                            | "provider_name"
                            | "provider_id"
                            | "remote_compaction"
                    ))
            })
            .filter(|field| {
                !(station_mode
                    && self
                        .app_type
                        .map(provider_config::station_managed_fields)
                        .unwrap_or(&[])
                        .contains(&field.id.as_str())
                    && !(self.app_type == Some(AppType::Codex)
                        && field.id == "supports_websockets"))
            })
            .collect();
        if fields.is_empty() {
            return gpui::Empty.into_any_element();
        }

        let mut column = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .when(self.app_type != Some(AppType::Codex), |column| {
                column.child(layout::section_header(section.title.clone(), None))
            });
        for field in fields {
            column = column.child(self.render_field(field, stack_grid, cx));
        }
        column.into_any_element()
    }

    fn render_form_tools(&self, official_login: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Fetch-models/speedtest/balance all interrogate the typed-in
        // endpoint, which station channels simply don't have.
        if self.app_type.is_none()
            || official_login
            || self.source == ProviderSource::Station
            || self.tab != EditorTab::Advanced
        {
            return gpui::Empty.into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .child(
                components::button(
                    "editor-fetch-models",
                    t(k::PROVIDER_EDITOR_TOOLS_FETCH_MODELS),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.fetch_models(cx))),
            )
            .child(
                components::button(
                    "editor-speedtest",
                    t(k::PROVIDER_EDITOR_TOOLS_SPEEDTEST),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.speedtest_base_url(cx);
                })),
            )
            .child(
                components::button(
                    "editor-balance",
                    t(k::PROVIDER_EDITOR_TOOLS_BALANCE),
                    ButtonTone::Neutral,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.query_balance(cx))),
            )
            .into_any_element()
    }

    fn render_form_item(
        &self,
        index: usize,
        stack_grid: bool,
        official_login: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let schema_len = self.schema.len();
        if index > 0
            && index <= schema_len
            && self.section_is_advanced(&self.schema[index - 1])
                != (self.tab == EditorTab::Advanced)
        {
            return gpui::Empty.into_any_element();
        }
        if index == schema_len + 1
            && (!self.common_config_supported || self.tab != EditorTab::Advanced)
        {
            return gpui::Empty.into_any_element();
        }
        if index == schema_len + 2 && self.tab != EditorTab::Advanced {
            return gpui::Empty.into_any_element();
        }
        let content = if index == 0 {
            self.render_form_intro(cx)
        } else if index <= schema_len {
            self.render_form_section(index - 1, stack_grid, official_login, cx)
        } else if index == schema_len + 1 {
            if self.common_config_supported {
                self.render_common_config(cx)
            } else {
                gpui::Empty.into_any_element()
            }
        } else if index == schema_len + 2 {
            self.render_form_tools(official_login, cx)
        } else {
            gpui::Empty.into_any_element()
        };

        div()
            .w_full()
            .min_w_0()
            .when(index < schema_len + 2, |item| item.pb_5())
            .when(index == schema_len + 2, |item| item.pb_6())
            .child(content)
            .into_any_element()
    }
}

impl Render for ProviderEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_width = window.viewport_size().width;
        let compact_layout = editor_screen::is_compact(window_width.into());
        let form_width = editor_screen::form_width(window_width.into(), false);
        let stack_grid = self
            .schema
            .iter()
            .flat_map(|section| &section.fields)
            .any(|field| {
                if let FieldKind::ModelGrid { columns } = &field.kind {
                    let text = columns
                        .iter()
                        .filter(|c| matches!(c.kind, GridCellKind::Text { .. }))
                        .count();
                    editor_screen::stacks_field_grid(form_width, text, columns.len() - text)
                } else {
                    false
                }
            });
        let raw_initial = self.raw_edit.as_ref().map(|raw| raw.input.focus_handle(cx));
        self.raw_focus
            .sync(self.raw_edit.is_some(), raw_initial.as_ref(), window, cx);
        self.convert_focus.sync(self.convert_open, None, window, cx);
        let official_login = self.uses_official_login(cx);
        if self.form_stack_grid != stack_grid || self.form_official_login != official_login {
            self.form_stack_grid = stack_grid;
            self.form_official_login = official_login;
            self.form_list_state.remeasure();
        }

        let title = if self.is_editing() {
            t(k::PROVIDER_EDITOR_PAGE_TITLE_EDIT)
        } else {
            t(k::PROVIDER_EDITOR_PAGE_TITLE_ADD)
        };
        let subtitle = SharedString::from(tf!(
            k::PROVIDER_EDITOR_PAGE_SUBTITLE,
            app = self.plugin.display_name()
        ));

        let modal = self.render_raw_modal(window, cx);
        let convert_modal = self.render_convert_modal(window, cx);
        let actions = div()
            .flex()
            .items_center()
            .gap_2()
            .when(self.app_type.is_some(), |actions| {
                actions.child(
                    components::button(
                        "editor-convert",
                        t(k::PROVIDER_EDITOR_ACTION_CONVERT),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_convert(cx))),
                )
            })
            .child(
                components::button(
                    "editor-docs",
                    t(k::PROVIDER_EDITOR_ACTION_DOCS),
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(|_, _, cx| {
                    cx.open_url(match ochub_core::i18n::current() {
                        ochub_core::i18n::Locale::Zh => {
                            "https://docs.ochub.org/zh/guides/providers"
                        }
                        ochub_core::i18n::Locale::Ja => {
                            "https://docs.ochub.org/ja/guides/providers"
                        }
                        ochub_core::i18n::Locale::En => "https://docs.ochub.org/guides/providers",
                    })
                }),
            )
            .child(
                components::button(
                    "editor-cancel",
                    t(k::PROVIDER_EDITOR_ACTION_CANCEL),
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.saving {
                        cx.emit(EditorEvent::Cancelled);
                    }
                })),
            );
        let tab_labels = [
            t(k::PROVIDER_EDITOR_WORKBENCH_BASIC),
            t(k::PROVIDER_EDITOR_WORKBENCH_ADVANCED),
            t(k::PROVIDER_EDITOR_WORKBENCH_FILES),
        ];
        let on_tab = cx.listener(|this, index: &usize, _, cx| {
            if *index != 2 && !this.merge_pending_raw(cx) {
                return;
            }
            this.tab = match index {
                1 => EditorTab::Advanced,
                2 => EditorTab::Files,
                _ => EditorTab::Basic,
            };
            this.show_advanced_fields = this.tab == EditorTab::Advanced;
            this.form_list_state.remeasure();
            cx.notify();
        });
        let tabs = components::segmented(
            "editor-depth",
            &tab_labels
                .iter()
                .map(SharedString::as_ref)
                .collect::<Vec<_>>(),
            match self.tab {
                EditorTab::Basic => 0,
                EditorTab::Advanced => 1,
                EditorTab::Files => 2,
            },
            move |index, window, cx| on_tab(&index, window, cx),
        );
        let available = self.backend_available && self.backend.supports_editor_drafts();
        let save_button = if !self.can_save_draft() {
            components::disabled_button(
                "editor-save",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Neutral,
                ButtonSize::Sm,
                true,
            )
        } else {
            components::button(
                "editor-save",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| this.submit_draft(false, false, cx)))
        };
        let apply_label = if self.pending_apply.is_some() {
            t(k::PROVIDER_EDITOR_WORKBENCH_RECONCILE).to_string()
        } else if self.app_type == Some(AppType::CherryStudio) {
            tf!(
                k::PROVIDER_EDITOR_WORKBENCH_IMPORT,
                app = self.plugin.display_name()
            )
        } else if self.plugin.mode() == AppMode::Additive {
            tf!(
                k::PROVIDER_EDITOR_WORKBENCH_ADD,
                app = self.plugin.display_name()
            )
        } else {
            tf!(
                k::PROVIDER_EDITOR_WORKBENCH_APPLY,
                app = self.plugin.display_name()
            )
        };
        let apply_button = if self.saving {
            components::busy_button(
                "editor-apply",
                apply_label,
                ButtonTone::Primary,
                ButtonSize::Sm,
                true,
            )
        } else if !available {
            components::disabled_button(
                "editor-apply",
                apply_label,
                ButtonTone::Primary,
                ButtonSize::Sm,
                true,
            )
        } else {
            components::button(
                "editor-apply",
                apply_label,
                ButtonTone::Primary,
                ButtonSize::Sm,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.pending_apply.is_some() {
                    this.reconcile_pending_apply(cx);
                } else {
                    this.submit_draft(true, false, cx);
                }
            }))
        };
        let dirty = self.is_dirty(cx);
        let state_label = if self.pending_apply.is_some() {
            t(k::PROVIDER_EDITOR_WORKBENCH_PENDING_TAG)
        } else if self.saving {
            t(k::PROVIDER_EDITOR_WORKBENCH_BUSY)
        } else if dirty {
            t(k::PROVIDER_EDITOR_WORKBENCH_MODIFIED)
        } else if self.applied {
            t(k::PROVIDER_EDITOR_WORKBENCH_APPLIED_TAG)
        } else if self.draft_revision.is_some() {
            t(k::PROVIDER_EDITOR_WORKBENCH_SAVED_TAG)
        } else {
            t(k::PROVIDER_EDITOR_WORKBENCH_UNCHANGED)
        };
        let note = if self.pending_apply.is_some() {
            t(k::PROVIDER_EDITOR_WORKBENCH_PENDING_REQUIRED)
        } else if !self.backend_available {
            t(k::PROVIDER_EDITOR_WORKBENCH_OFFLINE)
        } else if !available {
            t(k::PROVIDER_EDITOR_WORKBENCH_UNSUPPORTED)
        } else if self.applied && !dirty {
            t(k::PROVIDER_EDITOR_WORKBENCH_CLIENT_UNKNOWN)
        } else if self.preview_cache.error_count > 0 {
            SharedString::from(format!(
                "{} · {}",
                t(k::PROVIDER_EDITOR_WORKBENCH_NOT_APPLIED),
                tf!(
                    k::PROVIDER_EDITOR_ISSUE_ERROR_COUNT,
                    count = self.preview_cache.error_count
                )
            ))
        } else if self.preview_cache.warning_count > 0 {
            SharedString::from(format!(
                "{} · {}",
                t(k::PROVIDER_EDITOR_WORKBENCH_NOT_APPLIED),
                tf!(
                    k::PROVIDER_EDITOR_ISSUE_WARNING_COUNT,
                    count = self.preview_cache.warning_count
                )
            ))
        } else {
            t(k::PROVIDER_EDITOR_WORKBENCH_NOT_APPLIED)
        };
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .when(compact_layout, |footer| footer.flex_wrap())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .child(div().text_xs().text_color(theme::text()).child(state_label))
                    .child(div().text_xs().text_color(theme::muted()).child(note)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        components::button(
                            "editor-check",
                            t(k::PROVIDER_EDITOR_CODEX_CHECK),
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.saving {
                                this.check_configuration(window, cx);
                            }
                        })),
                    )
                    .child(save_button)
                    .child(apply_button),
            );

        let form_list = gpui::list(
            self.form_list_state.clone(),
            cx.processor(move |this, index: usize, _window, cx| {
                this.render_form_item(index, stack_grid, official_login, cx)
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0()
        .min_w_0()
        .pr_2();
        let contained_form_state = self.form_list_state.clone();

        let form_scroll = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .id("editor-form-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .on_scroll_wheel(crate::scrollbar::contain_vertical_scroll(
                        contained_form_state,
                    ))
                    .child(form_list),
            );

        let content = if self.tab == EditorTab::Files {
            self.render_preview(false, px(0.), cx).into_any_element()
        } else {
            form_scroll.into_any_element()
        };
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap_3()
            .when_some(self.operation_note.clone(), |content, note| {
                content.child(
                    div()
                        .id("editor-operation-result")
                        .max_h(px(112.))
                        .overflow_y_scroll()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .border_1()
                        .border_color(theme::border())
                        .rounded_md()
                        .text_sm()
                        .text_color(if self.operation_failed {
                            theme::red()
                        } else {
                            theme::subtext()
                        })
                        .child(note)
                        .when(self.receipt_id.is_some(), |result| {
                            result.child(
                                components::button(
                                    "editor-undo",
                                    t(k::PROVIDER_EDITOR_WORKBENCH_UNDO),
                                    ButtonTone::Ghost,
                                    ButtonSize::Sm,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.undo_apply(cx))),
                            )
                        }),
                )
            })
            .child(content);
        editor_screen::provider_editor_page(editor_screen::ProviderEditorPage {
            title,
            subtitle,
            actions: actions.into_any_element(),
            tabs: tabs.into_any_element(),
            footer: footer.into_any_element(),
            form_scroll: content.into_any_element(),
            preview: None,
            // The form is the editor page's primary scroll context. Its rail
            // belongs to the full-width page chrome rather than the split
            // boundary between the form and the independent file preview.
            form_scrollbar: (self.tab != EditorTab::Files).then(|| {
                crate::scrollbar::VerticalScrollbar::new(
                    "editor-form-scrollbar",
                    self.form_list_state.clone(),
                )
                .into_any_element()
            }),
            modal,
            convert_modal,
            compact_layout,
        })
    }
}

fn chrono_now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn common_config_supported(app: AppType) -> bool {
    matches!(app, AppType::Claude | AppType::Codex)
}

fn preview_file_basename(path: &str) -> SharedString {
    // Remote paths may use either separator, regardless of the local OS.
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
        .into()
}

fn preview_path_row(id: &'static str, path: SharedString) -> impl IntoElement {
    let copied = path.clone();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .px_4()
        .py_1()
        .min_w_0()
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .overflow_x_scroll()
                .text_xs()
                .text_color(theme::muted())
                .font_family(ochub_ui::assets::monospace_font_family())
                .child(div().whitespace_nowrap().child(path)),
        )
        .child(
            components::icon_button_tone(
                SharedString::from(format!("{id}-copy")),
                t(k::PROVIDER_EDITOR_PREVIEW_COPY_PATH),
                IconName::Copy,
                ButtonTone::Ghost,
                ButtonSize::Sm,
            )
            .flex_none()
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied.to_string()));
            }),
        )
}

fn grid_text_display(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

/// Text controls may display native numbers, bools, or structured values. An
/// untouched cell must not change their type simply because the form mounted.
fn grid_text_value(original: Option<&Value>, text: &str) -> Value {
    match original {
        Some(value) if !value.is_string() && grid_text_display(Some(value)) == text => {
            value.clone()
        }
        _ => Value::String(text.to_owned()),
    }
}

/// A raw document owns its declared store completely (including deletions),
/// while plugin metadata outside those stores remains untouched.
fn replace_document_stores(prior: &Value, parsed: Value) -> Value {
    match (prior.as_object(), parsed) {
        (Some(prior), Value::Object(parsed)) => {
            let mut merged = prior.clone();
            merged.extend(parsed);
            Value::Object(merged)
        }
        (_, parsed) => parsed,
    }
}

fn build_preview_cache(
    codec: &dyn AppConfig,
    values: &FormValues,
    working_base: &Value,
    category: Option<&str>,
    collapsed_regions: &HashSet<(usize, usize)>,
    raw_documents: Option<&[String]>,
) -> PreviewBuild {
    let started = Instant::now();
    let mut issues = codec.validate_for_category(values, category);
    if let Some(raw) = raw_documents
        && let Err(error) = codec.parse_files(raw)
    {
        issues.insert(0, ConfigIssue::error(error));
    }
    issues.sort_by_key(|issue| match issue.severity {
        Severity::Error => 0u8,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
    let error_count = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .count();
    let warning_count = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Warning)
        .count();
    let files = codec.preview(values, working_base);
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;
    let documents = files
        .into_iter()
        .enumerate()
        .map(|(file_index, mut file)| {
            if let Some(raw) = raw_documents.and_then(|files| files.get(file_index)) {
                file.content.clone_from(raw);
            }
            let lang = match file.language {
                ochub_core::provider_config::Language::Json => Lang::Json,
                ochub_core::provider_config::Language::Toml => Lang::Toml,
                ochub_core::provider_config::Language::Yaml => Lang::Yaml,
                ochub_core::provider_config::Language::Env => Lang::Env,
            };
            let language_label = match file.language {
                Language::Toml => "TOML",
                Language::Json => "JSON",
                Language::Yaml => "YAML",
                Language::Env => "ENV",
            };
            let content: Arc<str> = Arc::from(file.content);
            let line_ranges = Arc::new(preview_line_ranges(&content));
            // A conservative monospace bound also accounts for long lines
            // outside the virtualized viewport. Cache once per document,
            // rather than scanning all text on every rendered frame.
            let max_line_cells = line_ranges
                .iter()
                .map(|range| {
                    content[range.clone()]
                        .chars()
                        .map(|c| {
                            if c == '\t' {
                                8
                            } else if c.is_ascii() {
                                1
                            } else {
                                2
                            }
                        })
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0);
            let regions = Arc::new(fold_regions(lang, &content));
            let region_headers = Arc::new(
                regions
                    .iter()
                    .map(|region| region.header)
                    .collect::<HashSet<_>>(),
            );
            let collapsed = collapsed_regions
                .iter()
                .filter_map(|(index, header)| (*index == file_index).then_some(*header))
                .collect::<HashSet<_>>();
            let visible_rows = Arc::new(preview_visible_rows(
                line_ranges.len(),
                &regions,
                &collapsed,
            ));
            total_bytes += content.len();
            total_lines += line_ranges.len();
            PreviewDocument {
                filename: SharedString::from(file.filename),
                language_label,
                lang,
                content,
                line_ranges,
                regions,
                region_headers,
                visible_rows,
                max_line_cells,
            }
        })
        .collect();

    PreviewBuild {
        cache: PreviewCache {
            files: documents,
            issues,
            error_count,
            warning_count,
        },
        total_lines,
        total_bytes,
        elapsed: started.elapsed(),
    }
}

fn preview_line_ranges(content: &str) -> Vec<Range<usize>> {
    let estimated_lines = (content.len() / 48).clamp(1, 16_384);
    let mut ranges = Vec::with_capacity(estimated_lines);
    let mut start = 0usize;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            ranges.push(start..index);
            start = index + 1;
        }
    }
    ranges.push(start..content.len());
    ranges
}

fn preview_visible_rows(
    line_count: usize,
    regions: &[FoldRegion],
    collapsed: &HashSet<usize>,
) -> Vec<usize> {
    if line_count == 0 {
        return Vec::new();
    }

    // Difference marks make nested/overlapping folds linear in the document
    // size instead of touching every hidden line once per collapsed region.
    let mut hidden_delta = vec![0i32; line_count + 1];
    for region in regions {
        if !collapsed.contains(&region.header) {
            continue;
        }
        let start = (region.header + 1).min(line_count);
        let end = region.last.saturating_add(1).min(line_count);
        if start < end {
            hidden_delta[start] += 1;
            hidden_delta[end] -= 1;
        }
    }

    let mut depth = 0i32;
    let mut rows = Vec::with_capacity(line_count);
    for (line, delta) in hidden_delta.into_iter().take(line_count).enumerate() {
        depth += delta;
        if depth == 0 {
            rows.push(line);
        }
    }
    rows
}

fn format_preview_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The message *and* its severity: a balance response can report a failure, an
/// empty quota, or real numbers, and the toast host must not have to guess
/// which from the wording.
fn format_usage_result(result: &UsageResult) -> (NotificationLevel, String) {
    if !result.success {
        return (
            NotificationLevel::Error,
            result
                .error
                .as_ref()
                .map(|err| tf!(k::PROVIDER_EDITOR_BALANCE_FAILED, error = err))
                .unwrap_or_else(|| raw(k::PROVIDER_EDITOR_BALANCE_FAILED_PLAIN).to_string()),
        );
    }
    let Some(data) = &result.data else {
        return (
            NotificationLevel::Warning,
            raw(k::PROVIDER_EDITOR_BALANCE_NO_DATA).to_string(),
        );
    };
    let parts = data
        .iter()
        .take(3)
        .map(|item| {
            let name = item
                .plan_name
                .as_deref()
                .unwrap_or(raw(k::PROVIDER_EDITOR_BALANCE_DEFAULT_PLAN));
            let remaining = item
                .remaining
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| raw(k::PROVIDER_EDITOR_COMMON_UNKNOWN).to_string());
            let unit = item.unit.as_deref().unwrap_or("");
            tf!(
                k::PROVIDER_EDITOR_BALANCE_ITEM,
                name = name,
                remaining = remaining,
                unit = unit
            )
        })
        .collect::<Vec<_>>()
        .join(raw(k::PROVIDER_EDITOR_BALANCE_ITEM_JOIN));
    if parts.is_empty() {
        (
            NotificationLevel::Warning,
            raw(k::PROVIDER_EDITOR_BALANCE_NO_DATA).to_string(),
        )
    } else {
        (NotificationLevel::Success, parts)
    }
}

// Not `impl_status_toasts_leveled!`: this view has two message slots, and the
// error channel outranks `status`. `status_level` is set alongside whichever
// slot was written, so it always describes the message `take_toast` hands over.
impl crate::notifications::ToastSource for ProviderEditor {
    fn take_toast(&mut self) -> Option<SharedString> {
        self.error.take().or_else(|| self.status.take())
    }

    fn take_toast_level(&mut self) -> Option<NotificationLevel> {
        self.status_level.take()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_preview_cache, grid_text_display, grid_text_value, preview_line_ranges,
        preview_visible_rows, replace_document_stores,
    };
    use crate::fold::FoldRegion;
    use ochub_core::provider_config::{AppConfig, CodexConfig};
    use serde_json::{Value, json};
    use std::collections::HashSet;

    #[test]
    fn untouched_native_grid_cells_preserve_value_type_and_explicit_edits_replace_it() {
        for original in [
            json!(32768),
            json!(true),
            Value::Null,
            json!({"future": [1, 2]}),
        ] {
            let display = grid_text_display(Some(&original));
            assert_eq!(grid_text_value(Some(&original), &display), original);
            assert_eq!(grid_text_value(Some(&original), "edited"), json!("edited"));
        }
        assert_eq!(grid_text_display(Some(&json!(32768))), "32768");
        assert_eq!(grid_text_value(Some(&json!(32768)), ""), json!(""));
    }

    #[test]
    fn raw_document_edit_preserves_unknown_metadata_but_honors_key_deletion() {
        let prior = json!({"settings":{"keep":1,"remove":2},"pluginMetadata":{"future":true}});
        let parsed = json!({"settings":{"keep":3}});
        let result = replace_document_stores(&prior, parsed);
        assert_eq!(result["pluginMetadata"], prior["pluginMetadata"]);
        assert_eq!(result["settings"], json!({"keep":3}));
        assert!(result["settings"].get("remove").is_none());
    }

    #[test]
    fn invalid_saved_raw_document_is_visible_instead_of_a_generated_replacement() {
        let codec = CodexConfig;
        let values = codec.decode(&serde_json::Value::Null, None);
        let documents = vec!["model = [".to_string(), "{}".to_string()];
        let preview = build_preview_cache(
            &codec,
            &values,
            &serde_json::Value::Null,
            None,
            &HashSet::new(),
            Some(&documents),
        );
        assert_eq!(preview.cache.files[0].content.as_ref(), "model = [");
        assert!(preview.cache.error_count > 0);
    }

    #[test]
    fn preview_line_index_preserves_empty_and_trailing_lines() {
        let content = "first\n\nlast\n";
        let ranges = preview_line_ranges(content);
        let lines = ranges
            .iter()
            .map(|range| &content[range.clone()])
            .collect::<Vec<_>>();
        assert_eq!(lines, vec!["first", "", "last", ""]);
    }

    #[test]
    fn preview_line_index_handles_a_hundred_thousand_lines() {
        let content = "key = \"value\"\n".repeat(100_000);
        let ranges = preview_line_ranges(&content);
        assert_eq!(ranges.len(), 100_001);
        assert_eq!(&content[ranges[99_999].clone()], "key = \"value\"");
        assert_eq!(&content[ranges[100_000].clone()], "");
    }

    #[test]
    fn preview_visible_rows_handles_nested_collapsed_regions() {
        let regions = vec![
            FoldRegion { header: 0, last: 8 },
            FoldRegion { header: 2, last: 5 },
            FoldRegion {
                header: 10,
                last: 12,
            },
        ];
        let collapsed = HashSet::from([0, 2, 10]);
        assert_eq!(
            preview_visible_rows(14, &regions, &collapsed),
            vec![0, 9, 10, 13]
        );
    }
}
