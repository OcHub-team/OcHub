//! Schema-driven provider add/edit form.
//!
//! Instead of one generic name/baseURL/key/model form, this renders whatever
//! [`routedeck_core::provider_config::AppConfig`] the selected app exposes: typed field
//! widgets (text / secret / select / toggle / key-value / model-grid) grouped in
//! sections, a live preview of the exact file(s) the app will receive, and a
//! validation strip. Saving encodes the edited values back into both
//! `settingsConfig` *and* `meta`.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, SharedString, Window};
use routedeck_core::provider_config::{
    self, bool_val, str_val, AppConfig, FieldKind, FormField, FormSection, FormValues,
    GridCellKind, Language, Severity,
};
use routedeck_core::services::provider::ProviderService;
use routedeck_core::services::ConfigService;
use routedeck_core::{AppState, AppType, Provider, ProviderMeta, UsageResult};
use serde_json::{json, Map, Value};

use crate::components;
use crate::layout;
use crate::text_input::TextInput;
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
    input: Entity<TextInput>,
    error: Option<SharedString>,
}

pub struct ProviderEditor {
    app: Arc<AppState>,
    app_type: AppType,
    codec: Box<dyn AppConfig>,
    schema: Vec<FormSection>,
    /// Working form values; text/kv/grid inputs are pulled into this on demand.
    values: FormValues,
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
    show_preview: bool,
    /// When `Some`, a modal code editor for one preview file is open.
    raw_edit: Option<RawEdit>,
    /// Whether this app has a common-config snippet concept (Claude / Codex /
    /// OpenCode / OpenClaw). Hermes / Claude Desktop do not.
    common_config_supported: bool,
    /// Per-provider toggle: apply the app-level common-config snippet on write.
    common_config_enabled: bool,
    /// App-level shared snippet editor (persisted via `ConfigService`).
    common_snippet: Entity<TextInput>,
    /// Snippet content loaded from the DB, to detect edits worth persisting.
    original_snippet: String,
    /// When `true`, the "copy to another app" picker is shown.
    convert_open: bool,
    error: Option<SharedString>,
    status: Option<SharedString>,
}

impl ProviderEditor {
    pub fn new_add(app: Arc<AppState>, app_type: AppType, cx: &mut Context<Self>) -> Self {
        let codec = provider_config::config_for(app_type)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let schema = codec.schema();
        let values = codec.decode(&Value::Null, None);
        let mut this = Self::base(app, app_type, codec, schema, values, None, None, cx);
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
        let common_config_supported = common_config_supported(app_type);
        let common_config_enabled = original_provider
            .as_ref()
            .and_then(|p| p.meta.as_ref())
            .and_then(|m| m.common_config_enabled)
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
            let mut input = TextInput::new(cx, "{ \"env\": { } }").code(true);
            input.set_content(snippet_seed, cx);
            input
        });
        Self {
            app,
            app_type,
            codec,
            schema,
            values,
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
            show_preview: true,
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

    /// Apply a built-in preset: replace values and rebuild inputs.
    fn apply_preset(&mut self, index: usize, cx: &mut Context<Self>) {
        let presets = self.codec.presets();
        if let Some(preset) = presets.into_iter().nth(index) {
            self.values = preset.values;
            self.text_inputs.clear();
            self.kv_rows.clear();
            self.grid_rows.clear();
            self.build_inputs(cx);
            cx.notify();
        }
    }

    /// Open the modal code editor for preview file `index`.
    fn open_raw_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.pull_values(cx);
        let files = self.codec.preview(&self.values);
        if let Some(file) = files.get(index) {
            let content = file.content.clone();
            let filename = SharedString::from(file.filename.clone());
            let input = cx.new(|cx| {
                let mut input = TextInput::new(cx, "").code(true);
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
        self.pull_values(cx);
        let prior = self
            .original_provider
            .as_ref()
            .map(|p| p.settings_config.clone())
            .unwrap_or(Value::Null);
        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let cur_meta = self
            .codec
            .encode(&self.values, &prior, prior_meta.as_ref())
            .meta;
        let mut contents: Vec<String> = self
            .codec
            .preview(&self.values)
            .into_iter()
            .map(|f| f.content)
            .collect();
        if idx < contents.len() {
            contents[idx] = edited;
        }
        match self.codec.parse_files(&contents) {
            Ok(settings) => {
                self.values = self.codec.decode(&settings, cur_meta.as_ref());
                self.text_inputs.clear();
                self.kv_rows.clear();
                self.grid_rows.clear();
                self.build_inputs(cx);
                self.raw_edit = None;
                self.status = Some(SharedString::from("已应用文件编辑"));
                cx.notify();
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
        cx.notify();
    }

    fn toggle_bool(&mut self, field_id: String, cx: &mut Context<Self>) {
        let cur = bool_val(&self.values, &field_id);
        self.values.insert(field_id, Value::Bool(!cur));
        cx.notify();
    }

    fn kv_add(&mut self, field_id: String, cx: &mut Context<Self>) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let key = cx.new(|cx| TextInput::new(cx, "key"));
        let value = cx.new(|cx| TextInput::new(cx, "value"));
        self.kv_rows
            .entry(field_id)
            .or_default()
            .push(KvRow { id, key, value });
        cx.notify();
    }

    fn kv_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.kv_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
        cx.notify();
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
            .entry(field_id)
            .or_default()
            .push(GridRow { id, cells, toggles });
        cx.notify();
    }

    fn grid_remove(&mut self, field_id: String, row_id: usize, cx: &mut Context<Self>) {
        if let Some(rows) = self.grid_rows.get_mut(&field_id) {
            rows.retain(|r| r.id != row_id);
        }
        cx.notify();
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
        cx.notify();
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

        let issues = self.codec.validate(&self.values);
        if let Some(err) = issues.iter().find(|i| i.severity == Severity::Error) {
            self.error = Some(SharedString::from(format!("配置无效：{}", err.message)));
            cx.notify();
            return;
        }

        let prior = self
            .original_provider
            .as_ref()
            .map(|p| p.settings_config.clone())
            .unwrap_or(Value::Null);
        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let mut encoded = self.codec.encode(&self.values, &prior, prior_meta.as_ref());

        // Persist the app-level common-config snippet before saving the provider so
        // `normalize_provider_common_config_for_storage` strips it back out of the
        // stored settings (it is re-applied on live-write).
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
        let category = nonempty(self.category.read(cx).content().trim().to_string());
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
            let result = routedeck_core::services::model_fetch::fetch_models(
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
                routedeck_core::services::SpeedtestService::test_endpoints(vec![base_url], Some(8)).await;
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
            let result = routedeck_core::services::balance::get_balance(&base_url, &api_key).await;
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

    // ---- common config ------------------------------------------------------

    fn toggle_common_config(&mut self, cx: &mut Context<Self>) {
        self.common_config_enabled = !self.common_config_enabled;
        cx.notify();
    }

    /// Extract a shared snippet from the current form values and load it into the
    /// snippet editor (mirrors cc-switch "从当前配置提取").
    fn extract_common_config(&mut self, cx: &mut Context<Self>) {
        self.pull_values(cx);
        let prior = self
            .original_provider
            .as_ref()
            .map(|p| p.settings_config.clone())
            .unwrap_or(Value::Null);
        let prior_meta = self.original_provider.as_ref().and_then(|p| p.meta.clone());
        let settings = self
            .codec
            .encode(&self.values, &prior, prior_meta.as_ref())
            .settings_config;
        match ConfigService::extract_common_config_snippet(&self.app, self.app_type, Some(&settings))
        {
            Ok(snippet) => {
                self.common_snippet
                    .update(cx, |i, cx| i.set_content(snippet, cx));
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

    // ---- copy to another app ------------------------------------------------

    fn open_convert(&mut self, cx: &mut Context<Self>) {
        self.convert_open = true;
        cx.notify();
    }

    fn close_convert(&mut self, cx: &mut Context<Self>) {
        self.convert_open = false;
        cx.notify();
    }

    /// Re-encode the current form values with `target`'s codec and save the result
    /// as a new provider under that app. Shared fields (base URL / key / model) live
    /// under the same value keys across all codecs, so the conversion carries over.
    fn convert_to(&mut self, target: AppType, cx: &mut Context<Self>) {
        self.pull_values(cx);
        let target_codec = provider_config::config_for(target)
            .unwrap_or_else(|| Box::new(provider_config::CodexConfig));
        let encoded = target_codec.encode(&self.values, &Value::Null, None);

        let base_name = self.name.read(cx).content().trim().to_string();
        let name = if base_name.is_empty() {
            format!("来自 {} 的配置", app_label(self.app_type))
        } else {
            format!("{base_name}（来自 {}）", app_label(self.app_type))
        };
        let website_url = nonempty(self.website_url.read(cx).content().trim().to_string());
        let category = nonempty(self.category.read(cx).content().trim().to_string());
        let notes = nonempty(self.notes.read(cx).content().trim().to_string());

        let mut provider = Provider::with_id(
            uuid::Uuid::new_v4().to_string(),
            name,
            encoded.settings_config,
            website_url,
        );
        provider.meta = encoded.meta;
        provider.category = category;
        provider.notes = notes;
        provider.created_at = Some(chrono_now_millis());

        match ProviderService::add(&self.app, target, provider, false) {
            Ok(_) => {
                self.convert_open = false;
                self.status = Some(SharedString::from(format!(
                    "已复制到 {}",
                    app_label(target)
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

    fn render_label(label: &str, help: Option<&str>, required: bool) -> impl IntoElement {
        let mut col = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_color(theme::c(theme::SUBTEXT))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(label.to_string())),
                )
                .when(required, |s| {
                    s.child(div().text_color(theme::c(theme::RED)).text_xs().child("*"))
                }),
        );
        if let Some(help) = help {
            col = col.child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(SharedString::from(help.to_string())),
            );
        }
        col
    }

    fn render_field(&self, field: &FormField, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body = match &field.kind {
            FieldKind::Text { .. } | FieldKind::Secret { .. } => self
                .text_inputs
                .get(&field.id)
                .map(|i| i.clone().into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            FieldKind::Select { options } => {
                let current = str_val(&self.values, &field.id).to_string();
                let mut row = div().flex().flex_row().flex_wrap().gap_2();
                for opt in options {
                    let selected = opt.value == current;
                    let fid = field.id.clone();
                    let val = opt.value.clone();
                    let label = match &opt.hint {
                        Some(hint) => format!("{} · {hint}", opt.label),
                        None => opt.label.clone(),
                    };
                    row = row.child(
                        div()
                            .id(SharedString::from(format!(
                                "sel-{}-{}",
                                field.id, opt.value
                            )))
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_sm()
                            .bg(theme::c(if selected {
                                theme::ACCENT_SOFT
                            } else {
                                theme::INSET
                            }))
                            .text_color(theme::c(if selected {
                                theme::ACCENT
                            } else {
                                theme::SUBTEXT
                            }))
                            .when(selected, |s| s.font_weight(FontWeight::MEDIUM))
                            .child(SharedString::from(label))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.set_select(fid.clone(), val.clone(), cx);
                            })),
                    );
                }
                row.into_any_element()
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
            FieldKind::ModelGrid { columns } => {
                self.render_grid(&field.id, columns, cx).into_any_element()
            }
        };

        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .w_full()
            .child(Self::render_label(
                &field.label,
                field.help.as_deref(),
                field.required,
            ))
            .child(body)
            .into_any_element()
    }

    fn render_kv(&self, field_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col().gap_2().w_full();
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
                        .child(div().flex_1().child(row.key.clone()))
                        .child(div().flex_1().child(row.value.clone()))
                        .child(
                            components::action_button(
                                SharedString::from(format!("kv-del-{field_id}-{rid}")),
                                "删除",
                                false,
                            )
                            .on_click(cx.listener(
                                move |this, _e, _w, cx| {
                                    this.kv_remove(fid.clone(), rid, cx);
                                },
                            )),
                        ),
                );
            }
        }
        let fid = field_id.to_string();
        col.child(
            components::action_button(
                SharedString::from(format!("kv-add-{field_id}")),
                "+ 添加",
                false,
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut col = div().flex().flex_col().gap_2().w_full();
        let mut header = div().flex().flex_row().gap_2().px_1();
        for c in columns {
            header = header.child(
                div()
                    .flex_1()
                    .text_color(theme::c(theme::MUTED))
                    .text_xs()
                    .child(SharedString::from(c.label.clone())),
            );
        }
        header = header.child(div().w(px(56.)));
        col = col.child(header);

        if let Some(rows) = self.grid_rows.get(field_id) {
            for row in rows {
                let fid = field_id.to_string();
                let rid = row.id;
                let mut r = div().flex().flex_row().items_center().gap_2().w_full();
                for c in columns {
                    let cell = match &c.kind {
                        GridCellKind::Text { .. } => row
                            .cells
                            .get(&c.key)
                            .map(|i| i.clone().into_any_element())
                            .unwrap_or_else(|| div().into_any_element()),
                        GridCellKind::Toggle => {
                            let on = row.toggles.get(&c.key).copied().unwrap_or(false);
                            let fid2 = fid.clone();
                            let key = c.key.clone();
                            div()
                                .id(SharedString::from(format!(
                                    "grid-tog-{field_id}-{rid}-{}",
                                    c.key
                                )))
                                .cursor_pointer()
                                .child(layout::toggle(on))
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    this.grid_toggle(fid2.clone(), rid, key.clone(), cx);
                                }))
                                .into_any_element()
                        }
                    };
                    r = r.child(div().flex_1().child(cell));
                }
                let fid_del = fid.clone();
                r = r.child(
                    div().w(px(56.)).child(
                        components::action_button(
                            SharedString::from(format!("grid-del-{field_id}-{rid}")),
                            "删除",
                            false,
                        )
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.grid_remove(fid_del.clone(), rid, cx);
                        })),
                    ),
                );
                col = col.child(r);
            }
        }
        let fid = field_id.to_string();
        col.child(
            components::action_button(
                SharedString::from(format!("grid-add-{field_id}")),
                "+ 添加模型",
                false,
            )
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.grid_add(fid.clone(), cx);
            })),
        )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.codec.preview(&self.values);
        let issues = self.codec.validate(&self.values);

        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(400.))
            .flex_shrink_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("将写入的文件"),
                    )
                    .child(
                        components::action_button("editor-refresh-preview", "刷新", false)
                            .on_click(cx.listener(|_t, _e, _w, cx| cx.notify())),
                    ),
            );

        for (idx, file) in files.into_iter().enumerate() {
            let filename = file.filename;
            let content = file.content;
            let lang = match file.language {
                Language::Toml => "TOML",
                Language::Json => "JSON",
                Language::Yaml => "YAML",
                Language::Env => "ENV",
            };
            let lines: Vec<gpui::AnyElement> = content
                .lines()
                .map(|l| div().min_h(px(15.)).child(l.to_string()).into_any_element())
                .collect();
            col = col.child(
                div()
                    .id(SharedString::from(format!("preview-file-{idx}")))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _e, _w, cx| this.open_raw_edit(idx, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::c(theme::SUBTEXT))
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(SharedString::from(filename)),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(theme::c(theme::INSET))
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child(lang),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_color(theme::c(theme::ACCENT))
                                    .text_xs()
                                    .child("点击编辑 ✎"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(theme::c(theme::MANTLE))
                            .border_1()
                            .border_color(theme::c(theme::BORDER))
                            .text_xs()
                            .text_color(theme::c(theme::TEXT))
                            .children(lines),
                    ),
            );
        }

        if !issues.is_empty() {
            let mut list = div().flex().flex_col().gap_1();
            for issue in issues {
                let (color, tag) = match issue.severity {
                    Severity::Error => (theme::RED, "错误"),
                    Severity::Warning => (theme::YELLOW, "警告"),
                    Severity::Info => (theme::SUBTEXT, "提示"),
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .text_xs()
                        .child(div().text_color(theme::c(color)).flex_shrink_0().child(tag))
                        .child(
                            div()
                                .text_color(theme::c(theme::SUBTEXT))
                                .child(SharedString::from(issue.message)),
                        ),
                );
            }
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("校验"),
                    )
                    .child(list),
            );
        }
        col
    }

    fn render_raw_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let raw = self.raw_edit.as_ref()?;
        let card = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(760.))
            .max_h(px(640.))
            .bg(theme::c(theme::SURFACE))
            .rounded_lg()
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .shadow(theme::shadow_popover())
            .p_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(raw.filename.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child("直接编辑文件内容，应用后会同步回上方表单。"),
                    ),
            )
            .when_some(raw.error.clone(), |s, err| {
                s.child(
                    div()
                        .text_color(theme::c(theme::RED))
                        .text_xs()
                        .child(err),
                )
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
                    .border_color(theme::c(theme::BORDER))
                    .child(raw.input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        components::action_button("raw-cancel", "取消", false)
                            .on_click(cx.listener(|this, _e, _w, cx| this.close_raw_edit(cx))),
                    )
                    .child(
                        components::action_button("raw-apply", "应用", true)
                            .on_click(cx.listener(|this, _e, _w, cx| this.apply_raw_edit(cx))),
                    ),
            );
        Some(
            div()
                .id("raw-modal-scrim")
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::translucent(0x000000, 0.45))
                .child(card)
                .into_any_element(),
        )
    }

    /// Common-config snippet section: enable toggle + shared snippet editor +
    /// "extract from current" action. Only rendered for apps that support it.
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
                            .gap_0p5()
                            .child(
                                div()
                                    .text_color(theme::c(theme::TEXT))
                                    .text_sm()
                                    .child("应用通用配置到此供应商"),
                            )
                            .child(
                                div()
                                    .text_color(theme::c(theme::MUTED))
                                    .text_xs()
                                    .child("写入 live 配置时合并下方共享片段"),
                            ),
                    )
                    .child(
                        div()
                            .id("common-config-toggle")
                            .cursor_pointer()
                            .child(layout::toggle(enabled))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.toggle_common_config(cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child("此片段按应用共享，供所有开启开关的供应商使用。"),
                    )
                    .child(
                        components::action_button(
                            "common-config-extract",
                            "从当前配置提取",
                            false,
                        )
                        .on_click(cx.listener(|this, _e, _w, cx| this.extract_common_config(cx))),
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
                    .border_color(theme::c(theme::BORDER))
                    .child(self.common_snippet.clone()),
            )
            .into_any_element()
    }

    fn render_convert_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.convert_open {
            return None;
        }
        let mut targets = div().flex().flex_col().gap_2().w_full();
        for app in AppType::all() {
            if app == self.app_type {
                continue;
            }
            targets = targets.child(
                div()
                    .id(SharedString::from(format!("convert-target-{}", app.as_str())))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme::c(theme::INSET))
                    .text_color(theme::c(theme::TEXT))
                    .text_sm()
                    .hover(|s| {
                        s.bg(theme::c(theme::ACCENT_SOFT))
                            .text_color(theme::c(theme::ACCENT))
                    })
                    .child(SharedString::from(app_label(app)))
                    .child(div().text_color(theme::c(theme::MUTED)).text_xs().child("→"))
                    .on_click(cx.listener(move |this, _e, _w, cx| this.convert_to(app, cx))),
            );
        }
        let card = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(420.))
            .bg(theme::c(theme::SURFACE))
            .rounded_lg()
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .shadow(theme::shadow_popover())
            .p_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::c(theme::TEXT))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("复制到其他应用"),
                    )
                    .child(
                        div()
                            .text_color(theme::c(theme::MUTED))
                            .text_xs()
                            .child("使用目标应用的编解码器重新生成，并另存为新供应商。"),
                    ),
            )
            .child(targets)
            .child(
                div().flex().flex_row().justify_end().child(
                    components::action_button("convert-cancel", "取消", false)
                        .on_click(cx.listener(|this, _e, _w, cx| this.close_convert(cx))),
                ),
            );
        Some(
            div()
                .id("convert-modal-scrim")
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::translucent(0x000000, 0.45))
                .child(card)
                .into_any_element(),
        )
    }

    fn render_identity(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .child(field_block("名称", &self.name))
            .when(!self.is_editing(), |s| {
                s.child(field_block("供应商 ID（可选）", &self.provider_id))
            })
            .child(field_block("网站 URL", &self.website_url))
            .child(field_block("分类", &self.category))
            .child(field_block("备注", &self.notes))
    }
}

impl Render for ProviderEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep `values` in sync with the inputs so preview/validation reflect the
        // latest edits (refreshes on any interaction; a 刷新 button forces it).
        self.pull_values(cx);

        let title = if self.is_editing() {
            "编辑供应商"
        } else {
            "新增供应商"
        };

        let identity = self.render_identity().into_any_element();
        let presets = self.codec.presets();
        let preset_picker = if presets.is_empty() {
            None
        } else {
            let mut row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_color(theme::c(theme::SUBTEXT))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .child("从预设开始"),
                );
            for (i, p) in presets.iter().enumerate() {
                let name = p.name.clone();
                row = row.child(
                    div()
                        .id(SharedString::from(format!("preset-{i}")))
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .text_sm()
                        .bg(theme::c(theme::INSET))
                        .text_color(theme::c(theme::SUBTEXT))
                        .hover(|s| {
                            s.bg(theme::c(theme::ACCENT_SOFT))
                                .text_color(theme::c(theme::ACCENT))
                        })
                        .child(SharedString::from(name))
                        .on_click(cx.listener(move |this, _e, _w, cx| this.apply_preset(i, cx))),
                );
            }
            Some(row.into_any_element())
        };
        let sections: Vec<gpui::AnyElement> = self
            .schema
            .clone()
            .into_iter()
            .map(|section| {
                let caption = if section.advanced { "高级选项" } else { "" };
                let mut col = div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w_full()
                    .child(layout::section_header(section.title.clone(), caption));
                for field in &section.fields {
                    if field.is_visible(&self.values) {
                        col = col.child(self.render_field(field, cx));
                    }
                }
                col.into_any_element()
            })
            .collect();
        let preview = if self.show_preview {
            Some(self.render_preview(cx).into_any_element())
        } else {
            None
        };
        let common_config = if self.common_config_supported {
            Some(self.render_common_config(cx))
        } else {
            None
        };
        let error = self.error.clone();
        let status = self.status.clone();
        let modal = self.render_raw_modal(cx);
        let convert_modal = self.render_convert_modal(cx);

        layout::page()
            .relative()
            .child(
                layout::page_header(title, None).child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            components::action_button("editor-convert", "复制到应用", false)
                                .on_click(cx.listener(|this, _e, _w, cx| this.open_convert(cx))),
                        )
                        .child(
                            components::action_button("editor-save", "保存", true)
                                .on_click(cx.listener(|this, _e, _w, cx| this.do_save(cx))),
                        )
                        .child(
                            components::action_button("editor-cancel", "取消", false).on_click(
                                cx.listener(|_t, _e, _w, cx| cx.emit(EditorEvent::Cancelled)),
                            ),
                        ),
                ),
            )
            .when_some(error, |s, error| {
                s.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_color(theme::c(theme::RED))
                        .text_xs()
                        .child(error),
                )
            })
            .when_some(status, |s, status| {
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
                    .id("editor-body")
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_6()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_5()
                            .flex_1()
                            .min_w_0()
                            .when_some(preset_picker, |s, picker| s.child(picker))
                            .child(identity)
                            .children(sections)
                            .when_some(common_config, |s, cc| s.child(cc))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        components::action_button(
                                            "editor-fetch-models",
                                            "拉取模型",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _e, _w, cx| this.fetch_models(cx)),
                                        ),
                                    )
                                    .child(
                                        components::action_button(
                                            "editor-speedtest",
                                            "测试 URL",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _e, _w, cx| {
                                                this.speedtest_base_url(cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        components::action_button(
                                            "editor-balance",
                                            "查询余额",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _e, _w, cx| this.query_balance(cx)),
                                        ),
                                    ),
                            ),
                    )
                    .when_some(preview, |s, preview| s.child(preview)),
            )
            .when_some(modal, |s, modal| s.child(modal))
            .when_some(convert_modal, |s, modal| s.child(modal))
    }
}

fn field_block(label: &str, input: &Entity<TextInput>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .w_full()
        .child(
            div()
                .text_color(theme::c(theme::SUBTEXT))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .child(SharedString::from(label.to_string())),
        )
        .child(input.clone())
}

/// Apps whose common-config snippet actually round-trips into live settings.
/// Must mirror the *non-no-op* arms of
/// `provider::live::apply_common_config_to_settings` (currently Claude + Codex).
/// OpenCode/OpenClaw have working `extract_*` arms but a no-op `apply` arm, so
/// advertising the section for them would silently persist a snippet that never
/// merges back — do not add them here without a real apply arm.
fn common_config_supported(app: AppType) -> bool {
    matches!(app, AppType::Claude | AppType::Codex)
}

fn app_label(app: AppType) -> &'static str {
    match app {
        AppType::Claude => "Claude Code",
        AppType::ClaudeDesktop => "Claude Desktop",
        AppType::Codex => "Codex",
        AppType::OpenCode => "OpenCode",
        AppType::OpenClaw => "OpenClaw",
        AppType::Hermes => "Hermes",
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
