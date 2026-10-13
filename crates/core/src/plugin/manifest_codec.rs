//! [`AppConfig`] implementation driven by a checked [`AppManifest`].
//!
//! One generic codec reproduces what each native per-app codec did by hand:
//! `schema` walks the manifest sections, `encode`/`decode` map form values on and
//! off the declared files (preserving unknown top-level keys and passthrough
//! stores verbatim), `preview` serializes each file exactly as the live writer
//! will, and `validate`/`presets`/`parse_files` interpret their declarative rules.

use std::collections::BTreeSet;
use std::path::PathBuf;
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
    preview_root: Option<PathBuf>,
}

impl ManifestCodec {
    pub fn new(manifest: Arc<AppManifest>, hooks: Arc<HookRegistry>) -> Self {
        Self {
            manifest,
            hooks,
            preview_root: None,
        }
    }

    pub(crate) fn with_preview_root(mut self, root: PathBuf) -> Self {
        self.preview_root = Some(root);
        self
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
    fn build_store(&self, file: &FileSpec, values: &FormValues, prior: &Value) -> Value {
        // clear_when: an emptied store (form-level condition baked into encode).
        if let Some(cond) = &file.clear_when
            && condition_matches(values, cond)
        {
            return Value::Object(Map::new());
        }

        let claimed = self.claimed_keys(&file.id);
        // Fields own only their mapped paths. Rebuilding the whole store from
        // an empty map used to discard settings entered in the file editor,
        // including siblings of a mapped nested key.
        let mut store = prior.as_object().cloned().unwrap_or_default();

        for field in self.manifest.fields() {
            let Some(map) = &field.map else { continue };
            if map.file != file.id {
                continue;
            }
            // Remove the previous owned value before writing its replacement.
            // This also makes omit_empty and emit_when=false actual removals,
            // without removing a neighbouring key that the form cannot edit.
            match map.target() {
                MapTarget::EnvKey(key) => {
                    store.remove(key);
                }
                MapTarget::Pointer(pointer) => {
                    remove_path(&mut store, &pointer_tokens(pointer));
                }
                MapTarget::TomlPath(path) => {
                    let tokens = path.split('.').map(str::to_string).collect::<Vec<_>>();
                    remove_path(&mut store, &tokens);
                }
                MapTarget::Rest => {
                    // The key/value editor represents unclaimed strings only.
                    // Preserve unrepresented objects, arrays, numbers and bools.
                    store.retain(|key, value| claimed.contains(key) || !value.is_string());
                }
            }
            if let Some(cond) = &map.emit_when
                && !condition_matches(values, cond)
            {
                continue;
            }

            match map.target() {
                MapTarget::EnvKey(key) => {
                    if let Some(v) = mapped_scalar(field, values, map) {
                        // Environment files carry strings even when the form
                        // represents a toggle as a boolean.
                        let value = match v {
                            Value::Bool(value) => Value::String(value.to_string()),
                            other => other,
                        };
                        store.insert(key.to_string(), value);
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

    fn secret_paths(&self) -> Vec<Vec<String>> {
        self.manifest
            .fields()
            .filter(|field| field.kind == FieldKindSpec::Secret)
            .filter_map(|field| {
                let map = field.map.as_ref()?;
                let mut path = vec![self.store_key_of(&map.file)?.to_string()];
                match map.target() {
                    MapTarget::EnvKey(key) => path.push(key.to_string()),
                    MapTarget::Pointer(pointer) => path.extend(pointer_tokens(pointer)),
                    MapTarget::TomlPath(dotted) => {
                        path.extend(dotted.split('.').map(str::to_string))
                    }
                    MapTarget::Rest => return None,
                }
                Some(path)
            })
            .collect()
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
                    if field.kind == FieldKindSpec::Toggle {
                        set_bool(&mut values, &field.id, v.parse().unwrap_or(false));
                    } else {
                        set_str(&mut values, &field.id, v);
                    }
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
                let store = self.build_store(
                    file,
                    values,
                    prior.get(&file.store_key).unwrap_or(&Value::Null),
                );
                settings.insert(file.store_key.clone(), store);
            }
        }

        EncodeResult {
            settings_config: Value::Object(settings),
            meta: prior_meta.cloned(),
        }
    }

    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile> {
        let encoded = self.encode(values, prior, None);
        self.manifest
            .files
            .iter()
            .map(|file| {
                let store = encoded
                    .settings_config
                    .get(&file.store_key)
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                let content = super::format::serialize(file.format, &store).unwrap_or_default();
                PreviewFile {
                    filename: self
                        .preview_root
                        .as_ref()
                        .map(|root| root.join(&file.path).to_string_lossy().into_owned())
                        .unwrap_or_else(|| {
                            format!("{}/{}", self.manifest.config_dir_default(), file.path)
                        }),
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
                        && value_is_empty(values.get(field))
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

        for field in self.manifest.fields() {
            if field.required
                && field
                    .visible_when
                    .as_ref()
                    .is_none_or(|condition| condition_matches(values, condition))
                && value_is_empty(values.get(&field.id))
                && !issues.iter().any(|issue| {
                    issue.field.as_deref() == Some(field.id.as_str())
                        && issue.severity == crate::provider_config::Severity::Error
                })
            {
                issues.push(
                    ConfigIssue::error(
                        crate::AppError::localized_ja(
                            "manifest.field_required",
                            format!("{}不能为空。", field.label),
                            format!("{} is required.", field.label),
                            format!("{} を入力してください。", field.label),
                        )
                        .to_string(),
                    )
                    .for_field(field.id.clone()),
                );
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
    match values.get(&cond.field) {
        Some(Value::Bool(value)) => value.to_string() == cond.equals,
        Some(Value::Number(value)) => value.to_string() == cond.equals,
        _ => str_val(values, &cond.field) == cond.equals,
    }
}

fn value_is_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value.trim().is_empty(),
        Some(Value::Array(value)) => value.is_empty(),
        Some(Value::Object(value)) => value.is_empty(),
        Some(Value::Bool(_) | Value::Number(_)) => false,
    }
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
    if matches!(
        field.kind,
        FieldKindSpec::KeyValue | FieldKindSpec::ModelGrid
    ) {
        let value = values.get(&field.id).cloned().unwrap_or_else(|| {
            if field.kind == FieldKindSpec::ModelGrid {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            }
        });
        return (!map.omit_empty || !value_is_empty(Some(&value))).then_some(value);
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
    } else if field.kind == FieldKindSpec::ModelGrid {
        values.insert(
            field.id.clone(),
            value
                .filter(|value| value.is_array())
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        );
    } else if field.kind == FieldKindSpec::KeyValue {
        values.insert(
            field.id.clone(),
            value
                .filter(|value| value.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
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

/// Remove only a field-owned leaf, pruning empty parents created by that path.
fn remove_path(map: &mut Map<String, Value>, tokens: &[String]) {
    match tokens {
        [] => {}
        [last] => {
            map.remove(last);
        }
        [head, rest @ ..] => {
            let empty = match map.get_mut(head).and_then(Value::as_object_mut) {
                Some(child) => {
                    remove_path(child, rest);
                    child.is_empty()
                }
                None => false,
            };
            if empty {
                map.remove(head);
            }
        }
    }
}
