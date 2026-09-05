use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

fn ochcli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home)
        .args(args)
        .output()
        .expect("run ochcli")
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

#[test]
fn version_does_not_initialize_the_data_store() {
    let home = tempfile::tempdir().unwrap();
    let output = ochcli(home.path(), &["--json", "version"]);
    let value = json(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["name"], "ochcli");
    assert!(!home.path().join(".ochub/ochub.db").exists());
}

#[test]
fn daemon_help_uses_the_single_cli_binary_without_starting_the_runtime() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["daemon", "run", "--help"])
        .output()
        .expect("run ochcli daemon help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    assert!(!home.path().join(".ochub/ochub.db").exists());
    assert!(!home.path().join(".ochub/runtime/owner.json").exists());
}

#[test]
fn node_status_reports_the_managed_layout_without_installing_it() {
    let home = tempfile::tempdir().unwrap();
    let value = json(&ochcli(home.path(), &["--json", "node", "status"]));
    assert_eq!(value["data"]["managed"], false);
    assert_eq!(value["data"]["currentVersion"], env!("CARGO_PKG_VERSION"));
    assert!(value["data"]["managedRoot"].as_str().is_some());
    assert!(value["data"]["commandLink"].as_str().is_some());
    assert!(!home.path().join(".ochub/ochub.db").exists());
}

#[test]
fn managed_install_plan_runs_the_daemon_from_ochcli() {
    let home = tempfile::tempdir().unwrap();
    let value = json(&ochcli(
        home.path(),
        &["--json", "--dry-run", "node", "install"],
    ));
    assert_eq!(value["data"]["action"], "install-managed-node");
    assert_eq!(
        value["data"]["service"]["arguments"],
        serde_json::json!(["daemon", "run"])
    );
    assert!(
        value["data"]["service"]["program"]
            .as_str()
            .unwrap()
            .ends_with("ochcli")
    );
    assert!(!home.path().join(".ochub/ochub.db").exists());
}

#[test]
fn gateway_lifecycle_dry_run_never_starts_or_changes_autostart() {
    let home = tempfile::tempdir().unwrap();
    for (command, action, enabled) in [
        ("start", "start-gateway", Some(true)),
        ("stop", "stop-gateway", Some(false)),
        ("restart", "restart-gateway", Some(true)),
        ("serve", "serve-gateway", None),
    ] {
        let value = json(&ochcli(
            home.path(),
            &["--json", "--dry-run", "gateway", command],
        ));
        assert_eq!(value["data"]["action"], action);
        assert_eq!(value["data"]["enabled"], serde_json::json!(enabled));
        assert_eq!(value["data"]["dryRun"], true);
    }

    assert!(!home.path().join(".ochub/runtime/owner.json").exists());
    let config = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "show"],
    ));
    assert_eq!(config["data"]["enabled"], false);
    assert!(!home.path().join(".ochub/runtime/owner.json").exists());

    let enabled = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "set", "--enabled", "true"],
    ));
    assert_eq!(enabled["data"]["enabled"], true);
    let stopped = json(&ochcli(home.path(), &["--json", "gateway", "stop"]));
    assert_eq!(stopped["data"]["stopped"], true);
    let config = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "show"],
    ));
    assert_eq!(config["data"]["enabled"], false);
}

#[test]
fn app_list_uses_canonical_ids_and_json_envelope() {
    let home = tempfile::tempdir().unwrap();
    let output = ochcli(home.path(), &["--json", "app", "list"]);
    let value = json(&output);
    let apps = value["data"].as_array().unwrap();
    assert!(apps.iter().any(|app| app["id"] == "grokbuild"));
    assert!(!apps.iter().any(|app| app["id"] == "grok-build"));
    assert_eq!(value["schemaVersion"], "1");
}

#[test]
fn provider_show_redacts_secrets_by_default() {
    let home = tempfile::tempdir().unwrap();
    let provider_path = home.path().join("provider.json");
    std::fs::write(
        &provider_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "id": "test-claude",
            "name": "Test Claude",
            "settingsConfig": {
                "env": {
                    "ANTHROPIC_BASE_URL": "https://example.com",
                    "ANTHROPIC_AUTH_TOKEN": "super-secret"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let add = ochcli(
        home.path(),
        &[
            "--json",
            "provider",
            "add",
            "--app",
            "claude",
            "--from",
            provider_path.to_str().unwrap(),
        ],
    );
    json(&add);

    let show = ochcli(
        home.path(),
        &[
            "--json",
            "provider",
            "show",
            "test-claude",
            "--app",
            "claude",
        ],
    );
    let value = json(&show);
    let encoded = serde_json::to_string(&value).unwrap();
    assert!(!encoded.contains("super-secret"));
    assert!(encoded.contains("******"));
    assert!(encoded.contains("https://example.com"));
}

#[test]
fn provider_add_supports_dotted_set_and_secret_file_without_leaking_secret() {
    let home = tempfile::tempdir().unwrap();
    let secret_path = home.path().join("token.txt");
    std::fs::write(&secret_path, "file-only-secret\n").unwrap();

    let add = ochcli(
        home.path(),
        &[
            "--json",
            "provider",
            "add",
            "--app",
            "claude",
            "--set",
            "id=patched-claude",
            "--set",
            "name=Patched Claude",
            "--set",
            "settingsConfig.env.ANTHROPIC_BASE_URL=https://example.test",
            "--secret",
            &format!(
                "settingsConfig.env.ANTHROPIC_AUTH_TOKEN=@{}",
                secret_path.display()
            ),
        ],
    );
    let add = json(&add);
    assert!(
        !serde_json::to_string(&add)
            .unwrap()
            .contains("file-only-secret")
    );

    let show = json(&ochcli(
        home.path(),
        &[
            "--json",
            "provider",
            "show",
            "patched-claude",
            "--app",
            "claude",
        ],
    ));
    let encoded = serde_json::to_string(&show).unwrap();
    assert_eq!(show["data"]["provider"]["name"], "Patched Claude");
    assert!(encoded.contains("https://example.test"));
    assert!(encoded.contains("******"));
    assert!(!encoded.contains("file-only-secret"));
}

#[test]
fn declarative_apply_is_idempotent_and_plan_redacts_environment_secrets() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join("desired.yaml");
    std::fs::write(
        &config_path,
        r#"
apiVersion: ochub.io/v1alpha1
kind: OcHubConfig
metadata:
  name: integration-test
spec:
  apps:
    - id: codex
      enabled: false
  providers:
    - id: declared-claude
      app: claude
      config:
        name: Declared Claude
        settingsConfig:
          env:
            ANTHROPIC_BASE_URL: https://declarative.example
            ANTHROPIC_AUTH_TOKEN:
              fromEnv: OCHCLI_TEST_PROVIDER_SECRET
"#,
    )
    .unwrap();

    let mut first = Command::new(env!("CARGO_BIN_EXE_ochcli"));
    let first = first
        .env("OCHUB_TEST_HOME", home.path())
        .env("OCHCLI_TEST_PROVIDER_SECRET", "declarative-secret")
        .args(["--json", "apply", "--file", config_path.to_str().unwrap()])
        .output()
        .expect("apply declarative config");
    let first = json(&first);
    assert_eq!(first["data"]["summary"]["create"], 1);
    assert_eq!(first["data"]["summary"]["update"], 1);
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("declarative-secret")
    );

    let mut second = Command::new(env!("CARGO_BIN_EXE_ochcli"));
    let second = second
        .env("OCHUB_TEST_HOME", home.path())
        .env("OCHCLI_TEST_PROVIDER_SECRET", "declarative-secret")
        .args(["--json", "plan", "--file", config_path.to_str().unwrap()])
        .output()
        .expect("plan declarative config");
    let second = json(&second);
    assert_eq!(second["data"]["summary"]["noop"], 2);
    let encoded = serde_json::to_string(&second).unwrap();
    assert!(!encoded.contains("declarative-secret"));
    assert!(encoded.contains("******"));
}

#[test]
fn destructive_delete_requires_yes() {
    let home = tempfile::tempdir().unwrap();
    let output = ochcli(
        home.path(),
        &["--json", "backup", "delete", "missing.sqlite3"],
    );
    assert_eq!(output.status.code(), Some(2));
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "INVALID_ARGUMENT");
}

#[cfg(unix)]
#[test]
fn daemon_rpc_executes_mutations_and_blocks_direct_bypass() {
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let home = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ochcli daemon");
    let mut child = ChildGuard(child);

    let mut status = None;
    for _ in 0..50 {
        let output = ochcli(home.path(), &["--json", "daemon", "status"]);
        if output.status.success() {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if value["data"]["running"] == true {
                status = Some(value);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(status.is_some(), "daemon did not become ready");

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let gateway_port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    let gateway_config = json(&ochcli(
        home.path(),
        &[
            "--json",
            "gateway",
            "config",
            "set",
            "--port",
            &gateway_port,
            "--health-interval",
            "0",
        ],
    ));
    assert_eq!(gateway_config["data"]["enabled"], false);

    let gateway_started = json(&ochcli(home.path(), &["--json", "gateway", "start"]));
    assert_eq!(gateway_started["data"]["running"], true);
    assert_eq!(gateway_started["data"]["port"].to_string(), gateway_port);
    let gateway_config = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "show"],
    ));
    assert_eq!(gateway_config["data"]["enabled"], true);

    let gateway_stopped = json(&ochcli(home.path(), &["--json", "gateway", "stop"]));
    assert_eq!(gateway_stopped["data"]["stopped"], true);
    let gateway_config = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "show"],
    ));
    assert_eq!(gateway_config["data"]["enabled"], false);

    let mutation = json(&ochcli(home.path(), &["--json", "app", "disable", "codex"]));
    assert_eq!(mutation["data"]["enabled"], false);
    assert_eq!(mutation["meta"]["source"], "owner");

    let direct = ochcli(home.path(), &["--json", "--direct", "app", "list"]);
    assert_eq!(direct.status.code(), Some(4));
    let error: serde_json::Value = serde_json::from_slice(&direct.stderr).unwrap();
    assert_eq!(error["error"]["code"], "OWNER_CONFLICT");

    let stopped = json(&ochcli(home.path(), &["--json", "daemon", "stop"]));
    assert_eq!(stopped["data"]["stopped"], true);
    let status = child.0.wait().expect("wait for ochcli daemon");
    assert!(status.success());
    std::mem::forget(child);
}

#[test]
fn real_chatgpt_station_apply_preserves_live_login_and_changes_route_binding() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    drop(listener);
    json(&ochcli(
        home.path(),
        &[
            "--json",
            "gateway",
            "config",
            "set",
            "--port",
            &port,
            "--health-interval",
            "0",
        ],
    ));
    let auth_dir = home.path().join(".codex");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let original = "{\n  \"tokens\": {\"access_token\":\"real-access\",\"refresh_token\":\"real-refresh\",\"id_token\":\"real-id\",\"account_id\":\"real-account\"}\n}\n";
    std::fs::write(auth_dir.join("auth.json"), original).unwrap();
    let mut prior_binding = None;
    for name in ["first", "second"] {
        let path = home.path().join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({
            "id": name, "name": name, "default_model":"test-model",
            "channels":[{"id":format!("{name}-responses"),"name":"Responses","dialect":"responses","base_url":"http://127.0.0.1:9","api_key":"upstream-secret","models":["test-model"],"enabled":true,"priority":0,"weight":1}]
        })).unwrap()).unwrap();
        let added = json(&ochcli(
            home.path(),
            &["--json", "station", "add", "--from", path.to_str().unwrap()],
        ));
        let id = added["data"]["id"].as_str().unwrap();
        json(&ochcli(
            home.path(),
            &["--json", "station", "apply", id, "--app", "codex"],
        ));
        assert_eq!(
            std::fs::read_to_string(auth_dir.join("auth.json")).unwrap(),
            original
        );
        let text = std::fs::read_to_string(auth_dir.join("config.toml")).unwrap();
        assert!(text.contains("requires_openai_auth = true"));
        assert!(text.contains("/backend-api/codex"));
        assert!(!text.contains("experimental_bearer_token"));
        let config = json(&ochcli(
            home.path(),
            &["--json", "gateway", "config", "show"],
        ));
        assert_eq!(config["data"]["codex_backend_accept_any_bearer"], true);
        let binding = config["data"]["codex_backend_oauth_key_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(prior_binding.as_ref(), Some(&binding));
        prior_binding = Some(binding);
    }
    let config = json(&ochcli(
        home.path(),
        &["--json", "gateway", "config", "set", "--clear-codex-oauth"],
    ));
    assert_eq!(config["data"]["codex_backend_accept_any_bearer"], false);
    assert!(config["data"]["codex_backend_oauth_key_id"].is_null());
}
