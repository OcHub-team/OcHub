//! Per-app provider configuration codecs.
//!
//! Each managed app stores its provider config in a different on-disk shape
//! (Codex = `auth.json` + `config.toml`,
//! OpenCode = a typed JSON block, …) plus, for some apps, fields that live in
//! [`ProviderMeta`] rather than `settingsConfig`. The legacy add/edit form
//! flattened all of this into name/baseURL/key/model, which is both lossy and,
//! for several apps, actively wrong.
//!
//! This module replaces that with one structured-but-typed description per app.
//! An [`AppConfig`] knows how to: expose a schema of [`FormSection`]s, decode an
//! existing provider into [`FormValues`], encode edited values back into
//! `(settingsConfig, meta)` while preserving unknown native keys, render a live
//! [`PreviewFile`] of the exact files the app will receive, and validate the
//! result. The GPUI editor renders any app generically by walking the schema.

use serde_json::Value;

use crate::model::ProviderMeta;
use crate::AppType;

mod claude;
mod claude_desktop;
mod codex;
mod grokbuild;
mod hermes;
mod openclaw;
mod opencode;

pub use claude::ClaudeConfig;
pub use claude_desktop::ClaudeDesktopConfig;
pub use codex::CodexConfig;
pub use grokbuild::GrokBuildConfig;
pub use hermes::HermesConfig;
pub use openclaw::OpenClawConfig;
pub use opencode::OpenCodeConfig;

/// Field id -> current value. Text/secret/select live as `Value::String`,
/// toggles as `Value::Bool`, key/value maps as `Value::Object`, model grids as
/// `Value::Array` of row objects.
pub type FormValues = serde_json::Map<String, Value>;

/// A selectable option for [`FieldKind::Select`].
#[derive(Debug, Clone)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The cell type of one [`FieldKind::ModelGrid`] column.
#[derive(Debug, Clone)]
pub enum GridCellKind {
    Text { placeholder: String },
    Toggle,
}

/// One column of a [`FieldKind::ModelGrid`] row.
#[derive(Debug, Clone)]
pub struct GridColumn {
    pub key: String,
    pub label: String,
    pub kind: GridCellKind,
}

impl GridColumn {
    pub fn text(
        key: impl Into<String>,
        label: impl Into<String>,
        placeholder: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: GridCellKind::Text {
                placeholder: placeholder.into(),
            },
        }
    }

    pub fn toggle(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: GridCellKind::Toggle,
        }
    }
}

/// The widget a [`FormField`] renders as.
#[derive(Debug, Clone)]
pub enum FieldKind {
    Text {
        placeholder: String,
    },
    Secret {
        placeholder: String,
    },
    Select {
        options: Vec<SelectOption>,
    },
    Toggle,
    /// String -> string map (e.g. `query_params`, `http_headers`).
    KeyValue {
        key_placeholder: String,
        value_placeholder: String,
    },
    /// A list of model rows; each row is a JSON object keyed by [`GridColumn::key`].
    ModelGrid {
        columns: Vec<GridColumn>,
    },
}

/// One editable field.
#[derive(Debug, Clone)]
pub struct FormField {
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
    pub help: Option<String>,
    pub required: bool,
    /// Only render when the named sibling field currently equals this value.
    pub visible_when: Option<(String, String)>,
}

impl FormField {
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            help: None,
            required: false,
            visible_when: None,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn visible_when(mut self, field: impl Into<String>, equals: impl Into<String>) -> Self {
        self.visible_when = Some((field.into(), equals.into()));
        self
    }

    /// Whether this field should render given the current values (honours
    /// `visible_when`).
    pub fn is_visible(&self, values: &FormValues) -> bool {
        match &self.visible_when {
            Some((field, expected)) => str_val(values, field) == expected,
            None => true,
        }
    }
}

/// A titled group of fields. `advanced` sections can render collapsed.
#[derive(Debug, Clone)]
pub struct FormSection {
    pub title: String,
    pub fields: Vec<FormField>,
    pub advanced: bool,
}

impl FormSection {
    pub fn new(title: impl Into<String>, fields: Vec<FormField>) -> Self {
        Self {
            title: title.into(),
            fields,
            advanced: false,
        }
    }

    pub fn advanced(mut self) -> Self {
        self.advanced = true;
        self
    }
}

/// Severity of a [`ConfigIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A validation finding for the current form values.
#[derive(Debug, Clone)]
pub struct ConfigIssue {
    pub severity: Severity,
    pub field: Option<String>,
    pub message: String,
}

impl ConfigIssue {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            field: None,
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            field: None,
            message: message.into(),
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            field: None,
            message: message.into(),
        }
    }

    pub fn for_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
}

/// Source language of a [`PreviewFile`], for syntax presentation in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Toml,
    Json,
    Yaml,
    Env,
}

/// One file the app will receive when this provider is applied.
#[derive(Debug, Clone)]
pub struct PreviewFile {
    pub filename: String,
    pub language: Language,
    pub content: String,
}

/// Result of encoding edited values back into a provider's persisted shape.
#[derive(Debug, Clone, Default)]
pub struct EncodeResult {
    pub settings_config: Value,
    pub meta: Option<ProviderMeta>,
}

/// A named one-click configuration that pre-fills the form values.
#[derive(Debug, Clone)]
pub struct Preset {
    pub name: String,
    pub values: FormValues,
}

/// A per-app structured config codec backing the provider editor.
pub trait AppConfig {
    /// The open [`AppId`](crate::app_id::AppId) of the app this codec serves.
    ///
    /// Built-in codecs return their `AppType`'s id; the manifest codec returns
    /// its manifest id. This replaced the old `app() -> AppType` accessor so
    /// user-defined manifest apps (which have no `AppType`) can implement it.
    fn app_id(&self) -> crate::app_id::AppId;

    /// The structured fields, grouped into sections.
    fn schema(&self) -> Vec<FormSection>;

    /// Populate form values from an existing provider (or defaults for a new one
    /// when `settings_config` is empty/null).
    fn decode(&self, settings_config: &Value, meta: Option<&ProviderMeta>) -> FormValues;

    /// Build the persisted `(settingsConfig, meta)` from edited values, merging
    /// into `prior` so unknown/native keys survive a round-trip.
    fn encode(
        &self,
        values: &FormValues,
        prior: &Value,
        prior_meta: Option<&ProviderMeta>,
    ) -> EncodeResult;

    /// The exact file(s) the app will receive, for live preview.
    ///
    /// `prior` is the authoritative working document (the stored
    /// `settingsConfig`, or the result of a direct file edit): the preview must
    /// merge form values ONTO it, exactly like [`encode`](AppConfig::encode),
    /// so sibling/unknown keys the form doesn't manage still show up. Pass
    /// `Value::Null` for a from-scratch preview.
    fn preview(&self, values: &FormValues, prior: &Value) -> Vec<PreviewFile>;

    /// Validate edited values.
    fn validate(&self, values: &FormValues) -> Vec<ConfigIssue>;

    /// Validate edited values with provider-level context that is intentionally
    /// not stored inside [`FormValues`].
    ///
    /// Most codecs do not need this distinction. Codecs with a first-party
    /// login mode can override it so an `official` provider does not inherit
    /// third-party endpoint/API-key requirements.
    fn validate_for_category(
        &self,
        values: &FormValues,
        _category: Option<&str>,
    ) -> Vec<ConfigIssue> {
        self.validate(values)
    }

    /// Built-in one-click presets for common providers (empty by default).
    fn presets(&self) -> Vec<Preset> {
        Vec::new()
    }

    /// Inverse of [`preview`](AppConfig::preview): reconstruct a `settingsConfig`
    /// from the (possibly hand-edited) native file contents, given in the same
    /// order `preview` emits them. Powers direct file editing in the editor.
    /// Returns `Err` for apps whose config can't round-trip through files.
    fn parse_files(&self, _contents: &[String]) -> Result<Value, String> {
        Err("此应用暂不支持直接编辑文件，请使用上方表单。".to_string())
    }
}

/// The structured codec for an app, or `None` for apps not yet migrated off the
/// legacy generic form.
pub fn config_for(app: AppType) -> Option<Box<dyn AppConfig>> {
    match app {
        AppType::Claude => Some(Box::new(ClaudeConfig)),
        AppType::ClaudeDesktop => Some(Box::new(ClaudeDesktopConfig)),
        AppType::Codex => Some(Box::new(CodexConfig)),
        AppType::GrokBuild => Some(Box::new(GrokBuildConfig)),
        AppType::OpenCode => Some(Box::new(OpenCodeConfig)),
        AppType::OpenClaw => Some(Box::new(OpenClawConfig)),
        AppType::Hermes => Some(Box::new(HermesConfig)),
    }
}

// ---- FormValues helpers -----------------------------------------------------

/// Read a string field (empty string when missing or not a string).
pub fn str_val<'a>(values: &'a FormValues, id: &str) -> &'a str {
    values.get(id).and_then(Value::as_str).unwrap_or("")
}

/// Read a boolean field (false when missing).
pub fn bool_val(values: &FormValues, id: &str) -> bool {
    values.get(id).and_then(Value::as_bool).unwrap_or(false)
}

/// Set a string field.
pub fn set_str(values: &mut FormValues, id: &str, value: impl Into<String>) {
    values.insert(id.to_string(), Value::String(value.into()));
}

/// Set a boolean field.
pub fn set_bool(values: &mut FormValues, id: &str, value: bool) {
    values.insert(id.to_string(), Value::Bool(value));
}
