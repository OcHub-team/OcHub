//! [`AppConfig`] implementation driven by a checked [`AppManifest`].
//!
//! One generic codec reproduces what each native per-app codec did by hand:
//! `schema` walks the manifest sections, `encode`/`decode` map form values on and
//! off the declared files (preserving unknown top-level keys and passthrough
//! stores verbatim), `preview` serializes each file exactly as the live writer
//! will, and `validate`/`presets`/`parse_files` interpret their declarative rules.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::app_id::AppId;
use crate::model::ProviderMeta;
use crate::provider_config::{
    AppConfig, ConfigIssue, EncodeResult, FieldKind, FormField, FormSection, FormValues,
    GridCellKind, GridColumn, Preset, PreviewFile, SelectOption, bool_val, set_bool, set_str,
    str_val,
};

use super::hooks::HookRegistry;
use super::manifest::{
    AppManifest, ColumnKind, Condition, FieldKindSpec, FieldSpec, FileSpec, MapSpec, MapTarget,
    PresetValue, ValidateRule,
};

/// A provider-editor codec backed by a manifest.
pub struct ManifestCodec {
    pub manifest: Arc<AppManifest>,
    pub hooks: Arc<HookRegistry>,
}

impl ManifestCodec {
    pub fn new(manifest: Arc<AppManifest>, hooks: Arc<HookRegistry>) -> Self {
        Self { manifest, hooks }
    }

    /// The store-subtree key backing the file a mapping targets.
    fn store_key_of(&self, file_id: &str) -> Option<&str> {
        self.manifest.file(file_id).map(|f| f.store_key.as_str())
    }

    /// Top-level keys claimed by non-`rest` sibling fields of a file (so `rest`
    /// can skip them). Mirrors Gemini's `RESERVED_ENV_KEYS`, computed generically.
    fn claimed_keys(&self, file_id: &str) -> BTreeSet<String> {
        let mut claimed = BTreeSet::new();
        for field in self.manifest.fields() {
            let Some(map) = &field.map else { continue };
            if map.file != file_id {
                continue;
            }
            match map.target() {
                MapTarget::EnvKey(k) => {
                    claimed.insert(k.to_string());
                }
                MapTarget::Pointer(p) => {
                    if let Some(seg) = pointer_tokens(p).into_iter().next() {
                        claimed.insert(seg);
                    }
                }
                MapTarget::TomlPath(t) => {
                    if let Some(seg) = t.split('.').next() {
                        claimed.insert(seg.to_string());
                    }
                }
                MapTarget::Rest => {}
            }
        }
        claimed
    }

    /// Build the store subtree for one mapped (non-passthrough) file.
    fn build_store(&self, file: &FileSpec, values: &FormValues) -> Value {
        // clear_when: an emptied store (form-level condition baked into encode).
        if let Some(cond) = &file.clear_when
            && condition_matches(values, cond)
        {
            return Value::Object(Map::new());
        }

        let claimed = self.claimed_keys(&file.id);
        let mut store = Map::new();

        for field in self.manifest.fields() {
            let Some(map) = &field.map else { continue };
            if map.file != file.id {
                continue;
            }
            if let Some(cond) = &map.emit_when
                && !condition_matches(values, cond)
            {
                continue;
            }

            match map.target() {
                MapTarget::EnvKey(key) => {
                    if let Some(v) = mapped_scalar(field, values, map) {
                        store.insert(key.to_string(), v);
                    }
                }
                MapTarget::Pointer(p) => {
                    if let Some(v) = mapped_scalar(field, values, map) {
                        insert_path(&mut store, &pointer_tokens(p), v);
                    }
                }
                MapTarget::TomlPath(t) => {
                    if let Some(v) = mapped_scalar(field, values, map) {
                        let tokens: Vec<String> = t.split('.').map(str::to_string).collect();
                        insert_path(&mut store, &tokens, v);
                    }
                }
                MapTarget::Rest => {
                    if let Some(obj) = values.get(&field.id).and_then(Value::as_object) {
                        for (k, v) in obj {
                            let key = k.trim();
                            if key.is_empty() || claimed.contains(key) {
                                continue;
                            }
                            if let Some(s) = v.as_str() {
                                store.insert(key.to_string(), Value::String(s.to_string()));
                            }
                        }
                    }
                }
            }
        }

        Value::Object(store)
    }
}

impl AppConfig for ManifestCodec {
    fn app_id(&self) -> AppId {
        self.manifest
            .app_id()
            .expect("manifest id validated at load time")
    }

    fn schema(&self) -> Vec<FormSection> {
        self.manifest
            .sections
            .iter()
            .map(|section| {
                let fields = section.fields.iter().map(build_form_field).collect();
                let mut form = FormSection::new(section.title.clone(), fields);
                if section.advanced {
                    form = form.advanced();
                }
                form
            })
            .collect()
    }

    fn decode(&self, settings_config: &Value, meta: Option<&ProviderMeta>) -> FormValues {
        let mut values = FormValues::new();

        for field in self.manifest.fields() {
            // Virtual field: value derived from a store's emptiness.
            if let Some(dec) = &field.decode {
                let Some(store_key) = self.store_key_of(&dec.file) else {
                    continue;
                };
                let empty = store_is_empty(settings_config, store_key);
                let value = if empty { &dec.if_empty } else { &dec.otherwise };
                set_str(&mut values, &field.id, value.clone());
                continue;
            }

            let Some(map) = &field.map else { continue };
            let Some(store_key) = self.store_key_of(&map.file) else {
                continue;
            };
            let store = settings_config.get(store_key);

            match map.target() {
                MapTarget::EnvKey(key) => {
                    let v = store
                        .and_then(|s| s.get(key))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    set_str(&mut values, &field.id, v);
                }
                MapTarget::Pointer(p) => {
                    let v = store.and_then(|s| s.pointer(p));
                    decode_scalar(&mut values, field, v);
                }
                MapTarget::TomlPath(t) => {
                    let v = store.and_then(|s| get_dotted(s, t));
                    decode_scalar(&mut values, field, v);
                }
                MapTarget::Rest => {
                    let claimed = self.claimed_keys(&map.file);
                    let mut rest = Map::new();
                    if let Some(obj) = store.and_then(Value::as_object) {
                        for (k, v) in obj {
                            if claimed.contains(k) {
                                continue;
                            }
                            if let Some(s) = v.as_str() {
                                rest.insert(k.clone(), Value::String(s.to_string()));
                            }
                        }
                    }
                    values.insert(field.id.clone(), Value::Object(rest));
                }
            }
        }

        // decode hook augments the mapped values.
        if let Some(name) = &self.manifest.hooks.decode
            && let Some(hook) = self.hooks.decode(name)
        {
            for (k, v) in hook(settings_config, meta) {
                values.insert(k, v);
            }
        }

        values
    }

    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult {
        // Preserve unknown top-level keys of settingsConfig.
        let mut settings = prior.as_object().cloned().unwrap_or_default();

        for file in &self.manifest.files {
            if file.passthrough {
                match prior.get(&file.store_key) {
                    Some(v) if v.is_object() || v.is_null() => {
                        settings.insert(file.store_key.clone(), v.clone());
                    }
                    Some(_) => {
                        settings.insert(file.store_key.clone(), Value::Object(Map::new()));
                    }
                    None => {
                        // Leave absent so the live writer preserves the file.
                    }
                }
            } else {
                let store = self.build_store(file, values);
                settings.insert(file.store_key.clone(), store);
            }
        }

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: prior_meta.cloned(),
        }
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        self.manifest
            .files
            .iter()
            .map(|file| {
                let store = if file.passthrough {
                    prior
                        .get(&file.store_key)
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()))
                } else {
                    self.build_store(file, values)
                };
                let content = super::format::serialize(file.format, &store).unwrap_or_default();
                PreviewFile {
                    filename: format!("{}/{}", self.manifest.config_dir_default(), file.path),
                    language: super::format::language(file.format),
                    content,
                }
            })
            .collect()
    }

    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();

        for rule in &self.manifest.validate {
            if let Some(cond) = &rule.when
                && !condition_matches(values, cond)
            {
                continue;
            }
            match rule.rule {
                ValidateRule::Require => {
                    if let Some(field) = &rule.field
                        && str_val(values, field).trim().is_empty()
                    {
                        issues.push(ConfigIssue::error(rule.message.clone()).for_field(field));
                    }
                }
                ValidateRule::WarnEmpty => {
                    if let Some(field) = &rule.field
                        && str_val(values, field).trim().is_empty()
                    {
                        issues.push(ConfigIssue::warning(rule.message.clone()).for_field(field));
                    }
                }
                ValidateRule::WarnWhitespaceOnly => {
                    if let Some(field) = &rule.field {
                        let v = str_val(values, field);
                        if !v.is_empty() && v.trim().is_empty() {
                            issues
                                .push(ConfigIssue::warning(rule.message.clone()).for_field(field));
                        }
                    }
                }
                ValidateRule::InfoAnyNonempty => {
                    let any = rule
                        .fields
                        .iter()
                        .any(|f| !str_val(values, f).trim().is_empty());
                    if any {
                        issues.push(ConfigIssue::info(rule.message.clone()));
                    }
                }
            }
        }

        issues
    }

    fn presets(&self) -> Vec<Preset> {
        self.manifest
            .presets
            .iter()
            .map(|p| {
                let mut values = FormValues::new();
                for (k, v) in &p.values {
                    let value = match v {
                        PresetValue::Bool(b) => Value::Bool(*b),
                        PresetValue::Str(s) => Value::String(s.clone()),
                    };
                    values.insert(k.clone(), value);
                }
                Preset::new(p.name.clone(), values)
            })
            .collect()
    }

    fn parse_files(&self, contents: &[String]) -> Result<Value, String> {
        let mut settings = Map::new();
        for (i, file) in self.manifest.files.iter().enumerate() {
            let content = contents.get(i).map(String::as_str).unwrap_or("");
            // env stores always materialize (even empty); structured stores are
            // only inserted when the file has content, matching the native codec.
            if !matches!(file.format, super::manifest::FileFormat::Env) && content.trim().is_empty()
            {
                continue;
            }
            let value = super::format::parse(file.format, content, &file.id)?;
            settings.insert(file.store_key.clone(), value);
        }
        Ok(Value::Object(settings))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn build_form_field(field: &FieldSpec) -> FormField {
    let kind = match field.kind {
        FieldKindSpec::Text => FieldKind::Text {
            placeholder: field.placeholder.clone().unwrap_or_default(),
        },
        FieldKindSpec::Secret => FieldKind::Secret {
            placeholder: field.placeholder.clone().unwrap_or_default(),
        },
        FieldKindSpec::Select => FieldKind::Select {
            options: field
                .options
                .iter()
                .map(|o| {
                    let mut opt = SelectOption::new(o.value.clone(), o.label.clone());
                    if let Some(hint) = &o.hint {
                        opt = opt.with_hint(hint.clone());
                    }
                    opt
                })
                .collect(),
        },
        FieldKindSpec::Toggle => FieldKind::Toggle,
        FieldKindSpec::KeyValue => FieldKind::KeyValue {
            key_placeholder: field.key_placeholder.clone().unwrap_or_default(),
            value_placeholder: field.value_placeholder.clone().unwrap_or_default(),
        },
        FieldKindSpec::ModelGrid => FieldKind::ModelGrid {
            columns: field
                .columns
                .iter()
                .map(|c| GridColumn {
                    key: c.key.clone(),
                    label: c.label.clone(),
                    kind: match c.kind {
                        ColumnKind::Text => GridCellKind::Text {
                            placeholder: c.placeholder.clone().unwrap_or_default(),
                        },
                        ColumnKind::Toggle => GridCellKind::Toggle,
                    },
                })
                .collect(),
        },
    };

    let mut form = FormField::new(field.id.clone(), field.label.clone(), kind);
    if let Some(help) = &field.help {
        form = form.help(help.clone());
    }
    if field.required {
        form = form.required();
    }
    if let Some(cond) = &field.visible_when {
        form = form.visible_when(cond.field.clone(), cond.equals.clone());
    }
    form
}

/// Whether a form field currently equals a condition's value.
fn condition_matches(values: &FormValues, cond: &Condition) -> bool {
    str_val(values, &cond.field) == cond.equals
}

/// A store subtree counts as "empty" when it is missing, null, a non-object, or
/// an empty object — matching Gemini's `env.map(|e| e.is_empty()).unwrap_or(true)`.
fn store_is_empty(settings_config: &Value, store_key: &str) -> bool {
    match settings_config.get(store_key) {
        Some(Value::Object(o)) => o.is_empty(),
        _ => true,
    }
}

/// Resolve a mapped scalar value with `trim` / `omit_empty` applied. Toggle
/// fields become booleans; everything else becomes a (possibly trimmed) string.
fn mapped_scalar(field: &FieldSpec, values: &FormValues, map: &MapSpec) -> Option<Value> {
    if matches!(field.kind, FieldKindSpec::Toggle) {
        return Some(Value::Bool(bool_val(values, &field.id)));
    }
    let raw = str_val(values, &field.id);
    let owned = if map.trim {
        raw.trim().to_string()
    } else {
        raw.to_string()
    };
    if map.omit_empty && owned.is_empty() {
        return None;
    }
    Some(Value::String(owned))
}

/// Write a decoded scalar into form values, honoring the field's kind.
fn decode_scalar(values: &mut FormValues, field: &FieldSpec, value: Option<&Value>) {
    if matches!(field.kind, FieldKindSpec::Toggle) {
        set_bool(
            values,
            &field.id,
            value.and_then(Value::as_bool).unwrap_or(false),
        );
    } else {
        set_str(
            values,
            &field.id,
            value.and_then(Value::as_str).unwrap_or_default(),
        );
    }
}

/// Split an RFC6901 JSON pointer into unescaped tokens (`/a/b` → `["a","b"]`).
fn pointer_tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Navigate a store by dotted path (`a.b.c`).
fn get_dotted<'a>(store: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = store;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Insert `value` at a nested path, creating intermediate objects.
fn insert_path(map: &mut Map<String, Value>, tokens: &[String], value: Value) {
    match tokens {
        [] => {}
        [last] => {
            map.insert(last.clone(), value);
        }
        [head, rest @ ..] => {
            let entry = map
                .entry(head.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Some(obj) = entry.as_object_mut() {
                insert_path(obj, rest, value);
            }
        }
    }
}
