//! Declarative app manifest: v1 TOML schema, parsing, and semantic checking.
//!
//! A manifest describes one managed app end to end — its identity/UI metadata,
//! the on-disk files it writes, the provider-editor form (sections + fields with
//! mappings back onto those files), validation rules, presets, and named hooks.
//! [`AppManifest::parse`] deserializes the TOML (rejecting unknown keys and any
//! `manifest_version` other than 1); [`AppManifest::check`] runs the semantic
//! pass (unique ids, dangling references, parseable colors/modes, known hooks).
//!
//! The engine that turns a checked manifest into a live editor codec lives in
//! [`super::manifest_codec`]; the plugin wrapper lives in
//! [`super::plugin_manifest`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::app_id::AppId;

use super::hooks::HookRegistry;
use super::AppMode;

/// The only manifest schema version this build understands.
pub const MANIFEST_VERSION: u32 = 1;

/// A manifest that failed to parse or failed semantic checking.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ManifestError {
    /// TOML syntax / shape error (missing required key, wrong type, unknown key).
    #[error("清单解析失败 (manifest parse error): {0}")]
    Parse(String),
    /// `manifest_version` is not 1.
    #[error(
        "不支持的 manifest_version: {0}（仅支持 {MANIFEST_VERSION}）(unsupported manifest_version)"
    )]
    UnsupportedVersion(u32),
    /// Semantic validation failure (dangling reference, duplicate id, …).
    #[error("清单校验失败 (manifest check error): {0}")]
    Check(String),
}

impl ManifestError {
    fn check(msg: impl Into<String>) -> Self {
        ManifestError::Check(msg.into())
    }
}

// ---------------------------------------------------------------------------
// Top-level document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub manifest_version: u32,
    pub app: AppMeta,
    #[serde(default)]
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub sections: Vec<SectionSpec>,
    #[serde(default)]
    pub validate: Vec<ValidateSpec>,
    #[serde(default)]
    pub presets: Vec<PresetSpec>,
    #[serde(default)]
    pub hooks: HooksSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppMeta {
    pub id: String,
    pub name: String,
    pub icon: IconSpec,
    /// `#RRGGBB`, parsed to a `0xRRGGBB` u32 in [`AppManifest::check`].
    pub accent: String,
    pub sort_order: i32,
    /// `"switch"` | `"additive"`.
    pub mode: String,
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
    pub config_dir: ConfigDirSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IconSpec {
    /// Bundled icon key the UI maps to an asset.
    #[serde(default)]
    pub builtin: Option<String>,
    /// A single glyph for a letter-avatar fallback.
    #[serde(default)]
    pub glyph: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDirSpec {
    /// Default config dir, e.g. `"~/.myapp"`. A leading `~/` expands at runtime.
    pub default: String,
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Env,
    Json,
    Toml,
    Yaml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Overwrite the file with the serialized store.
    #[default]
    Replace,
    /// Read the existing file, shallow-merge the store's top-level keys, write.
    MergeShallow,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub id: String,
    /// Path relative to the app config dir.
    pub path: String,
    pub format: FileFormat,
    /// Subtree key of `settingsConfig` backing this file.
    pub store_key: String,
    #[serde(default)]
    pub write: WriteMode,
    /// Octal string like `"0700"` for the parent dir (unix only).
    #[serde(default)]
    pub dir_mode: Option<String>,
    /// Octal string like `"0600"` for the file (unix only).
    #[serde(default)]
    pub file_mode: Option<String>,
    #[serde(default = "default_true")]
    pub atomic: bool,
    /// When the named field equals a value, emit an empty store for this file.
    #[serde(default)]
    pub clear_when: Option<Condition>,
    /// No field mappings; the store subtree is copied verbatim.
    #[serde(default)]
    pub passthrough: bool,
    /// Store missing/null ⇒ skip writing this file entirely.
    #[serde(default)]
    pub absent_preserves: bool,
}

impl FileSpec {
    pub fn dir_mode_u32(&self) -> Option<u32> {
        self.dir_mode.as_deref().and_then(parse_octal)
    }

    pub fn file_mode_u32(&self) -> Option<u32> {
        self.file_mode.as_deref().and_then(parse_octal)
    }
}

// ---------------------------------------------------------------------------
// Sections & fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionSpec {
    pub title: String,
    #[serde(default)]
    pub advanced: bool,
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKindSpec {
    Text,
    Secret,
    Select,
    Toggle,
    #[serde(rename = "keyvalue")]
    KeyValue,
    ModelGrid,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    pub id: String,
    pub label: String,
    pub kind: FieldKindSpec,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub visible_when: Option<Condition>,
    /// `select` only.
    #[serde(default)]
    pub options: Vec<OptionSpec>,
    /// `keyvalue` only.
    #[serde(default)]
    pub key_placeholder: Option<String>,
    #[serde(default)]
    pub value_placeholder: Option<String>,
    /// `model_grid` only.
    #[serde(default)]
    pub columns: Vec<ColumnSpec>,
    /// How this field's value maps onto a file (absent for virtual fields).
    #[serde(default)]
    pub map: Option<MapSpec>,
    /// Virtual-field decode rule (absent for mapped fields).
    #[serde(default)]
    pub decode: Option<DecodeSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionSpec {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnKind {
    #[default]
    Text,
    Toggle,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnSpec {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub kind: ColumnKind,
    #[serde(default)]
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub field: String,
    pub equals: String,
}

/// How a field value maps onto exactly one file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapSpec {
    pub file: String,
    /// env-format key.
    #[serde(default)]
    pub env_key: Option<String>,
    /// RFC6901 JSON pointer into a json/yaml store.
    #[serde(default)]
    pub pointer: Option<String>,
    /// Dotted path into a toml store.
    #[serde(default)]
    pub toml_path: Option<String>,
    /// `"rest"`: keyvalue overflow (all unclaimed keys of the file).
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub omit_empty: bool,
    #[serde(default)]
    pub trim: bool,
    /// Only emit when the named field equals a value.
    #[serde(default)]
    pub emit_when: Option<Condition>,
}

/// The map "target" resolved from the mutually-exclusive fields.
pub enum MapTarget<'a> {
    EnvKey(&'a str),
    Pointer(&'a str),
    TomlPath(&'a str),
    Rest,
}

impl MapSpec {
    /// The resolved (checked) target. Panics only on an unchecked manifest.
    pub fn target(&self) -> MapTarget<'_> {
        if let Some(k) = &self.env_key {
            MapTarget::EnvKey(k)
        } else if let Some(p) = &self.pointer {
            MapTarget::Pointer(p)
        } else if let Some(t) = &self.toml_path {
            MapTarget::TomlPath(t)
        } else {
            MapTarget::Rest
        }
    }
}

/// Virtual-field decode rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeSpec {
    /// Only `"value_if_store_empty"` in v1.
    pub rule: String,
    pub file: String,
    pub if_empty: String,
    pub otherwise: String,
}

// ---------------------------------------------------------------------------
// Validation & presets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidateRule {
    /// Error when the field is empty (after trim).
    Require,
    /// Warning when the field is empty (after trim).
    WarnEmpty,
    /// Warning when the field is non-empty but whitespace-only.
    WarnWhitespaceOnly,
    /// Info when any of `fields` is non-empty (after trim).
    InfoAnyNonempty,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateSpec {
    pub rule: ValidateRule,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    pub message: String,
    #[serde(default)]
    pub when: Option<Condition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PresetValue {
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetSpec {
    pub name: String,
    #[serde(default)]
    pub values: BTreeMap<String, PresetValue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksSpec {
    #[serde(default)]
    pub live_validate: Option<String>,
    #[serde(default)]
    pub post_write: Vec<String>,
    #[serde(default)]
    pub decode: Option<String>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse `#RRGGBB` into a `0xRRGGBB` u32.
pub fn parse_accent(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Parse an octal permission string like `"0700"` / `"700"`.
fn parse_octal(s: &str) -> Option<u32> {
    let s = s.trim();
    let digits = s
        .strip_prefix("0o")
        .or_else(|| s.strip_prefix("0O"))
        .unwrap_or(s);
    u32::from_str_radix(digits, 8).ok()
}

impl AppManifest {
    /// Parse a manifest from TOML, rejecting unknown keys and any
    /// `manifest_version` other than [`MANIFEST_VERSION`].
    pub fn parse(toml_str: &str) -> Result<Self, ManifestError> {
        // Probe the version first so a future v2 manifest yields a clear error
        // instead of an "unknown field" avalanche from deny_unknown_fields.
        #[derive(Deserialize)]
        struct VersionProbe {
            manifest_version: u32,
        }
        let probe: VersionProbe =
            toml::from_str(toml_str).map_err(|e| ManifestError::Parse(e.to_string()))?;
        if probe.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(probe.manifest_version));
        }

        toml::from_str(toml_str).map_err(|e| ManifestError::Parse(e.to_string()))
    }

    /// The declared app id, parsed (assumes [`check`](Self::check) passed).
    pub fn app_id(&self) -> Result<AppId, ManifestError> {
        AppId::parse(&self.app.id).map_err(|e| ManifestError::check(e.to_string()))
    }

    /// `0xRRGGBB` accent (0 if unparseable — [`check`](Self::check) rejects that).
    pub fn accent_u32(&self) -> u32 {
        parse_accent(&self.app.accent).unwrap_or(0)
    }

    /// Switch / additive mode (defaults to Switch if unparseable).
    pub fn mode(&self) -> AppMode {
        match self.app.mode.as_str() {
            "additive" => AppMode::Additive,
            _ => AppMode::Switch,
        }
    }

    /// The default config dir string exactly as written (e.g. `"~/.gemini"`).
    pub fn config_dir_default(&self) -> &str {
        &self.app.config_dir.default
    }

    /// The bundled icon key, or `""` for a glyph / letter-avatar fallback.
    pub fn icon_key(&self) -> &str {
        self.app.icon.builtin.as_deref().unwrap_or("")
    }

    /// Look up a file spec by id.
    pub fn file(&self, id: &str) -> Option<&FileSpec> {
        self.files.iter().find(|f| f.id == id)
    }

    /// All fields across all sections, in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = &FieldSpec> {
        self.sections.iter().flat_map(|s| s.fields.iter())
    }

    /// Semantic validation against a hook registry.
    pub fn check(&self, hooks: &HookRegistry) -> Result<(), ManifestError> {
        // --- app metadata ---
        AppId::parse(&self.app.id).map_err(|e| ManifestError::check(e.to_string()))?;

        if parse_accent(&self.app.accent).is_none() {
            return Err(ManifestError::check(format!(
                "accent 必须是 #RRGGBB 形式: {}",
                self.app.accent
            )));
        }
        if !matches!(self.app.mode.as_str(), "switch" | "additive") {
            return Err(ManifestError::check(format!(
                "mode 必须是 switch 或 additive: {}",
                self.app.mode
            )));
        }
        match (&self.app.icon.builtin, &self.app.icon.glyph) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(ManifestError::check(
                    "icon 必须且只能指定 builtin 或 glyph 之一".to_string(),
                ));
            }
        }
        if self.app.config_dir.default.trim().is_empty() {
            return Err(ManifestError::check(
                "app.config_dir.default 不能为空".to_string(),
            ));
        }

        // --- files: unique ids ---
        let mut file_ids = BTreeSet::new();
        for f in &self.files {
            if !file_ids.insert(f.id.as_str()) {
                return Err(ManifestError::check(format!("重复的文件 id: {}", f.id)));
            }
            if let Some(m) = &f.dir_mode {
                if parse_octal(m).is_none() {
                    return Err(ManifestError::check(format!("无效的 dir_mode: {m}")));
                }
            }
            if let Some(m) = &f.file_mode {
                if parse_octal(m).is_none() {
                    return Err(ManifestError::check(format!("无效的 file_mode: {m}")));
                }
            }
        }

        // --- fields: unique ids, collect for reference checks ---
        let mut field_ids = BTreeSet::new();
        for field in self.fields() {
            if !field_ids.insert(field.id.as_str()) {
                return Err(ManifestError::check(format!("重复的字段 id: {}", field.id)));
            }
        }

        let check_field_ref = |field: &str, ctx: &str| -> Result<(), ManifestError> {
            if field_ids.contains(field) {
                Ok(())
            } else {
                Err(ManifestError::check(format!(
                    "{ctx} 引用了不存在的字段: {field}"
                )))
            }
        };

        // --- per-field checks ---
        for field in self.fields() {
            if let Some(cond) = &field.visible_when {
                check_field_ref(&cond.field, "visible_when")?;
            }
            if let Some(map) = &field.map {
                if self.file(&map.file).is_none() {
                    return Err(ManifestError::check(format!(
                        "字段 {} 的 map.file 引用了不存在的文件: {}",
                        field.id, map.file
                    )));
                }
                let target_count = [
                    map.env_key.is_some(),
                    map.pointer.is_some(),
                    map.toml_path.is_some(),
                    map.rule.is_some(),
                ]
                .iter()
                .filter(|b| **b)
                .count();
                if target_count != 1 {
                    return Err(ManifestError::check(format!(
                        "字段 {} 的 map 必须且只能指定 env_key / pointer / toml_path / rule 之一",
                        field.id
                    )));
                }
                if let Some(rule) = &map.rule {
                    if rule != "rest" {
                        return Err(ManifestError::check(format!(
                            "字段 {} 的 map.rule 仅支持 \"rest\": {rule}",
                            field.id
                        )));
                    }
                }
                if let Some(cond) = &map.emit_when {
                    check_field_ref(&cond.field, "emit_when")?;
                }
            }
            if let Some(dec) = &field.decode {
                if dec.rule != "value_if_store_empty" {
                    return Err(ManifestError::check(format!(
                        "字段 {} 的 decode.rule 仅支持 \"value_if_store_empty\": {}",
                        field.id, dec.rule
                    )));
                }
                if self.file(&dec.file).is_none() {
                    return Err(ManifestError::check(format!(
                        "字段 {} 的 decode.file 引用了不存在的文件: {}",
                        field.id, dec.file
                    )));
                }
            }
        }

        // --- validate rules ---
        for v in &self.validate {
            if let Some(cond) = &v.when {
                check_field_ref(&cond.field, "validate.when")?;
            }
            match v.rule {
                ValidateRule::InfoAnyNonempty => {
                    if v.fields.is_empty() {
                        return Err(ManifestError::check(
                            "info_any_nonempty 规则必须提供 fields 列表".to_string(),
                        ));
                    }
                    for f in &v.fields {
                        check_field_ref(f, "validate.fields")?;
                    }
                }
                _ => {
                    let field = v.field.as_deref().ok_or_else(|| {
                        ManifestError::check(format!("{:?} 规则必须提供 field", v.rule))
                    })?;
                    check_field_ref(field, "validate.field")?;
                }
            }
        }

        // --- presets reference existing fields ---
        for p in &self.presets {
            for key in p.values.keys() {
                check_field_ref(key, "preset.values")?;
            }
        }

        // --- hooks exist in registry ---
        if let Some(name) = &self.hooks.live_validate {
            if !hooks.has_live_validate(name) {
                return Err(ManifestError::check(format!(
                    "未知的 live_validate hook: {name}"
                )));
            }
        }
        for name in &self.hooks.post_write {
            if !hooks.has_post_write(name) {
                return Err(ManifestError::check(format!(
                    "未知的 post_write hook: {name}"
                )));
            }
        }
        if let Some(name) = &self.hooks.decode {
            if !hooks.has_decode(name) {
                return Err(ManifestError::check(format!("未知的 decode hook: {name}")));
            }
        }

        Ok(())
    }
}
