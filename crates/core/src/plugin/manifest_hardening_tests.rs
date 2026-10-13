//! End-to-end contracts for a plugin whose ID is unknown to the host enum.

use std::fs;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{AppManifest, AppPlugin, HookRegistry, LiveConfigOps, ManifestPlugin, ManifestSource};
use crate::provider_config::{AppConfig, set_str};

const NOVA: &str = include_str!("fixtures/audit-nova.toml");

fn manifest() -> AppManifest {
    AppManifest::parse(NOVA).unwrap()
}

fn plugin_at(root: &std::path::Path) -> Arc<ManifestPlugin> {
    let mut manifest = manifest();
    manifest.app.config_dir.default = root.to_string_lossy().into_owned();
    ManifestPlugin::from_manifest(
        manifest,
        Arc::new(HookRegistry::new()),
        ManifestSource::User(root.join("plugin.toml")),
    )
    .unwrap()
}

fn provider(settings: Value) -> crate::Provider {
    crate::Provider::with_id("nova-one".into(), "Nova One".into(), settings, None)
}

#[test]
fn unknown_app_form_file_roundtrip_preserves_nested_siblings_and_grid() {
    let codec = super::ManifestCodec::new(Arc::new(manifest()), Arc::new(HookRegistry::new()));
    assert!(crate::AppType::from_app_id(&codec.app_id()).is_none());
    let original = json!({
        "unmanaged_store": { "leave": true },
        "config": {
            "endpoint": { "url": "https://before.invalid", "timeout": 45 },
            "model": "before-model", "telemetry": { "enabled": false, "sample": 3 },
            "models": [{ "id": "extra-model", "enabled": true }],
            "native": { "array": [1, 2], "enabled": true }
        },
        "auth": { "credentials": { "token": "synthetic-secret", "account": "preserve" } }
    });
    let mut values = codec.decode(&original, None);
    set_str(&mut values, "model", "after-model");
    set_str(&mut values, "api_key", "");
    let encoded = codec.encode(&values, &original, None).settings_config;
    assert_eq!(encoded["config"]["endpoint"]["timeout"], 45);
    assert_eq!(encoded["config"]["native"], original["config"]["native"]);
    assert_eq!(encoded["config"]["telemetry"]["sample"], 3);
    assert_eq!(encoded["config"]["model"], "after-model");
    assert_eq!(encoded["config"]["models"], original["config"]["models"]);
    assert_eq!(
        encoded["auth"]["credentials"],
        json!({"account": "preserve"})
    );
    assert_eq!(encoded["unmanaged_store"], original["unmanaged_store"]);

    let preview = codec.preview(&values, &original);
    let parsed = codec
        .parse_files(
            &preview
                .iter()
                .map(|file| file.content.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
    assert_eq!(parsed["config"], encoded["config"]);
    assert_eq!(parsed["auth"], encoded["auth"]);
}

#[test]
fn keyvalue_removal_preserves_values_the_control_does_not_represent() {
    let mut manifest = manifest();
    let field: super::manifest::FieldSpec = toml::from_str(
        r#"
id = "extras"
label = "Extras"
kind = "keyvalue"
map = { file = "config", rule = "rest" }
"#,
    )
    .unwrap();
    manifest.sections[1].fields.push(field);
    manifest.check(&HookRegistry::new()).unwrap();
    let codec = super::ManifestCodec::new(Arc::new(manifest), Arc::new(HookRegistry::new()));
    let original = json!({"config": {
        "model": "model-one", "remove_me": "a string",
        "unrepresented": {"keep": true}, "number": 17
    }});
    let mut values = codec.decode(&original, None);
    values.insert("extras".into(), json!({"new_key": "new value"}));
    let out = codec.encode(&values, &original, None).settings_config;
    assert!(out["config"].get("remove_me").is_none());
    assert_eq!(out["config"]["new_key"], "new value");
    assert_eq!(out["config"]["unrepresented"], json!({"keep": true}));
    assert_eq!(out["config"]["number"], 17);
}

#[test]
fn required_fields_are_enforced_and_toggle_conditions_are_typed() {
    let mut manifest = manifest();
    manifest.sections[0].fields[1].visible_when = Some(super::manifest::Condition {
        field: "telemetry".into(),
        equals: "true".into(),
    });
    let codec = super::ManifestCodec::new(Arc::new(manifest), Arc::new(HookRegistry::new()));
    let mut values = codec.decode(&Value::Null, None);
    let issues = codec.validate(&values);
    assert!(
        issues
            .iter()
            .any(|issue| issue.field.as_deref() == Some("base_url"))
    );
    assert!(
        !issues
            .iter()
            .any(|issue| issue.field.as_deref() == Some("api_key"))
    );
    values.insert("telemetry".into(), Value::Bool(true));
    assert!(
        codec
            .validate(&values)
            .iter()
            .any(|issue| issue.field.as_deref() == Some("api_key"))
    );
}

#[test]
fn manifest_rejects_unsupported_mode_and_ambiguous_or_escaping_targets() {
    let hooks = HookRegistry::new();
    let mut candidate = manifest();
    candidate.app.mode = "additive".into();
    assert!(
        candidate
            .check(&hooks)
            .unwrap_err()
            .to_string()
            .contains("additive")
    );
    for path in [
        "../outside.json",
        "/tmp/outside.json",
        "C:/outside.json",
        "a/../../b",
        r"a\b",
        "a//b",
        "a/./b",
        "CON",
        "file.",
        "file ",
    ] {
        let mut candidate = manifest();
        candidate.files[0].path = path.into();
        assert!(candidate.check(&hooks).is_err(), "accepted {path}");
    }
    let mut candidate = manifest();
    candidate.files[1].path = "CONFIG.TOML".into();
    assert!(candidate.check(&hooks).is_err());
    let mut candidate = manifest();
    candidate.files[1].store_key = candidate.files[0].store_key.clone();
    assert!(candidate.check(&hooks).is_err());
    let mut candidate = manifest();
    candidate.sections[0].fields[2]
        .map
        .as_mut()
        .unwrap()
        .toml_path = Some("endpoint".into());
    assert!(
        candidate.check(&hooks).is_err(),
        "overlapping endpoint path accepted"
    );
}

#[test]
fn schema_secret_paths_do_not_depend_on_native_key_names() {
    let mut manifest = manifest();
    manifest.sections[0].fields[1].map.as_mut().unwrap().pointer =
        Some("/credentials/accessValue".into());
    let codec = super::ManifestCodec::new(Arc::new(manifest), Arc::new(HookRegistry::new()));
    assert_eq!(
        codec.secret_paths(),
        vec![vec![
            "auth".to_string(),
            "credentials".to_string(),
            "accessValue".to_string()
        ]]
    );
}

#[test]
fn adapter_revision_tracks_checked_manifest_and_ignores_comments() {
    let build = |content: &str| {
        ManifestPlugin::from_manifest(
            AppManifest::parse(content).unwrap(),
            Arc::new(HookRegistry::new()),
            ManifestSource::User("synthetic.toml".into()),
        )
        .unwrap()
    };
    let first = build(NOVA);
    let same = build(&format!("# additional comment\n{NOVA}"));
    let changed = build(&NOVA.replace("endpoint.url", "endpoint.base_url"));
    assert!(first.configuration_revision().is_some());
    assert_eq!(
        first.configuration_revision(),
        same.configuration_revision()
    );
    assert_ne!(
        first.configuration_revision(),
        changed.configuration_revision()
    );
}

#[test]
fn prepared_manifest_bytes_include_live_merge_and_execute_without_reencoding() {
    let home = crate::test_support::TestHome::new();
    let root = home.path().join("nova");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("config.toml"),
        "model = \"before\"\nexternal = true\n",
    )
    .unwrap();
    fs::write(root.join("auth.json"), "{\"old\":true}").unwrap();
    let plugin = plugin_at(&root);
    let db = crate::db::Database::memory().unwrap();
    let provider = provider(json!({
        "config": {"model": "after"},
        "auth": {"credentials": {"token": "synthetic-secret"}}
    }));
    let plan = plugin.prepare_write(&db, &provider).unwrap().unwrap();
    assert_eq!(plan.files.len(), 2);
    let config_plan = &plan.files[0];
    let after = String::from_utf8(config_plan.after.clone().unwrap()).unwrap();
    assert!(after.contains("external = true"));
    assert!(
        fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("before")
    );
    assert_eq!(
        config_plan.before.as_ref().unwrap(),
        &fs::read(root.join("config.toml")).unwrap()
    );
    crate::services::config_commit::execute_files(&plan).unwrap();
    for file in &plan.files {
        assert_eq!(fs::read(&file.path).unwrap(), *file.after.as_ref().unwrap());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn absent_replace_file_stays_out_of_the_plan() {
    let home = crate::test_support::TestHome::new();
    let root = home.path().join("nova");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("auth.json"), "{\"keep\":true}").unwrap();
    let mut manifest = manifest();
    manifest.app.config_dir.default = root.to_string_lossy().into_owned();
    manifest.files[1].absent_preserves = true;
    let plugin = ManifestPlugin::from_manifest(
        manifest,
        Arc::new(HookRegistry::new()),
        ManifestSource::User(root.join("p.toml")),
    )
    .unwrap();
    let db = crate::db::Database::memory().unwrap();
    let plan = plugin
        .prepare_write(&db, &provider(json!({"config": {"model":"one"}})))
        .unwrap()
        .unwrap();
    assert_eq!(plan.files.len(), 1);
    assert!(!plan.files[0].path.ends_with("auth.json"));
    crate::services::config_commit::execute_files(&plan).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("auth.json")).unwrap(),
        "{\"keep\":true}"
    );
}

#[test]
fn invalid_second_file_prevents_all_writes() {
    let home = crate::test_support::TestHome::new();
    let root = home.path().join("nova");
    fs::create_dir_all(root.join("auth.json")).unwrap();
    fs::write(root.join("config.toml"), "model = \"before\"").unwrap();
    let plugin = plugin_at(&root);
    let db = crate::db::Database::memory().unwrap();
    assert!(
        plugin
            .write_live(
                &db,
                &provider(json!({"config":{"model":"after"},"auth":{}}))
            )
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(root.join("config.toml")).unwrap(),
        "model = \"before\""
    );
}

#[cfg(unix)]
#[test]
fn symlinked_descendant_is_refused_for_reads_and_writes() {
    let home = crate::test_support::TestHome::new();
    let root = home.path().join("nova");
    let outside = home.path().join("outside.json");
    fs::create_dir_all(&root).unwrap();
    fs::write(&outside, "{\"keep\":true}").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("auth.json")).unwrap();
    let plugin = plugin_at(&root);
    let db = crate::db::Database::memory().unwrap();
    assert!(plugin.read_live().is_err());
    assert!(
        plugin
            .prepare_write(&db, &provider(json!({"config":{},"auth":{}})))
            .is_err()
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "{\"keep\":true}");
    assert!(!root.join("config.toml").exists());
}

#[test]
fn failed_reload_keeps_the_previous_registry_and_held_adapter() {
    let _home = crate::test_support::TestHome::new();
    let directory = super::user_plugins_dir();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("nova.toml");
    fs::write(&path, NOVA).unwrap();
    assert!(super::reload_user_plugins().is_empty());
    let id = crate::AppId::parse("audit-nova").unwrap();
    let original = super::get_plugin(&id).unwrap();
    fs::write(&path, "malformed manifest").unwrap();
    assert!(!super::reload_user_plugins().is_empty());
    assert!(Arc::ptr_eq(&original, &super::get_plugin(&id).unwrap()));
    fs::write(&path, NOVA.replace("Nova QA", "Nova Updated")).unwrap();
    assert!(super::reload_user_plugins().is_empty());
    assert_eq!(
        super::get_plugin(&id).unwrap().display_name(),
        "Nova Updated"
    );
    assert_eq!(original.display_name(), "Nova QA");
    fs::remove_file(&path).unwrap();
    assert!(super::reload_user_plugins().is_empty());
    assert!(super::get_plugin(&id).is_none());
}
