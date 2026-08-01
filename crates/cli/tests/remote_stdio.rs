use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use ochub_protocol::{
    Frame, GoodbyeFrame, HelloAckFrame, HelloFrame, PROTOCOL_MAX, PROTOCOL_MIN, RequestFrame,
    ResponseFrame, decode_frame, encode_frame, methods,
};

fn read_frame(reader: &mut impl BufRead) -> Frame {
    let mut line = Vec::new();
    let count = reader.read_until(b'\n', &mut line).expect("read frame");
    assert!(count > 0, "remote server closed before returning a frame");
    decode_frame(&line).expect("valid protocol frame")
}

fn start_remote(
    home: &Path,
    device_id: &str,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
    HelloAckFrame,
) {
    start_remote_mode(home, device_id, true)
}

fn start_remote_mode(
    home: &Path,
    device_id: &str,
    ephemeral: bool,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
    HelloAckFrame,
) {
    let mut args = vec!["remote", "serve", "--stdio"];
    if ephemeral {
        args.push("--ephemeral");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remote stdio server");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    stdin
        .write_all(
            &encode_frame(&Frame::Hello(HelloFrame {
                protocol_min: PROTOCOL_MIN,
                protocol_max: PROTOCOL_MAX,
                client_version: "integration-test".to_string(),
                locale: Some("en".to_string()),
                device_id: Some(device_id.to_string()),
            }))
            .unwrap(),
        )
        .unwrap();
    stdin.flush().unwrap();
    let ack = match read_frame(&mut stdout) {
        Frame::HelloAck(ack) => ack,
        frame => panic!("expected helloAck, got {frame:?}"),
    };
    (child, stdin, stdout, ack)
}

fn request(
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
    protocol_version: u32,
    request_id: &str,
    method: &str,
    params: serde_json::Value,
    conditions: (Option<&str>, Option<&str>),
) -> ResponseFrame {
    let (idempotency_key, expected_revision) = conditions;
    stdin
        .write_all(
            &encode_frame(&Frame::Request(RequestFrame {
                protocol_version,
                request_id: request_id.to_string(),
                method: method.to_string(),
                params,
                trace_id: Some(format!("trace-{request_id}")),
                idempotency_key: idempotency_key.map(str::to_string),
                expected_revision: expected_revision.map(str::to_string),
            }))
            .unwrap(),
        )
        .unwrap();
    stdin.flush().unwrap();
    match read_frame(stdout) {
        Frame::Response(response) => response,
        frame => panic!("expected response, got {frame:?}"),
    }
}

fn close_remote(
    mut child: std::process::Child,
    mut stdin: std::process::ChildStdin,
    protocol_reason: &str,
) {
    stdin
        .write_all(
            &encode_frame(&Frame::Goodbye(GoodbyeFrame {
                reason: protocol_reason.to_string(),
            }))
            .unwrap(),
        )
        .unwrap();
    drop(stdin);
    let status = child.wait().expect("wait for remote server");
    assert!(status.success());
}

#[test]
fn probe_reports_protocol_identity_without_starting_a_listener() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["--json", "remote", "probe"])
        .output()
        .expect("run remote probe");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["protocolMin"], PROTOCOL_MIN);
    assert_eq!(value["data"]["protocolMax"], PROTOCOL_MAX);
    assert!(value["data"]["node"]["id"].as_str().is_some());
    assert!(
        value["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "status.read")
    );
    assert!(
        value["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "node.update.read")
    );
    assert!(
        !value["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "node.update.install")
    );
}

#[test]
fn node_update_status_is_available_without_contacting_the_release_server() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) =
        start_remote(home.path(), "desktop-node-update-status");

    assert!(
        ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "node.update.read")
    );
    assert!(
        !ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "node.update.install")
    );

    let status = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "node-update-status",
        methods::NODE_UPDATE_STATUS,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(status.ok, "node update status error: {:?}", status.error);
    assert_eq!(status.data["currentVersion"], env!("CARGO_PKG_VERSION"));
    assert!(status.data["target"].as_str().is_some());
    assert_eq!(status.data["managed"], false);

    close_remote(child, stdin, "node-update-status-complete");
}

#[test]
fn node_update_install_and_relay_require_explicit_remote_policy() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".ochub");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("remote.toml"),
        concat!(
            "schemaVersion = 1\n",
            "enabled = true\n",
            "allowWrite = true\n",
            "allowGatewayLifecycle = true\n",
            "allowDaemonLifecycle = true\n",
            "allowSecretsWrite = false\n",
            "allowBackupRestore = false\n",
            "allowUpdateInstall = true\n",
        ),
    )
    .unwrap();

    let (child, stdin, _stdout, ack) = start_remote(home.path(), "desktop-node-update-policy");
    assert!(
        ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "node.update.install")
    );
    assert!(
        ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "node.update.relay")
    );
    close_remote(child, stdin, "node-update-policy-complete");
}

#[test]
fn stdio_server_negotiates_and_executes_a_typed_status_request() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["remote", "serve", "--stdio", "--ephemeral"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remote stdio server");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    stdin
        .write_all(
            &encode_frame(&Frame::Hello(HelloFrame {
                protocol_min: PROTOCOL_MIN,
                protocol_max: PROTOCOL_MAX,
                client_version: "integration-test".to_string(),
                locale: Some("en".to_string()),
                device_id: Some("test-desktop".to_string()),
            }))
            .unwrap(),
        )
        .unwrap();
    stdin.flush().unwrap();

    let ack = match read_frame(&mut stdout) {
        Frame::HelloAck(ack) => ack,
        frame => panic!("expected helloAck, got {frame:?}"),
    };
    assert_eq!(ack.protocol_version, PROTOCOL_MAX);
    assert!(!ack.node.id.is_empty());

    stdin
        .write_all(
            &encode_frame(&Frame::Request(RequestFrame {
                protocol_version: ack.protocol_version,
                request_id: "status-request-1".to_string(),
                method: methods::STATUS_READ.to_string(),
                params: serde_json::Value::Null,
                trace_id: Some("remote-stdio-test".to_string()),
                idempotency_key: None,
                expected_revision: None,
            }))
            .unwrap(),
        )
        .unwrap();
    stdin.flush().unwrap();

    let response = match read_frame(&mut stdout) {
        Frame::Response(response) => response,
        frame => panic!("expected response, got {frame:?}"),
    };
    assert_eq!(response.request_id, "status-request-1");
    assert!(response.ok, "remote error: {:?}", response.error);
    assert_eq!(
        response.data["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(response.data["dataDir"].as_str().is_some());

    stdin
        .write_all(
            &encode_frame(&Frame::Goodbye(GoodbyeFrame {
                reason: "test-complete".to_string(),
            }))
            .unwrap(),
        )
        .unwrap();
    drop(stdin);
    let status = child.wait().expect("wait for remote server");
    let mut stderr = String::new();
    BufReader::new(child.stderr.take().expect("child stderr"))
        .read_to_string(&mut stderr)
        .expect("read child stderr");
    assert!(status.success(), "stderr={stderr}",);
}

#[test]
fn plan_and_idempotency_survive_ssh_session_reconnects() {
    let home = tempfile::tempdir().unwrap();
    for (id, name, url) in [
        ("remote-first", "Remote First", "https://first.example"),
        ("remote-second", "Remote Second", "https://second.example"),
    ] {
        let path = home.path().join(format!("{id}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "id": id,
                "name": name,
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_BASE_URL": url,
                        "ANTHROPIC_AUTH_TOKEN": format!("secret-{id}")
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ochcli"))
            .env("OCHUB_TEST_HOME", home.path())
            .args([
                "--json",
                "--direct",
                "provider",
                "add",
                "--app",
                "claude",
                "--from",
                path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "provider add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let initial = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args([
            "--json",
            "--direct",
            "provider",
            "switch",
            "remote-first",
            "--app",
            "claude",
            "--on-drift",
            "discard",
        ])
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "initial switch failed: {}",
        String::from_utf8_lossy(&initial.stderr)
    );

    let (first_child, mut first_in, mut first_out, first_ack) =
        start_remote(home.path(), "desktop-reconnect-test");
    let plan = request(
        &mut first_in,
        &mut first_out,
        first_ack.protocol_version,
        "plan-1",
        methods::PROVIDER_SWITCH_PLAN,
        serde_json::json!({
            "app": "claude",
            "providerId": "remote-second",
            "onDrift": "discard"
        }),
        (None, None),
    );
    assert!(plan.ok, "plan error: {:?}", plan.error);
    let plan_id = plan.data["planId"].as_str().unwrap().to_string();
    let revision = plan.data["revision"].as_str().unwrap().to_string();
    assert_eq!(plan.data["operationId"], plan_id);
    close_remote(first_child, first_in, "simulate-ssh-reconnect");

    let (second_child, mut second_in, mut second_out, second_ack) =
        start_remote(home.path(), "desktop-reconnect-test");
    let applied = request(
        &mut second_in,
        &mut second_out,
        second_ack.protocol_version,
        "apply-1",
        methods::PROVIDER_SWITCH_APPLY,
        serde_json::json!({ "planId": plan_id }),
        (Some("stable-idempotency-key"), Some(&revision)),
    );
    assert!(applied.ok, "apply error: {:?}", applied.error);
    assert_eq!(applied.data["operationId"], plan_id);
    close_remote(second_child, second_in, "applied");

    let (third_child, mut third_in, mut third_out, third_ack) =
        start_remote(home.path(), "desktop-reconnect-test");
    let replay = request(
        &mut third_in,
        &mut third_out,
        third_ack.protocol_version,
        "apply-replay",
        methods::PROVIDER_SWITCH_APPLY,
        serde_json::json!({ "planId": plan_id }),
        (Some("stable-idempotency-key"), Some(&revision)),
    );
    assert!(replay.ok, "idempotent replay error: {:?}", replay.error);
    assert_eq!(replay.data, applied.data);
    close_remote(third_child, third_in, "replay-complete");

    let inspect = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["--json", "--direct", "operation", "inspect", &plan_id])
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "operation inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(record["data"]["id"], plan_id);
    assert_eq!(record["data"]["actor"], "remote-desktop");
    assert_eq!(record["data"]["state"], "completed");
    assert_eq!(
        record["data"]["inputSummary"]["deviceId"],
        "desktop-reconnect-test"
    );
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(!encoded.contains("secret-remote-first"));
    assert!(!encoded.contains("secret-remote-second"));
}

#[test]
fn remote_provider_crud_uses_typed_payloads_and_secret_policy() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-provider-crud");

    let baseline = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-create-baseline",
        methods::PROVIDER_CREATE,
        serde_json::json!({
            "app": "claude",
            "provider": {
                "id": "remote-baseline",
                "name": "Remote Baseline",
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://baseline.example.com"
                    }
                }
            },
            "addToLive": false
        }),
        (Some("provider-create-baseline-key"), None),
    );
    assert!(baseline.ok, "baseline create error: {:?}", baseline.error);

    let created = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-create",
        methods::PROVIDER_CREATE,
        serde_json::json!({
            "app": "claude",
            "provider": {
                "id": "remote-team",
                "name": "Remote Team",
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://old.example.com"
                    }
                }
            },
            "addToLive": false
        }),
        (Some("provider-create-key"), None),
    );
    assert!(created.ok, "create error: {:?}", created.error);
    assert_eq!(created.data["provider"]["id"], "remote-team");
    let replayed_create = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-create-replay",
        methods::PROVIDER_CREATE,
        serde_json::json!({
            "app": "claude",
            "provider": {
                "id": "remote-team",
                "name": "Remote Team",
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://old.example.com"
                    }
                }
            },
            "addToLive": false
        }),
        (Some("provider-create-key"), None),
    );
    assert!(
        replayed_create.ok,
        "idempotent create replay error: {:?}",
        replayed_create.error
    );
    assert_eq!(replayed_create.data, created.data);

    let updated = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-update",
        methods::PROVIDER_UPDATE,
        serde_json::json!({
            "app": "claude",
            "providerId": "remote-team",
            "patch": {
                "name": "Remote Team Updated",
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://new.example.com"
                    }
                }
            }
        }),
        (Some("provider-update-key"), None),
    );
    assert!(updated.ok, "update error: {:?}", updated.error);
    assert_eq!(updated.data["provider"]["name"], "Remote Team Updated");
    assert_eq!(
        updated.data["provider"]["settingsConfig"]["env"]["ANTHROPIC_BASE_URL"],
        "https://new.example.com"
    );

    let duplicated = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-duplicate",
        methods::PROVIDER_DUPLICATE,
        serde_json::json!({
            "app": "claude",
            "providerId": "remote-team"
        }),
        (Some("provider-duplicate-key"), None),
    );
    assert!(duplicated.ok, "duplicate error: {:?}", duplicated.error);
    let duplicate_id = duplicated.data["provider"]["id"]
        .as_str()
        .expect("duplicate provider id")
        .to_string();
    assert_ne!(duplicate_id, "remote-team");
    assert_eq!(
        duplicated.data["provider"]["settingsConfig"]["env"]["ANTHROPIC_BASE_URL"],
        "https://new.example.com"
    );

    let secret_denied = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-secret-update",
        methods::PROVIDER_UPDATE,
        serde_json::json!({
            "app": "claude",
            "providerId": "remote-team",
            "patch": {
                "settingsConfig": {
                    "env": {
                        "ANTHROPIC_AUTH_TOKEN": "must-not-cross-policy"
                    }
                }
            }
        }),
        (Some("provider-secret-key"), None),
    );
    assert!(!secret_denied.ok);
    assert_eq!(
        secret_denied
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("PERMISSION_DENIED")
    );
    assert!(
        !serde_json::to_string(&secret_denied)
            .unwrap()
            .contains("must-not-cross-policy")
    );

    let deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-delete",
        methods::PROVIDER_DELETE,
        serde_json::json!({
            "app": "claude",
            "providerId": "remote-team"
        }),
        (Some("provider-delete-key"), None),
    );
    assert!(deleted.ok, "delete error: {:?}", deleted.error);

    let duplicate_deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "provider-duplicate-delete",
        methods::PROVIDER_DELETE,
        serde_json::json!({
            "app": "claude",
            "providerId": duplicate_id
        }),
        (Some("provider-duplicate-delete-key"), None),
    );
    assert!(
        duplicate_deleted.ok,
        "duplicate delete error: {:?}",
        duplicate_deleted.error
    );

    close_remote(child, stdin, "provider-crud-complete");
}

#[test]
fn remote_mcp_crud_and_sync_use_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-mcp-crud");

    let created = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "mcp-upsert",
        methods::MCP_UPSERT,
        serde_json::json!({
            "server": {
                "id": "remote-mcp",
                "name": "Remote MCP",
                "server": {
                    "type": "stdio",
                    "command": "remote-mcp-command",
                    "args": []
                },
                "apps": {},
                "description": null,
                "homepage": null,
                "docs": null,
                "tags": []
            }
        }),
        (Some("mcp-upsert-key"), None),
    );
    assert!(created.ok, "mcp create error: {:?}", created.error);
    assert_eq!(created.data["id"], "remote-mcp");

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "mcp-list",
        methods::MCP_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(listed.ok, "mcp list error: {:?}", listed.error);
    assert!(
        listed
            .data
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["id"] == "remote-mcp"))
    );

    let enabled = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "mcp-enable",
        methods::MCP_SET_APP,
        serde_json::json!({
            "id": "remote-mcp",
            "app": "claude",
            "enabled": true
        }),
        (Some("mcp-enable-key"), None),
    );
    assert!(enabled.ok, "mcp enable error: {:?}", enabled.error);

    let synced = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "mcp-sync-all",
        methods::MCP_SYNC_ALL,
        serde_json::Value::Null,
        (Some("mcp-sync-all-key"), None),
    );
    assert!(synced.ok, "mcp sync error: {:?}", synced.error);
    assert_eq!(synced.data["synced"], 1);

    let deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "mcp-delete",
        methods::MCP_DELETE,
        serde_json::json!({ "id": "remote-mcp" }),
        (Some("mcp-delete-key"), None),
    );
    assert!(deleted.ok, "mcp delete error: {:?}", deleted.error);

    close_remote(child, stdin, "mcp-crud-complete");
}

#[test]
fn remote_skill_repository_crud_uses_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-skill-repo-crud");

    let installed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "skill-list",
        methods::SKILL_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(installed.ok, "skill list error: {:?}", installed.error);
    assert!(installed.data.is_array());

    let created = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "skill-repo-create",
        methods::SKILL_REPO_UPSERT,
        serde_json::json!({
            "repo": {
                "owner": "remote-test-owner",
                "name": "remote-test-skills",
                "branch": "main",
                "enabled": true
            }
        }),
        (Some("skill-repo-create-key"), None),
    );
    assert!(created.ok, "skill repo create error: {:?}", created.error);
    assert_eq!(created.data["owner"], "remote-test-owner");
    assert_eq!(created.data["enabled"], true);

    let updated = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "skill-repo-update",
        methods::SKILL_REPO_UPSERT,
        serde_json::json!({
            "originalId": "remote-test-owner/remote-test-skills",
            "repo": {
                "owner": "remote-test-owner",
                "name": "remote-test-skills",
                "branch": "develop",
                "enabled": false
            }
        }),
        (Some("skill-repo-update-key"), None),
    );
    assert!(updated.ok, "skill repo update error: {:?}", updated.error);
    assert_eq!(updated.data["branch"], "develop");
    assert_eq!(updated.data["enabled"], false);

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "skill-repo-list",
        methods::SKILL_REPO_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(listed.ok, "skill repo list error: {:?}", listed.error);
    assert!(listed.data.as_array().is_some_and(|repos| {
        repos.iter().any(|repo| {
            repo["owner"] == "remote-test-owner"
                && repo["name"] == "remote-test-skills"
                && repo["branch"] == "develop"
                && repo["enabled"] == false
        })
    }));

    let deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "skill-repo-delete",
        methods::SKILL_REPO_DELETE,
        serde_json::json!({ "id": "remote-test-owner/remote-test-skills" }),
        (Some("skill-repo-delete-key"), None),
    );
    assert!(deleted.ok, "skill repo delete error: {:?}", deleted.error);

    close_remote(child, stdin, "skill-repo-crud-complete");
}

#[test]
fn remote_usage_and_pricing_use_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-usage-pricing");

    let summary = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "usage-summary",
        methods::USAGE_SUMMARY,
        serde_json::json!({}),
        (None, None),
    );
    assert!(summary.ok, "usage summary error: {:?}", summary.error);
    assert_eq!(summary.data["totalRequests"], 0);

    let logs = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "usage-logs",
        methods::USAGE_LOGS,
        serde_json::json!({ "page": 0, "pageSize": 20 }),
        (None, None),
    );
    assert!(logs.ok, "usage logs error: {:?}", logs.error);
    assert_eq!(logs.data["total"], 0);
    assert!(logs.data["data"].is_array());

    let defaults = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "pricing-defaults-set",
        methods::PRICING_DEFAULTS_SET,
        serde_json::json!({
            "defaults": [
                {
                    "app": "claude",
                    "multiplier": "1.25",
                    "modelSource": "request"
                },
                {
                    "app": "codex",
                    "multiplier": "0.75",
                    "modelSource": "response"
                }
            ]
        }),
        (Some("pricing-defaults-set-key"), None),
    );
    assert!(
        defaults.ok,
        "pricing defaults set error: {:?}",
        defaults.error
    );
    assert!(defaults.data.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["app"] == "claude" && item["multiplier"] == "1.25")
    }));

    let override_set = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "pricing-override-set",
        methods::PRICING_OVERRIDE_SET,
        serde_json::json!({
            "modelId": "remote-test-model",
            "pricing": {
                "modelId": "remote-test-model",
                "displayName": "Remote Test Model",
                "inputCostPerMillion": "1",
                "outputCostPerMillion": "2",
                "cacheReadCostPerMillion": "0",
                "cacheCreationCostPerMillion": "0",
                "cacheCreation1hCostPerMillion": "0"
            }
        }),
        (Some("pricing-override-set-key"), None),
    );
    assert!(
        override_set.ok,
        "pricing override set error: {:?}",
        override_set.error
    );
    assert_eq!(override_set.data["modelId"], "remote-test-model");

    let override_list = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "pricing-override-list",
        methods::PRICING_OVERRIDE_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(
        override_list.ok,
        "pricing override list error: {:?}",
        override_list.error
    );
    assert!(override_list.data.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["modelId"] == "remote-test-model")
    }));

    let override_delete = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "pricing-override-delete",
        methods::PRICING_OVERRIDE_DELETE,
        serde_json::json!({ "modelId": "remote-test-model" }),
        (Some("pricing-override-delete-key"), None),
    );
    assert!(
        override_delete.ok,
        "pricing override delete error: {:?}",
        override_delete.error
    );

    close_remote(child, stdin, "usage-pricing-complete");
}

#[test]
fn remote_sessions_use_typed_read_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-sessions");

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "session-list",
        methods::SESSION_LIST,
        serde_json::json!({}),
        (None, None),
    );
    assert!(listed.ok, "session list error: {:?}", listed.error);
    assert!(listed.data.is_array());

    let missing = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "session-get-missing",
        methods::SESSION_GET,
        serde_json::json!({ "app": "codex", "id": "missing-session" }),
        (None, None),
    );
    assert!(!missing.ok);
    assert_eq!(
        missing.error.as_ref().map(|error| error.code.as_str()),
        Some("NOT_FOUND")
    );

    let built = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "session-index-build",
        methods::SESSION_INDEX_BUILD,
        serde_json::Value::Null,
        (Some("session-index-build-key"), None),
    );
    assert!(built.ok, "session index build error: {:?}", built.error);
    assert!(built.data["indexed"].as_u64().is_some());

    let status = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "session-index-status",
        methods::SESSION_INDEX_STATUS,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(status.ok, "session index status error: {:?}", status.error);
    assert!(status.data["stats"].is_object());

    let searched = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "session-search",
        methods::SESSION_SEARCH,
        serde_json::json!({ "query": "nothing-to-find", "limit": 10 }),
        (None, None),
    );
    assert!(searched.ok, "session search error: {:?}", searched.error);
    assert!(searched.data.is_array());

    close_remote(child, stdin, "sessions-complete");
}

#[test]
fn remote_station_crud_uses_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-stations");

    let created = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "station-create",
        methods::STATION_CREATE,
        serde_json::json!({
            "station": {
                "id": "remote-station",
                "name": "Remote Station",
                "channels": [{
                    "id": "remote-station-chat",
                    "name": "Remote Station Chat",
                    "dialect": "chat",
                    "base_url": "https://gateway.example.com",
                    "api_key": "",
                    "enabled": true
                }],
                "enabled": true
            }
        }),
        (Some("station-create-key"), None),
    );
    assert!(created.ok, "station create error: {:?}", created.error);
    assert_eq!(created.data["id"], "remote-station");

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "station-list",
        methods::STATION_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(listed.ok, "station list error: {:?}", listed.error);
    assert!(listed.data.as_array().is_some_and(|stations| {
        stations
            .iter()
            .any(|station| station["id"] == "remote-station")
    }));

    let updated = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "station-update",
        methods::STATION_UPDATE,
        serde_json::json!({
            "stationId": "remote-station",
            "patch": { "name": "Remote Station Updated" }
        }),
        (Some("station-update-key"), None),
    );
    assert!(updated.ok, "station update error: {:?}", updated.error);
    assert_eq!(updated.data["name"], "Remote Station Updated");

    let disabled = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "station-disable",
        methods::STATION_SET_ENABLED,
        serde_json::json!({ "stationId": "remote-station", "enabled": false }),
        (Some("station-disable-key"), None),
    );
    assert!(disabled.ok, "station disable error: {:?}", disabled.error);
    assert_eq!(disabled.data["enabled"], false);

    let deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "station-delete",
        methods::STATION_DELETE,
        serde_json::json!({ "stationId": "remote-station" }),
        (Some("station-delete-key"), None),
    );
    assert!(deleted.ok, "station delete error: {:?}", deleted.error);
    assert_eq!(deleted.data["deleted"], true);

    close_remote(child, stdin, "stations-complete");
}

#[test]
fn remote_proxy_settings_use_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-proxy");

    let saved = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "proxy-set",
        methods::PROXY_SET,
        serde_json::json!({
            "proxy": {
                "enabled": false,
                "protocol": "socks5",
                "host": "127.0.0.1",
                "port": 1080,
                "username": "",
                "password": ""
            }
        }),
        (Some("proxy-set-key"), None),
    );
    assert!(saved.ok, "proxy set error: {:?}", saved.error);
    assert_eq!(saved.data["protocol"], "socks5");
    assert_eq!(saved.data["port"], 1080);

    let loaded = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "proxy-get",
        methods::PROXY_GET,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(loaded.ok, "proxy get error: {:?}", loaded.error);
    assert_eq!(loaded.data["host"], "127.0.0.1");
    assert_eq!(loaded.data["port"], 1080);

    close_remote(child, stdin, "proxy-complete");
}

#[test]
fn remote_generic_settings_support_read_write_and_reset() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-settings");

    assert!(
        ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "settings.read")
    );
    assert!(
        ack.capabilities
            .iter()
            .any(|capability| capability.as_str() == "settings.write")
    );

    let initial = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "settings-initial",
        methods::SETTINGS_GET,
        serde_json::json!({ "path": "autoUpdateCheck" }),
        (None, None),
    );
    assert!(initial.ok, "settings get error: {:?}", initial.error);
    assert_eq!(initial.data, true);

    let saved = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "settings-set",
        methods::SETTINGS_SET,
        serde_json::json!({
            "path": "autoUpdateCheck",
            "value": false
        }),
        (Some("settings-set-key"), None),
    );
    assert!(saved.ok, "settings set error: {:?}", saved.error);
    assert_eq!(saved.data, false);

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "settings-list",
        methods::SETTINGS_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(listed.ok, "settings list error: {:?}", listed.error);
    assert_eq!(listed.data["autoUpdateCheck"], false);

    let reset = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "settings-unset",
        methods::SETTINGS_UNSET,
        serde_json::json!({ "path": "autoUpdateCheck" }),
        (Some("settings-unset-key"), None),
    );
    assert!(reset.ok, "settings unset error: {:?}", reset.error);
    assert_eq!(reset.data, true);

    let enabled = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "app-enable",
        methods::APP_SET_ENABLED,
        serde_json::json!({ "app": "hermes", "enabled": true }),
        (Some("app-enable-key"), None),
    );
    assert!(enabled.ok, "app enable error: {:?}", enabled.error);
    assert_eq!(enabled.data["id"], "hermes");
    assert_eq!(enabled.data["enabled"], true);

    close_remote(child, stdin, "settings-complete");
}

#[test]
fn remote_sync_configuration_and_backup_crud_use_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-maintenance");

    let configured = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "sync-configure",
        methods::SYNC_CONFIGURE,
        serde_json::json!({
            "backend": "webdav",
            "settings": {
                "enabled": false,
                "autoSync": false,
                "baseUrl": "https://dav.example.test",
                "username": "test-user",
                "password": "",
                "remoteRoot": "ochub-sync",
                "profile": "default"
            },
            "clearSecret": false
        }),
        (Some("sync-configure-key"), None),
    );
    assert!(
        configured.ok,
        "sync configure error: {:?}",
        configured.error
    );
    assert_eq!(
        configured.data["settings"]["baseUrl"],
        "https://dav.example.test"
    );

    let sync_status = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "sync-status",
        methods::SYNC_STATUS,
        serde_json::json!({ "backend": "webdav" }),
        (None, None),
    );
    assert!(sync_status.ok, "sync status error: {:?}", sync_status.error);
    assert_eq!(sync_status.data["settings"]["username"], "test-user");

    let created = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "backup-create",
        methods::BACKUP_CREATE,
        serde_json::json!({ "name": "remote-test" }),
        (Some("backup-create-key"), None),
    );
    assert!(created.ok, "backup create error: {:?}", created.error);
    let filename = created.data["filename"]
        .as_str()
        .expect("created backup filename")
        .to_string();

    let listed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "backup-list",
        methods::BACKUP_LIST,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(listed.ok, "backup list error: {:?}", listed.error);
    assert!(
        listed
            .data
            .as_array()
            .is_some_and(|items| { items.iter().any(|item| item["filename"] == filename) })
    );

    let renamed = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "backup-rename",
        methods::BACKUP_RENAME,
        serde_json::json!({ "id": filename, "name": "renamed-remote-test" }),
        (Some("backup-rename-key"), None),
    );
    assert!(renamed.ok, "backup rename error: {:?}", renamed.error);
    let renamed_filename = renamed.data["filename"]
        .as_str()
        .expect("renamed backup filename")
        .to_string();

    let deleted = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "backup-delete",
        methods::BACKUP_DELETE,
        serde_json::json!({ "id": renamed_filename }),
        (Some("backup-delete-key"), None),
    );
    assert!(deleted.ok, "backup delete error: {:?}", deleted.error);
    assert_eq!(deleted.data["deleted"], true);

    close_remote(child, stdin, "maintenance-complete");
}

#[test]
fn remote_data_directory_and_advanced_tools_use_typed_methods() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) = start_remote(home.path(), "desktop-advanced-tools");

    let data_path = home.path().join("relocated-data");
    let set = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "data-dir-set",
        methods::DATA_DIR_SET,
        serde_json::json!({ "path": data_path }),
        (Some("data-dir-set-key"), None),
    );
    assert!(set.ok, "data dir set error: {:?}", set.error);
    let canonical_data_path = data_path.canonicalize().unwrap();
    assert_eq!(set.data["path"].as_str(), canonical_data_path.to_str());

    let shown = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "data-dir-show",
        methods::DATA_DIR_SHOW,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(shown.ok, "data dir show error: {:?}", shown.error);
    assert_eq!(
        shown.data["persistentOverride"].as_str(),
        canonical_data_path.to_str()
    );

    let codex_status = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "advanced-read",
        methods::TOOL_ADVANCED_READ,
        serde_json::json!({
            "action": "codex.history.status",
            "params": null
        }),
        (None, None),
    );
    assert!(
        codex_status.ok,
        "advanced read error: {:?}",
        codex_status.error
    );
    assert!(codex_status.data["backupExists"].as_bool().is_some());

    let onboarding = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "advanced-write",
        methods::TOOL_ADVANCED_WRITE,
        serde_json::json!({
            "action": "claude.onboarding.skip",
            "params": null
        }),
        (Some("advanced-write-key"), None),
    );
    assert!(
        onboarding.ok,
        "advanced write error: {:?}",
        onboarding.error
    );
    assert_eq!(onboarding.data["completed"], true);

    let reset = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "data-dir-reset",
        methods::DATA_DIR_RESET,
        serde_json::Value::Null,
        (Some("data-dir-reset-key"), None),
    );
    assert!(reset.ok, "data dir reset error: {:?}", reset.error);
    assert_eq!(reset.data["takesEffectAfterRestart"], true);

    close_remote(child, stdin, "advanced-tools-complete");
}

#[test]
fn non_ephemeral_bridge_starts_an_owner_that_survives_disconnect() {
    let home = tempfile::tempdir().unwrap();
    let (child, mut stdin, mut stdout, ack) =
        start_remote_mode(home.path(), "desktop-owner-test", false);
    assert!(
        ack.runtime.persistent,
        "the normal bridge should route through a persistent owner"
    );
    let status = request(
        &mut stdin,
        &mut stdout,
        ack.protocol_version,
        "owner-status",
        methods::STATUS_READ,
        serde_json::Value::Null,
        (None, None),
    );
    assert!(
        status.ok,
        "owner-forwarded status failed: {:?}",
        status.error
    );
    close_remote(child, stdin, "desktop-disconnected");

    let daemon_status = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["--json", "daemon", "status"])
        .output()
        .unwrap();
    assert!(
        daemon_status.status.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&daemon_status.stderr)
    );
    let daemon: serde_json::Value = serde_json::from_slice(&daemon_status.stdout).unwrap();
    assert_eq!(daemon["data"]["running"], true);

    let stop = Command::new(env!("CARGO_BIN_EXE_ochcli"))
        .env("OCHUB_TEST_HOME", home.path())
        .args(["--json", "daemon", "stop"])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}
