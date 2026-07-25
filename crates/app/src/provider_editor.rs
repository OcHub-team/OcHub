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
    div, prelude::*, px, relative, uniform_list, Context, Entity, FontWeight, HighlightStyle,
    ListAlignment, ListState, MouseButton, Pixels, SharedString, StyledText, Task, Window,
};
use ochub_core::provider_config::{
    self, bool_val, str_val, AppConfig, ConfigIssue, FieldKind, FormField, FormSection, FormValues,
    GridCellKind, Language, Severity,
};
use ochub_core::services::provider::ProviderService;
use ochub_core::services::ConfigService;
use ochub_core::{AppState, AppType, Provider, ProviderMeta, UsageResult};
use serde_json::{json, Map, Value};

use crate::code_editor::CodeEditor;
use crate::components;
use crate::components::{BadgeTone, ButtonSize, ButtonTone};
use crate::fold::{fold_regions, FoldRegion};
use crate::highlight::{self, Lang};
use crate::i18n::{k, raw, t};
use crate::icons::IconName;
use crate::layout;
use crate::notifications::NotificationLevel;
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
}

const PREVIEW_REFRESH_DELAY: Duration = Duration::from_millis(140);
const EDITOR_MAX_WIDTH: f32 = 1320.;
/// Side-by-side form + preview. 1500 left almost every real window in the
/// cramped stacked mode; a 13" laptop should get the split.
const EDITOR_SPLIT_MIN_WINDOW_WIDTH: f32 = 1200.;
const EDITOR_STACK_GRID_MAX_WINDOW_WIDTH: f32 = 1050.;
/// Preview pane share of the split row, bounded so it neither starves the
/// form on narrow windows nor balloons on wide ones.
const PREVIEW_SPLIT_FRACTION: f32 = 0.38;
const PREVIEW_SPLIT_MIN_WIDTH: f32 = 400.;
const PREVIEW_SPLIT_MAX_WIDTH: f32 = 560.;

pub struct ProviderEditor {
    app: Arc<AppState>,
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
    preview_refresh_task: Option<Task<()>>,
    /// When `Some`, a modal code editor for one preview file is open.
    raw_edit: Option<RawEdit>,
    common_config_supported: bool,
    common_config_enabled: bool,
    common_snippet: Entity<TextInput>,
    original_snippet: String,
    convert_open: bool,
    error: Option<SharedString>,
    status: Option<SharedString>,
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
        if self.raw_edit.is_some() {
            self.apply_raw_edit(cx);
        } else {
            self.do_save(cx);
        }
    }

    pub(crate) fn shortcut_cancel(&mut self, cx: &mut Context<Self>) {
        if self.convert_open {
            self.close_convert(cx);
        } else if self.raw_edit.is_some() {
            self.close_raw_edit(cx);
        } else if self.open_select_field.take().is_some() {
            cx.notify();
        } else {
            cx.emit(EditorEvent::Cancelled);
        }
    }

    pub fn new_add(app: Arc<AppState>, app_type: AppType, cx: &mut Context<Self>) -> Self {
        let codec = provider_config::config_for(app_type)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let schema = codec.schema();
        let values = codec.decode(&Value::Null, None);
        let mut this = Self::base(app, app_type, codec, schema, values, None, None, cx);
        Self::observe_preview_input(&this.category, cx);
        this.build_inputs(cx);
        this
    }

    pub fn new_edit(
        app: Arc<AppState>,
        app_type: AppType,
        provider: &Provider,
        cx: &mut Context<Self>,
    ) -> Self {
        let codec = provider_config::config_for(app_type)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let schema = codec.schema();
        let values = codec.decode(&provider.settings_config, provider.meta.as_ref());
        let mut this = Self::base(
            app,
            app_type,
            codec,
            schema,
            values,
            Some(provider.id.clone()),
            Some(provider.clone()),
            cx,
        );
        this.set_identity(provider, cx);
        Self::observe_preview_input(&this.category, cx);
        this.build_inputs(cx);
        this
    }

    #[allow(clippy::too_many_arguments)]
    fn base(
        app: Arc<AppState>,
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
        let common_config_enabled = original_provider
            .as_ref()
            .and_then(|provider| provider.meta.as_ref())
            .and_then(|meta| meta.common_config_enabled)
            .unwrap_or(false);
        let original_snippet = if common_config_supported {
            ConfigService::get_common_config_snippet(&app, app_type.as_str())
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            String::new()
        };
        let snippet_seed = original_snippet.clone();
        let form_item_count = schema.len() + 3;
        let common_snippet = cx.new(|cx| {
            let mut input =
                TextInput::new(cx, t(k::PROVIDER_EDITOR_COMMON_CONFIG_SNIPPET_PLACEHOLDER))
                    .code(true)
                    .multiline(true);
            input.set_content(snippet_seed, cx);
            input
        });
        Self {
            app,
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
            open_select_field: None,
            show_preview: true,
            preview_expanded: false,
            preview_collapsed: HashSet::new(),
            preview_active_file: 0,
            preview_cache: PreviewCache::default(),
            preview_dirty: true,
            preview_refresh_task: None,
            raw_edit: None,
            common_config_supported,
            common_config_enabled,
            common_snippet,
            original_snippet,
            convert_open: false,
            error: None,
            status: None,
            status_level: None,
            form_list_state: ListState::new(form_item_count, ListAlignment::Top, px(720.)),
            form_stack_grid: false,
            form_official_login: false,
        }
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
            this.schedule_preview_refresh(cx);
        })
        .detach();
    }

    /// Text fields can emit several changes in one typing burst. Keep the form
    /// responsive immediately and rebuild the potentially multi-megabyte native
    /// document only after the user pauses briefly.
    fn schedule_preview_refresh(&mut self, cx: &mut Context<Self>) {
        self.preview_dirty = true;
        self.preview_refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_REFRESH_DELAY).await;
            this.update(cx, |this, cx| {
                if this.preview_dirty {
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn invalidate_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_dirty = true;
        self.preview_refresh_task.take();
        cx.notify();
    }

    fn ensure_preview_current(&mut self, cx: &Context<Self>) {
        if !self.preview_dirty {
            return;
        }
        self.pull_values(cx);
        let category = self.category.read(cx).content().trim().to_string();
        self.rebuild_preview_cache((!category.is_empty()).then_some(category.as_str()));
    }

    fn rebuild_preview_cache(&mut self, category: Option<&str>) {
        let started = Instant::now();
        let issues = self.codec.validate_for_category(&self.values, category);
        let files = self.codec.preview(&self.values, &self.working_base);
        let mut total_bytes = 0usize;
        let mut total_lines = 0usize;
        let documents = files
            .into_iter()
            .enumerate()
            .map(|(file_index, file)| {
                let lang = Lang::from_core(file.language);
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
                let collapsed = self
                    .preview_collapsed
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

        self.preview_cache = PreviewCache {
            files: documents,
            issues,
        };
        self.preview_active_file = self
            .preview_active_file
            .min(self.preview_cache.files.len().saturating_sub(1));
        self.preview_dirty = false;
        log::debug!(
            "provider preview cache rebuilt: {} files, {} lines, {} bytes in {:?}",
            self.preview_cache.files.len(),
            total_lines,
            total_bytes,
            started.elapsed()
        );
    }

    /// Apply a built-in preset: replace values and rebuild inputs.
    fn apply_preset(&mut self, index: usize, cx: &mut Context<Self>) {
        let presets = self.codec.presets();
        if let Some(preset) = presets.into_iter().nth(index) {
            self.values = preset.values;
            self.text_inputs.clear();
            self.kv_rows.clear();
            self.grid_rows.clear();
            self.build_inputs(cx);
            self.selected_preset = Some(index);
            self.open_select_field = None;
            self.invalidate_preview(cx);
        }
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
        if let Some(rows) = self.kv_rows.get(&field_id) {
            if let Some(row) = rows.last() {
                Self::observe_preview_input(&row.key, cx);
                Self::observe_preview_input(&row.value, cx);
            }
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
        if let Some(rows) = self.grid_rows.get(&field_id) {
            if let Some(row) = rows.last() {
                for input in row.cells.values() {
                    Self::observe_preview_input(input, cx);
                }
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
        if let Some(rows) = self.grid_rows.get_mut(&field_id) {
            if let Some(row) = rows.iter_mut().find(|r| r.id == row_id) {
                let cur = row.toggles.get(&col).copied().unwrap_or(false);
                row.toggles.insert(col, !cur);
            }
        }
        self.invalidate_preview(cx);
    }

    fn columns_for(&self, field_id: &str) -> Vec<provider_config::GridColumn> {
        for section in self.schema.iter() {
            for field in &section.fields {
                if field.id == field_id {
                    if let FieldKind::ModelGrid { columns } = &field.kind {
                        return columns.clone();
                    }
                }
            }
        }
        Vec::new()
    }

    fn do_save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).content().trim().to_string();
        if name.is_empty() {
            self.set_error(t(k::PROVIDER_EDITOR_IDENTITY_NAME_REQUIRED));
            cx.notify();
            return;
        }
        self.pull_values(cx);
        let category = nonempty(self.category.read(cx).content().trim().to_string());

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

        if self.common_config_supported {
            let snippet = self.common_snippet.read(cx).content().to_string();
            if snippet != self.original_snippet {
                if let Err(err) = ConfigService::set_common_config_snippet(
                    &self.app,
                    self.app_type.as_str(),
                    snippet.clone(),
                ) {
                    self.set_error(tf!(k::PROVIDER_EDITOR_COMMON_CONFIG_INVALID, error = err));
                    cx.notify();
                    return;
                }
                self.original_snippet = snippet;
            }
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

        let result = if let Some(original_id) = self.original_id.clone() {
            let mut provider = self.original_provider.clone().unwrap_or_else(|| {
                Provider::with_id(original_id.clone(), name.clone(), json!({}), None)
            });
            provider.name = name;
            provider.settings_config = encoded.settings_config;
            provider.meta = encoded.meta;
            provider.website_url = website_url;
            provider.category = category;
            provider.notes = notes;
            ProviderService::update(&self.app, self.app_type, Some(&original_id), provider)
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
            ProviderService::add(&self.app, self.app_type, provider, true)
        };

        match result {
            Ok(_) => cx.emit(EditorEvent::Saved),
            Err(err) => {
                self.set_error(tf!(k::PROVIDER_EDITOR_SAVE_FAILED, error = err));
                cx.notify();
            }
        }
    }

    // ---- helper actions (operate on `base_url` / `api_key` fields) -----------

    fn field_text(&self, id: &str, cx: &Context<Self>) -> String {
        self.text_inputs
            .get(id)
            .map(|i| i.read(cx).content().trim().to_string())
            .unwrap_or_default()
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) {
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
            let result = ochub_core::services::model_fetch::fetch_models(
                &base_url, &api_key, false, None, None,
            )
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(models) => {
                        let preview = models
                            .iter()
                            .take(6)
                            .map(|m| m.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        // An empty list is a successful call with nothing to
                        // show, not a failure and not a plain progress note.
                        if preview.is_empty() {
                            this.set_status(
                                NotificationLevel::Warning,
                                t(k::PROVIDER_EDITOR_MODELS_NONE),
                            );
                        } else {
                            this.set_status(
                                NotificationLevel::Success,
                                tf!(
                                    k::PROVIDER_EDITOR_MODELS_FETCHED,
                                    count = models.len(),
                                    models = preview
                                ),
                            );
                        }
                    }
                    Err(err) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_MODELS_FETCH_FAILED, error = err));
                        this.status = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn speedtest_base_url(&mut self, cx: &mut Context<Self>) {
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
            let result =
                ochub_core::services::SpeedtestService::test_endpoints(vec![base_url], Some(8))
                    .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(results) => {
                        let (level, msg) = results
                            .first()
                            .map(|item| {
                                if let Some(err) = &item.error {
                                    (
                                        NotificationLevel::Error,
                                        tf!(k::PROVIDER_EDITOR_SPEEDTEST_FAILED, error = err),
                                    )
                                } else {
                                    let unknown = raw(k::PROVIDER_EDITOR_COMMON_UNKNOWN);
                                    (
                                        NotificationLevel::Success,
                                        tf!(
                                            k::PROVIDER_EDITOR_SPEEDTEST_OK,
                                            status = item
                                                .status
                                                .map(|s| s.to_string())
                                                .unwrap_or_else(|| unknown.to_string()),
                                            latency = item
                                                .latency
                                                .map(|v| v.to_string())
                                                .unwrap_or_else(|| unknown.to_string())
                                        ),
                                    )
                                }
                            })
                            // The call came back with nothing to report: a
                            // caveat, not a failed request.
                            .unwrap_or_else(|| {
                                (
                                    NotificationLevel::Warning,
                                    raw(k::PROVIDER_EDITOR_SPEEDTEST_NO_RESULT).to_string(),
                                )
                            });
                        this.set_status(level, msg);
                    }
                    Err(err) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_SPEEDTEST_FAILED, error = err));
                        this.status = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn query_balance(&mut self, cx: &mut Context<Self>) {
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
            let result = ochub_core::services::balance::get_balance(&base_url, &api_key).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(result) => {
                        // The call succeeded; the payload decides whether that
                        // is a balance, an empty quota, or a reported failure.
                        let (level, text) = format_usage_result(&result);
                        this.set_status(level, text);
                    }
                    Err(err) => {
                        this.set_error(tf!(k::PROVIDER_EDITOR_BALANCE_FAILED, error = err));
                        this.status = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_common_config(&mut self, cx: &mut Context<Self>) {
        self.common_config_enabled = !self.common_config_enabled;
        cx.notify();
    }

    fn extract_common_config(&mut self, cx: &mut Context<Self>) {
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
        match ProviderService::add(&self.app, target, provider, false) {
            Ok(_) => {
                self.convert_open = false;
                self.set_status(
                    NotificationLevel::Success,
                    tf!(
                        k::PROVIDER_EDITOR_CONVERT_COPIED,
                        app = crate::app_meta::label(target)
                    ),
                );
                self.error = None;
            }
            Err(err) => {
                self.set_error(tf!(k::PROVIDER_EDITOR_CONVERT_FAILED, error = err));
                self.status = None;
            }
        }
        cx.notify();
    }

    // ---- rendering ----------------------------------------------------------

    fn uses_official_login(&self, cx: &Context<Self>) -> bool {
        matches!(
            self.app_type,
            AppType::Claude | AppType::ClaudeDesktop | AppType::Codex
        ) && self.category.read(cx).content().trim() == "official"
    }

    fn render_official_auth_section(&self) -> gpui::AnyElement {
        let app_label = crate::app_meta::label(self.app_type);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(layout::section_header(
                t(k::PROVIDER_EDITOR_OFFICIAL_SECTION_TITLE),
                t(k::PROVIDER_EDITOR_OFFICIAL_SECTION_CAPTION),
            ))
            .child(
                components::card()
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
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(t(k::PROVIDER_EDITOR_OFFICIAL_DESC)),
                    ),
            )
            .into_any_element()
    }

    fn render_field(
        &self,
        field: &FormField,
        stack_grid: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let body = match &field.kind {
            FieldKind::Text { .. } | FieldKind::Secret { .. } => self
                .text_inputs
                .get(&field.id)
                .map(|i| i.clone().into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            FieldKind::Select { options } => {
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
                let fid = field.id.clone();
                div()
                    .id(SharedString::from(format!("tog-{}", field.id)))
                    .cursor_pointer()
                    .child(layout::toggle(on))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.toggle_bool(fid.clone(), cx);
                    }))
                    .into_any_element()
            }
            FieldKind::KeyValue { .. } => self.render_kv(&field.id, cx).into_any_element(),
            FieldKind::ModelGrid { columns } => self
                .render_grid(&field.id, columns, stack_grid, cx)
                .into_any_element(),
        };

        components::field(
            field.label.clone(),
            field.required,
            field.help.clone().map(SharedString::from),
            body,
        )
        .into_any_element()
    }

    fn render_kv(&self, field_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
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
    ) -> impl IntoElement {
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

        if let Some(document) = self.preview_cache.files.get_mut(file_index) {
            let collapsed = self
                .preview_collapsed
                .iter()
                .filter_map(|(index, header)| (*index == file_index).then_some(*header))
                .collect::<HashSet<_>>();
            document.visible_rows = Arc::new(preview_visible_rows(
                document.line_count(),
                &document.regions,
                &collapsed,
            ));
        }
        cx.notify();
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
    ) -> impl IntoElement {
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
        let mut sorted: Vec<&ConfigIssue> = self.preview_cache.issues.iter().collect();
        sorted.sort_by_key(|issue| match issue.severity {
            Severity::Error => 0u8,
            Severity::Warning => 1,
            Severity::Info => 2,
        });
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
                .children(sorted.into_iter().map(|issue| {
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
    fn render_preview_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        components::card()
            .p_0()
            .flex_none()
            .overflow_hidden()
            .child(
                div()
                    .id("preview-summary-expand")
                    .role(gpui::Role::Button)
                    .aria_label(t(k::PROVIDER_EDITOR_PREVIEW_EXPAND_ARIA))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.preview_expanded = true;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::muted())
                            .text_xs()
                            .child("▸"),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(k::PROVIDER_EDITOR_PREVIEW_TITLE)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::muted())
                            .text_xs()
                            .font_family("Menlo")
                            .child(files),
                    )
                    .when(errors > 0, |row| {
                        row.child(components::badge(
                            BadgeTone::Danger,
                            tf!(k::PROVIDER_EDITOR_ISSUE_ERROR_COUNT, count = errors),
                        ))
                    })
                    .when(warnings > 0, |row| {
                        row.child(components::badge(
                            BadgeTone::Warning,
                            tf!(k::PROVIDER_EDITOR_ISSUE_WARNING_COUNT, count = warnings),
                        ))
                    }),
            )
    }

    fn preview_issue_counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for issue in &self.preview_cache.issues {
            match issue.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => {}
            }
        }
        (errors, warnings)
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
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(t(k::PROVIDER_EDITOR_RAW_DESC)),
                    )
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
                t(k::PROVIDER_EDITOR_COMMON_CONFIG_SECTION_CAPTION),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .text_sm()
                                    .child(t(k::PROVIDER_EDITOR_COMMON_CONFIG_TOGGLE_LABEL)),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(t(k::PROVIDER_EDITOR_COMMON_CONFIG_TOGGLE_DESC)),
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
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child(t(k::PROVIDER_EDITOR_COMMON_CONFIG_SHARED_DESC)),
                    )
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
                    .child(
                        components::modal_body()
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child(t(k::PROVIDER_EDITOR_CONVERT_DESC)),
                            )
                            .child(targets),
                    )
                    .child(components::modal_footer(vec![components::button(
                        "convert-cancel",
                        t(k::PROVIDER_EDITOR_CONVERT_CANCEL),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.close_convert(cx);
                    }))
                    .into_any_element()])),
            )
            .into_any_element(),
        )
    }

    fn render_identity(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
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

    fn render_form_intro(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let presets = self.codec.presets();
        let mut intro = div().flex().flex_col().gap_5().w_full().min_w_0();
        if !presets.is_empty() {
            let names: Vec<&str> = presets.iter().map(|preset| preset.name.as_str()).collect();
            let on_select = cx.listener(|this, index: &usize, _window, cx| {
                this.apply_preset(*index, cx);
            });
            intro = intro.child(components::field(
                t(k::PROVIDER_EDITOR_FORM_PRESETS_LABEL),
                false,
                None,
                components::segmented(
                    "editor-presets",
                    &names,
                    self.selected_preset.unwrap_or(usize::MAX),
                    move |index, window, cx| on_select(&index, window, cx),
                ),
            ));
        }
        intro.child(self.render_identity()).into_any_element()
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
        if official_login && section.title == "端点与鉴权" {
            return self.render_official_auth_section();
        }

        let caption = if section.advanced {
            raw(k::PROVIDER_EDITOR_FORM_ADVANCED_CAPTION)
        } else {
            ""
        };
        let mut column = div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(layout::section_header(section.title.clone(), caption));
        for field in &section.fields {
            if field.is_visible(&self.values) {
                column = column.child(self.render_field(field, stack_grid, cx));
            }
        }
        column.into_any_element()
    }

    fn render_form_tools(&self, official_login: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        if official_login {
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
        // The cached native document is rebuilt only when form values changed;
        // caret blinks and unrelated view updates reuse the existing line index.
        self.ensure_preview_current(cx);
        let window_width = window.viewport_size().width;
        let compact_layout = window_width < px(EDITOR_SPLIT_MIN_WINDOW_WIDTH);
        let stack_grid = window_width < px(EDITOR_STACK_GRID_MAX_WINDOW_WIDTH);
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
            .child(
                components::button(
                    "editor-save",
                    t(k::PROVIDER_EDITOR_ACTION_SAVE),
                    ButtonTone::Primary,
                    ButtonSize::Md,
                )
                .on_click(cx.listener(|this, _e, _w, cx| this.do_save(cx))),
            )
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

        let body = div()
            .flex()
            .items_stretch()
            .flex_1()
            .min_h_0()
            .gap_4()
            .w_full()
            .when(compact_layout, |body| body.flex_col())
            .when(!compact_layout, |body| body.flex_row())
            .child(form_scroll)
            .when_some(preview, |s, preview| s.child(preview));

        let editor_body = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .items_center()
            .when(stack_grid, |editor| editor.p_4())
            .when(!stack_grid, |editor| editor.p_6())
            .child(
                layout::wide_column()
                    .max_w(px(EDITOR_MAX_WIDTH))
                    .h_full()
                    .min_h_0()
                    .child(body),
            )
            // The form is the editor page's primary scroll context. Its rail
            // belongs to the full-width page chrome rather than the split
            // boundary between the form and the independent file preview.
            .child(crate::scrollbar::VerticalScrollbar::new(
                "editor-form-scrollbar",
                self.form_list_state.clone(),
            ));

        layout::page()
            .relative()
            .child(layout::page_header(title, Some(subtitle)).child(actions))
            .child(editor_body)
            .when_some(modal, |s, modal| s.child(modal))
            .when_some(convert_modal, |s, modal| s.child(modal))
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
