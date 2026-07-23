//! Acceptance tests for the generic manifest engine and user-manifest loader.

#![cfg(test)]

use std::sync::Arc;

use serde_json::{json, Value};

use crate::provider_config::{set_str, str_val, AppConfig};

use super::hooks::HookRegistry;
use super::manifest::AppManifest;
use super::manifest_codec::ManifestCodec;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

#[cfg(any())]
const AUTH_API_KEY: &str = "api_key";
#[cfg(any())]
const AUTH_OAUTH: &str = "oauth";
#[cfg(any())]
const ENV_BASE_URL: &str = "GOOGLE_GEMINI_BASE_URL";
#[cfg(any())]
const ENV_API_KEY: &str = "GEMINI_API_KEY";
#[cfg(any())]
const ENV_MODEL: &str = "GEMINI_MODEL";

/// The manifest-backed Gemini codec.
#[cfg(any())]
fn manifest_codec() -> ManifestCodec {
    super::builtin_gemini_plugin().codec()
}

/// Build a bare codec from an inline manifest (no hooks).
fn codec_from_toml(toml: &str) -> ManifestCodec {
    let manifest = AppManifest::parse(toml).expect("manifest parses");
    let hooks = Arc::new(HookRegistry::new());
    manifest.check(&hooks).expect("manifest checks");
    ManifestCodec::new(Arc::new(manifest), hooks)
}

// ---------------------------------------------------------------------------
// Shared Gemini equivalence suite (native GeminiConfig ⇔ ManifestCodec)
// ---------------------------------------------------------------------------

#[cfg(any())]
pub(crate) mod gemini_suite {
    use super::*;
    use crate::provider_config::FormValues;

    pub(crate) fn api_key_values() -> FormValues {
        let mut v = FormValues::new();
        set_str(&mut v, "auth_mode", AUTH_API_KEY);
        set_str(&mut v, "base_url", "https://gemini.example.com");
        set_str(&mut v, "api_key", "AIza-test");
        set_str(&mut v, "model", "gemini-2.5-pro");
        v
    }

    pub(crate) fn decode_defaults_for_new_provider(codec: &dyn AppConfig) {
        let values = codec.decode(&Value::Null, None);
        assert_eq!(str_val(&values, "auth_mode"), AUTH_OAUTH);
        assert_eq!(str_val(&values, "base_url"), "");
        assert_eq!(str_val(&values, "api_key"), "");
        assert_eq!(str_val(&values, "model"), "");
        assert!(values
            .get("extra_env")
            .and_then(Value::as_object)
            .unwrap()
            .is_empty());
    }

    pub(crate) fn decode_empty_object_is_oauth(codec: &dyn AppConfig) {
        let values = codec.decode(&json!({ "env": {}, "config": {} }), None);
        assert_eq!(str_val(&values, "auth_mode"), AUTH_OAUTH);
    }

    pub(crate) fn encode_api_key_writes_env_keys(codec: &dyn AppConfig) {
        let result = codec.encode(&api_key_values(), &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert_eq!(env[ENV_BASE_URL], json!("https://gemini.example.com"));
        assert_eq!(env[ENV_API_KEY], json!("AIza-test"));
        assert_eq!(env[ENV_MODEL], json!("gemini-2.5-pro"));
    }

    pub(crate) fn encode_omits_empty_base_url_and_key(codec: &dyn AppConfig) {
        let mut v = FormValues::new();
        set_str(&mut v, "auth_mode", AUTH_API_KEY);
        set_str(&mut v, "base_url", "");
        set_str(&mut v, "api_key", "");
        set_str(&mut v, "model", "gemini-2.5-flash");
        let result = codec.encode(&v, &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert!(
            !env.contains_key(ENV_BASE_URL),
            "empty base_url leaked: {env:?}"
        );
        assert!(
            !env.contains_key(ENV_API_KEY),
            "empty api_key leaked: {env:?}"
        );
        assert_eq!(env[ENV_MODEL], json!("gemini-2.5-flash"));

        let files = codec.preview(&v, &Value::Null);
        let env_file = files.iter().find(|f| f.filename.ends_with(".env")).unwrap();
        assert!(
            !env_file.content.contains(ENV_BASE_URL),
            "{}",
            env_file.content
        );
        assert!(
            !env_file.content.contains(ENV_API_KEY),
            "{}",
            env_file.content
        );
    }

    pub(crate) fn encode_oauth_emits_empty_env(codec: &dyn AppConfig) {
        let mut v = api_key_values();
        set_str(&mut v, "auth_mode", AUTH_OAUTH);
        let result = codec.encode(&v, &Value::Null, None);
        let env = result.settings_config["env"].as_object().unwrap();
        assert!(env.is_empty(), "oauth env not empty: {env:?}");
    }

    pub(crate) fn encode_preserves_config_object_verbatim(codec: &dyn AppConfig) {
        let prior = json!({
            "env": { "GEMINI_API_KEY": "old" },
            "config": { "mcpServers": { "fs": { "command": "x" } }, "theme": "dark" }
        });
        let result = codec.encode(&api_key_values(), &prior, None);
        assert_eq!(
            result.settings_config["config"]["mcpServers"]["fs"]["command"],
            json!("x")
        );
        assert_eq!(result.settings_config["config"]["theme"], json!("dark"));
    }

    pub(crate) fn encode_preserves_unknown_sibling_keys(codec: &dyn AppConfig) {
        let prior = json!({ "env": {}, "config": {}, "modelCatalog": { "models": [] } });
        let result = codec.encode(&api_key_values(), &prior, None);
        assert!(result.settings_config.get("modelCatalog").is_some());
    }

    pub(crate) fn extra_env_round_trips(codec: &dyn AppConfig) {
        let mut v = api_key_values();
        v.insert(
            "extra_env".into(),
            json!({ "GEMINI_API_VERSION": "v1beta" }),
        );
        let encoded = codec.encode(&v, &Value::Null, None);
        let env = encoded.settings_config["env"].as_object().unwrap();
        assert_eq!(env["GEMINI_API_VERSION"], json!("v1beta"));

        let decoded = codec.decode(&encoded.settings_config, None);
        assert_eq!(
            decoded["extra_env"]["GEMINI_API_VERSION"].as_str(),
            Some("v1beta")
        );
        let extra = decoded["extra_env"].as_object().unwrap();
        assert!(!extra.contains_key(ENV_API_KEY));
    }

    pub(crate) fn round_trip_preserves_api_key_fields(codec: &dyn AppConfig) {
        let original = api_key_values();
        let encoded = codec.encode(&original, &Value::Null, None);
        let decoded = codec.decode(&encoded.settings_config, None);
        for key in ["auth_mode", "base_url", "api_key", "model"] {
            assert_eq!(
                str_val(&decoded, key),
                str_val(&original, key),
                "field {key}"
            );
        }
    }

    pub(crate) fn preview_emits_env_and_settings_files(codec: &dyn AppConfig) {
        let files = codec.preview(&api_key_values(), &Value::Null);
        assert_eq!(files.len(), 2);
        let env_file = files.iter().find(|f| f.filename.ends_with(".env")).unwrap();
        assert_eq!(env_file.language, Language::Env);
        assert!(
            env_file.content.contains("GEMINI_API_KEY=AIza-test"),
            "{}",
            env_file.content
        );
        assert!(env_file
            .content
            .contains("GOOGLE_GEMINI_BASE_URL=https://gemini.example.com"));
        assert!(env_file.content.contains("GEMINI_MODEL=gemini-2.5-pro"));

        let settings_file = files
            .iter()
            .find(|f| f.filename.ends_with("settings.json"))
            .unwrap();
        assert_eq!(settings_file.language, Language::Json);
    }

    pub(crate) fn validate_warns_missing_api_key(codec: &dyn AppConfig) {
        let mut v = api_key_values();
        set_str(&mut v, "api_key", "");
        let issues = codec.validate(&v);
        assert!(issues.iter().any(|i| i.field.as_deref() == Some("api_key")));
    }
}

/// Run every suite case against a codec.
#[cfg(any())]
fn run_gemini_suite(codec: &dyn AppConfig) {
    gemini_suite::decode_defaults_for_new_provider(codec);
    gemini_suite::decode_empty_object_is_oauth(codec);
    gemini_suite::encode_api_key_writes_env_keys(codec);
    gemini_suite::encode_omits_empty_base_url_and_key(codec);
    gemini_suite::encode_oauth_emits_empty_env(codec);
    gemini_suite::encode_preserves_config_object_verbatim(codec);
    gemini_suite::encode_preserves_unknown_sibling_keys(codec);
    gemini_suite::extra_env_round_trips(codec);
    gemini_suite::round_trip_preserves_api_key_fields(codec);
    gemini_suite::preview_emits_env_and_settings_files(codec);
    gemini_suite::validate_warns_missing_api_key(codec);
}

#[test]
#[cfg(any())]
fn gemini_suite_native_codec() {
    run_gemini_suite(&GeminiConfig);
}

#[test]
#[cfg(any())]
fn gemini_suite_manifest_codec() {
    run_gemini_suite(&manifest_codec());
}

// ---------------------------------------------------------------------------
// Schema equality
// ---------------------------------------------------------------------------

#[cfg(any())]
fn kind_tag(kind: &crate::provider_config::FieldKind) -> &'static str {
    match kind {
        FieldKind::Text { .. } => "text",
        FieldKind::Secret { .. } => "secret",
        FieldKind::Select { .. } => "select",
        FieldKind::Toggle => "toggle",
        FieldKind::KeyValue { .. } => "keyvalue",
        FieldKind::ModelGrid { .. } => "model_grid",
    }
}

#[cfg(any())]
type SchemaShape = Vec<(
    String,
    bool,
    Vec<(String, &'static str, Option<(String, String)>)>,
)>;

#[cfg(any())]
fn schema_shape(codec: &dyn AppConfig) -> SchemaShape {
    codec
        .schema()
        .iter()
        .map(|section| {
            let fields = section
                .fields
                .iter()
                .map(|f| (f.id.clone(), kind_tag(&f.kind), f.visible_when.clone()))
                .collect();
            (section.title.clone(), section.advanced, fields)
        })
        .collect()
}

#[test]
#[cfg(any())]
fn manifest_schema_matches_native() {
    assert_eq!(schema_shape(&GeminiConfig), schema_shape(&manifest_codec()));
}

// ---------------------------------------------------------------------------
// Embedded manifest sanity
// ---------------------------------------------------------------------------

#[test]
#[cfg(any())]
fn embedded_gemini_manifest_parses_and_checks() {
    let manifest = AppManifest::parse(super::GEMINI_MANIFEST_TOML).expect("parses");
    manifest.check(&HookRegistry::builtin()).expect("checks");
    assert_eq!(manifest.app_id().unwrap().as_str(), "gemini");
    assert_eq!(manifest.mode(), super::AppMode::Switch);
    assert_eq!(manifest.accent_u32(), 0x4285f4);
}

// ---------------------------------------------------------------------------
// Live-write equivalence (unix): native write_gemini_live ⇔ manifest write path
// ---------------------------------------------------------------------------

#[cfg(any())]
mod live_equiv {
    use super::*;
    use crate::model::Provider;
    use crate::plugin::AppPlugin;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn provider(name: &str, settings: Value) -> Provider {
        Provider::with_id("p".into(), name.into(), settings, None)
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Write `provider` two ways into two temp homes and assert the resulting
    /// `.env` / `settings.json` contents and modes are identical.
    fn assert_equivalent(provider: &Provider, pre_settings: Option<&Value>) {
        let _guard = crate::test_support::env_lock();

        let home_native = tempfile::tempdir().unwrap();
        let home_manifest = tempfile::tempdir().unwrap();

        // Optionally seed an existing settings.json in both homes identically.
        if let Some(pre) = pre_settings {
            for home in [home_native.path(), home_manifest.path()] {
                let path = home.join(".gemini").join("settings.json");
                crate::paths::write_json_file(&path, pre).unwrap();
            }
        }

        // Native path.
        std::env::set_var("OCHUB_TEST_HOME", home_native.path());
        crate::settings::reload_settings().unwrap();
        crate::services::provider::live::write_gemini_live(provider).unwrap();

        // Manifest path.
        std::env::set_var("OCHUB_TEST_HOME", home_manifest.path());
        crate::settings::reload_settings().unwrap();
        let db = crate::db::Database::memory().unwrap();
        super::super::builtin_gemini_plugin()
            .live()
            .write_live(&db, provider)
            .unwrap();

        std::env::remove_var("OCHUB_TEST_HOME");
        crate::settings::reload_settings().ok();

        for rel in [".gemini/.env", ".gemini/settings.json"] {
            let a = home_native.path().join(rel);
            let b = home_manifest.path().join(rel);
            assert_eq!(a.exists(), b.exists(), "existence mismatch for {rel}");
            if a.exists() {
                assert_eq!(
                    fs::read(&a).unwrap(),
                    fs::read(&b).unwrap(),
                    "content mismatch for {rel}"
                );
                assert_eq!(mode_of(&a), mode_of(&b), "mode mismatch for {rel}");
            }
        }

        // The `.env` is always 0600 in both writers.
        assert_eq!(mode_of(&home_native.path().join(".gemini/.env")), 0o600);
    }

    #[test]
    fn api_key_provider() {
        let p = provider(
            "Generic Provider",
            json!({
                "env": {
                    "GEMINI_API_KEY": "sk-generic",
                    "GOOGLE_GEMINI_BASE_URL": "https://api.example.com",
                    "GEMINI_MODEL": "gemini-2.5-pro"
                },
                "config": {}
            }),
        );
        assert_equivalent(&p, None);
    }

    #[test]
    fn oauth_provider_empty_env() {
        let p = provider("My OAuth", json!({ "env": {}, "config": {} }));
        assert_equivalent(&p, None);
    }

    #[test]
    fn packycode_named_provider() {
        let p = provider(
            "PackyCode",
            json!({ "env": { "GEMINI_API_KEY": "packy-key" }, "config": {} }),
        );
        assert_equivalent(&p, None);
    }

    #[test]
    fn preexisting_settings_with_mcp_servers() {
        let pre = json!({ "mcpServers": { "fs": { "command": "x" } } });
        // config absent ⇒ absent_preserves keeps the on-disk settings.json.
        let p = provider(
            "Generic Two",
            json!({ "env": { "GEMINI_API_KEY": "sk-two" } }),
        );
        assert_equivalent(&p, Some(&pre));
    }
}

// ---------------------------------------------------------------------------
// Engine mapping primitives
// ---------------------------------------------------------------------------

const MINI_HEADER: &str = r##"manifest_version = 1
[app]
id = "mini"
name = "Mini"
accent = "#112233"
sort_order = 100
mode = "switch"
[app.icon]
glyph = "M"
[app.config_dir]
default = "~/.mini"
"##;

#[test]
fn pointer_mapping_into_json_store() {
    let toml = format!(
        r#"{MINI_HEADER}
[[files]]
id = "cfg"
path = "config.json"
format = "json"
store_key = "config"
[[sections]]
title = "S"
[[sections.fields]]
id = "token"
label = "Token"
kind = "text"
map = {{ file = "cfg", pointer = "/auth/token", omit_empty = true, trim = true }}
"#
    );
    let codec = codec_from_toml(&toml);

    let mut v = crate::provider_config::FormValues::new();
    set_str(&mut v, "token", "  abc  ");
    let encoded = codec.encode(&v, &Value::Null, None);
    assert_eq!(
        encoded.settings_config["config"]["auth"]["token"],
        json!("abc")
    );

    let decoded = codec.decode(&encoded.settings_config, None);
    assert_eq!(str_val(&decoded, "token"), "abc");

    // omit_empty drops an empty value entirely.
    let empty = crate::provider_config::FormValues::new();
    let encoded_empty = codec.encode(&empty, &Value::Null, None);
    assert!(encoded_empty.settings_config["config"]
        .as_object()
        .unwrap()
        .is_empty());
}

#[test]
fn toml_path_mapping() {
    let toml = format!(
        r#"{MINI_HEADER}
[[files]]
id = "cfg"
path = "config.toml"
format = "toml"
store_key = "cfg"
[[sections]]
title = "S"
[[sections.fields]]
id = "x"
label = "X"
kind = "text"
map = {{ file = "cfg", toml_path = "a.b" }}
"#
    );
    let codec = codec_from_toml(&toml);
    let mut v = crate::provider_config::FormValues::new();
    set_str(&mut v, "x", "v");
    let encoded = codec.encode(&v, &Value::Null, None);
    assert_eq!(encoded.settings_config["cfg"]["a"]["b"], json!("v"));
}

#[test]
fn passthrough_object_null_coercion() {
    let toml = format!(
        r#"{MINI_HEADER}
[[files]]
id = "data"
path = "data.json"
format = "json"
store_key = "data"
passthrough = true
"#
    );
    let codec = codec_from_toml(&toml);
    let v = crate::provider_config::FormValues::new();

    // object kept verbatim
    let out = codec.encode(&v, &json!({ "data": { "k": 1 } }), None);
    assert_eq!(out.settings_config["data"], json!({ "k": 1 }));
    // null kept
    let out = codec.encode(&v, &json!({ "data": null }), None);
    assert_eq!(out.settings_config["data"], json!(null));
    // non-object coerced to {}
    let out = codec.encode(&v, &json!({ "data": "oops" }), None);
    assert_eq!(out.settings_config["data"], json!({}));
    // absent stays absent
    let out = codec.encode(&v, &json!({}), None);
    assert!(out.settings_config.get("data").is_none());
}

#[test]
fn parse_files_error_path() {
    let toml = format!(
        r#"{MINI_HEADER}
[[files]]
id = "cfg"
path = "config.json"
format = "json"
store_key = "config"
"#
    );
    let codec = codec_from_toml(&toml);
    let err = codec.parse_files(&["{ not json".to_string()]).unwrap_err();
    assert!(err.contains("cfg"), "{err}");
}

#[test]
fn validate_require_and_when_rules() {
    let toml = format!(
        r#"{MINI_HEADER}
[[files]]
id = "cfg"
path = "config.json"
format = "json"
store_key = "config"
[[sections]]
title = "S"
[[sections.fields]]
id = "mode"
label = "Mode"
kind = "text"
[[sections.fields]]
id = "token"
label = "Token"
kind = "text"
map = {{ file = "cfg", pointer = "/token" }}
[[validate]]
rule = "require"
field = "token"
when = {{ field = "mode", equals = "on" }}
message = "token required"
"#
    );
    let codec = codec_from_toml(&toml);

    // when doesn't match ⇒ no issue
    let mut off = crate::provider_config::FormValues::new();
    set_str(&mut off, "mode", "off");
    assert!(codec.validate(&off).is_empty());

    // when matches + empty ⇒ error issue on the field
    let mut on = crate::provider_config::FormValues::new();
    set_str(&mut on, "mode", "on");
    let issues = codec.validate(&on);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].field.as_deref(), Some("token"));
    assert_eq!(issues[0].severity, crate::provider_config::Severity::Error);
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

mod loader_tests {
    use super::*;
    use std::fs;

    fn user_manifest(id: &str) -> String {
        format!(
            r##"manifest_version = 1
[app]
id = "{id}"
name = "User {id}"
accent = "#0a0b0c"
sort_order = 200
mode = "switch"
[app.icon]
glyph = "U"
[app.config_dir]
default = "~/.{id}"
[[files]]
id = "cfg"
path = "config.json"
format = "json"
store_key = "config"
"##
        )
    }

    #[test]
    fn missing_dir_is_empty() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", home.path());

        let loaded = super::super::loader::load_user_manifests(Arc::new(HookRegistry::new()));
        assert!(loaded.plugins.is_empty());
        assert!(loaded.errors.is_empty());

        std::env::remove_var("OCHUB_TEST_HOME");
    }

    #[test]
    fn bad_toml_becomes_error_not_panic() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", home.path());
        let dir = super::super::loader::user_plugins_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("broken.toml"), "this is not = valid = toml").unwrap();

        let loaded = super::super::loader::load_user_manifests(Arc::new(HookRegistry::new()));
        assert!(loaded.plugins.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].path.contains("broken.toml"));

        std::env::remove_var("OCHUB_TEST_HOME");
    }

    #[test]
    fn valid_manifest_registers_and_lists() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", home.path());
        let dir = super::super::loader::user_plugins_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mine.toml"), user_manifest("loadtest-one")).unwrap();

        let errors = super::super::loader::load_and_register_user_plugins();
        assert!(errors.is_empty(), "{errors:?}");
        let id = crate::app_id::AppId::parse("loadtest-one").unwrap();
        assert!(super::super::all_plugins().iter().any(|p| p.id() == &id));
        assert!(super::super::get_plugin(&id).unwrap().is_user_manifest());

        // cleanup
        super::super::unregister_plugin(&id).unwrap();
        std::env::remove_var("OCHUB_TEST_HOME");
    }

    #[test]
    fn builtin_id_collision_rejected() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", home.path());
        let dir = super::super::loader::user_plugins_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("clash.toml"), user_manifest("claude")).unwrap();

        let errors = super::super::loader::load_and_register_user_plugins();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].path.contains("clash.toml"));

        std::env::remove_var("OCHUB_TEST_HOME");
    }

    #[test]
    fn duplicate_user_ids_second_errors() {
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", home.path());
        let dir = super::super::loader::user_plugins_dir();
        fs::create_dir_all(&dir).unwrap();
        // sorted by filename: a.toml registers, b.toml collides.
        fs::write(dir.join("a.toml"), user_manifest("loadtest-dup")).unwrap();
        fs::write(dir.join("b.toml"), user_manifest("loadtest-dup")).unwrap();

        let errors = super::super::loader::load_and_register_user_plugins();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].path.contains("b.toml"));

        let id = crate::app_id::AppId::parse("loadtest-dup").unwrap();
        super::super::unregister_plugin(&id).unwrap();
        std::env::remove_var("OCHUB_TEST_HOME");
    }
}
