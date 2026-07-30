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
