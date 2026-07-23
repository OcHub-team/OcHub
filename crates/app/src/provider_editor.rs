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
    div, prelude::*, px, uniform_list, Context, Entity, FontWeight, HighlightStyle, MouseButton,
    SharedString, StyledText, Window,
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
use crate::icons::IconName;
use crate::layout;
use crate::text_input::{TextInput, TextInputEvent};
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
const EDITOR_SPLIT_MIN_WINDOW_WIDTH: f32 = 1500.;
const EDITOR_STACK_GRID_MAX_WINDOW_WIDTH: f32 = 1050.;

pub struct ProviderEditor {
    app: Arc<AppState>,
    app_type: AppType,
    codec: Box<dyn AppConfig>,
    schema: Vec<FormSection>,
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
    show_preview: bool,
    /// Collapsed fold regions in the preview pane: (file index, header line).
    preview_collapsed: HashSet<(usize, usize)>,
    /// Only the selected file is mounted; this keeps multi-file previews from
    /// stacking several independent documents into one enormous page.
    preview_active_file: usize,
    preview_cache: PreviewCache,
    preview_dirty: bool,
    preview_refresh_epoch: usize,
    /// When `Some`, a modal code editor for one preview file is open.
    raw_edit: Option<RawEdit>,
    common_config_supported: bool,
    common_config_enabled: bool,
    common_snippet: Entity<TextInput>,
    original_snippet: String,
    convert_open: bool,
    error: Option<SharedString>,
    status: Option<SharedString>,
}

impl ProviderEditor {
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
        let common_snippet = cx.new(|cx| {
            let mut input = TextInput::new(cx, "输入共享配置片段")
                .code(true)
                .multiline(true);
            input.set_content(snippet_seed, cx);
            input
        });
        Self {
            app,
            app_type,
            codec,
            schema,
            values,
            working_base,
            original_id,
            original_provider,
            provider_id: cx.new(|cx| TextInput::new(cx, "可选供应商 ID")),
            name: cx.new(|cx| TextInput::new(cx, "供应商名称")),
            website_url: cx.new(|cx| TextInput::new(cx, "https://example.com")),
            category: cx.new(|cx| TextInput::new(cx, "aggregator / official / third_party")),
            notes: cx.new(|cx| TextInput::new(cx, "备注").multiline(true)),
            text_inputs: HashMap::new(),
            kv_rows: HashMap::new(),
            grid_rows: HashMap::new(),
            next_row_id: 0,
            selected_preset: None,
            show_preview: true,
            preview_collapsed: HashSet::new(),
            preview_active_file: 0,
            preview_cache: PreviewCache::default(),
            preview_dirty: true,
            preview_refresh_epoch: 0,
            raw_edit: None,
            common_config_supported,
            common_config_enabled,
            common_snippet,
            original_snippet,
            convert_open: false,
            error: None,
            status: None,
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
        self.preview_refresh_epoch = self.preview_refresh_epoch.wrapping_add(1);
        let epoch = self.preview_refresh_epoch;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_REFRESH_DELAY).await;
            this.update(cx, |this, cx| {
                if this.preview_refresh_epoch == epoch && this.preview_dirty {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn invalidate_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_dirty = true;
        self.preview_refresh_epoch = self.preview_refresh_epoch.wrapping_add(1);
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
                self.status = Some(SharedString::from("已应用文件编辑"));
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

    // ---- mutation handlers --------------------------------------------------

    fn set_select(&mut self, field_id: String, value: String, cx: &mut Context<Self>) {
        self.values.insert(field_id, Value::String(value));
        self.invalidate_preview(cx);
    }

    fn toggle_bool(&mut self, field_id: String, cx: &mut Context<Self>) {
        let cur = bool_val(&self.values, &field_id);
        self.values.insert(field_id, Value::Bool(!cur));
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
        self.invalidate_preview(cx);
    }

    fn kv_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.kv_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
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
        self.invalidate_preview(cx);
    }

    fn grid_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.grid_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
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
        for section in &self.schema {
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
            self.error = Some(SharedString::from("名称不能为空"));
            cx.notify();
            return;
        }
        self.pull_values(cx);
        let category = nonempty(self.category.read(cx).content().trim().to_string());

        let issues = self
            .codec
            .validate_for_category(&self.values, category.as_deref());
        if let Some(err) = issues.iter().find(|i| i.severity == Severity::Error) {
            self.error = Some(SharedString::from(format!("配置无效：{}", err.message)));
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
                    self.error = Some(SharedString::from(format!("通用配置片段无效: {err}")));
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
                self.error = Some(SharedString::from(format!("保存失败: {err}")));
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
            self.error = Some(SharedString::from("拉取模型需要基础 URL 和 API 密钥"));
            cx.notify();
            return;
        }
        self.error = None;
        self.status = Some(SharedString::from("正在拉取模型列表..."));
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
                        this.status = Some(SharedString::from(if preview.is_empty() {
                            "没有返回模型".to_string()
                        } else {
                            format!("拉取到 {} 个模型：{preview}", models.len())
                        }));
                    }
                    Err(err) => {
                        this.error = Some(SharedString::from(format!("拉取模型失败: {err}")));
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
            self.error = Some(SharedString::from("测速需要基础 URL"));
            cx.notify();
            return;
        }
        self.error = None;
        self.status = Some(SharedString::from("正在测试端点延迟..."));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                ochub_core::services::SpeedtestService::test_endpoints(vec![base_url], Some(8))
                    .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(results) => {
                        let msg = results
                            .first()
                            .map(|item| {
                                if let Some(err) = &item.error {
                                    format!("测速失败：{err}")
                                } else {
                                    format!(
                                        "测速成功：HTTP {}，{} ms",
                                        item.status
                                            .map(|s| s.to_string())
                                            .unwrap_or_else(|| "未知".to_string()),
                                        item.latency
                                            .map(|v| v.to_string())
                                            .unwrap_or_else(|| "未知".to_string())
                                    )
                                }
                            })
                            .unwrap_or_else(|| "没有测速结果".to_string());
                        this.status = Some(SharedString::from(msg));
                    }
                    Err(err) => {
                        this.error = Some(SharedString::from(format!("测速失败: {err}")));
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
            self.error = Some(SharedString::from("查询余额需要基础 URL 和 API 密钥"));
            cx.notify();
            return;
        }
        self.error = None;
        self.status = Some(SharedString::from("正在查询余额..."));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = ochub_core::services::balance::get_balance(&base_url, &api_key).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(result) => {
                        this.status = Some(SharedString::from(format_usage_result(&result)))
                    }
                    Err(err) => {
                        this.error = Some(SharedString::from(format!("余额查询失败: {err}")));
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
                self.status = Some(SharedString::from("已从当前配置提取通用配置片段"));
                self.error = None;
            }
            Err(err) => {
                self.error = Some(SharedString::from(format!("提取失败: {err}")));
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
            format!("来自 {source_label} 的配置")
        } else {
            format!("{base_name}（来自 {source_label}）")
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
                self.status = Some(SharedString::from(format!(
                    "已复制到 {}",
                    crate::app_meta::label(target)
                )));
                self.error = None;
            }
            Err(err) => {
                self.error = Some(SharedString::from(format!("复制失败: {err}")));
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
            .child(layout::section_header("登录与鉴权", "官方供应商"))
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
                            .child(SharedString::from(format!("使用 {app_label} 官方登录"))),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_xs()
                            .child("沿用工具自身的登录状态，不写入第三方 Base URL 或 API Key。"),
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
                let fid = field.id.clone();
                let on_select = cx.listener(move |this, ix: &usize, _w, cx| {
                    if let Some(value) = values.get(*ix).cloned() {
                        this.set_select(fid.clone(), value, cx);
                    }
                });
                let mut control = div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(components::segmented(
                        SharedString::from(format!("select-{}", field.id)),
                        &labels,
                        selected,
                        move |ix, window, cx| on_select(&ix, window, cx),
                    ));
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
                                    "删除",
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
                "+ 添加",
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
                                "删除",
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
                    "+ 添加模型",
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
                            "删除",
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
                "+ 添加模型",
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

    fn render_preview(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = components::card()
            .p_0()
            .min_h_0()
            .flex_none()
            .overflow_hidden()
            .when(compact, |pane| pane.w_full().h(px(320.)))
            .when(!compact, |pane| pane.w(px(420.)).h_full())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .min_w_0()
                            .text_color(theme::text())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("将写入的文件"),
                    )
                    .child(
                        components::button(
                            "editor-refresh-preview",
                            "刷新",
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.invalidate_preview(cx);
                            },
                        )),
                    ),
            );

        if self.preview_cache.files.is_empty() {
            return pane
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(theme::muted())
                        .text_sm()
                        .child("没有可预览的文件"),
                )
                .into_any_element();
        }

        if self.preview_cache.files.len() > 1 {
            let mut tabs = div()
                .id("preview-file-tabs")
                .flex()
                .flex_row()
                .flex_none()
                .gap_1()
                .px_3()
                .py_2()
                .overflow_x_scroll()
                .border_b_1()
                .border_color(theme::border());
            tabs.style().restrict_scroll_to_axis = Some(true);
            for (index, document) in self.preview_cache.files.iter().enumerate() {
                tabs = tabs.child(
                    components::button(
                        SharedString::from(format!("preview-tab-{index}")),
                        document.filename.clone(),
                        if index == self.preview_active_file {
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
            pane = pane.child(tabs);
        }

        let file_index = self
            .preview_active_file
            .min(self.preview_cache.files.len().saturating_sub(1));
        let document = self.preview_cache.files[file_index].clone();
        let visible_count = document.visible_rows.len();
        let metadata = SharedString::from(format!(
            "{} 行 · {}",
            document.line_count(),
            format_preview_bytes(document.content.len())
        ));

        pane = pane.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_4()
                .py_3()
                .flex_none()
                .border_b_1()
                .border_color(theme::border())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::subtext())
                        .text_xs()
                        .font_family("Menlo")
                        .child(document.filename.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::muted())
                        .text_xs()
                        .child(metadata),
                )
                .child(components::badge(
                    BadgeTone::Neutral,
                    document.language_label,
                ))
                .child(
                    components::button(
                        SharedString::from(format!("preview-edit-{file_index}")),
                        "编辑",
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_raw_edit(file_index, cx);
                    })),
                ),
        );

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

        pane = pane.child(
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
        );

        if !self.preview_cache.issues.is_empty() {
            let mut issues = div()
                .id("preview-issues")
                .flex()
                .flex_col()
                .flex_none()
                .max_h(px(120.))
                .overflow_y_scroll()
                .gap_1()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme::border());
            for issue in &self.preview_cache.issues {
                let (color, tag) = match issue.severity {
                    Severity::Error => (theme::red(), "错误"),
                    Severity::Warning => (theme::yellow(), "警告"),
                    Severity::Info => (theme::subtext(), "提示"),
                };
                issues = issues.child(
                    div()
                        .flex()
                        .flex_row()
                        .min_w_0()
                        .gap_2()
                        .text_xs()
                        .child(div().text_color(color).flex_none().child(tag))
                        .child(
                            div()
                                .min_w_0()
                                .text_color(theme::subtext())
                                .child(SharedString::from(issue.message.clone())),
                        ),
                );
            }
            pane = pane.child(issues);
        }

        pane.into_any_element()
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
                        "关闭",
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
                            .child("直接编辑文件内容，应用后会同步回上方表单。"),
                    )
                    .when_some(raw.error.clone(), |s, err| {
                        s.child(div().text_color(theme::red()).text_xs().child(err))
                    })
                    .child(
                        div()
                            .id("raw-editor-scroll")
                            .flex()
                            .flex_1()
                            .min_h(px(0.))
                            .w_full()
                            .overflow_y_scroll()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .child(raw.input.clone()),
                    ),
            )
            .child(components::modal_footer(vec![
                components::button("raw-cancel", "取消", ButtonTone::Neutral, ButtonSize::Sm)
                    .on_click(cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx)))
                    .into_any_element(),
                components::button("raw-apply", "应用", ButtonTone::Primary, ButtonSize::Sm)
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
            .child(layout::section_header("通用配置", "跨供应商共享片段"))
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
                                    .child("应用通用配置到此供应商"),
                            )
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child("写入工具配置时会合并下方共享片段。"),
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
                            .child("此片段按应用共享，供所有已启用的供应商使用。"),
                    )
                    .child(
                        components::button(
                            "common-config-extract",
                            "从当前配置提取",
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
            .child(
                div()
                    .id("common-config-editor")
                    .flex()
                    .w_full()
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::border())
                    .child(self.common_snippet.clone()),
            )
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
                    .child(components::modal_header("复制到其他应用"))
                    .child(
                        components::modal_body()
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .text_xs()
                                    .child("使用目标应用的格式重新生成，并另存为新供应商。"),
                            )
                            .child(targets),
                    )
                    .child(components::modal_footer(vec![components::button(
                        "convert-cancel",
                        "取消",
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
            .child(components::field("名称", true, None, self.name.clone()))
            .when(!self.is_editing(), |s| {
                s.child(components::field(
                    "供应商 ID（可选）",
                    false,
                    None,
                    self.provider_id.clone(),
                ))
            })
            .child(components::field(
                "网站 URL",
                false,
                None,
                self.website_url.clone(),
            ))
            .child(components::field(
                "分类",
                false,
                None,
                self.category.clone(),
            ))
            .child(components::field("备注", false, None, self.notes.clone()))
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

        let title = if self.is_editing() {
            "编辑供应商"
        } else {
            "新增供应商"
        };
        let subtitle = SharedString::from(format!(
            "目标应用：{}",
            crate::app_meta::label(self.app_type)
        ));

        let identity = self.render_identity().into_any_element();
        let presets = self.codec.presets();
        let preset_picker = if presets.is_empty() {
            None
        } else {
            let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
            let on_select = cx.listener(|this, ix: &usize, _w, cx| this.apply_preset(*ix, cx));
            Some(
                components::field(
                    "从预设开始",
                    false,
                    None,
                    components::segmented(
                        "editor-presets",
                        &names,
                        self.selected_preset.unwrap_or(usize::MAX),
                        move |ix, window, cx| on_select(&ix, window, cx),
                    ),
                )
                .into_any_element(),
            )
        };
        let sections: Vec<gpui::AnyElement> = self
            .schema
            .clone()
            .into_iter()
            .map(|section| {
                if official_login && section.title == "端点与鉴权" {
                    return self.render_official_auth_section();
                }
                let caption = if section.advanced { "高级选项" } else { "" };
                let mut col = div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w_full()
                    .child(layout::section_header(section.title.clone(), caption));
                for field in &section.fields {
                    if field.is_visible(&self.values) {
                        col = col.child(self.render_field(field, stack_grid, cx));
                    }
                }
                col.into_any_element()
            })
            .collect();
        let preview = if self.show_preview {
            Some(self.render_preview(compact_layout, cx).into_any_element())
        } else {
            None
        };
        let common_config = self
            .common_config_supported
            .then(|| self.render_common_config(cx));
        let modal = self.render_raw_modal(cx);
        let convert_modal = self.render_convert_modal(cx);

        let actions = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                components::button(
                    "editor-convert",
                    "复制到应用",
                    ButtonTone::Neutral,
                    ButtonSize::Md,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.open_convert(cx);
                })),
            )
            .child(
                components::button("editor-save", "保存", ButtonTone::Primary, ButtonSize::Md)
                    .on_click(cx.listener(|this, _e, _w, cx| this.do_save(cx))),
            )
            .child(
                components::button("editor-cancel", "取消", ButtonTone::Neutral, ButtonSize::Md)
                    .on_click(cx.listener(|_t, _e, _w, cx| cx.emit(EditorEvent::Cancelled))),
            );

        let form_column = div()
            .flex()
            .flex_col()
            .gap_5()
            .flex_1()
            .min_w_0()
            .when_some(preset_picker, |s, picker| s.child(picker))
            .child(identity)
            .children(sections)
            .when_some(common_config, |form, common| form.child(common))
            .when(!official_login, |form| {
                form.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            components::button(
                                "editor-fetch-models",
                                "拉取模型",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _e, _w, cx| this.fetch_models(cx))),
                        )
                        .child(
                            components::button(
                                "editor-speedtest",
                                "测试 URL",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _e, _w, cx| this.speedtest_base_url(cx))),
                        )
                        .child(
                            components::button(
                                "editor-balance",
                                "查询余额",
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(cx.listener(|this, _e, _w, cx| this.query_balance(cx))),
                        ),
                )
            });

        let form_scroll = div()
            .id("editor-form-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .pr_2()
            .overflow_y_scroll()
            .child(form_column.pb_6());

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
            );

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

fn format_usage_result(result: &UsageResult) -> String {
    if !result.success {
        return result
            .error
            .as_ref()
            .map(|err| format!("余额查询失败：{err}"))
            .unwrap_or_else(|| "余额查询失败".to_string());
    }
    let Some(data) = &result.data else {
        return "余额查询成功，但没有返回额度数据".to_string();
    };
    let parts = data
        .iter()
        .take(3)
        .map(|item| {
            let name = item.plan_name.as_deref().unwrap_or("默认计划");
            let remaining = item
                .remaining
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "未知".to_string());
            let unit = item.unit.as_deref().unwrap_or("");
            format!("{name}: 剩余 {remaining}{unit}")
        })
        .collect::<Vec<_>>()
        .join("；");
    if parts.is_empty() {
        "余额查询成功，但没有返回额度数据".to_string()
    } else {
        parts
    }
}

impl crate::notifications::ToastSource for ProviderEditor {
    fn take_toast(&mut self) -> Option<SharedString> {
        self.error.take().or_else(|| self.status.take())
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
