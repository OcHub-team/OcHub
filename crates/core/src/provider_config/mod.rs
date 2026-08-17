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

use crate::AppType;
use crate::model::ProviderMeta;

mod cherry_studio;
mod claude;
mod claude_desktop;
mod codex;
mod grokbuild;
mod hermes;
mod kimi_code;
mod openclaw;
mod opencode;
pub mod sponsors;

pub use cherry_studio::CherryStudioConfig;
pub use claude::ClaudeConfig;
pub use claude_desktop::ClaudeDesktopConfig;
pub use codex::CodexConfig;
pub use grokbuild::GrokBuildConfig;
pub use hermes::HermesConfig;
pub use kimi_code::{KimiCodeConfig, apply_official_defaults};
pub use openclaw::OpenClawConfig;
pub use opencode::OpenCodeConfig;
pub use sponsors::{RouteKind, Sponsor, SponsorId, SponsorRoute};

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
#[derive(Debug, Clone, Default)]
pub struct Preset {
    pub name: String,
    pub values: FormValues,
    pub category: Option<String>,
    pub display_name: Option<String>,
    pub website_url: Option<String>,
}

impl Preset {
    pub fn new(name: impl Into<String>, values: FormValues) -> Self {
        Self {
            name: name.into(),
            values,
            category: None,
            display_name: None,
            website_url: None,
        }
    }

    pub fn with_identity(
        mut self,
        category: impl Into<String>,
        display_name: impl Into<String>,
        website_url: impl Into<String>,
    ) -> Self {
        self.category = Some(category.into());
        self.display_name = Some(display_name.into());
        self.website_url = Some(website_url.into());
        self
    }
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
        AppType::CherryStudio => Some(Box::new(CherryStudioConfig)),
        AppType::Codex => Some(Box::new(CodexConfig)),
        AppType::GrokBuild => Some(Box::new(GrokBuildConfig)),
        AppType::KimiCode => Some(Box::new(KimiCodeConfig)),
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

// ---- Relay-station sourced channels -----------------------------------------

/// Whether the provider editor can offer "relay station" as a source when
/// creating a channel for this app. The station supplies endpoint + key via
/// the local gateway; the form only asks for model(s).
pub fn station_source_supported(app: AppType) -> bool {
    matches!(app, AppType::Claude | AppType::Codex)
}

/// What the selected station can back, for the fields a station-sourced
/// channel still lets the user decide.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct StationCapabilities {
    /// The station route has WebSocket transport enabled.
    pub websockets: bool,
    /// The station has an enabled Responses upstream, the only kind the
    /// gateway will hand a remote-compaction request to.
    pub remote_compaction: bool,
}

/// Schema field ids the station manages (endpoint, credentials, wire-level
/// options). The editor hides these when the source is a relay station and
/// [`inject_station_endpoint`] overwrites them, so stale values from a
/// previous direct-connection incarnation can never leak into the generated
/// config.
///
/// Fields the station *constrains* rather than owns — Codex's `auth_mode` and
/// `remote_compaction` — stay out of this list and are clamped instead by
/// [`clamp_station_fields`].
pub fn station_managed_fields(app: AppType) -> &'static [&'static str] {
    match app {
        AppType::Claude => &[
            "base_url",
            "auth_field",
            "api_key",
            "api_format",
            "custom_user_agent",
            "is_full_url",
        ],
        AppType::Codex => &[
            "provider_id",
            "name",
            "base_url",
            "api_key",
            "wire_api",
            "supports_websockets",
            "disable_response_storage",
            "query_params",
            "http_headers",
        ],
        _ => &[],
    }
}

/// Select options a station-sourced channel must not offer, because the
/// station's own credential is the only thing that reaches the gateway.
pub fn station_hidden_options(app: AppType, field_id: &str) -> &'static [&'static str] {
    match (app, field_id) {
        (AppType::Codex, "auth_mode") => codex::STATION_HIDDEN_AUTH_MODES,
        _ => &[],
    }
}

/// Help text replacing a field's schema help while the source is a relay
/// station, where the control keeps a different meaning. `supported` is false
/// when the selected station cannot back the field at all, in which case the
/// editor shows it disabled with the reason.
pub fn station_field_help(app: AppType, field_id: &str, supported: bool) -> Option<&'static str> {
    match (app, field_id) {
        (AppType::Codex, "auth_mode") => Some(
            "模型供应商模式下 Authorization 一律由网关签发的密钥承担；选择组合模式可在此之上保留 auth.json 里的 ChatGPT 登录态。",
        ),
        (AppType::Codex, "remote_compaction") if supported => Some(
            "远程压缩请求会转发到该模型供应商的 Responses 上游；开启后 config.toml 里的 provider name 会强制写为 OpenAI。",
        ),
        (AppType::Codex, "remote_compaction") => {
            Some("该模型供应商没有启用中的 Responses 上游，网关无法转发远程压缩请求。")
        }
        _ => None,
    }
}

/// Clamp the fields a station constrains but does not own, so a choice carried
/// over from direct-connection mode (or from a station with richer upstreams)
/// can never produce a config the gateway would reject.
pub fn clamp_station_fields(values: &mut FormValues, app: AppType, caps: StationCapabilities) {
    if app != AppType::Codex {
        return;
    }
    if !codex::station_auth_mode_supported(str_val(values, "auth_mode")) {
        set_str(values, "auth_mode", codex::AUTH_API_KEY);
    }
    if !caps.remote_compaction {
        set_bool(values, "remote_compaction", false);
    }
}

/// The `base_url` a given app's config expects for a relay `origin`.
///
/// Anthropic-dialect clients take the bare origin and append `/v1/messages`
/// themselves; OpenAI-dialect clients take `origin/v1`. This is the single
/// place that difference is encoded — both station injection
/// ([`inject_station_endpoint`]) and the sponsor catalogue
/// ([`sponsors`]) go through here.
pub fn dialect_base_url(app: AppType, origin: &str) -> String {
    let origin = origin.trim().trim_end_matches('/');
    match app {
        AppType::Claude | AppType::ClaudeDesktop => origin.to_string(),
        _ => format!("{origin}/v1"),
    }
}

/// Overwrite the station-managed fields of `values` so the codec encodes a
/// config that points at the local gateway. `base_url` is the running gateway
/// origin (no path); `key` is the gateway-issued client key. Codex provider
/// identity fields deliberately remain user-owned: `model_provider` is also
/// Codex's session-history bucket and must not inherit OcHub's internal UUID.
/// `caps` clamps the fields the user still owns ([`clamp_station_fields`]).
pub fn inject_station_endpoint(
    values: &mut FormValues,
    app: AppType,
    base_url: &str,
    key: &str,
    caps: StationCapabilities,
) {
    clamp_station_fields(values, app, caps);
    match app {
        AppType::Claude => {
            set_str(values, "base_url", dialect_base_url(app, base_url));
            set_str(values, "auth_field", "ANTHROPIC_AUTH_TOKEN");
            set_str(values, "api_key", key);
            // Station channels always speak native Anthropic Messages to the
            // gateway; conversion for foreign upstreams happens gateway-side.
            set_str(values, "api_format", "anthropic");
            set_str(values, "custom_user_agent", "");
            set_bool(values, "is_full_url", false);
        }
        AppType::Codex => {
            set_str(values, "base_url", dialect_base_url(app, base_url));
            set_str(values, "api_key", key);
            set_str(values, "wire_api", "responses");
            set_bool(values, "supports_websockets", caps.websockets);
            // The gateway does not implement the Responses store.
            set_bool(values, "disable_response_storage", true);
            set_str(values, "_legacy_env_key", "");
            values.insert("query_params".into(), Value::Object(Default::default()));
            values.insert("http_headers".into(), Value::Object(Default::default()));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Station injection and the sponsor catalogue must keep agreeing about
    /// which apps want `/v1` appended — [`dialect_base_url`] is the shared
    /// source of truth, and this pins the behaviour it replaced.
    #[test]
    fn station_injection_uses_dialect_base_url() {
        let caps = StationCapabilities {
            websockets: false,
            remote_compaction: false,
        };
        let origin = "http://127.0.0.1:8080";

        let mut claude = FormValues::new();
        inject_station_endpoint(&mut claude, AppType::Claude, origin, "k", caps);
        assert_eq!(str_val(&claude, "base_url"), "http://127.0.0.1:8080");

        let mut codex = FormValues::new();
        inject_station_endpoint(&mut codex, AppType::Codex, origin, "k", caps);
        assert_eq!(str_val(&codex, "base_url"), "http://127.0.0.1:8080/v1");
    }

    /// A trailing slash on the origin must not produce `//v1`.
    #[test]
    fn dialect_base_url_trims_trailing_slashes() {
        assert_eq!(
            dialect_base_url(AppType::Codex, "https://example.test/"),
            "https://example.test/v1"
        );
        assert_eq!(
            dialect_base_url(AppType::Claude, "https://example.test/"),
            "https://example.test"
        );
    }
}
