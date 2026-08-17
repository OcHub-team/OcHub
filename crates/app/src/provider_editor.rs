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

use gpui::{
    Anchor, Context, Entity, FontWeight, HighlightStyle, ListAlignment, ListState, MouseButton,
    Pixels, SharedString, StyledText, Task, Window, anchored, deferred, div, point, prelude::*, px,
    relative, uniform_list,
};
use ochub_core::gateway::apply;
use ochub_core::provider_config::{
    self, AppConfig, ConfigIssue, FieldKind, FormField, FormSection, FormValues, GridCellKind,
    Language, Severity, StationCapabilities, bool_val, str_val,
};
use ochub_core::services::ConfigService;
use ochub_core::services::provider::ProviderService;
use ochub_core::{AppState, AppType, Provider, ProviderMeta, UsageResult};
use ochub_ui::screens::provider_editor::{
    self as editor_screen, PREVIEW_SPLIT_FRACTION, PREVIEW_SPLIT_MAX_WIDTH, PREVIEW_SPLIT_MIN_WIDTH,
};
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
use crate::remote::WorkspaceBackend;
use crate::text_input::{TextInput, TextInputEvent};
use crate::tf;
use crate::theme;

/// Outcome of the editor, observed by the host view via `cx.subscribe`.
pub enum EditorEvent {
    Saved,
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
}

/// Active "edit this file directly" modal over the preview.
struct RawEdit {
    file_index: usize,
    filename: SharedString,
    input: Entity<CodeEditor>,
    error: Option<SharedString>,
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
enum ProviderSaveFailure {
    CommonConfig(String),
    Provider(String),
    HistoryMigration(String),
}

struct ProviderSaveOutcome {
    saved_snippet: Option<String>,
    result: Result<(), ProviderSaveFailure>,
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
    /// A schema text field (currently always `model`).
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
    app_type: AppType,
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
    show_preview: bool,
    /// Stacked (narrow-window) mode only: whether the preview pane is expanded
    /// or collapsed to its one-line summary bar.
    preview_expanded: bool,
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
    common_config_supported: bool,
    common_config_enabled: bool,
    common_snippet: Entity<TextInput>,
    original_snippet: String,
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
    /// The schema-driven inputs are deliberately left alone: their
    /// placeholders come from `ochub_core::provider_config`, which owns that
    /// text and is translated there.
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
        self.form_list_state.remeasure();
        cx.notify();
    }

    pub(crate) fn shortcut_save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.raw_edit.is_some() {
            self.apply_raw_edit(cx);
        } else {
            self.do_save(cx);
        }
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
        app_type: AppType,
        cx: &mut Context<Self>,
    ) -> Self {
        let codec = provider_config::config_for(app_type)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let schema = codec.schema();
        let values = codec.decode(&Value::Null, None);
        let mut this = Self::base(
            app, backend, app_type, codec, schema, values, None, None, cx,
        );
        Self::observe_preview_input(&this.category, cx);
        this.build_inputs(cx);
        this.load_station_options(cx);
        this.start_preview_build(cx);
        this
    }

    pub fn new_edit(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_type: AppType,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> Self {
        let codec = provider_config::config_for(app_type)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let schema = codec.schema();
        let values = codec.decode(&provider.settings_config, provider.meta.as_ref());
        let station_route = (!backend.is_remote())
            .then(|| {
                provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.gateway_route_id.clone())
                    .filter(|_| provider_config::station_source_supported(app_type))
            })
            .flatten();
        let mut this = Self::base(
            app,
            backend,
            app_type,
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
        this
    }

    #[allow(clippy::too_many_arguments)]
    fn base(
        app: Arc<AppState>,
        backend: WorkspaceBackend,
        app_type: AppType,
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
        let common_config_supported = common_config_supported(app_type);
        let presets = Arc::new(codec.presets());
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
        let mut this = Self {
            app,
            backend,
            app_type,
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
            kv_rows: HashMap::new(),
            grid_rows: HashMap::new(),
            next_row_id: 0,
            selected_preset: None,
            presets,
            open_select_field: None,
            show_preview: true,
            preview_expanded: false,
            preview_collapsed: HashSet::new(),
            preview_active_file: 0,
            preview_cache: PreviewCache::default(),
            preview_dirty: true,
            preview_generation: 1,
            preview_applied_generation: 0,
            preview_refresh_task: None,
            raw_edit: None,
            common_config_supported,
            common_config_enabled,
            common_snippet,
            original_snippet,
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
        };
        this.load_common_snippet(cx);
        this
    }

    fn load_common_snippet(&mut self, cx: &mut Context<Self>) {
        if !self.common_config_supported {
            return;
        }
        let backend = self.backend.clone();
        let app_type = self.app_type;
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(
                    async move { backend.common_config(&app_type.app_id()).await },
                )
                .await;
            this.update(cx, |this, cx| {
                let snippet = match result {
                    Ok(snippet) => snippet.unwrap_or_default(),
                    Err(error) => {
                        log::warn!(
                            "failed to load common config snippet for {}: {}",
                            app_type.as_str(),
                            error
                        );
                        String::new()
                    }
                };
                // A fast typist may have changed the empty editor before the
                // database result returned. Keep their text, while still
                // recording the stored baseline for the save comparison.
                let untouched = this.common_snippet.read(cx).content().is_empty();
                this.original_snippet = snippet.clone();
                if untouched {
                    this.common_snippet
                        .update(cx, |input, cx| input.set_content(snippet, cx));
                }
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
        if self.backend.is_remote() || !provider_config::station_source_supported(self.app_type) {
            self.station_options_loaded = true;
            return;
        }
        let app = self.app.clone();
        let app_type = self.app_type;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { apply::station_channel_options(&app, app_type) })
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
        let app = self.app.clone();
        let app_type = self.app_type;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let config = app.db.get_gateway_config()?;
                    let origin = format!("http://127.0.0.1:{}", config.port);
                    let key = apply::ensure_key_for_route(
                        &app,
                        &apply::gateway_key_label(app_type, &route_id),
                        Some(&route_id),
                    )?;
                    Ok::<_, ochub_core::error::AppError>((route_id, origin, key.key))
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
        self.source = source;
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
        provider_config::clamp_station_fields(&mut self.values, self.app_type, caps);
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
                    let masked = matches!(field.kind, FieldKind::Secret { .. });
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
                            for col in &columns {
                                match &col.kind {
                                    GridCellKind::Text { placeholder } => {
                                        let content = row
                                            .get(&col.key)
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string();
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
                            rows.push(GridRow { id, cells, toggles });
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
            // An open model popover filters against the live input text, so a
            // keystroke has to repaint the form, not just the input itself.
            if this.open_model_suggestion.is_some() {
                cx.notify();
            }
            this.schedule_preview_refresh(cx);
        })
        .detach();
    }

    /// Text fields can emit several changes in one typing burst. Keep the form
    /// responsive immediately and rebuild the potentially multi-megabyte native
    /// document only after the user pauses briefly.
    fn schedule_preview_refresh(&mut self, cx: &mut Context<Self>) {
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
        let app_type = self.app_type;
        let values = self.values.clone();
        let working_base = self.working_base.clone();
        let category = self.category.read(cx).content().trim().to_string();
        let collapsed = self.preview_collapsed.clone();
        self.preview_dirty = false;
        self.preview_refresh_task = None;

        cx.spawn(async move |this, cx| {
            let build = cx
                .background_spawn(async move {
                    let codec = provider_config::config_for(app_type)
                        .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
                    build_preview_cache(
                        codec.as_ref(),
                        &values,
                        &working_base,
                        (!category.is_empty()).then_some(category.as_str()),
                        &collapsed,
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.preview_generation {
                    return;
                }
                this.apply_preview_build(build);
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
            if current.is_empty() {
                self.name
                    .update(cx, |input, cx| input.set_content(display_name, cx));
            }
        }
        if let Some(website_url) = preset.website_url.clone() {
            let current = self.website_url.read(cx).content().trim().to_string();
            if current.is_empty() {
                self.website_url
                    .update(cx, |input, cx| input.set_content(website_url, cx));
            }
        }
        self.apply_values(values, cx);
        self.selected_preset = Some(index);
    }

    /// Open the modal code editor for preview file `index`.
    fn open_raw_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.ensure_preview_current(cx);
        if let Some(file) = self.preview_cache.files.get(index) {
            let content = SharedString::from(file.content.to_string());
            let filename = file.filename.clone();
            let lang = file.lang;
            let input = cx.new(|cx| {
                let mut input = CodeEditor::new(cx, lang, "");
                input.set_content(content, cx);
                input
            });
            self.raw_edit = Some(RawEdit {
                file_index: index,
                filename,
                input,
                error: None,
            });
            cx.notify();
        }
    }

    fn close_raw_edit(&mut self, cx: &mut Context<Self>) {
        self.raw_edit = None;
        cx.notify();
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
        let mut contents: Vec<String> = self
            .preview_cache
            .files
            .iter()
            .map(|file| file.content.to_string())
            .collect();
        if idx < contents.len() {
            contents[idx] = edited;
        }
        match self.codec.parse_files(&contents) {
            Ok(settings) => {
                // The edited document becomes the new authoritative base, so
                // keys the form doesn't model are preserved through save.
                self.working_base = settings.clone();
                self.values = self.codec.decode(&settings, cur_meta.as_ref());
                self.text_inputs.clear();
                self.kv_rows.clear();
                self.grid_rows.clear();
                self.build_inputs(cx);
                self.raw_edit = None;
                self.set_status(
                    NotificationLevel::Success,
                    t(k::PROVIDER_EDITOR_RAW_APPLIED),
                );
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
                let mut obj = Map::new();
                for (k, input) in &row.cells {
                    obj.insert(
                        k.clone(),
                        Value::String(input.read(cx).content().to_string()),
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
        {
            let caps = self.station_capabilities();
            provider_config::inject_station_endpoint(
                &mut self.values,
                self.app_type,
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
            .push(GridRow { id, cells, toggles });
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

    fn do_save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.source == ProviderSource::Station {
            self.do_save_station(cx);
            return;
        }
        let name = self.name.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_IDENTITY_NAME_REQUIRED));
            cx.notify();
            return;
        }
        self.pull_values(cx);
        let category = nonempty(self.category.read(cx).content().trim().to_string());
        if self.app_type == AppType::KimiCode && category.as_deref() == Some("official") {
            provider_config::apply_official_defaults(&mut self.values);
        }

        let issues = self
            .codec
            .validate_for_category(&self.values, category.as_deref());
        if let Some(err) = issues.iter().find(|i| i.severity == Severity::Error) {
            self.set_error(tf!(
                k::PROVIDER_EDITOR_SAVE_CONFIG_INVALID,
                message = err.message
            ));
            cx.notify();
            return;
        }

        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let mut encoded = self
            .codec
            .encode(&self.values, &self.working_base, prior_meta.as_ref());

        let snippet_update = self.common_snippet_update(cx);
        if self.common_config_supported {
            if self.common_config_enabled {
                encoded
                    .meta
                    .get_or_insert_with(ProviderMeta::default)
                    .common_config_enabled = Some(true);
            } else if let Some(meta) = encoded.meta.as_mut() {
                meta.common_config_enabled = None;
            }
        }

        let website_url = nonempty(self.website_url.read(cx).content().trim().to_string());
        let notes = nonempty(self.notes.read(cx).content().trim().to_string());

        let (original_id, provider) = if let Some(original_id) = self.original_id.clone() {
            let mut provider = self.original_provider.clone().unwrap_or_else(|| {
                Provider::with_id(original_id.clone(), name.clone(), json!({}), None)
            });
            provider.name = name;
            provider.settings_config = encoded.settings_config;
            provider.meta = encoded.meta;
            provider.website_url = website_url;
            provider.category = category;
            provider.notes = notes;
            (Some(original_id), provider)
        } else {
            let id_input = self.provider_id.read(cx).content().trim().to_string();
            let id = if id_input.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                id_input
            };
            let mut provider = Provider::with_id(id, name, encoded.settings_config, website_url);
            provider.meta = encoded.meta;
            provider.category = category;
            provider.notes = notes;
            provider.created_at = Some(chrono_now_millis());
            (None, provider)
        };

        self.saving = true;
        self.error = None;
        let backend = self.backend.clone();
        let app_type = self.app_type;
        cx.spawn(async move |this, cx| {
            let outcome = crate::core_async::run(async move {
                let app_id = app_type.app_id();
                let mut saved_snippet = None;
                if let Some(snippet) = snippet_update {
                    if let Err(error) = backend.set_common_config(&app_id, snippet.clone()).await {
                        return ProviderSaveOutcome {
                            saved_snippet,
                            result: Err(ProviderSaveFailure::CommonConfig(error.to_string())),
                        };
                    }
                    saved_snippet = Some(snippet);
                }

                let result = match original_id {
                    Some(original_id) => match serde_json::to_value(provider) {
                        Ok(patch) => backend
                            .update_provider(&app_id, &original_id, patch)
                            .await
                            .map(|_| ()),
                        Err(error) => Err(error.into()),
                    },
                    None => backend
                        .create_provider(&app_id, provider, true)
                        .await
                        .map(|_| ()),
                }
                .map_err(|error| ProviderSaveFailure::Provider(error.to_string()));
                ProviderSaveOutcome {
                    saved_snippet,
                    result,
                }
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                if let Some(snippet) = outcome.saved_snippet {
                    this.original_snippet = snippet;
                }
                match outcome.result {
                    Ok(()) => cx.emit(EditorEvent::Saved),
                    Err(ProviderSaveFailure::CommonConfig(error)) => {
                        this.set_error(tf!(
                            k::PROVIDER_EDITOR_COMMON_CONFIG_INVALID,
                            error = error
                        ));
                        cx.notify();
                    }
                    Err(ProviderSaveFailure::Provider(error)) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_SAVE_FAILED, error = error));
                        cx.notify();
                    }
                    Err(ProviderSaveFailure::HistoryMigration(error)) => {
                        this.set_error(tf!(
                            k::PROVIDER_EDITOR_HISTORY_MIGRATION_FAILED,
                            error = error
                        ));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Save path for `source == Station`: endpoint + credential come from the
    /// running local gateway (never from form fields), so the provider is
    /// built in core once the gateway origin is known. The form only
    /// contributes identity fields and model choices.
    fn do_save_station(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_IDENTITY_NAME_REQUIRED));
            cx.notify();
            return;
        }
        let Some(route_id) = self.selected_station.clone() else {
            self.set_error(t(k::PROVIDER_EDITOR_STATION_REQUIRED));
            cx.notify();
            return;
        };
        self.pull_values(cx);
        let values = self.values.clone();
        let category = nonempty(self.category.read(cx).content().trim().to_string());
        let website_url = nonempty(self.website_url.read(cx).content().trim().to_string());
        let notes = nonempty(self.notes.read(cx).content().trim().to_string());
        let original_id = self.original_id.clone();
        let channel_id = original_id.clone().unwrap_or_else(|| {
            let typed = self.provider_id.read(cx).content().trim().to_string();
            if typed.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                typed
            }
        });
        let identity = apply::StationChannelIdentity {
            id: channel_id,
            name,
            website_url,
            category,
            notes,
        };
        let snippet_update = self.common_snippet_update(cx);
        let common_config_supported = self.common_config_supported;
        let common_config_enabled = self.common_config_enabled;
        let working_base = self.working_base.clone();
        let original_provider = self.original_provider.clone();
        let prior_meta = original_provider.as_ref().and_then(|p| p.meta.clone());

        self.saving = true;
        self.error = None;
        let app = self.app.clone();
        let app_type = self.app_type;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let mut saved_snippet = None;
                    if let Some(snippet) = snippet_update {
                        if let Err(error) = ConfigService::set_common_config_snippet(
                            &app,
                            app_type.as_str(),
                            snippet.clone(),
                        ) {
                            return ProviderSaveOutcome {
                                saved_snippet,
                                result: Err(ProviderSaveFailure::CommonConfig(error.to_string())),
                            };
                        }
                        saved_snippet = Some(snippet);
                    }

                    let result = (|| {
                        // The channel points at the local gateway, so saving
                        // implies the gateway should run — same contract as
                        // applying a station from the gateway page.
                        let mut config = app
                            .db
                            .get_gateway_config()
                            .map_err(|error| ProviderSaveFailure::Provider(error.to_string()))?;
                        if !config.enabled {
                            config.enabled = true;
                            app.db.set_gateway_config(&config).map_err(|error| {
                                ProviderSaveFailure::Provider(error.to_string())
                            })?;
                        }
                        let status = futures::executor::block_on(app.gateway.start())
                            .map_err(|error| ProviderSaveFailure::Provider(error.to_string()))?;
                        let mut provider = apply::build_station_channel(
                            &app,
                            app_type,
                            &route_id,
                            &values,
                            identity,
                            &status.base_url,
                            &working_base,
                            prior_meta.as_ref(),
                        )
                        .map_err(|error| ProviderSaveFailure::Provider(error.to_string()))?;
                        // Legacy station entries used the OcHub record UUID as
                        // Codex's model_provider. Only that exact generated
                        // shape is safe to migrate automatically: shared,
                        // user-chosen buckets (for example `custom`) may be
                        // referenced by more than one connection.
                        let history_bucket_rename = if app_type == AppType::Codex {
                            original_provider.as_ref().and_then(|original| {
                                let old_id = original
                                    .settings_config
                                    .get("config")
                                    .and_then(Value::as_str)
                                    .and_then(
                                        ochub_core::apps::codex::extract_codex_model_provider_id,
                                    )?;
                                let new_id = provider
                                    .settings_config
                                    .get("config")
                                    .and_then(Value::as_str)
                                    .and_then(
                                        ochub_core::apps::codex::extract_codex_model_provider_id,
                                    )?;
                                let legacy_coupled_id = old_id == original.id
                                    && (uuid::Uuid::parse_str(&old_id).is_ok()
                                        || old_id.starts_with("local-gateway-"));
                                (legacy_coupled_id && old_id != new_id).then_some((old_id, new_id))
                            })
                        } else {
                            None
                        };
                        // Editing must not strip fields the form doesn't own.
                        if let Some(original) = original_provider.as_ref() {
                            provider.created_at = original.created_at;
                            provider.sort_index = original.sort_index;
                            provider.icon.clone_from(&original.icon);
                            provider.icon_color.clone_from(&original.icon_color);
                        }
                        if common_config_supported {
                            if common_config_enabled {
                                provider
                                    .meta
                                    .get_or_insert_with(ProviderMeta::default)
                                    .common_config_enabled = Some(true);
                            } else if let Some(meta) = provider.meta.as_mut() {
                                meta.common_config_enabled = None;
                            }
                        }
                        match original_id {
                            Some(original_id) => ProviderService::update(
                                &app,
                                app_type,
                                Some(&original_id),
                                provider,
                            )
                            .map(|_| ()),
                            None => {
                                ProviderService::add(&app, app_type, provider, true).map(|_| ())
                            }
                        }
                        .map_err(|error| ProviderSaveFailure::Provider(error.to_string()))?;
                        if let Some((old_id, new_id)) = history_bucket_rename {
                            ochub_core::services::migrate_codex_history_provider_bucket(
                                &old_id, &new_id,
                            )
                            .map_err(|error| {
                                ProviderSaveFailure::HistoryMigration(error.to_string())
                            })?;
                        }
                        Ok(())
                    })();
                    ProviderSaveOutcome {
                        saved_snippet,
                        result,
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                if let Some(snippet) = outcome.saved_snippet {
                    this.original_snippet = snippet;
                }
                match outcome.result {
                    Ok(()) => cx.emit(EditorEvent::Saved),
                    Err(ProviderSaveFailure::CommonConfig(error)) => {
                        this.set_error(tf!(
                            k::PROVIDER_EDITOR_COMMON_CONFIG_INVALID,
                            error = error
                        ));
                        cx.notify();
                    }
                    Err(ProviderSaveFailure::Provider(error)) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_SAVE_FAILED, error = error));
                        cx.notify();
                    }
                    Err(ProviderSaveFailure::HistoryMigration(error)) => {
                        this.set_error(tf!(
                            k::PROVIDER_EDITOR_HISTORY_MIGRATION_FAILED,
                            error = error
                        ));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- helper actions (operate on `base_url` / `api_key` fields) -----------

    fn field_text(&self, id: &str, cx: &Context<Self>) -> String {
        self.text_inputs
            .get(id)
            .map(|i| i.read(cx).content().trim().to_string())
            .unwrap_or_default()
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) {
        if let Some(provider_id) = self.original_id.clone() {
            let backend = self.backend.clone();
            let app_id = self.app_type.app_id();
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
            let app_id = self.app_type.app_id();
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
            let app_id = self.app_type.app_id();
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
        cx.notify();
    }

    fn extract_common_config(&mut self, cx: &mut Context<Self>) {
        if self.backend.is_remote() {
            let backend = self.backend.clone();
            let app_id = self.app_type.app_id();
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
        match ConfigService::extract_common_config_snippet(
            &self.app,
            self.app_type,
            Some(&settings),
        ) {
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
        if self.saving {
            return;
        }
        self.pull_values(cx);
        let target_codec = provider_config::config_for(target)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let encoded = target_codec.encode(&self.values, &Value::Null, None);
        let base_name = self.name.read(cx).content().trim().to_string();
        let source_label = crate::app_meta::label(self.app_type);
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
                    .create_provider(&target.app_id(), provider, false)
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
                AppType::Claude
                    | AppType::ClaudeDesktop
                    | AppType::Codex
                    | AppType::KimiCode
                    | AppType::GrokBuild
            )
            && self.category.read(cx).content().trim() == "official"
    }

    fn render_official_auth_section(&self) -> gpui::AnyElement {
        let app_label = crate::app_meta::label(self.app_type);
        let cli_hint = match self.app_type {
            AppType::Claude => Some("`claude /login`"),
            AppType::KimiCode => Some("`kimi login`"),
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
        let body = match &field.kind {
            FieldKind::Text { .. } | FieldKind::Secret { .. } => {
                let input = self
                    .text_inputs
                    .get(&field.id)
                    .map(|i| i.clone().into_any_element())
                    .unwrap_or_else(|| div().into_any_element());
                if self.source == ProviderSource::Station && field.id == "model" {
                    self.with_model_suggestions(
                        ModelSuggestionTarget::Field(field.id.clone()),
                        input,
                        cx,
                    )
                } else {
                    input
                }
            }
            FieldKind::Select { options } => {
                // A station channel reaches the gateway with the gateway's own
                // key, so options that would leave it credential-less are not
                // offered while that source is active.
                let hidden: &[&str] = if self.source == ProviderSource::Station {
                    provider_config::station_hidden_options(self.app_type, &field.id)
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
                let mut control = div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(selector);
                // The per-option hint (previously baked into each pill label)
                // is shown for the selected option beneath the control.
                if let Some(hint) = options.get(selected).and_then(|o| o.hint.as_ref()) {
                    control = control.child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(SharedString::from(hint.clone())),
                    );
                }
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
                    control
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

        components::field(
            field.label.clone(),
            field.required,
            self.field_help(field),
            body,
        )
        .into_any_element()
    }

    /// A toggle the selected station leaves no room for: the transport it
    /// declares (`supports_websockets`), or a feature it has no upstream for.
    /// It stays visible so "why can't I change this?" has an answer right
    /// under it — see [`Self::field_help`].
    fn station_toggle_disabled(&self, field: &FormField) -> bool {
        if self.source != ProviderSource::Station || self.app_type != AppType::Codex {
            return false;
        }
        match field.id.as_str() {
            "supports_websockets" => true,
            "remote_compaction" => !self.station_capabilities().remote_compaction,
            _ => false,
        }
    }

    /// The field's schema help, or the station-mode variant when the source
    /// changes what the control means.
    fn field_help(&self, field: &FormField) -> Option<SharedString> {
        if self.source == ProviderSource::Station
            && let Some(help) = provider_config::station_field_help(
                self.app_type,
                &field.id,
                !self.station_toggle_disabled(field),
            )
        {
            return Some(SharedString::new_static(help));
        }
        field.help.clone().map(SharedString::from)
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
        header = header.child(div().w(px(72.)).flex_none());
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
                    div().flex_none().child(
                        components::icon_button_tone(
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
        compact: bool,
        expanded_height: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let file_count = self.preview_cache.files.len();
        let file_index = self.preview_active_file.min(file_count.saturating_sub(1));
        let document = self.preview_cache.files.get(file_index).cloned();

        // One header row carries everything the old three rows did: title,
        // file switcher, per-file meta and actions. Every row saved here goes
        // straight to visible preview lines.
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
                        file.filename.clone(),
                        if index == file_index {
                            ButtonTone::Neutral
                        } else {
                            ButtonTone::Ghost
                        },
                        ButtonSize::Sm,
                    )
                    .flex_none()
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
                    .font_family("Menlo")
                    .child(file.filename.clone()),
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
        if compact {
            header = header.child(
                components::button(
                    "preview-collapse",
                    t(k::PROVIDER_EDITOR_PREVIEW_COLLAPSE),
                    ButtonTone::Ghost,
                    ButtonSize::Sm,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.preview_expanded = false;
                    cx.notify();
                })),
            );
        }

        let mut pane = components::card()
            .p_0()
            .min_h_0()
            .flex_none()
            .overflow_hidden()
            .when(compact, |pane| pane.w_full().h(expanded_height))
            .when(!compact, |pane| {
                pane.w(relative(PREVIEW_SPLIT_FRACTION))
                    .min_w(px(PREVIEW_SPLIT_MIN_WIDTH))
                    .max_w(px(PREVIEW_SPLIT_MAX_WIDTH))
                    .h_full()
            })
            .child(header);

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
                .font_family("Menlo")
                .text_color(theme::text())
                .child(preview_list),
        )
        .into_any_element()
    }

    /// Validation strip: errors first, soft severity tint per row, no inner
    /// scroll region — the handful of issues a config can raise should simply
    /// be visible.
    fn render_preview_issues(&self) -> Option<gpui::AnyElement> {
        if self.preview_cache.issues.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
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

    /// Stacked-mode collapsed bar: one line naming the files plus issue
    /// counts. Keeps narrow windows on a single scroll context until the
    /// preview is explicitly wanted.
    fn render_preview_summary(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (errors, warnings) = self.preview_issue_counts();
        let files: SharedString = if self.preview_cache.files.is_empty() {
            t(k::PROVIDER_EDITOR_PREVIEW_SUMMARY_EMPTY)
        } else {
            SharedString::from(
                self.preview_cache
                    .files
                    .iter()
                    .map(|file| file.filename.to_string())
                    .collect::<Vec<_>>()
                    .join(raw(k::PROVIDER_EDITOR_PREVIEW_FILE_JOIN)),
            )
        };
        editor_screen::preview_summary_card(
            editor_screen::preview_summary(editor_screen::PreviewSummary {
                title: t(k::PROVIDER_EDITOR_PREVIEW_TITLE),
                files,
                errors: (errors > 0).then(|| {
                    SharedString::from(tf!(k::PROVIDER_EDITOR_ISSUE_ERROR_COUNT, count = errors))
                }),
                warnings: (warnings > 0).then(|| {
                    SharedString::from(tf!(
                        k::PROVIDER_EDITOR_ISSUE_WARNING_COUNT,
                        count = warnings
                    ))
                }),
            })
            .aria_label(t(k::PROVIDER_EDITOR_PREVIEW_EXPAND_ARIA))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.preview_expanded = true;
                cx.notify();
            })),
        )
    }

    fn preview_issue_counts(&self) -> (usize, usize) {
        (
            self.preview_cache.error_count,
            self.preview_cache.warning_count,
        )
    }

    /// Issue-count chip rendered beside the save action: the reason a save
    /// will fail belongs in the same eyeline as the button that commits it.
    /// In collapsed stacked mode clicking it expands the preview pane.
    fn render_issue_chip(
        &self,
        id: &'static str,
        tone: BadgeTone,
        label: String,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let badge = components::badge(tone, label);
        if compact && !self.preview_expanded {
            div()
                .id(id)
                .cursor_pointer()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.preview_expanded = true;
                    cx.notify();
                }))
                .child(badge)
                .into_any_element()
        } else {
            badge.into_any_element()
        }
    }
    fn render_raw_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let raw = self.raw_edit.as_ref()?;
        let card = components::modal_card()
            .w(px(760.))
            .max_h(px(640.))
            .child(
                components::modal_header(raw.filename.clone()).child(
                    components::icon_button_tone(
                        "raw-close",
                        t(k::PROVIDER_EDITOR_RAW_CLOSE),
                        IconName::Close,
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx))),
                ),
            )
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
            components::modal_overlay(card)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx)),
                )
                .into_any_element(),
        )
    }

    fn render_common_config(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div().flex().flex_col().gap_1().child(
                            div()
                                .text_color(theme::text())
                                .text_sm()
                                .child(t(k::PROVIDER_EDITOR_COMMON_CONFIG_TOGGLE_LABEL)),
                        ),
                    )
                    .child(
                        div()
                            .id("common-config-toggle")
                            .cursor_pointer()
                            .child(layout::toggle(enabled))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.toggle_common_config(cx);
                            })),
                    ),
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

    fn render_convert_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.convert_open {
            return None;
        }
        let mut targets = div().flex().flex_col().gap_2().w_full();
        for app in crate::app_meta::enabled_app_types() {
            if app == self.app_type {
                continue;
            }
            targets = targets.child(
                div()
                    .id(SharedString::from(format!(
                        "convert-target-{}",
                        app.as_str()
                    )))
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
            components::modal_overlay(
                components::modal_card()
                    .child(components::modal_header(t(
                        k::PROVIDER_EDITOR_CONVERT_TITLE,
                    )))
                    .child(components::modal_body().child(targets))
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
                !self.backend.is_remote()
                    && provider_config::station_source_supported(self.app_type),
                |column| {
                    column.child(components::field(
                        t(k::PROVIDER_EDITOR_SOURCE_LABEL),
                        false,
                        None,
                        self.render_source_selector(cx),
                    ))
                },
            )
            .child(components::field(
                t(k::PROVIDER_EDITOR_IDENTITY_NAME_LABEL),
                true,
                None,
                self.name.clone(),
            ))
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
    }

    /// Direct-connection vs. relay-station toggle, plus the station picker
    /// when the station source is active.
    fn render_source_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let labels = [
            t(k::PROVIDER_EDITOR_SOURCE_DIRECT),
            t(k::PROVIDER_EDITOR_SOURCE_STATION),
        ];
        let label_refs: Vec<&str> = labels.iter().map(SharedString::as_ref).collect();
        let selected = match self.source {
            ProviderSource::Direct => 0,
            ProviderSource::Station => 1,
        };
        let on_select = cx.listener(|this, index: &usize, _window, cx| {
            this.set_source(
                if *index == 1 {
                    ProviderSource::Station
                } else {
                    ProviderSource::Direct
                },
                cx,
            );
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
        if self.source == ProviderSource::Direct && !self.presets.is_empty() {
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
        if official_login
            && (section.title == "端点与鉴权"
                || (self.app_type == AppType::KimiCode && section_index == 0))
        {
            return self.render_official_auth_section();
        }
        if official_login && self.app_type == AppType::KimiCode && section_index > 0 {
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
                !(station_mode
                    && provider_config::station_managed_fields(self.app_type)
                        .contains(&field.id.as_str())
                    && !(self.app_type == AppType::Codex && field.id == "supports_websockets"))
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
            .child(layout::section_header(section.title.clone(), None));
        for field in fields {
            column = column.child(self.render_field(field, stack_grid, cx));
        }
        column.into_any_element()
    }

    fn render_form_tools(&self, official_login: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Fetch-models/speedtest/balance all interrogate the typed-in
        // endpoint, which station channels simply don't have.
        if official_login || self.source == ProviderSource::Station {
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
        let stack_grid = editor_screen::stacks_field_grid(window_width.into());
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
            app = crate::app_meta::label(self.app_type)
        ));

        // Stacked mode: the pane takes a real share of the window when
        // expanded and folds to a one-line summary bar when not, so narrow
        // windows keep a single scroll context.
        let preview_height = (window.viewport_size().height * 0.45).clamp(px(320.), px(620.));
        let preview = if !self.show_preview {
            None
        } else if !compact_layout || self.preview_expanded {
            Some(
                self.render_preview(compact_layout, preview_height, cx)
                    .into_any_element(),
            )
        } else {
            Some(self.render_preview_summary(cx).into_any_element())
        };
        let modal = self.render_raw_modal(cx);
        let convert_modal = self.render_convert_modal(cx);

        let (error_count, warning_count) = self.preview_issue_counts();
        let mut actions = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap_2();
        if error_count > 0 {
            actions = actions.child(self.render_issue_chip(
                "editor-error-chip",
                BadgeTone::Danger,
                tf!(k::PROVIDER_EDITOR_ISSUE_ERROR_COUNT, count = error_count),
                compact_layout,
                cx,
            ));
        }
        if warning_count > 0 {
            actions = actions.child(self.render_issue_chip(
                "editor-warning-chip",
                BadgeTone::Warning,
                tf!(
                    k::PROVIDER_EDITOR_ISSUE_WARNING_COUNT,
                    count = warning_count
                ),
                compact_layout,
                cx,
            ));
        }
        let save_button = if self.saving {
            components::busy_button(
                "editor-save",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Primary,
                ButtonSize::Md,
                true,
            )
        } else {
            components::button(
                "editor-save",
                t(k::PROVIDER_EDITOR_ACTION_SAVE),
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(cx.listener(|this, _event, _window, cx| this.do_save(cx)))
        };
        let actions = actions
            .child(
                components::button(
                    "editor-convert",
                    t(k::PROVIDER_EDITOR_ACTION_CONVERT),
                    ButtonTone::Neutral,
                    ButtonSize::Md,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_convert(cx);
                })),
            )
            .child(save_button)
            .child(
                components::button(
                    "editor-cancel",
                    t(k::PROVIDER_EDITOR_ACTION_CANCEL),
                    ButtonTone::Neutral,
                    ButtonSize::Md,
                )
                .on_click(cx.listener(|_t, _e, _w, cx| cx.emit(EditorEvent::Cancelled))),
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

        editor_screen::provider_editor_page(editor_screen::ProviderEditorPage {
            title,
            subtitle,
            actions: actions.into_any_element(),
            form_scroll: form_scroll.into_any_element(),
            preview,
            // The form is the editor page's primary scroll context. Its rail
            // belongs to the full-width page chrome rather than the split
            // boundary between the form and the independent file preview.
            form_scrollbar: Some(
                crate::scrollbar::VerticalScrollbar::new(
                    "editor-form-scrollbar",
                    self.form_list_state.clone(),
                )
                .into_any_element(),
            ),
            modal,
            convert_modal,
            compact_layout,
            stack_grid,
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

fn build_preview_cache(
    codec: &dyn AppConfig,
    values: &FormValues,
    working_base: &Value,
    category: Option<&str>,
    collapsed_regions: &HashSet<(usize, usize)>,
) -> PreviewBuild {
    let started = Instant::now();
    let mut issues = codec.validate_for_category(values, category);
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
        .map(|(file_index, file)| {
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
    use super::{preview_line_ranges, preview_visible_rows};
    use crate::fold::FoldRegion;
    use std::collections::HashSet;

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
