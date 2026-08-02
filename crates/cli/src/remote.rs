//! SSH stdio bridge for OcHub Remote Nodes.
//!
//! This module intentionally exposes typed, allowlisted methods. It never
//! accepts a shell command or arbitrary argv from the remote client.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser as _;
use fs2::FileExt as _;
use ochub_core::application::{Application, OpenOptions as ApplicationOpenOptions, redact_json};
use ochub_core::runtime::{IpcError, OwnerGuard, OwnerKind};
use ochub_protocol::{
    ApplyPlanParams, Capability, Frame, HelloAckFrame, HelloFrame, MAX_FRAME_SIZE, NodeDescriptor,
    PROTOCOL_MAX, PROTOCOL_MIN, PingFrame, PongFrame, ProtocolErrorFrame, ProviderCreateParams,
    ProviderUpdateParams, RemoteError, RequestFrame, ResponseFrame, RuntimeDescriptor,
    SCHEMA_VERSION, decode_frame, encode_frame, methods, negotiate_protocol, validate_request_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::command::{Cli, RemoteCommand, RemotePolicyCommand};
use crate::error::CliError;
use crate::output::Output;

const PLAN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_STORED_PLANS: usize = 128;
const MAX_IDEMPOTENCY_RESULTS: usize = 256;
const REMOTE_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_REMOTE_STATE_BYTES: u64 = 4 * 1024 * 1024;

pub async fn execute(cli: &Cli, command: &RemoteCommand, output: &Output) -> Result<(), CliError> {
    handoff_to_managed_cli()?;
    match command {
        RemoteCommand::Probe => output.success(&probe(cli).await?, &[]),
        RemoteCommand::Serve { stdio, ephemeral } => {
            if !stdio {
                return Err(CliError::InvalidInput(
                    "remote serve requires --stdio; no network listener is provided".to_string(),
                ));
            }
            serve_stdio(cli, *ephemeral).await
        }
        RemoteCommand::Policy { command } => match command {
            RemotePolicyCommand::Show => output.success(&ochub_core::remote_policy::status()?, &[]),
            RemotePolicyCommand::Validate => {
                let status = ochub_core::remote_policy::validate_file()?;
                output.success(
                    &json!({
                        "valid": true,
                        "path": status.path,
                        "exists": status.exists,
                        "policy": status.policy
                    }),
                    &[],
                )
            }
        },
    }
}

#[cfg(unix)]
fn handoff_to_managed_cli() -> Result<(), CliError> {
    use std::os::unix::process::CommandExt as _;

    let Some(managed) = crate::node::managed_entrypoint() else {
        return Ok(());
    };
    let current = std::env::current_exe()?;
    if paths_refer_to_same_executable(&current, &managed) {
        return Ok(());
    }

    // A saved SSH connection may still name the original bootstrap binary.
    // Replace that process before it reads a protocol frame so every remote
    // session follows the atomically switched managed version.
    let error = std::process::Command::new(managed)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(error.into())
}

#[cfg(not(unix))]
fn handoff_to_managed_cli() -> Result<(), CliError> {
    Ok(())
}

#[cfg(unix)]
fn paths_refer_to_same_executable(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

async fn probe(cli: &Cli) -> Result<Value, CliError> {
    let policy_status = ochub_core::remote_policy::status()?;
    let node = node_descriptor()?;
    let runtime = runtime_descriptor(cli).await;
    Ok(json!({
        "protocolMin": PROTOCOL_MIN,
        "protocolMax": PROTOCOL_MAX,
        "schemaVersion": SCHEMA_VERSION,
        "serverVersion": env!("CARGO_PKG_VERSION"),
        "node": node,
        "runtime": runtime,
        "capabilities": capabilities(&policy_status.policy),
        "policy": policy_status,
        "maxFrameSize": MAX_FRAME_SIZE
    }))
}

async fn serve_stdio(cli: &Cli, ephemeral: bool) -> Result<(), CliError> {
    let policy = ochub_core::remote_policy::load()?;
    if !policy.enabled {
        return Err(CliError::InvalidInput(
            "remote access is disabled by the device policy".to_string(),
        ));
    }

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let hello = read_hello(&mut reader, &mut stdout).await?;
    let protocol_version = match negotiate_protocol(
        hello.protocol_min,
        hello.protocol_max,
        PROTOCOL_MIN,
        PROTOCOL_MAX,
    ) {
        Ok(version) => version,
        Err(error) => {
            write_frame(
                &mut stdout,
                &Frame::ProtocolError(ProtocolErrorFrame {
                    code: "PROTOCOL_INCOMPATIBLE".to_string(),
                    message: error.to_string(),
                    details: json!({
                        "serverMin": PROTOCOL_MIN,
                        "serverMax": PROTOCOL_MAX
                    }),
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let execution = RemoteExecution::open(cli, ephemeral).await?;
    let runtime = execution.runtime_descriptor().await;
    let node = node_descriptor()?;
    let node_id = node.id.clone();
    write_frame(
        &mut stdout,
        &Frame::HelloAck(HelloAckFrame {
            protocol_version,
            schema_version: SCHEMA_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            node,
            runtime,
            capabilities: capabilities(&policy),
            max_frame_size: MAX_FRAME_SIZE,
        }),
    )
    .await?;

    let mut session = RemoteSession::new(
        protocol_version,
        policy,
        execution,
        crate::node::RemoteNodeOptions::from_cli(cli),
        hello.device_id,
        node_id,
    );
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line).await?;
        if count == 0 {
            break;
        }
        if line.len() > MAX_FRAME_SIZE + 1 {
            write_protocol_error(
                &mut stdout,
                "FRAME_TOO_LARGE",
                "remote protocol frame exceeds the maximum size",
            )
            .await?;
            break;
        }
        let frame = match decode_frame(&line) {
            Ok(frame) => frame,
            Err(error) => {
                write_protocol_error(&mut stdout, "INVALID_FRAME", &error.to_string()).await?;
                continue;
            }
        };
        match frame {
            Frame::Request(request) => {
                let response = session.handle_request(request).await;
                write_frame(&mut stdout, &Frame::Response(response)).await?;
            }
            Frame::Ping(PingFrame { timestamp }) => {
                write_frame(&mut stdout, &Frame::Pong(PongFrame { timestamp })).await?;
            }
            Frame::Goodbye(_) => break,
            Frame::Cancel(cancel) => {
                write_frame(
                    &mut stdout,
                    &Frame::Response(error_response(
                        protocol_version,
                        cancel.request_id,
                        "CANCELLED",
                        "request is not in flight",
                        false,
                        Value::Null,
                    )),
                )
                .await?;
            }
            _ => {
                write_protocol_error(
                    &mut stdout,
                    "UNEXPECTED_FRAME",
                    "expected request, ping, cancel, or goodbye after handshake",
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn read_hello(
    reader: &mut BufReader<tokio::io::Stdin>,
    stdout: &mut tokio::io::Stdout,
) -> Result<HelloFrame, CliError> {
    let mut line = Vec::new();
    let count = reader.read_until(b'\n', &mut line).await?;
    if count == 0 {
        return Err(CliError::InvalidInput(
            "remote client closed before protocol handshake".to_string(),
        ));
    }
    if line.len() > MAX_FRAME_SIZE + 1 {
        write_protocol_error(
            stdout,
            "FRAME_TOO_LARGE",
            "remote protocol hello exceeds the maximum size",
        )
        .await?;
        return Err(CliError::InvalidInput(
            "remote protocol hello exceeds the maximum size".to_string(),
        ));
    }
    match decode_frame(&line) {
        Ok(Frame::Hello(hello)) => Ok(hello),
        Ok(_) => {
            write_protocol_error(
                stdout,
                "HANDSHAKE_REQUIRED",
                "the first remote protocol frame must be hello",
            )
            .await?;
            Err(CliError::InvalidInput(
                "the first remote protocol frame must be hello".to_string(),
            ))
        }
        Err(error) => {
            write_protocol_error(stdout, "INVALID_FRAME", &error.to_string()).await?;
            Err(CliError::InvalidInput(error.to_string()))
        }
    }
}

async fn write_protocol_error(
    stdout: &mut tokio::io::Stdout,
    code: &str,
    message: &str,
) -> Result<(), CliError> {
    write_frame(
        stdout,
        &Frame::ProtocolError(ProtocolErrorFrame {
            code: code.to_string(),
            message: message.to_string(),
            details: Value::Null,
        }),
    )
    .await
}

async fn write_frame(stdout: &mut tokio::io::Stdout, frame: &Frame) -> Result<(), CliError> {
    let bytes = encode_frame(frame)
        .map_err(|error| CliError::InvalidInput(format!("cannot encode remote frame: {error}")))?;
    stdout.write_all(&bytes).await?;
    stdout.flush().await?;
    Ok(())
}

enum RemoteExecution {
    Owner {
        socket: Option<std::path::PathBuf>,
        timeout: u64,
    },
    Ephemeral {
        application: Arc<Application>,
        _owner: OwnerGuard,
    },
}

impl RemoteExecution {
    async fn open(cli: &Cli, ephemeral: bool) -> Result<Self, CliError> {
        if ochub_core::runtime::active_owner()?.is_some() {
            return Ok(Self::Owner {
                socket: cli.socket.clone(),
                timeout: cli.timeout,
            });
        }
        if !ephemeral {
            crate::daemon::start_background(cli).await?;
            return Ok(Self::Owner {
                socket: cli.socket.clone(),
                timeout: cli.timeout,
            });
        }
        ochub_core::app_store::refresh_app_config_dir_override();
        let data_dir = ochub_core::paths::get_app_config_dir();
        let owner = OwnerGuard::acquire(
            OwnerKind::Foreground,
            &data_dir,
            format!("stdio:{}", std::process::id()),
        )?;
        let application = Arc::new(Application::open(ApplicationOpenOptions::default())?);
        Ok(Self::Ephemeral {
            application,
            _owner: owner,
        })
    }

    async fn execute(&self, argv: Vec<String>) -> Result<ExecutionResult, CliError> {
        match self {
            Self::Owner { socket, timeout } => {
                let response =
                    crate::runtime_client::execute_argv(socket.as_deref(), *timeout, argv).await?;
                Ok(ExecutionResult {
                    ok: response.ok,
                    data: response.data,
                    warnings: response.warnings,
                    error: response.error,
                })
            }
            Self::Ephemeral { application, .. } => {
                let mut parse_argv = vec!["ochcli".to_string()];
                parse_argv.extend(argv);
                let parsed = Cli::try_parse_from(parse_argv)
                    .map_err(|error| CliError::InvalidInput(error.to_string()))?;
                let _mutation = ochub_core::runtime::MutationGuard::acquire()?;
                let (capture, handle) = Output::capture();
                match crate::run::execute_with_application(application, &parsed, &capture).await {
                    Ok(()) => {
                        let captured = handle.take().unwrap_or(crate::output::CapturedOutput {
                            data: Value::Null,
                            warnings: Vec::new(),
                        });
                        Ok(ExecutionResult {
                            ok: true,
                            data: captured.data,
                            warnings: captured.warnings,
                            error: None,
                        })
                    }
                    Err(error) => Ok(ExecutionResult {
                        ok: false,
                        data: Value::Null,
                        warnings: Vec::new(),
                        error: Some(IpcError {
                            code: error.code().to_string(),
                            message: error.to_string(),
                            retryable: error.retryable(),
                            details: error.details(),
                            exit_code: error.exit_code_u8(),
                        }),
                    }),
                }
            }
        }
    }

    async fn runtime_descriptor(&self) -> RuntimeDescriptor {
        match self {
            Self::Owner { socket, timeout } => {
                runtime_descriptor_at(socket.as_deref(), *timeout).await
            }
            Self::Ephemeral { application, .. } => RuntimeDescriptor {
                persistent: false,
                owner_kind: Some("ephemeral".to_string()),
                owner_pid: Some(std::process::id()),
                gateway: serde_json::to_value(application.state().gateway.status().await)
                    .unwrap_or(Value::Null),
            },
        }
    }
}

struct ExecutionResult {
    ok: bool,
    data: Value,
    warnings: Vec<String>,
    error: Option<IpcError>,
}

enum Payload {
    Json(Value),
    Text(String),
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvancedParams {
    action: String,
    #[serde(default)]
    params: Value,
}

fn advanced_params(value: &Value, method: &str) -> Result<AdvancedParams, String> {
    let params: AdvancedParams = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid {method} parameters: {error}"))?;
    validate_text(&params.action, "action")?;
    Ok(params)
}

fn advanced_read_argv(value: &Value) -> Result<Vec<String>, String> {
    let params = advanced_params(value, methods::TOOL_ADVANCED_READ)?;
    let argv = match params.action.as_str() {
        "env.scan" => vec!["env", "scan"],
        "omo.localFile" => vec!["opencode", "omo", "local-file"],
        "omoSlim.localFile" => vec!["opencode", "omo-slim", "local-file"],
        "claude.mcp.config" => vec!["claude", "mcp", "config", "show"],
        "claude.mcp.validatePaths" => vec!["claude", "mcp", "path", "validate"],
        "claude.mcp.validateCommand" => {
            let command = params.params["command"]
                .as_str()
                .ok_or_else(|| "advanced command validation requires params.command".to_string())?;
            validate_text(command, "params.command")?;
            return Ok(vec![
                "claude".into(),
                "mcp".into(),
                "path".into(),
                "validate-command".into(),
                command.to_string(),
            ]);
        }
        "codex.history.status" => vec!["codex", "history", "status"],
        "openclaw.health" => vec!["openclaw", "health"],
        "openclaw.defaultModel" => vec!["openclaw", "model", "default", "get"],
        "openclaw.env" => vec!["openclaw", "env", "get"],
        "openclaw.tools" => vec!["openclaw", "tools", "get"],
        "hermes.models" => vec!["hermes", "models", "get"],
        "hermes.memory.status" => vec!["hermes", "memory", "status"],
        "hermes.memory.limits" => vec!["hermes", "memory", "limits"],
        "hermes.memory.read" | "hermes.user.read" => vec![
            "hermes",
            "memory",
            "read",
            if params.action == "hermes.memory.read" {
                "memory"
            } else {
                "user"
            },
        ],
        _ => {
            return Err(format!(
                "unsupported advanced read action: {}",
                params.action
            ));
        }
    };
    Ok(argv.into_iter().map(str::to_string).collect())
}

fn advanced_write_payload(value: &Value) -> Result<(Payload, Vec<String>), String> {
    let params = advanced_params(value, methods::TOOL_ADVANCED_WRITE)?;
    let (payload, argv): (Payload, Vec<&str>) = match params.action.as_str() {
        "env.clean" | "env.restore" => {
            let id = params.params["id"]
                .as_str()
                .ok_or_else(|| "advanced environment action requires params.id".to_string())?;
            validate_text(id, "params.id")?;
            return Ok((
                Payload::None,
                vec![
                    "--yes".into(),
                    "env".into(),
                    if params.action == "env.clean" {
                        "clean".into()
                    } else {
                        "restore".into()
                    },
                    id.to_string(),
                ],
            ));
        }
        "omo.disable" => (Payload::None, vec!["--yes", "opencode", "omo", "disable"]),
        "omoSlim.disable" => (
            Payload::None,
            vec!["--yes", "opencode", "omo-slim", "disable"],
        ),
        "claude.plugin.apply" => (
            Payload::Json(json!({ "official": false })),
            vec!["claude", "plugin", "apply", "--from"],
        ),
        "claude.plugin.restore" => (Payload::None, vec!["--yes", "claude", "plugin", "restore"]),
        "claude.onboarding.skip" => (Payload::None, vec!["claude", "mcp", "onboarding", "skip"]),
        "claude.onboarding.clear" => (Payload::None, vec!["claude", "mcp", "onboarding", "clear"]),
        "codex.history.restore" => (Payload::None, vec!["--yes", "codex", "history", "restore"]),
        "openclaw.defaultModel.set" => (
            Payload::Json(params.params["value"].clone()),
            vec!["openclaw", "model", "default", "set", "--from"],
        ),
        "openclaw.env.set" => (
            Payload::Json(params.params["value"].clone()),
            vec!["openclaw", "env", "set", "--from"],
        ),
        "openclaw.tools.set" => (
            Payload::Json(params.params["value"].clone()),
            vec!["openclaw", "tools", "set", "--from"],
        ),
        "hermes.models.set" => (
            Payload::Json(params.params["value"].clone()),
            vec!["hermes", "models", "set", "--from"],
        ),
        "hermes.memory.write" | "hermes.user.write" => (
            Payload::Text(
                params.params["content"]
                    .as_str()
                    .ok_or_else(|| "advanced Hermes write requires params.content".to_string())?
                    .to_string(),
            ),
            vec![
                "hermes",
                "memory",
                "write",
                if params.action == "hermes.memory.write" {
                    "memory"
                } else {
                    "user"
                },
                "--from",
            ],
        ),
        "hermes.memory.enable"
        | "hermes.memory.disable"
        | "hermes.user.enable"
        | "hermes.user.disable" => {
            let user = params.action.starts_with("hermes.user");
            let enabled = params.action.ends_with(".enable");
            (
                Payload::None,
                vec![
                    "hermes",
                    "memory",
                    if enabled { "enable" } else { "disable" },
                    if user { "user" } else { "memory" },
                ],
            )
        }
        _ => {
            return Err(format!(
                "unsupported advanced write action: {}",
                params.action
            ));
        }
    };
    Ok((payload, argv.into_iter().map(str::to_string).collect()))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannedOperation {
    plan_argv: Vec<String>,
    apply_argv: Vec<String>,
    revision: String,
    created_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CachedMutation {
    data: Value,
    warnings: Vec<String>,
    revision: Option<String>,
    #[serde(default)]
    request_hash: Option<String>,
    completed_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistentRemoteState {
    schema_version: u32,
    #[serde(default)]
    plans: HashMap<String, PlannedOperation>,
    #[serde(default)]
    idempotency: HashMap<String, CachedMutation>,
}

impl Default for PersistentRemoteState {
    fn default() -> Self {
        Self {
            schema_version: REMOTE_STATE_SCHEMA_VERSION,
            plans: HashMap::new(),
            idempotency: HashMap::new(),
        }
    }
}

struct RemoteSession {
    protocol_version: u32,
    policy: ochub_core::remote_policy::RemotePolicy,
    execution: RemoteExecution,
    node_options: crate::node::RemoteNodeOptions,
    device_id: Option<String>,
    node_id: String,
}

impl RemoteSession {
    fn new(
        protocol_version: u32,
        policy: ochub_core::remote_policy::RemotePolicy,
        execution: RemoteExecution,
        node_options: crate::node::RemoteNodeOptions,
        device_id: Option<String>,
        node_id: String,
    ) -> Self {
        Self {
            protocol_version,
            policy,
            execution,
            node_options,
            device_id,
            node_id,
        }
    }

    async fn handle_request(&mut self, request: RequestFrame) -> ResponseFrame {
        if request.protocol_version != self.protocol_version {
            return error_response(
                self.protocol_version,
                request.request_id,
                "PROTOCOL_INCOMPATIBLE",
                "request protocolVersion does not match the negotiated version",
                false,
                json!({ "negotiated": self.protocol_version }),
            );
        }
        if let Err(error) = validate_request_id(&request.request_id) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                &error.to_string(),
                false,
                Value::Null,
            );
        }
        let Some(required) = required_capability(&request.method) else {
            return error_response(
                self.protocol_version,
                request.request_id,
                "METHOD_NOT_FOUND",
                "unknown or unsupported remote method",
                false,
                json!({ "method": request.method }),
            );
        };
        if !capabilities(&self.policy).contains(&required) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "PERMISSION_DENIED",
                "the remote policy does not allow this method",
                false,
                json!({ "method": request.method, "capability": required }),
            );
        }
        if request.method == methods::PROVIDER_SWITCH_PLAN {
            return self.plan_provider_switch(request).await;
        }
        if request.method == methods::PROVIDER_SWITCH_APPLY {
            return self.apply_provider_switch(request).await;
        }
        if matches!(
            request.method.as_str(),
            methods::NODE_UPDATE_STATUS
                | methods::NODE_UPDATE_CHECK
                | methods::NODE_UPDATE_INSTALL_DIRECT
        ) {
            return self.handle_node_update(request).await;
        }
        if matches!(
            request.method.as_str(),
            methods::PROVIDER_CREATE
                | methods::PROVIDER_UPDATE
                | methods::PROVIDER_COMMON_SET
                | methods::MCP_UPSERT
                | methods::SKILL_INSTALL
                | methods::PRICING_OVERRIDE_SET
                | methods::PRICING_DEFAULTS_SET
                | methods::STATION_CREATE
                | methods::STATION_UPDATE
                | methods::STATION_APPLY
                | methods::STATION_DETECT_DIALECTS
                | methods::STATION_FETCH_MODELS
                | methods::STATION_TEST_ENDPOINT
                | methods::PROXY_SET
                | methods::PROXY_TEST
                | methods::SETTINGS_SET
                | methods::SYNC_CONFIGURE
                | methods::SYNC_TEST
                | methods::TOOL_ADVANCED_WRITE
        ) {
            return self.execute_payload_request(request).await;
        }
        let mut argv = match argv_for_request(&request) {
            Ok(argv) => argv,
            Err(error) => {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "INVALID_ARGUMENT",
                    &error,
                    false,
                    Value::Null,
                );
            }
        };
        if self.policy.allow_secrets_write
            && matches!(
                request.method.as_str(),
                methods::GATEWAY_CONNECTION_INFO | methods::STATION_CONNECTION_INFO
            )
        {
            argv.insert(0, "--show-secrets".into());
        }
        if is_direct_mutation(&request.method) {
            self.execute_mutation_response(request, argv).await
        } else {
            self.execute_response(request, argv, None).await
        }
    }

    async fn handle_node_update(&self, request: RequestFrame) -> ResponseFrame {
        if let Err(error) = expect_empty_params(&request.params) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                &error,
                false,
                Value::Null,
            );
        }
        let result = match request.method.as_str() {
            methods::NODE_UPDATE_STATUS => crate::node::status()
                .await
                .and_then(|value| serde_json::to_value(value).map_err(CliError::from)),
            methods::NODE_UPDATE_CHECK => crate::node::check_for_update(true)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(CliError::from)),
            methods::NODE_UPDATE_INSTALL_DIRECT => {
                crate::node::install_direct_remote(&self.node_options)
                    .await
                    .and_then(|value| serde_json::to_value(value).map_err(CliError::from))
            }
            _ => unreachable!(),
        };
        match result {
            Ok(data) => ResponseFrame {
                protocol_version: self.protocol_version,
                request_id: request.request_id,
                ok: true,
                data,
                warnings: Vec::new(),
                error: None,
                revision: None,
            },
            Err(error) => cli_error_response(self.protocol_version, request.request_id, error),
        }
    }

    async fn execute_payload_request(&self, request: RequestFrame) -> ResponseFrame {
        let (payload, argv) = match request.method.as_str() {
            methods::PROVIDER_CREATE => {
                let mut params =
                    match serde_json::from_value::<ProviderCreateParams>(request.params.clone()) {
                        Ok(params) => params,
                        Err(error) => {
                            return invalid_params_response(
                                self.protocol_version,
                                request.request_id,
                                "provider.create",
                                error,
                            );
                        }
                    };
                if let Err(error) = validate_text(&params.app, "app") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                strip_redacted_secret_placeholders(&mut params.provider);
                if !self.policy.allow_secrets_write && contains_secret_write(&params.provider) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                let mut argv = vec!["provider".into(), "add".into(), "--app".into(), params.app];
                if params.add_to_live {
                    argv.push("--add-to-live".into());
                }
                argv.push("--from".into());
                (Payload::Json(params.provider), argv)
            }
            methods::PROVIDER_UPDATE => {
                let mut params =
                    match serde_json::from_value::<ProviderUpdateParams>(request.params.clone()) {
                        Ok(params) => params,
                        Err(error) => {
                            return invalid_params_response(
                                self.protocol_version,
                                request.request_id,
                                "provider.update",
                                error,
                            );
                        }
                    };
                if let Err(error) = validate_text(&params.app, "app")
                    .and_then(|_| validate_text(&params.provider_id, "providerId"))
                {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                strip_redacted_secret_placeholders(&mut params.patch);
                if !self.policy.allow_secrets_write && contains_secret_write(&params.patch) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Json(params.patch),
                    vec![
                        "provider".into(),
                        "edit".into(),
                        params.provider_id,
                        "--app".into(),
                        params.app,
                        "--patch".into(),
                    ],
                )
            }
            methods::PROVIDER_COMMON_SET => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    app: String,
                    snippet: String,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "provider.common.set",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_text(&params.app, "app") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                if !self.policy.allow_secrets_write
                    && common_config_may_contain_secret(&params.snippet)
                {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Text(params.snippet),
                    vec![
                        "config".into(),
                        "common".into(),
                        "set".into(),
                        "--app".into(),
                        params.app,
                        "--from".into(),
                    ],
                )
            }
            methods::MCP_UPSERT => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    server: Value,
                    #[serde(default)]
                    original_id: Option<String>,
                }
                let mut params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "mcp.upsert",
                            error,
                        );
                    }
                };
                if let Some(original_id) = params.original_id.as_deref()
                    && let Err(error) = validate_text(original_id, "originalId")
                {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                strip_redacted_secret_placeholders(&mut params.server);
                if !self.policy.allow_secrets_write && contains_secret_write(&params.server) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                let argv = if let Some(original_id) = params.original_id {
                    vec!["mcp".into(), "edit".into(), original_id, "--patch".into()]
                } else {
                    vec!["mcp".into(), "add".into(), "--from".into()]
                };
                (Payload::Json(params.server), argv)
            }
            methods::SKILL_INSTALL => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    skill: Value,
                    app: String,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "skill.install",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_text(&params.app, "app") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                (
                    Payload::Json(params.skill),
                    vec!["skill".into(), "install".into(), "--app".into(), params.app],
                )
            }
            methods::PRICING_OVERRIDE_SET => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    model_id: String,
                    pricing: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "pricing.override.set",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_text(&params.model_id, "modelId") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                (
                    Payload::Json(params.pricing),
                    vec![
                        "pricing".into(),
                        "override".into(),
                        "set".into(),
                        "--model".into(),
                        params.model_id,
                        "--from".into(),
                    ],
                )
            }
            methods::PRICING_DEFAULTS_SET => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    defaults: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "pricing.defaults.set",
                            error,
                        );
                    }
                };
                (
                    Payload::Json(params.defaults),
                    vec![
                        "pricing".into(),
                        "defaults".into(),
                        "set".into(),
                        "--from".into(),
                    ],
                )
            }
            methods::STATION_CREATE => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    station: Value,
                }
                let mut params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "station.create",
                            error,
                        );
                    }
                };
                strip_redacted_secret_placeholders(&mut params.station);
                if !self.policy.allow_secrets_write && contains_secret_write(&params.station) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Json(params.station),
                    vec!["station".into(), "add".into(), "--from".into()],
                )
            }
            methods::STATION_UPDATE => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    station_id: String,
                    patch: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "station.update",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_text(&params.station_id, "stationId") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                if !self.policy.allow_secrets_write && contains_secret_write(&params.patch) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Json(params.patch),
                    vec![
                        "station".into(),
                        "edit".into(),
                        params.station_id,
                        "--patch".into(),
                    ],
                )
            }
            methods::STATION_APPLY => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    station_id: String,
                    app: String,
                    policy: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "station.apply",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_text(&params.station_id, "stationId")
                    .and_then(|_| validate_text(&params.app, "app"))
                {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                (
                    Payload::Json(params.policy),
                    vec![
                        "station".into(),
                        "apply".into(),
                        params.station_id,
                        "--app".into(),
                        params.app,
                        "--from".into(),
                    ],
                )
            }
            methods::STATION_DETECT_DIALECTS
            | methods::STATION_FETCH_MODELS
            | methods::STATION_TEST_ENDPOINT => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    url: String,
                    #[serde(default)]
                    api_key: String,
                    #[serde(default)]
                    station_id: Option<String>,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            request.method.as_str(),
                            error,
                        );
                    }
                };
                if let Err(error) = validate_url(&params.url) {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                if let Some(station_id) = params.station_id.as_deref()
                    && let Err(error) = validate_text(station_id, "stationId")
                {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                let redacted_key = !params.api_key.is_empty()
                    && params.api_key.chars().all(|character| character == '*');
                if redacted_key && params.station_id.is_none() {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        "a redacted apiKey requires stationId",
                    );
                }
                if !params.api_key.is_empty() && !redacted_key && !self.policy.allow_secrets_write {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                let mut argv = match request.method.as_str() {
                    methods::STATION_DETECT_DIALECTS => vec![
                        "gateway".into(),
                        "probe-dialect".into(),
                        "--url".into(),
                        params.url,
                    ],
                    methods::STATION_FETCH_MODELS => vec![
                        "gateway".into(),
                        "endpoint".into(),
                        "models".into(),
                        "--url".into(),
                        params.url,
                    ],
                    methods::STATION_TEST_ENDPOINT => vec![
                        "gateway".into(),
                        "endpoint".into(),
                        "test".into(),
                        "--url".into(),
                        params.url,
                    ],
                    _ => unreachable!(),
                };
                if let Some(station_id) = params.station_id {
                    argv.extend(["--station".into(), station_id]);
                }
                if params.api_key.is_empty() || redacted_key {
                    (Payload::None, argv)
                } else {
                    argv.push("--api-key-file".into());
                    (Payload::Text(params.api_key), argv)
                }
            }
            methods::PROXY_SET | methods::PROXY_TEST => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    proxy: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            request.method.as_str(),
                            error,
                        );
                    }
                };
                if !self.policy.allow_secrets_write && contains_secret_write(&params.proxy) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Json(params.proxy),
                    vec![
                        "settings".into(),
                        "proxy".into(),
                        if request.method == methods::PROXY_SET {
                            "set".into()
                        } else {
                            "test".into()
                        },
                        "--from".into(),
                    ],
                )
            }
            methods::SETTINGS_SET => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    path: String,
                    value: Value,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "settings.set",
                            error,
                        );
                    }
                };
                if let Err(error) = validate_setting_path(&params.path) {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        error,
                    );
                }
                if !self.policy.allow_secrets_write
                    && (contains_secret_write(&params.value)
                        || (is_secret_key(&params.path) && secret_value_is_present(&params.value)))
                {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (
                    Payload::Json(params.value),
                    vec![
                        "settings".into(),
                        "set".into(),
                        params.path,
                        "--from".into(),
                    ],
                )
            }
            methods::SYNC_CONFIGURE => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Params {
                    backend: String,
                    settings: Value,
                    #[serde(default)]
                    clear_secret: bool,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "sync.configure",
                            error,
                        );
                    }
                };
                if !matches!(params.backend.as_str(), "webdav" | "s3") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        "backend must be webdav or s3",
                    );
                }
                if !self.policy.allow_secrets_write && contains_secret_write(&params.settings) {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                let mut settings = params.settings;
                strip_redacted_secret_placeholders(&mut settings);
                let mut argv = vec!["sync".into(), params.backend, "configure".into()];
                if params.clear_secret {
                    argv.push("--clear-secret".into());
                }
                argv.push("--from".into());
                (Payload::Json(settings), argv)
            }
            methods::SYNC_TEST => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Params {
                    backend: String,
                    settings: Option<Value>,
                }
                let params = match serde_json::from_value::<Params>(request.params.clone()) {
                    Ok(params) => params,
                    Err(error) => {
                        return invalid_params_response(
                            self.protocol_version,
                            request.request_id,
                            "sync.test",
                            error,
                        );
                    }
                };
                if !matches!(params.backend.as_str(), "webdav" | "s3") {
                    return invalid_argument_response(
                        self.protocol_version,
                        request.request_id,
                        "backend must be webdav or s3",
                    );
                }
                let mut argv = vec!["sync".into(), params.backend, "test".into()];
                match params.settings {
                    Some(mut settings) => {
                        if !self.policy.allow_secrets_write && contains_secret_write(&settings) {
                            return secret_write_denied(self.protocol_version, request.request_id);
                        }
                        strip_redacted_secret_placeholders(&mut settings);
                        argv.push("--from".into());
                        (Payload::Json(settings), argv)
                    }
                    None => (Payload::None, argv),
                }
            }
            methods::TOOL_ADVANCED_WRITE => {
                let (payload, argv) = match advanced_write_payload(&request.params) {
                    Ok(result) => result,
                    Err(error) => {
                        return invalid_argument_response(
                            self.protocol_version,
                            request.request_id,
                            error,
                        );
                    }
                };
                if !self.policy.allow_secrets_write
                    && matches!(&payload, Payload::Json(value) if contains_secret_write(value))
                {
                    return secret_write_denied(self.protocol_version, request.request_id);
                }
                (payload, argv)
            }
            _ => unreachable!("payload methods are filtered by handle_request"),
        };

        let mut argv = argv;
        let mut file = if matches!(&payload, Payload::None) {
            None
        } else {
            match tempfile::Builder::new()
                .prefix("ochub-remote-")
                .suffix(match &payload {
                    Payload::Json(_) => ".json",
                    Payload::Text(_) => ".txt",
                    Payload::None => unreachable!(),
                })
                .tempfile_in(std::env::temp_dir())
            {
                Ok(file) => Some(file),
                Err(error) => {
                    return cli_error_response(
                        self.protocol_version,
                        request.request_id,
                        CliError::Io(error),
                    );
                }
            }
        };
        let write_result = match (&payload, file.as_mut()) {
            (Payload::Json(value), Some(file)) => serde_json::to_writer(&mut *file, value)
                .map_err(CliError::from)
                .and_then(|_| file.flush().map_err(CliError::from)),
            (Payload::Text(value), Some(file)) => file
                .write_all(value.as_bytes())
                .and_then(|_| file.flush())
                .map_err(CliError::from),
            (Payload::None, None) => Ok(()),
            _ => unreachable!("payload file state matches payload kind"),
        };
        if let Err(error) = write_result {
            return cli_error_response(self.protocol_version, request.request_id, error);
        }
        if let Some(file) = &file {
            argv.push(file.path().to_string_lossy().into_owned());
        }
        if is_direct_mutation(&request.method) {
            self.execute_mutation_response(request, argv).await
        } else {
            self.execute_response(request, argv, None).await
        }
    }

    async fn plan_provider_switch(&mut self, request: RequestFrame) -> ResponseFrame {
        let plan_count = match remote_plan_count() {
            Ok(count) => count,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        if plan_count >= MAX_STORED_PLANS {
            return error_response(
                self.protocol_version,
                request.request_id,
                "RESOURCE_EXHAUSTED",
                "too many unexpired remote plans",
                true,
                Value::Null,
            );
        }
        let params = match serde_json::from_value::<ochub_protocol::ProviderSwitchParams>(
            request.params.clone(),
        ) {
            Ok(params) => params,
            Err(error) => {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "INVALID_ARGUMENT",
                    &format!("invalid provider switch parameters: {error}"),
                    false,
                    Value::Null,
                );
            }
        };
        if let Err(message) = validate_switch_params(&params) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                &message,
                false,
                Value::Null,
            );
        }
        let plan_argv = vec![
            "--dry-run".to_string(),
            "provider".to_string(),
            "switch".to_string(),
            params.provider_id.clone(),
            "--app".to_string(),
            params.app.clone(),
            "--on-drift".to_string(),
            params.on_drift.clone(),
        ];
        let apply_argv = plan_argv
            .iter()
            .filter(|value| value.as_str() != "--dry-run")
            .cloned()
            .collect::<Vec<_>>();
        let result = match self
            .execution
            .execute(traced_argv(&request, plan_argv.clone()))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        if !result.ok {
            return execution_error_response(self.protocol_version, request.request_id, result);
        }
        let revision = revision_for(&result.data);
        let safe_plan = remote_safe_value(&result.data);
        let plan_id = uuid::Uuid::new_v4().to_string();
        let stored_plan = PlannedOperation {
            plan_argv,
            apply_argv,
            revision: revision.clone(),
            created_at: chrono::Utc::now().timestamp(),
        };
        match store_remote_plan(plan_id.clone(), stored_plan) {
            Ok(true) => {}
            Ok(false) => {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "RESOURCE_EXHAUSTED",
                    "too many unexpired remote plans",
                    true,
                    Value::Null,
                );
            }
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        }
        let audit = json!({
            "operation": "provider.switch",
            "operationId": plan_id,
            "planHash": revision,
            "nodeId": self.node_id,
            "deviceId": self.device_id,
            "sshUser": environment_value(&["USER", "USERNAME"]),
            "sshConnection": std::env::var("SSH_CONNECTION").ok(),
            "traceId": request.trace_id,
            "app": params.app,
            "providerId": params.provider_id,
            "configPath": safe_plan.get("configPath"),
            "wouldChange": safe_plan.get("wouldChange"),
        });
        if let Err(error) = ochub_core::runtime::journal::OperationHandle::plan(
            plan_id.clone(),
            "provider.switch",
            "remote-desktop",
            audit,
        ) {
            let _ = remove_remote_plan(&plan_id);
            return cli_error_response(self.protocol_version, request.request_id, error.into());
        }
        ResponseFrame {
            protocol_version: self.protocol_version,
            request_id: request.request_id,
            ok: true,
            data: json!({
                "planId": plan_id,
                "operationId": plan_id,
                "revision": revision,
                "expiresInSeconds": PLAN_TTL.as_secs(),
                "plan": safe_plan
            }),
            warnings: result.warnings,
            error: None,
            revision: Some(revision),
        }
    }

    async fn apply_provider_switch(&mut self, request: RequestFrame) -> ResponseFrame {
        let Some(idempotency_key) = request.idempotency_key.as_deref() else {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                "provider.switch.apply requires idempotencyKey",
                false,
                Value::Null,
            );
        };
        if let Err(error) = validate_request_id(idempotency_key) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                &format!("invalid idempotencyKey: {error}"),
                false,
                Value::Null,
            );
        }
        let cached = match cached_remote_mutation(idempotency_key) {
            Ok(cached) => cached,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        if let Some(cached) = cached {
            let requested_plan_id = request
                .params
                .get("planId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let cached_plan_id = cached
                .data
                .get("planId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if requested_plan_id != cached_plan_id
                || request.expected_revision.as_deref() != cached.revision.as_deref()
            {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "RESOURCE_CONFLICT",
                    "idempotencyKey was already used for a different plan or revision",
                    false,
                    json!({
                        "requestedPlanId": requested_plan_id,
                        "originalPlanId": cached_plan_id
                    }),
                );
            }
            return ResponseFrame {
                protocol_version: self.protocol_version,
                request_id: request.request_id,
                ok: true,
                data: cached.data,
                warnings: cached.warnings,
                error: None,
                revision: cached.revision,
            };
        }
        let params = match serde_json::from_value::<ApplyPlanParams>(request.params.clone()) {
            Ok(params) => params,
            Err(error) => {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "INVALID_ARGUMENT",
                    &format!("invalid apply parameters: {error}"),
                    false,
                    Value::Null,
                );
            }
        };
        let plan = match get_remote_plan(&params.plan_id) {
            Ok(plan) => plan,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        let Some(plan) = plan else {
            return error_response(
                self.protocol_version,
                request.request_id,
                "NOT_FOUND",
                "remote plan was not found or has expired",
                false,
                json!({ "planId": params.plan_id }),
            );
        };
        if request.expected_revision.as_deref() != Some(plan.revision.as_str()) {
            return error_response(
                self.protocol_version,
                request.request_id,
                "RESOURCE_CONFLICT",
                "expectedRevision does not match the planned revision",
                false,
                json!({ "planId": params.plan_id, "revision": plan.revision }),
            );
        }
        let refreshed = match self
            .execution
            .execute(traced_argv(&request, plan.plan_argv.clone()))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        if !refreshed.ok {
            return execution_error_response(self.protocol_version, request.request_id, refreshed);
        }
        let current_revision = revision_for(&refreshed.data);
        if current_revision != plan.revision {
            return error_response(
                self.protocol_version,
                request.request_id,
                "RESOURCE_CONFLICT",
                "the remote state changed after the plan was created",
                false,
                json!({
                    "planId": params.plan_id,
                    "plannedRevision": plan.revision,
                    "currentRevision": current_revision
                }),
            );
        }
        if let Err(error) = ochub_core::runtime::journal::annotate_operation(
            &params.plan_id,
            json!({
                "idempotencyKey": idempotency_key,
                "expectedRevision": request.expected_revision,
                "applyTraceId": request.trace_id,
            }),
        ) {
            return cli_error_response(self.protocol_version, request.request_id, error.into());
        }
        let applied = match self
            .execution
            .execute(audited_apply_argv(
                &request,
                plan.apply_argv,
                &params.plan_id,
            ))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        if !applied.ok {
            return execution_error_response(self.protocol_version, request.request_id, applied);
        }
        let response = ResponseFrame {
            protocol_version: self.protocol_version,
            request_id: request.request_id.clone(),
            ok: true,
            data: json!({
                "planId": params.plan_id,
                "operationId": params.plan_id,
                "applied": true,
                "result": remote_safe_value(&applied.data)
            }),
            warnings: applied.warnings,
            error: None,
            revision: Some(plan.revision),
        };
        let cached = CachedMutation {
            data: response.data.clone(),
            warnings: response.warnings.clone(),
            revision: response.revision.clone(),
            request_hash: None,
            completed_at: chrono::Utc::now().timestamp(),
        };
        if let Err(error) =
            complete_remote_mutation(&params.plan_id, idempotency_key.to_string(), cached)
        {
            return cli_error_response(self.protocol_version, request.request_id, error);
        }
        response
    }

    async fn execute_response(
        &self,
        request: RequestFrame,
        argv: Vec<String>,
        revision: Option<String>,
    ) -> ResponseFrame {
        match self.execution.execute(traced_argv(&request, argv)).await {
            Ok(result) if result.ok => ResponseFrame {
                protocol_version: self.protocol_version,
                request_id: request.request_id,
                ok: true,
                data: remote_safe_value(&result.data),
                warnings: result.warnings,
                error: None,
                revision,
            },
            Ok(result) => {
                execution_error_response(self.protocol_version, request.request_id, result)
            }
            Err(error) => cli_error_response(self.protocol_version, request.request_id, error),
        }
    }

    async fn execute_mutation_response(
        &self,
        request: RequestFrame,
        argv: Vec<String>,
    ) -> ResponseFrame {
        let Some(idempotency_key) = request.idempotency_key.as_deref() else {
            return error_response(
                self.protocol_version,
                request.request_id,
                "INVALID_ARGUMENT",
                "remote mutations require idempotencyKey",
                false,
                json!({ "method": request.method }),
            );
        };
        if let Err(error) = validate_request_id(idempotency_key) {
            return invalid_argument_response(
                self.protocol_version,
                request.request_id,
                format!("invalid idempotencyKey: {error}"),
            );
        }
        let request_hash = revision_for(&json!({
            "method": request.method,
            "params": request.params,
            "expectedRevision": request.expected_revision
        }));
        match cached_remote_mutation(idempotency_key) {
            Ok(Some(cached)) if cached.request_hash.as_deref() == Some(request_hash.as_str()) => {
                return ResponseFrame {
                    protocol_version: self.protocol_version,
                    request_id: request.request_id,
                    ok: true,
                    data: cached.data,
                    warnings: cached.warnings,
                    error: None,
                    revision: cached.revision,
                };
            }
            Ok(Some(_)) => {
                return error_response(
                    self.protocol_version,
                    request.request_id,
                    "RESOURCE_CONFLICT",
                    "idempotencyKey was already used for a different mutation",
                    false,
                    json!({ "method": request.method }),
                );
            }
            Ok(None) => {}
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        }
        let result = match self.execution.execute(traced_argv(&request, argv)).await {
            Ok(result) if result.ok => result,
            Ok(result) => {
                return execution_error_response(self.protocol_version, request.request_id, result);
            }
            Err(error) => {
                return cli_error_response(self.protocol_version, request.request_id, error);
            }
        };
        let data = remote_safe_value(&result.data);
        let revision = Some(revision_for(&data));
        let response = ResponseFrame {
            protocol_version: self.protocol_version,
            request_id: request.request_id.clone(),
            ok: true,
            data: data.clone(),
            warnings: result.warnings.clone(),
            error: None,
            revision: revision.clone(),
        };
        let cached = CachedMutation {
            data,
            warnings: result.warnings,
            revision,
            request_hash: Some(request_hash),
            completed_at: chrono::Utc::now().timestamp(),
        };
        if let Err(error) = cache_remote_mutation(idempotency_key.to_string(), cached) {
            return cli_error_response(self.protocol_version, request.request_id, error);
        }
        response
    }
}

fn traced_argv(request: &RequestFrame, argv: Vec<String>) -> Vec<String> {
    let trace = request
        .trace_id
        .as_deref()
        .unwrap_or(request.request_id.as_str());
    let mut traced = vec!["--trace-id".to_string(), trace.to_string()];
    traced.extend(argv);
    traced
}

fn audited_apply_argv(
    request: &RequestFrame,
    argv: Vec<String>,
    operation_id: &str,
) -> Vec<String> {
    let trace = request
        .trace_id
        .as_deref()
        .unwrap_or(request.request_id.as_str());
    let mut audited = vec![
        "--trace-id".to_string(),
        trace.to_string(),
        "--remote-operation-id".to_string(),
        operation_id.to_string(),
    ];
    audited.extend(argv);
    audited
}

fn remote_state_path() -> PathBuf {
    ochub_core::runtime::runtime_dir().join("remote-operations.json")
}

fn remote_state_lock_path() -> PathBuf {
    ochub_core::runtime::runtime_dir().join("remote-operations.lock")
}

fn with_remote_state<T>(
    update: impl FnOnce(&mut PersistentRemoteState) -> Result<T, CliError>,
) -> Result<T, CliError> {
    ochub_core::runtime::ensure_runtime_dir()?;
    let lock_path = remote_state_lock_path();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;
    }
    lock.lock_exclusive()?;
    let mut state = load_remote_state(&remote_state_path())?;
    expire_remote_state(&mut state);
    let result = update(&mut state);
    if result.is_ok() {
        save_remote_state(&remote_state_path(), &state)?;
    }
    fs2::FileExt::unlock(&lock)?;
    result
}

fn load_remote_state(path: &Path) -> Result<PersistentRemoteState, CliError> {
    if !path.exists() {
        return Ok(PersistentRemoteState::default());
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_REMOTE_STATE_BYTES {
        return Err(CliError::InvalidInput(format!(
            "unsafe remote operation state file: {}",
            path.display()
        )));
    }
    let state: PersistentRemoteState = serde_json::from_slice(&fs::read(path)?)?;
    if state.schema_version != REMOTE_STATE_SCHEMA_VERSION {
        return Err(CliError::InvalidInput(format!(
            "remote operation state schema {}, supported {}",
            state.schema_version, REMOTE_STATE_SCHEMA_VERSION
        )));
    }
    Ok(state)
}

fn save_remote_state(path: &Path, state: &PersistentRemoteState) -> Result<(), CliError> {
    let bytes = serde_json::to_vec_pretty(state)?;
    if bytes.len() as u64 > MAX_REMOTE_STATE_BYTES {
        return Err(CliError::InvalidInput(
            "remote operation state exceeds the maximum size".to_string(),
        ));
    }
    ochub_core::paths::atomic_write(path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn expire_remote_state(state: &mut PersistentRemoteState) {
    let cutoff = chrono::Utc::now().timestamp() - PLAN_TTL.as_secs() as i64;
    state
        .plans
        .retain(|_, plan| plan.created_at >= cutoff && plan.created_at > 0);
}

fn remote_plan_count() -> Result<usize, CliError> {
    with_remote_state(|state| Ok(state.plans.len()))
}

fn store_remote_plan(id: String, plan: PlannedOperation) -> Result<bool, CliError> {
    with_remote_state(|state| {
        if state.plans.len() >= MAX_STORED_PLANS {
            return Ok(false);
        }
        state.plans.insert(id, plan);
        Ok(true)
    })
}

fn get_remote_plan(id: &str) -> Result<Option<PlannedOperation>, CliError> {
    with_remote_state(|state| Ok(state.plans.get(id).cloned()))
}

fn remove_remote_plan(id: &str) -> Result<(), CliError> {
    with_remote_state(|state| {
        state.plans.remove(id);
        Ok(())
    })
}

fn cached_remote_mutation(key: &str) -> Result<Option<CachedMutation>, CliError> {
    with_remote_state(|state| Ok(state.idempotency.get(key).cloned()))
}

fn complete_remote_mutation(
    plan_id: &str,
    idempotency_key: String,
    cached: CachedMutation,
) -> Result<(), CliError> {
    with_remote_state(|state| {
        state.plans.remove(plan_id);
        if state.idempotency.len() >= MAX_IDEMPOTENCY_RESULTS
            && let Some(oldest) = state
                .idempotency
                .iter()
                .min_by_key(|(_, value)| value.completed_at)
                .map(|(key, _)| key.clone())
        {
            state.idempotency.remove(&oldest);
        }
        state.idempotency.insert(idempotency_key, cached);
        Ok(())
    })
}

fn cache_remote_mutation(idempotency_key: String, cached: CachedMutation) -> Result<(), CliError> {
    with_remote_state(|state| {
        if state.idempotency.len() >= MAX_IDEMPOTENCY_RESULTS
            && let Some(oldest) = state
                .idempotency
                .iter()
                .min_by_key(|(_, value)| value.completed_at)
                .map(|(key, _)| key.clone())
        {
            state.idempotency.remove(&oldest);
        }
        state.idempotency.insert(idempotency_key, cached);
        Ok(())
    })
}

fn is_direct_mutation(method: &str) -> bool {
    matches!(
        method,
        methods::PROVIDER_CREATE
            | methods::PROVIDER_UPDATE
            | methods::PROVIDER_DELETE
            | methods::PROVIDER_DUPLICATE
            | methods::PROVIDER_SORT
            | methods::PROVIDER_COPY
            | methods::PROVIDER_SEED_OFFICIAL
            | methods::PROVIDER_IMPORT_LIVE
            | methods::PROVIDER_SYNC_LIVE
            | methods::PROVIDER_ADD_TO_LIVE
            | methods::PROVIDER_REMOVE_FROM_LIVE
            | methods::PROVIDER_ENDPOINT_ADD
            | methods::PROVIDER_ENDPOINT_REMOVE
            | methods::PROVIDER_COMMON_SET
            | methods::PROVIDER_COMMON_APPLY
            | methods::MCP_UPSERT
            | methods::MCP_DELETE
            | methods::MCP_SET_APP
            | methods::MCP_SYNC_ALL
            | methods::MCP_IMPORT
            | methods::SKILL_INSTALL
            | methods::SKILL_UNINSTALL
            | methods::SKILL_UPDATE
            | methods::SKILL_UPDATE_ALL
            | methods::SKILL_SET_APP
            | methods::SKILL_REPO_UPSERT
            | methods::SKILL_REPO_DELETE
            | methods::USAGE_SYNC
            | methods::PRICING_REFRESH
            | methods::PRICING_OVERRIDE_SET
            | methods::PRICING_OVERRIDE_DELETE
            | methods::PRICING_DEFAULTS_SET
            | methods::SESSION_DELETE
            | methods::SESSION_INDEX_BUILD
            | methods::SESSION_INDEX_MAINTAIN
            | methods::SESSION_INDEX_DELETE
            | methods::GATEWAY_START
            | methods::GATEWAY_STOP
            | methods::GATEWAY_CONNECTION_INFO
            | methods::STATION_CREATE
            | methods::STATION_UPDATE
            | methods::STATION_DELETE
            | methods::STATION_SET_ENABLED
            | methods::STATION_SELECT
            | methods::STATION_APPLY
            | methods::STATION_DISCONNECT
            | methods::STATION_CONNECTION_INFO
            | methods::STATION_IMPORT_PROVIDER
            | methods::PROXY_SET
            | methods::SETTINGS_SET
            | methods::SETTINGS_UNSET
            | methods::SYNC_CONFIGURE
            | methods::SYNC_UPLOAD
            | methods::SYNC_DOWNLOAD
            | methods::BACKUP_CREATE
            | methods::BACKUP_RENAME
            | methods::BACKUP_RESTORE
            | methods::BACKUP_DELETE
            | methods::BACKUP_EXPORT_SQL
            | methods::BACKUP_IMPORT_SQL
            | methods::BACKUP_POLICY_SET
            | methods::TOOL_INSTALL
            | methods::TOOL_UPDATE
            | methods::TOOL_ADVANCED_WRITE
            | methods::UPDATE_INSTALL
            | methods::DATA_DIR_SET
            | methods::DATA_DIR_RESET
            | methods::MIGRATE_CCSWITCH_IMPORT
            | methods::APP_SET_ENABLED
    )
}

fn argv_for_request(request: &RequestFrame) -> Result<Vec<String>, String> {
    match request.method.as_str() {
        methods::STATUS_READ => expect_empty_params(&request.params).map(|_| vec!["status".into()]),
        methods::DOCTOR_RUN => {
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                #[serde(default)]
                network: bool,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["doctor".into()];
            if params.network {
                argv.push("--network".into());
            }
            Ok(argv)
        }
        methods::APP_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["app".into(), "list".into()])
        }
        methods::APP_GET | methods::APP_SCHEMA => {
            let params = app_params(&request.params, request.method.as_str())?;
            Ok(if request.method == methods::APP_GET {
                vec!["app".into(), "show".into(), params.app]
            } else {
                vec![
                    "app".into(),
                    "schema".into(),
                    params.app,
                    "--resource".into(),
                    "provider".into(),
                ]
            })
        }
        methods::PROVIDER_LIST => {
            let params: AppParams = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider.list parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "provider".into(),
                "list".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_GET => {
            let params: ProviderParams = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider.get parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            validate_text(&params.provider_id, "providerId")?;
            Ok(vec![
                "provider".into(),
                "show".into(),
                params.provider_id,
                "--app".into(),
                params.app,
            ])
        }
        methods::MCP_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["mcp".into(), "list".into()])
        }
        methods::MCP_GET => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid mcp.get parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec!["mcp".into(), "show".into(), params.id])
        }
        methods::MCP_DELETE => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid mcp.delete parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec![
                "--yes".into(),
                "mcp".into(),
                "delete".into(),
                params.id,
            ])
        }
        methods::MCP_SET_APP => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                id: String,
                app: String,
                enabled: bool,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid mcp.setApp parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "mcp".into(),
                if params.enabled {
                    "enable".into()
                } else {
                    "disable".into()
                },
                params.id,
                "--app".into(),
                params.app,
            ])
        }
        methods::MCP_SYNC_ALL => {
            expect_empty_params(&request.params).map(|_| vec!["mcp".into(), "sync-all".into()])
        }
        methods::MCP_IMPORT => {
            let params = app_params(&request.params, "mcp.import")?;
            Ok(vec![
                "mcp".into(),
                "import".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::SKILL_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["skill".into(), "list".into()])
        }
        methods::SKILL_GET => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.get parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec!["skill".into(), "show".into(), params.id])
        }
        methods::SKILL_SEARCH => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                query: String,
                limit: usize,
                offset: usize,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.search parameters: {error}"))?;
            validate_text(&params.query, "query")?;
            if params.limit == 0 || params.limit > 100 {
                return Err("limit must be between 1 and 100".to_string());
            }
            Ok(vec![
                "skill".into(),
                "search".into(),
                params.query,
                "--limit".into(),
                params.limit.to_string(),
                "--offset".into(),
                params.offset.to_string(),
            ])
        }
        methods::SKILL_DISCOVER => {
            expect_empty_params(&request.params).map(|_| vec!["skill".into(), "discover".into()])
        }
        methods::SKILL_UNINSTALL => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.uninstall parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec![
                "--yes".into(),
                "skill".into(),
                "uninstall".into(),
                params.id,
            ])
        }
        methods::SKILL_CHECK_ALL => {
            expect_empty_params(&request.params).map(|_| vec!["skill".into(), "check-all".into()])
        }
        methods::SKILL_UPDATE => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.update parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec!["skill".into(), "update".into(), params.id])
        }
        methods::SKILL_UPDATE_ALL => {
            expect_empty_params(&request.params).map(|_| vec!["skill".into(), "update-all".into()])
        }
        methods::SKILL_SET_APP => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                id: String,
                app: String,
                enabled: bool,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.setApp parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "skill".into(),
                if params.enabled {
                    "enable".into()
                } else {
                    "disable".into()
                },
                params.id,
                "--app".into(),
                params.app,
            ])
        }
        methods::SKILL_REPO_LIST => expect_empty_params(&request.params)
            .map(|_| vec!["skill".into(), "repo".into(), "list".into()]),
        methods::SKILL_REPO_UPSERT => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Repo {
                owner: String,
                name: String,
                branch: String,
                enabled: bool,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                repo: Repo,
                #[serde(default)]
                original_id: Option<String>,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid skill.repo.upsert parameters: {error}"))?;
            validate_text(&params.repo.owner, "repo.owner")?;
            validate_text(&params.repo.name, "repo.name")?;
            validate_text(&params.repo.branch, "repo.branch")?;
            if let Some(id) = params.original_id {
                validate_text(&id, "originalId")?;
                Ok(vec![
                    "skill".into(),
                    "repo".into(),
                    "update".into(),
                    id,
                    "--branch".into(),
                    params.repo.branch,
                    "--enabled".into(),
                    params.repo.enabled.to_string(),
                ])
            } else {
                Ok(vec![
                    "skill".into(),
                    "repo".into(),
                    "add".into(),
                    format!(
                        "https://github.com/{}/{}.git",
                        params.repo.owner, params.repo.name
                    ),
                    "--branch".into(),
                    params.repo.branch,
                    "--enabled".into(),
                    params.repo.enabled.to_string(),
                ])
            }
        }
        methods::APP_SET_ENABLED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                app: String,
                enabled: bool,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid app.setEnabled parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "app".into(),
                if params.enabled {
                    "enable".into()
                } else {
                    "disable".into()
                },
                params.app,
            ])
        }
        methods::SKILL_REPO_DELETE | methods::SKILL_REPO_CATALOG => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid {} parameters: {error}", request.method))?;
            validate_text(&params.id, "id")?;
            let mut argv = Vec::new();
            if request.method == methods::SKILL_REPO_DELETE {
                argv.push("--yes".into());
            }
            argv.extend([
                "skill".into(),
                "repo".into(),
                if request.method == methods::SKILL_REPO_DELETE {
                    "remove".into()
                } else {
                    "catalog".into()
                },
                params.id,
            ]);
            Ok(argv)
        }
        methods::USAGE_SUMMARY
        | methods::USAGE_BY_APP
        | methods::USAGE_PROVIDERS
        | methods::USAGE_MODELS => {
            let command = match request.method.as_str() {
                methods::USAGE_SUMMARY => "summary",
                methods::USAGE_BY_APP => "by-app",
                methods::USAGE_PROVIDERS => "providers",
                _ => "models",
            };
            usage_query_argv(&request.params, command)
        }
        methods::USAGE_SOURCES => {
            expect_empty_params(&request.params).map(|_| vec!["usage".into(), "sources".into()])
        }
        methods::USAGE_TREND => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(flatten)]
                query: UsageQueryParams,
                #[serde(default = "default_usage_interval")]
                interval: String,
            }
            let params: Params = params_or_default(&request.params)?;
            if !matches!(params.interval.as_str(), "day" | "week" | "month") {
                return Err("interval must be day, week, or month".to_string());
            }
            let mut argv = usage_query_argv_value(params.query, "trend")?;
            argv.extend(["--interval".into(), params.interval]);
            Ok(argv)
        }
        methods::USAGE_LOGS => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                #[serde(flatten)]
                query: UsageQueryParams,
                status: Option<u16>,
                #[serde(default)]
                page: u32,
                #[serde(default = "default_usage_page_size")]
                page_size: u32,
            }
            let params: Params = params_or_default(&request.params)?;
            if params.page_size == 0 || params.page_size > 1_000 {
                return Err("pageSize must be between 1 and 1000".to_string());
            }
            let mut argv = usage_query_argv_value(params.query, "logs")?;
            if let Some(status) = params.status {
                argv.extend(["--status".into(), status.to_string()]);
            }
            argv.extend([
                "--page".into(),
                params.page.to_string(),
                "--page-size".into(),
                params.page_size.to_string(),
            ]);
            Ok(argv)
        }
        methods::USAGE_GET => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                request_id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid usage.get parameters: {error}"))?;
            validate_text(&params.request_id, "requestId")?;
            Ok(vec!["usage".into(), "show".into(), params.request_id])
        }
        methods::USAGE_SYNC => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(default)]
                apps: Vec<String>,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["usage".into(), "sync".into()];
            for app in params.apps {
                validate_text(&app, "apps")?;
                argv.extend(["--app".into(), app]);
            }
            Ok(argv)
        }
        methods::USAGE_LIMITS => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Params {
                app: Option<String>,
                provider: Option<String>,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["usage".into(), "limits".into()];
            if let Some(app) = params.app {
                validate_text(&app, "app")?;
                argv.extend(["--app".into(), app]);
            }
            if let Some(provider) = params.provider {
                validate_text(&provider, "provider")?;
                argv.extend(["--provider".into(), provider]);
            }
            Ok(argv)
        }
        methods::PRICING_STATUS => {
            expect_empty_params(&request.params).map(|_| vec!["pricing".into(), "status".into()])
        }
        methods::PRICING_REFRESH => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(default)]
                force: bool,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["pricing".into(), "refresh".into()];
            if params.force {
                argv.push("--force".into());
            }
            Ok(argv)
        }
        methods::PRICING_OVERRIDE_LIST => expect_empty_params(&request.params)
            .map(|_| vec!["pricing".into(), "override".into(), "list".into()]),
        methods::PRICING_OVERRIDE_DELETE => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                model_id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid pricing.override.delete parameters: {error}"))?;
            validate_text(&params.model_id, "modelId")?;
            Ok(vec![
                "--yes".into(),
                "pricing".into(),
                "override".into(),
                "remove".into(),
                "--model".into(),
                params.model_id,
            ])
        }
        methods::PRICING_DEFAULTS_GET => expect_empty_params(&request.params)
            .map(|_| vec!["pricing".into(), "defaults".into(), "get".into()]),
        methods::SESSION_LIST => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Params {
                app: Option<String>,
                query: Option<String>,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["session".into(), "list".into()];
            if let Some(app) = params.app {
                validate_text(&app, "app")?;
                argv.extend(["--app".into(), app]);
            }
            if let Some(query) = params.query {
                validate_text(&query, "query")?;
                argv.extend(["--query".into(), query]);
            }
            Ok(argv)
        }
        methods::SESSION_GET | methods::SESSION_DELETE => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
                app: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid {} parameters: {error}", request.method))?;
            validate_text(&params.id, "id")?;
            validate_text(&params.app, "app")?;
            let mut argv = Vec::new();
            if request.method == methods::SESSION_DELETE {
                argv.push("--yes".into());
            }
            argv.extend([
                "session".into(),
                if request.method == methods::SESSION_DELETE {
                    "delete".into()
                } else {
                    "show".into()
                },
                params.id,
                "--app".into(),
                params.app,
            ]);
            Ok(argv)
        }
        methods::SESSION_SEARCH => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                query: String,
                #[serde(default = "default_session_search_limit")]
                limit: usize,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid session.search parameters: {error}"))?;
            validate_text(&params.query, "query")?;
            if !(1..=10_000).contains(&params.limit) {
                return Err("limit must be between 1 and 10000".to_string());
            }
            Ok(vec![
                "session".into(),
                "search".into(),
                params.query,
                "--limit".into(),
                params.limit.to_string(),
            ])
        }
        methods::SESSION_INDEX_STATUS
        | methods::SESSION_INDEX_BUILD
        | methods::SESSION_INDEX_DELETE => expect_empty_params(&request.params).map(|_| {
            let command = match request.method.as_str() {
                methods::SESSION_INDEX_STATUS => "index-status",
                methods::SESSION_INDEX_BUILD => "index-build",
                methods::SESSION_INDEX_DELETE => "index-delete",
                _ => unreachable!(),
            };
            let mut argv = vec!["session".into(), command.into()];
            if request.method == methods::SESSION_INDEX_DELETE {
                argv.insert(0, "--yes".into());
            }
            argv
        }),
        methods::SESSION_INDEX_MAINTAIN => {
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                #[serde(default = "default_session_index_budget")]
                budget_seconds: u64,
            }
            let params: Params = params_or_default(&request.params)?;
            if !(1..=300).contains(&params.budget_seconds) {
                return Err("budgetSeconds must be between 1 and 300".to_string());
            }
            Ok(vec![
                "session".into(),
                "index-maintain".into(),
                "--budget-seconds".into(),
                params.budget_seconds.to_string(),
            ])
        }
        methods::PROVIDER_DELETE => {
            let params = provider_params(&request.params, "provider.delete")?;
            Ok(vec![
                "--yes".into(),
                "provider".into(),
                "delete".into(),
                params.provider_id,
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_DUPLICATE => {
            let params = provider_params(&request.params, "provider.duplicate")?;
            Ok(vec![
                "provider".into(),
                "duplicate".into(),
                params.provider_id,
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_SORT => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                app: String,
                ids: Vec<String>,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider.sort parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            if params.ids.is_empty() {
                return Err("ids must contain at least one provider".to_string());
            }
            for id in &params.ids {
                validate_text(id, "ids")?;
            }
            let mut argv = vec!["provider".into(), "sort".into(), "--app".into(), params.app];
            argv.extend(params.ids);
            Ok(argv)
        }
        methods::PROVIDER_COPY => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                provider_id: String,
                from_app: String,
                to_app: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider.copy parameters: {error}"))?;
            validate_text(&params.provider_id, "providerId")?;
            validate_text(&params.from_app, "fromApp")?;
            validate_text(&params.to_app, "toApp")?;
            Ok(vec![
                "provider".into(),
                "copy".into(),
                params.provider_id,
                "--from-app".into(),
                params.from_app,
                "--to-app".into(),
                params.to_app,
            ])
        }
        methods::PROVIDER_SEED_OFFICIAL => {
            let params = app_params(&request.params, "provider.seedOfficial")?;
            Ok(vec![
                "provider".into(),
                "seed-official".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_IMPORT_LIVE => {
            let params = app_params(&request.params, "provider.importLive")?;
            Ok(vec![
                "provider".into(),
                "import-live".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_SYNC_LIVE => {
            let params = app_params(&request.params, "provider.syncLive")?;
            Ok(vec![
                "provider".into(),
                "sync-live".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_ADD_TO_LIVE
        | methods::PROVIDER_REMOVE_FROM_LIVE
        | methods::PROVIDER_TEST
        | methods::PROVIDER_SPEED_TEST
        | methods::PROVIDER_MODELS
        | methods::PROVIDER_BALANCE
        | methods::PROVIDER_QUOTA
        | methods::PROVIDER_ENDPOINT_LIST => {
            let params = provider_params(&request.params, request.method.as_str())?;
            let command = match request.method.as_str() {
                methods::PROVIDER_ADD_TO_LIVE => "add-to-live",
                methods::PROVIDER_REMOVE_FROM_LIVE => "remove-from-live",
                methods::PROVIDER_TEST => "test",
                methods::PROVIDER_SPEED_TEST => "speed-test",
                methods::PROVIDER_MODELS => "models",
                methods::PROVIDER_BALANCE => "balance",
                methods::PROVIDER_QUOTA => "quota",
                methods::PROVIDER_ENDPOINT_LIST => "endpoint",
                _ => unreachable!(),
            };
            if request.method == methods::PROVIDER_ENDPOINT_LIST {
                Ok(vec![
                    "provider".into(),
                    "endpoint".into(),
                    "list".into(),
                    params.provider_id,
                    "--app".into(),
                    params.app,
                ])
            } else {
                Ok(vec![
                    "provider".into(),
                    command.into(),
                    params.provider_id,
                    "--app".into(),
                    params.app,
                ])
            }
        }
        methods::PROVIDER_ENDPOINT_ADD | methods::PROVIDER_ENDPOINT_REMOVE => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                app: String,
                provider_id: String,
                url: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider endpoint parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            validate_text(&params.provider_id, "providerId")?;
            validate_url(&params.url)?;
            Ok(vec![
                "provider".into(),
                "endpoint".into(),
                if request.method == methods::PROVIDER_ENDPOINT_ADD {
                    "add".into()
                } else {
                    "remove".into()
                },
                params.provider_id,
                params.url,
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_COMMON_GET | methods::PROVIDER_COMMON_EXTRACT => {
            let params = app_params(&request.params, request.method.as_str())?;
            Ok(vec![
                "config".into(),
                "common".into(),
                if request.method == methods::PROVIDER_COMMON_GET {
                    "get".into()
                } else {
                    "extract".into()
                },
                "--app".into(),
                params.app,
            ])
        }
        methods::PROVIDER_COMMON_APPLY => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                app: String,
                #[serde(default)]
                provider_ids: Vec<String>,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid provider.common.apply parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            let mut argv = vec![
                "config".into(),
                "common".into(),
                "apply".into(),
                "--app".into(),
                params.app,
            ];
            for id in params.provider_ids {
                validate_text(&id, "providerIds")?;
                argv.extend(["--provider".into(), id]);
            }
            Ok(argv)
        }
        methods::GATEWAY_STATUS => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "status".into()])
        }
        methods::GATEWAY_START => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "start".into()])
        }
        methods::GATEWAY_STOP => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "stop".into()])
        }
        methods::GATEWAY_CONNECTION_INFO => expect_empty_params(&request.params)
            .map(|_| vec!["gateway".into(), "connection-info".into()]),
        methods::STATION_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["station".into(), "list".into()])
        }
        methods::STATION_GET
        | methods::STATION_DELETE
        | methods::STATION_PROBE
        | methods::STATION_QUOTA
        | methods::STATION_MODELS => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                station_id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid station parameters: {error}"))?;
            validate_text(&params.station_id, "stationId")?;
            let command = match request.method.as_str() {
                methods::STATION_GET => "show",
                methods::STATION_DELETE => "delete",
                methods::STATION_PROBE => "probe",
                methods::STATION_QUOTA => "quota",
                methods::STATION_MODELS => "models",
                _ => unreachable!(),
            };
            let mut argv = vec!["station".into(), command.into(), params.station_id];
            if request.method == methods::STATION_DELETE {
                argv.insert(0, "--yes".into());
            }
            Ok(argv)
        }
        methods::STATION_SET_ENABLED => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                station_id: String,
                enabled: bool,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid station.setEnabled parameters: {error}"))?;
            validate_text(&params.station_id, "stationId")?;
            Ok(vec![
                "station".into(),
                if params.enabled {
                    "enable".into()
                } else {
                    "disable".into()
                },
                params.station_id,
            ])
        }
        methods::STATION_SELECT => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                station_id: String,
                app: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid station.select parameters: {error}"))?;
            validate_text(&params.station_id, "stationId")?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "station".into(),
                "select".into(),
                params.station_id,
                "--app".into(),
                params.app,
            ])
        }
        methods::STATION_DISCONNECT => {
            let params = app_params(&request.params, request.method.as_str())?;
            Ok(vec![
                "station".into(),
                "disconnect".into(),
                "--app".into(),
                params.app,
            ])
        }
        methods::STATION_CONNECTION_INFO => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                station_id: String,
                app: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid station.connectionInfo parameters: {error}"))?;
            validate_text(&params.station_id, "stationId")?;
            validate_text(&params.app, "app")?;
            Ok(vec![
                "station".into(),
                "connection-info".into(),
                params.station_id,
                "--app".into(),
                params.app,
            ])
        }
        methods::STATION_IMPORT_PROVIDER => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                app: String,
                provider_id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid station.importProvider parameters: {error}"))?;
            validate_text(&params.app, "app")?;
            validate_text(&params.provider_id, "providerId")?;
            Ok(vec![
                "gateway".into(),
                "channel".into(),
                "import-provider".into(),
                "--app".into(),
                params.app,
                "--provider".into(),
                params.provider_id,
            ])
        }
        methods::PROXY_GET => expect_empty_params(&request.params)
            .map(|_| vec!["settings".into(), "proxy".into(), "show".into()]),
        methods::SETTINGS_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["settings".into(), "list".into()])
        }
        methods::SETTINGS_GET | methods::SETTINGS_UNSET => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                path: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid settings parameters: {error}"))?;
            validate_setting_path(&params.path)?;
            Ok(vec![
                "settings".into(),
                if request.method == methods::SETTINGS_GET {
                    "get".into()
                } else {
                    "unset".into()
                },
                params.path,
            ])
        }
        methods::SYNC_STATUS
        | methods::SYNC_TEST
        | methods::SYNC_UPLOAD
        | methods::SYNC_DOWNLOAD
        | methods::SYNC_REMOTE_INFO => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                backend: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid sync parameters: {error}"))?;
            if !matches!(params.backend.as_str(), "webdav" | "s3") {
                return Err("backend must be webdav or s3".to_string());
            }
            let command = match request.method.as_str() {
                methods::SYNC_STATUS => "status",
                methods::SYNC_TEST => "test",
                methods::SYNC_UPLOAD => "upload",
                methods::SYNC_DOWNLOAD => "download",
                methods::SYNC_REMOTE_INFO => "remote-info",
                _ => unreachable!(),
            };
            let mut argv = vec!["sync".into(), params.backend, command.into()];
            if request.method == methods::SYNC_DOWNLOAD {
                argv.insert(0, "--yes".into());
            }
            Ok(argv)
        }
        methods::BACKUP_LIST | methods::BACKUP_POLICY_GET => expect_empty_params(&request.params)
            .map(|_| {
                if request.method == methods::BACKUP_LIST {
                    vec!["backup".into(), "list".into()]
                } else {
                    vec!["backup".into(), "policy".into(), "show".into()]
                }
            }),
        methods::BACKUP_CREATE => {
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                name: Option<String>,
            }
            let params: Params = params_or_default(&request.params)?;
            let mut argv = vec!["backup".into(), "create".into()];
            if let Some(name) = params.name {
                validate_text(&name, "name")?;
                argv.extend(["--name".into(), name]);
            }
            Ok(argv)
        }
        methods::BACKUP_RENAME => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
                name: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid backup.rename parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            validate_text(&params.name, "name")?;
            Ok(vec![
                "backup".into(),
                "rename".into(),
                params.id,
                params.name,
            ])
        }
        methods::BACKUP_RESTORE | methods::BACKUP_DELETE => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid backup mutation parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec![
                "--yes".into(),
                "backup".into(),
                if request.method == methods::BACKUP_RESTORE {
                    "restore".into()
                } else {
                    "delete".into()
                },
                params.id,
            ])
        }
        methods::BACKUP_EXPORT_SQL | methods::BACKUP_IMPORT_SQL => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                path: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid backup SQL parameters: {error}"))?;
            validate_remote_path(&params.path)?;
            let mut argv = vec![
                "backup".into(),
                if request.method == methods::BACKUP_EXPORT_SQL {
                    "export-sql".into()
                } else {
                    "import-sql".into()
                },
                params.path,
            ];
            if request.method == methods::BACKUP_IMPORT_SQL {
                argv.insert(0, "--yes".into());
            }
            Ok(argv)
        }
        methods::BACKUP_POLICY_SET => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                interval_hours: u32,
                retain: u32,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid backup policy parameters: {error}"))?;
            if params.interval_hours == 0 || params.retain == 0 || params.retain > 1_000 {
                return Err(
                    "intervalHours must be positive and retain must be between 1 and 1000"
                        .to_string(),
                );
            }
            Ok(vec![
                "backup".into(),
                "policy".into(),
                "set".into(),
                "--interval".into(),
                format!("{}h", params.interval_hours),
                "--retain".into(),
                params.retain.to_string(),
            ])
        }
        methods::TOOL_VERSIONS => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(default)]
                tools: Vec<String>,
            }
            let params: Params = params_or_default(&request.params)?;
            for tool in &params.tools {
                validate_text(tool, "tool")?;
            }
            let mut argv = vec!["tool".into(), "versions".into()];
            argv.extend(params.tools);
            Ok(argv)
        }
        methods::TOOL_PROBE | methods::TOOL_INSTALL | methods::TOOL_UPDATE => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                tool: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid tool parameters: {error}"))?;
            validate_text(&params.tool, "tool")?;
            Ok(vec![
                "tool".into(),
                match request.method.as_str() {
                    methods::TOOL_PROBE => "probe".into(),
                    methods::TOOL_INSTALL => "install".into(),
                    methods::TOOL_UPDATE => "update".into(),
                    _ => unreachable!(),
                },
                params.tool,
            ])
        }
        methods::TOOL_ADVANCED_READ => advanced_read_argv(&request.params),
        methods::UPDATE_STATUS | methods::UPDATE_CHECK | methods::UPDATE_INSTALL => {
            expect_empty_params(&request.params).map(|_| {
                let command = match request.method.as_str() {
                    methods::UPDATE_STATUS => "status",
                    methods::UPDATE_CHECK => "check",
                    methods::UPDATE_INSTALL => "install",
                    _ => unreachable!(),
                };
                let mut argv = vec!["update".into(), command.into()];
                if request.method == methods::UPDATE_INSTALL {
                    argv.insert(0, "--yes".into());
                }
                argv
            })
        }
        methods::DATA_DIR_SHOW | methods::DATA_DIR_RESET => expect_empty_params(&request.params)
            .map(|_| {
                vec![
                    "data-dir".into(),
                    if request.method == methods::DATA_DIR_SHOW {
                        "show".into()
                    } else {
                        "reset".into()
                    },
                ]
            }),
        methods::DATA_DIR_SET => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                path: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid dataDir.set parameters: {error}"))?;
            validate_remote_path(&params.path)?;
            Ok(vec!["data-dir".into(), "set".into(), params.path])
        }
        methods::MIGRATE_CCSWITCH_DETECT
        | methods::MIGRATE_CCSWITCH_PLAN
        | methods::MIGRATE_CCSWITCH_IMPORT => expect_empty_params(&request.params).map(|_| {
            vec![
                "migrate".into(),
                "ccswitch".into(),
                match request.method.as_str() {
                    methods::MIGRATE_CCSWITCH_DETECT => "detect".into(),
                    methods::MIGRATE_CCSWITCH_PLAN => "plan".into(),
                    methods::MIGRATE_CCSWITCH_IMPORT => "import".into(),
                    _ => unreachable!(),
                },
            ]
        }),
        methods::OPERATION_LIST => {
            expect_empty_params(&request.params).map(|_| vec!["operation".into(), "list".into()])
        }
        methods::OPERATION_INSPECT => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params = serde_json::from_value(request.params.clone())
                .map_err(|error| format!("invalid operation.inspect parameters: {error}"))?;
            validate_text(&params.id, "id")?;
            Ok(vec!["operation".into(), "inspect".into(), params.id])
        }
        _ => Err("method does not have a direct execution mapping".to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppParams {
    app: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderParams {
    app: String,
    provider_id: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UsageQueryParams {
    from: Option<i64>,
    to: Option<i64>,
    app: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

fn default_usage_interval() -> String {
    "day".to_string()
}

fn default_usage_page_size() -> u32 {
    50
}

fn usage_query_argv(value: &Value, command: &str) -> Result<Vec<String>, String> {
    let params: UsageQueryParams = params_or_default(value)?;
    usage_query_argv_value(params, command)
}

fn usage_query_argv_value(params: UsageQueryParams, command: &str) -> Result<Vec<String>, String> {
    if params
        .from
        .zip(params.to)
        .is_some_and(|(from, to)| from > to)
    {
        return Err("from must not be later than to".to_string());
    }
    let mut argv = vec!["usage".into(), command.into()];
    if let Some(from) = params.from {
        argv.extend(["--from".into(), from.to_string()]);
    }
    if let Some(to) = params.to {
        argv.extend(["--to".into(), to.to_string()]);
    }
    for (flag, value) in [
        ("--app", params.app),
        ("--provider", params.provider),
        ("--model", params.model),
    ] {
        if let Some(value) = value {
            validate_text(&value, flag.trim_start_matches("--"))?;
            argv.extend([flag.into(), value]);
        }
    }
    Ok(argv)
}

fn app_params(value: &Value, method: &str) -> Result<AppParams, String> {
    let params: AppParams = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid {method} parameters: {error}"))?;
    validate_text(&params.app, "app")?;
    Ok(params)
}

fn provider_params(value: &Value, method: &str) -> Result<ProviderParams, String> {
    let params: ProviderParams = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid {method} parameters: {error}"))?;
    validate_text(&params.app, "app")?;
    validate_text(&params.provider_id, "providerId")?;
    Ok(params)
}

fn params_or_default<T>(value: &Value) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let value = if value.is_null() {
        json!({})
    } else {
        value.clone()
    };
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn expect_empty_params(value: &Value) -> Result<(), String> {
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err("this method does not accept parameters".to_string())
    }
}

fn validate_text(value: &str, field: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} must contain 1 to 256 printable characters"
        ));
    }
    Ok(())
}

fn validate_setting_path(path: &str) -> Result<(), String> {
    validate_text(path, "path")?;
    if path.starts_with('-')
        || path
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || "_-.".contains(character)))
    {
        return Err("path contains unsupported characters".to_string());
    }
    Ok(())
}

fn validate_remote_path(path: &str) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty()
        || path.len() > 4096
        || path.starts_with('-')
        || path.chars().any(char::is_control)
    {
        return Err("path must contain 1 to 4096 printable characters".to_string());
    }
    Ok(())
}

const fn default_session_search_limit() -> usize {
    500
}

const fn default_session_index_budget() -> u64 {
    20
}

fn validate_url(value: &str) -> Result<(), String> {
    validate_text(value, "url")?;
    let url = url::Url::parse(value).map_err(|error| format!("url is invalid: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("url must be an absolute http or https URL".to_string());
    }
    Ok(())
}

fn invalid_params_response(
    protocol_version: u32,
    request_id: String,
    method: &str,
    error: serde_json::Error,
) -> ResponseFrame {
    invalid_argument_response(
        protocol_version,
        request_id,
        format!("invalid {method} parameters: {error}"),
    )
}

fn invalid_argument_response(
    protocol_version: u32,
    request_id: String,
    message: impl AsRef<str>,
) -> ResponseFrame {
    error_response(
        protocol_version,
        request_id,
        "INVALID_ARGUMENT",
        message.as_ref(),
        false,
        Value::Null,
    )
}

fn secret_write_denied(protocol_version: u32, request_id: String) -> ResponseFrame {
    error_response(
        protocol_version,
        request_id,
        "PERMISSION_DENIED",
        "the remote policy does not allow writing secrets",
        false,
        json!({ "capability": "secrets.write" }),
    )
}

fn contains_secret_write(value: &Value) -> bool {
    match value {
        Value::Object(values) => values.iter().any(|(key, value)| {
            (is_secret_key(key) && secret_value_is_present(value)) || contains_secret_write(value)
        }),
        Value::Array(values) => {
            (values.len() == 2
                && values[0].as_str().is_some_and(is_secret_key)
                && secret_value_is_present(&values[1]))
                || values.iter().any(contains_secret_write)
        }
        _ => false,
    }
}

fn strip_redacted_secret_placeholders(value: &mut Value) {
    match value {
        Value::Object(values) => {
            values.retain(|key, value| {
                !(is_secret_key(key)
                    && value.as_str().is_some_and(|value| {
                        !value.is_empty() && value.chars().all(|character| character == '*')
                    }))
            });
            for value in values.values_mut() {
                strip_redacted_secret_placeholders(value);
            }
        }
        Value::Array(values) => {
            if values.len() == 2
                && values[0].as_str().is_some_and(is_secret_key)
                && values[1].as_str().is_some_and(|value| {
                    !value.is_empty() && value.chars().all(|character| character == '*')
                })
            {
                values[1] = Value::Null;
            } else {
                for value in values {
                    strip_redacted_secret_placeholders(value);
                }
            }
        }
        _ => {}
    }
}

fn secret_value_is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => {
            !value.is_empty() && !value.chars().all(|character| character == '*')
        }
        _ => true,
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("authorization")
        || normalized.contains("cookie")
        || normalized == "key"
        || normalized == "api_key"
        || normalized == "apikey"
        || normalized.ends_with("_api_key")
        || normalized.ends_with("_key")
}

fn common_config_may_contain_secret(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace(['-', '.'], "_");
    [
        "api_key",
        "apikey",
        "auth_token",
        "access_token",
        "authorization",
        "password",
        "secret",
        "cookie",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn validate_switch_params(params: &ochub_protocol::ProviderSwitchParams) -> Result<(), String> {
    validate_text(&params.app, "app")?;
    validate_text(&params.provider_id, "providerId")?;
    if !matches!(params.on_drift.as_str(), "abort" | "preserve" | "discard") {
        return Err("onDrift must be abort, preserve, or discard".to_string());
    }
    Ok(())
}

fn required_capability(method: &str) -> Option<Capability> {
    match method {
        methods::STATUS_READ => Some(Capability::StatusRead),
        methods::DOCTOR_RUN => Some(Capability::DoctorRun),
        methods::APP_LIST | methods::APP_GET | methods::APP_SCHEMA => Some(Capability::AppRead),
        methods::APP_SET_ENABLED => Some(Capability::AppWrite),
        methods::PROVIDER_LIST
        | methods::PROVIDER_GET
        | methods::PROVIDER_ENDPOINT_LIST
        | methods::PROVIDER_COMMON_GET
        | methods::PROVIDER_COMMON_EXTRACT => Some(Capability::ProviderRead),
        methods::PROVIDER_CREATE
        | methods::PROVIDER_UPDATE
        | methods::PROVIDER_DELETE
        | methods::PROVIDER_DUPLICATE
        | methods::PROVIDER_SORT
        | methods::PROVIDER_COPY
        | methods::PROVIDER_SEED_OFFICIAL
        | methods::PROVIDER_IMPORT_LIVE
        | methods::PROVIDER_SYNC_LIVE
        | methods::PROVIDER_ADD_TO_LIVE
        | methods::PROVIDER_REMOVE_FROM_LIVE
        | methods::PROVIDER_ENDPOINT_ADD
        | methods::PROVIDER_ENDPOINT_REMOVE
        | methods::PROVIDER_COMMON_SET
        | methods::PROVIDER_COMMON_APPLY
        | methods::PROVIDER_SWITCH_PLAN
        | methods::PROVIDER_SWITCH_APPLY => Some(Capability::ProviderWrite),
        methods::PROVIDER_TEST
        | methods::PROVIDER_SPEED_TEST
        | methods::PROVIDER_MODELS
        | methods::PROVIDER_BALANCE
        | methods::PROVIDER_QUOTA => Some(Capability::ProviderNetwork),
        methods::MCP_LIST | methods::MCP_GET => Some(Capability::McpRead),
        methods::MCP_UPSERT
        | methods::MCP_DELETE
        | methods::MCP_SET_APP
        | methods::MCP_SYNC_ALL
        | methods::MCP_IMPORT => Some(Capability::McpWrite),
        methods::SKILL_LIST | methods::SKILL_GET | methods::SKILL_REPO_LIST => {
            Some(Capability::SkillRead)
        }
        methods::SKILL_SEARCH
        | methods::SKILL_DISCOVER
        | methods::SKILL_CHECK_ALL
        | methods::SKILL_REPO_CATALOG => Some(Capability::SkillNetwork),
        methods::SKILL_INSTALL
        | methods::SKILL_UNINSTALL
        | methods::SKILL_UPDATE
        | methods::SKILL_UPDATE_ALL
        | methods::SKILL_SET_APP
        | methods::SKILL_REPO_UPSERT
        | methods::SKILL_REPO_DELETE => Some(Capability::SkillWrite),
        methods::USAGE_SUMMARY
        | methods::USAGE_SOURCES
        | methods::USAGE_BY_APP
        | methods::USAGE_TREND
        | methods::USAGE_PROVIDERS
        | methods::USAGE_MODELS
        | methods::USAGE_LOGS
        | methods::USAGE_GET
        | methods::USAGE_LIMITS
        | methods::PRICING_STATUS
        | methods::PRICING_OVERRIDE_LIST
        | methods::PRICING_DEFAULTS_GET => Some(Capability::UsageRead),
        methods::USAGE_SYNC
        | methods::PRICING_OVERRIDE_SET
        | methods::PRICING_OVERRIDE_DELETE
        | methods::PRICING_DEFAULTS_SET => Some(Capability::UsageWrite),
        methods::PRICING_REFRESH => Some(Capability::UsageNetwork),
        methods::SESSION_LIST
        | methods::SESSION_GET
        | methods::SESSION_SEARCH
        | methods::SESSION_INDEX_STATUS => Some(Capability::SessionRead),
        methods::SESSION_DELETE
        | methods::SESSION_INDEX_BUILD
        | methods::SESSION_INDEX_MAINTAIN
        | methods::SESSION_INDEX_DELETE => Some(Capability::SessionWrite),
        methods::PROXY_GET => Some(Capability::ProxyRead),
        methods::PROXY_SET => Some(Capability::ProxyWrite),
        methods::PROXY_TEST => Some(Capability::ProxyNetwork),
        methods::SETTINGS_LIST | methods::SETTINGS_GET => Some(Capability::SettingsRead),
        methods::SETTINGS_SET | methods::SETTINGS_UNSET => Some(Capability::SettingsWrite),
        methods::SYNC_STATUS => Some(Capability::SyncRead),
        methods::SYNC_CONFIGURE | methods::SYNC_UPLOAD => Some(Capability::SyncWrite),
        methods::SYNC_TEST | methods::SYNC_REMOTE_INFO => Some(Capability::SyncNetwork),
        methods::SYNC_DOWNLOAD => Some(Capability::BackupRestore),
        methods::BACKUP_LIST | methods::BACKUP_POLICY_GET => Some(Capability::BackupRead),
        methods::BACKUP_CREATE
        | methods::BACKUP_RENAME
        | methods::BACKUP_DELETE
        | methods::BACKUP_EXPORT_SQL
        | methods::BACKUP_POLICY_SET => Some(Capability::BackupWrite),
        methods::BACKUP_RESTORE | methods::BACKUP_IMPORT_SQL => Some(Capability::BackupRestore),
        methods::TOOL_VERSIONS | methods::TOOL_PROBE | methods::TOOL_ADVANCED_READ => {
            Some(Capability::ToolRead)
        }
        methods::TOOL_INSTALL | methods::TOOL_UPDATE | methods::TOOL_ADVANCED_WRITE => {
            Some(Capability::ToolWrite)
        }
        methods::UPDATE_STATUS | methods::UPDATE_CHECK => Some(Capability::UpdateRead),
        methods::UPDATE_INSTALL => Some(Capability::UpdateInstall),
        methods::NODE_UPDATE_STATUS | methods::NODE_UPDATE_CHECK => {
            Some(Capability::NodeUpdateRead)
        }
        methods::NODE_UPDATE_INSTALL_DIRECT => Some(Capability::NodeUpdateInstall),
        methods::DATA_DIR_SHOW
        | methods::MIGRATE_CCSWITCH_DETECT
        | methods::MIGRATE_CCSWITCH_PLAN => Some(Capability::DataRead),
        methods::DATA_DIR_SET | methods::DATA_DIR_RESET => Some(Capability::DataWrite),
        methods::MIGRATE_CCSWITCH_IMPORT => Some(Capability::DataImport),
        methods::GATEWAY_STATUS => Some(Capability::GatewayRead),
        methods::GATEWAY_START | methods::GATEWAY_STOP | methods::GATEWAY_CONNECTION_INFO => {
            Some(Capability::GatewayLifecycle)
        }
        methods::STATION_LIST | methods::STATION_GET | methods::STATION_MODELS => {
            Some(Capability::StationRead)
        }
        methods::STATION_PROBE
        | methods::STATION_QUOTA
        | methods::STATION_DETECT_DIALECTS
        | methods::STATION_FETCH_MODELS
        | methods::STATION_TEST_ENDPOINT => Some(Capability::StationNetwork),
        methods::STATION_CREATE
        | methods::STATION_UPDATE
        | methods::STATION_DELETE
        | methods::STATION_SET_ENABLED
        | methods::STATION_SELECT
        | methods::STATION_APPLY
        | methods::STATION_DISCONNECT
        | methods::STATION_CONNECTION_INFO
        | methods::STATION_IMPORT_PROVIDER => Some(Capability::StationWrite),
        methods::OPERATION_LIST | methods::OPERATION_INSPECT => Some(Capability::OperationRead),
        _ => None,
    }
}

fn capabilities(policy: &ochub_core::remote_policy::RemotePolicy) -> Vec<Capability> {
    if !policy.enabled {
        return Vec::new();
    }
    let mut values = BTreeSet::from([
        Capability::StatusRead,
        Capability::DoctorRun,
        Capability::AppRead,
        Capability::ProviderRead,
        Capability::ProviderNetwork,
        Capability::McpRead,
        Capability::SkillRead,
        Capability::SkillNetwork,
        Capability::UsageRead,
        Capability::UsageNetwork,
        Capability::SessionRead,
        Capability::ProxyRead,
        Capability::ProxyNetwork,
        Capability::SettingsRead,
        Capability::SyncRead,
        Capability::SyncNetwork,
        Capability::BackupRead,
        Capability::ToolRead,
        Capability::UpdateRead,
        Capability::NodeUpdateRead,
        Capability::DataRead,
        Capability::GatewayRead,
        Capability::StationRead,
        Capability::StationNetwork,
        Capability::OperationRead,
    ]);
    if policy.allow_write {
        values.insert(Capability::ProviderWrite);
        values.insert(Capability::AppWrite);
        values.insert(Capability::McpWrite);
        values.insert(Capability::SkillWrite);
        values.insert(Capability::UsageWrite);
        values.insert(Capability::SessionWrite);
        values.insert(Capability::ProxyWrite);
        values.insert(Capability::SettingsWrite);
        values.insert(Capability::SyncWrite);
        values.insert(Capability::BackupWrite);
        values.insert(Capability::ToolWrite);
        values.insert(Capability::DataWrite);
        values.insert(Capability::StationWrite);
    }
    if policy.allow_backup_restore {
        values.insert(Capability::BackupRestore);
        values.insert(Capability::DataImport);
    }
    if policy.allow_update_install && crate::node::managed_updates_supported() {
        values.insert(Capability::UpdateInstall);
        values.insert(Capability::NodeUpdateInstall);
        values.insert(Capability::NodeUpdateRelay);
    }
    if policy.allow_gateway_lifecycle {
        values.insert(Capability::GatewayLifecycle);
    }
    values.into_iter().collect()
}

fn node_descriptor() -> Result<NodeDescriptor, CliError> {
    let identity = ochub_core::node_identity::load_or_create()?;
    Ok(NodeDescriptor {
        id: identity.node_id,
        hostname: environment_value(&["HOSTNAME", "COMPUTERNAME"])
            .unwrap_or_else(|| "unknown".to_string()),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        user: environment_value(&["USER", "USERNAME"]).unwrap_or_else(|| "unknown".to_string()),
    })
}

fn environment_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

async fn runtime_descriptor(cli: &Cli) -> RuntimeDescriptor {
    runtime_descriptor_at(cli.socket.as_deref(), cli.timeout).await
}

async fn runtime_descriptor_at(
    socket: Option<&std::path::Path>,
    timeout: u64,
) -> RuntimeDescriptor {
    let owner = crate::runtime_client::owner_status().ok().flatten();
    let gateway = if owner.is_some() {
        crate::runtime_client::ping(socket, timeout)
            .await
            .ok()
            .and_then(|response| response.data.get("gateway").cloned())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    RuntimeDescriptor {
        persistent: owner.is_some(),
        owner_kind: owner
            .as_ref()
            .map(|record| format!("{:?}", record.kind).to_lowercase()),
        owner_pid: owner.as_ref().map(|record| record.pid),
        gateway,
    }
}

fn revision_for(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Produce the only shape that may cross the SSH protocol boundary.
///
/// The core redactor handles named secret fields. Drift conflicts also carry
/// scalar values under the generic `live` and `incoming` keys, so those values
/// are always masked while retaining the path needed to review the plan.
fn remote_safe_value(value: &Value) -> Value {
    let mut safe = redact_json(value);
    redact_drift_values(&mut safe);
    safe
}

fn redact_drift_values(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let is_drift_conflict = map.contains_key("path")
                && map.contains_key("live")
                && map.contains_key("incoming");
            if is_drift_conflict {
                map.insert("live".to_string(), Value::String("******".to_string()));
                map.insert("incoming".to_string(), Value::String("******".to_string()));
            }
            for child in map.values_mut() {
                redact_drift_values(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact_drift_values(child);
            }
        }
        _ => {}
    }
}

fn execution_error_response(
    protocol_version: u32,
    request_id: String,
    result: ExecutionResult,
) -> ResponseFrame {
    let error = result.error.unwrap_or(IpcError {
        code: "INTERNAL".to_string(),
        message: "runtime returned an error without details".to_string(),
        retryable: false,
        details: Value::Null,
        exit_code: 1,
    });
    error_response(
        protocol_version,
        request_id,
        &error.code,
        &error.message,
        error.retryable,
        error.details,
    )
}

fn cli_error_response(protocol_version: u32, request_id: String, error: CliError) -> ResponseFrame {
    error_response(
        protocol_version,
        request_id,
        error.code(),
        &error.to_string(),
        error.retryable(),
        error.details(),
    )
}

fn error_response(
    protocol_version: u32,
    request_id: String,
    code: &str,
    message: &str,
    retryable: bool,
    details: Value,
) -> ResponseFrame {
    ResponseFrame {
        protocol_version,
        request_id,
        ok: false,
        data: Value::Null,
        warnings: Vec::new(),
        error: Some(RemoteError {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
            details: remote_safe_value(&details),
        }),
        revision: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ochub_protocol::RequestFrame;

    #[cfg(unix)]
    #[test]
    fn managed_symlink_is_recognized_as_the_running_executable() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let versioned = directory.path().join("versions/1.2.3/ochcli");
        fs::create_dir_all(versioned.parent().unwrap()).unwrap();
        fs::write(&versioned, b"binary").unwrap();
        let managed = directory.path().join("ochcli");
        symlink(&versioned, &managed).unwrap();

        assert!(paths_refer_to_same_executable(&versioned, &managed));
        assert!(!paths_refer_to_same_executable(
            &versioned,
            &directory.path().join("old-ochcli")
        ));
    }

    fn request(method: &str, params: Value) -> RequestFrame {
        RequestFrame {
            protocol_version: PROTOCOL_MAX,
            request_id: "request-1".to_string(),
            method: method.to_string(),
            params,
            trace_id: None,
            idempotency_key: None,
            expected_revision: None,
        }
    }

    #[test]
    fn typed_methods_map_to_tokenized_cli_without_shell_parsing() {
        assert_eq!(
            argv_for_request(&request(methods::PROVIDER_LIST, json!({"app": "codex"}))).unwrap(),
            vec!["provider", "list", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::PROVIDER_GET,
                json!({"app": "claude", "providerId": "team"})
            ))
            .unwrap(),
            vec!["provider", "show", "team", "--app", "claude"]
        );
        assert!(
            argv_for_request(&request(
                methods::PROVIDER_GET,
                json!({"app": "codex", "providerId": "x", "command": "rm"})
            ))
            .is_err()
        );
        assert_eq!(
            argv_for_request(&request(
                methods::PROVIDER_DELETE,
                json!({"app": "codex", "providerId": "team"})
            ))
            .unwrap(),
            vec!["--yes", "provider", "delete", "team", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::PROVIDER_DUPLICATE,
                json!({"app": "codex", "providerId": "team"})
            ))
            .unwrap(),
            vec!["provider", "duplicate", "team", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(methods::MCP_LIST, Value::Null)).unwrap(),
            vec!["mcp", "list"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::MCP_SET_APP,
                json!({"id": "context7", "app": "claude", "enabled": true})
            ))
            .unwrap(),
            vec!["mcp", "enable", "context7", "--app", "claude"]
        );
        assert_eq!(
            argv_for_request(&request(methods::SKILL_LIST, Value::Null)).unwrap(),
            vec!["skill", "list"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::SKILL_SET_APP,
                json!({"id": "review", "app": "codex", "enabled": false})
            ))
            .unwrap(),
            vec!["skill", "disable", "review", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::SKILL_REPO_UPSERT,
                json!({
                    "repo": {
                        "owner": "openai",
                        "name": "skills",
                        "branch": "main",
                        "enabled": true
                    }
                })
            ))
            .unwrap(),
            vec![
                "skill",
                "repo",
                "add",
                "https://github.com/openai/skills.git",
                "--branch",
                "main",
                "--enabled",
                "true"
            ]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::USAGE_LOGS,
                json!({
                    "from": 100,
                    "to": 200,
                    "app": "claude",
                    "status": 200,
                    "page": 2,
                    "pageSize": 20
                })
            ))
            .unwrap(),
            vec![
                "usage",
                "logs",
                "--from",
                "100",
                "--to",
                "200",
                "--app",
                "claude",
                "--status",
                "200",
                "--page",
                "2",
                "--page-size",
                "20"
            ]
        );
        assert_eq!(
            argv_for_request(&request(methods::PRICING_DEFAULTS_GET, Value::Null)).unwrap(),
            vec!["pricing", "defaults", "get"]
        );
        assert_eq!(
            argv_for_request(&request(methods::PROXY_GET, Value::Null)).unwrap(),
            vec!["settings", "proxy", "show"]
        );
        assert_eq!(
            argv_for_request(&request(methods::SYNC_DOWNLOAD, json!({"backend": "s3"}))).unwrap(),
            vec!["--yes", "sync", "s3", "download"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::BACKUP_POLICY_SET,
                json!({"intervalHours": 12, "retain": 8})
            ))
            .unwrap(),
            vec![
                "backup",
                "policy",
                "set",
                "--interval",
                "12h",
                "--retain",
                "8"
            ]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::TOOL_VERSIONS,
                json!({"tools": ["codex", "claude"]})
            ))
            .unwrap(),
            vec!["tool", "versions", "codex", "claude"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::TOOL_ADVANCED_READ,
                json!({
                    "action": "claude.mcp.validateCommand",
                    "params": { "command": "npx" }
                })
            ))
            .unwrap(),
            vec!["claude", "mcp", "path", "validate-command", "npx"]
        );
        let (payload, argv) = advanced_write_payload(&json!({
            "action": "hermes.memory.write",
            "params": { "content": "remember this" }
        }))
        .unwrap();
        assert!(matches!(payload, Payload::Text(value) if value == "remember this"));
        assert_eq!(argv, vec!["hermes", "memory", "write", "memory", "--from"]);
        assert_eq!(
            argv_for_request(&request(methods::UPDATE_INSTALL, Value::Null)).unwrap(),
            vec!["--yes", "update", "install"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::DATA_DIR_SET,
                json!({"path": "/srv/ochub"})
            ))
            .unwrap(),
            vec!["data-dir", "set", "/srv/ochub"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::SESSION_DELETE,
                json!({"app": "codex", "id": "session-1"})
            ))
            .unwrap(),
            vec!["--yes", "session", "delete", "session-1", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::STATION_SET_ENABLED,
                json!({"stationId": "station-1", "enabled": false})
            ))
            .unwrap(),
            vec!["station", "disable", "station-1"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::STATION_QUOTA,
                json!({"stationId": "station-1"})
            ))
            .unwrap(),
            vec!["station", "quota", "station-1"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::STATION_CONNECTION_INFO,
                json!({"stationId": "station-1", "app": "codex"})
            ))
            .unwrap(),
            vec!["station", "connection-info", "station-1", "--app", "codex"]
        );
        assert_eq!(
            argv_for_request(&request(
                methods::STATION_IMPORT_PROVIDER,
                json!({"app": "codex", "providerId": "team"})
            ))
            .unwrap(),
            vec![
                "gateway",
                "channel",
                "import-provider",
                "--app",
                "codex",
                "--provider",
                "team"
            ]
        );
        assert_eq!(
            argv_for_request(&request(methods::GATEWAY_CONNECTION_INFO, Value::Null)).unwrap(),
            vec!["gateway", "connection-info"]
        );
        assert!(
            argv_for_request(&request(
                methods::STATION_GET,
                json!({"stationId": "station-1", "extra": true})
            ))
            .is_err()
        );
        assert_eq!(
            argv_for_request(&request(
                methods::PROVIDER_ENDPOINT_ADD,
                json!({
                    "app": "claude",
                    "providerId": "team",
                    "url": "https://api.example.com"
                })
            ))
            .unwrap(),
            vec![
                "provider",
                "endpoint",
                "add",
                "team",
                "https://api.example.com",
                "--app",
                "claude"
            ]
        );
    }

    #[test]
    fn policy_capabilities_enable_updates_but_keep_other_restricted_features_out() {
        let capabilities = capabilities(&ochub_core::remote_policy::RemotePolicy::default());
        assert!(capabilities.contains(&Capability::AppWrite));
        assert!(capabilities.contains(&Capability::ProviderWrite));
        assert!(capabilities.contains(&Capability::ProviderNetwork));
        assert!(capabilities.contains(&Capability::McpRead));
        assert!(capabilities.contains(&Capability::McpWrite));
        assert!(capabilities.contains(&Capability::SkillRead));
        assert!(capabilities.contains(&Capability::SkillWrite));
        assert!(capabilities.contains(&Capability::SkillNetwork));
        assert!(capabilities.contains(&Capability::UsageRead));
        assert!(capabilities.contains(&Capability::UsageWrite));
        assert!(capabilities.contains(&Capability::UsageNetwork));
        assert!(capabilities.contains(&Capability::SessionRead));
        assert!(capabilities.contains(&Capability::SessionWrite));
        assert!(capabilities.contains(&Capability::ProxyRead));
        assert!(capabilities.contains(&Capability::ProxyWrite));
        assert!(capabilities.contains(&Capability::ProxyNetwork));
        assert!(capabilities.contains(&Capability::SyncRead));
        assert!(capabilities.contains(&Capability::SyncWrite));
        assert!(capabilities.contains(&Capability::SyncNetwork));
        assert!(capabilities.contains(&Capability::BackupRead));
        assert!(capabilities.contains(&Capability::BackupWrite));
        assert!(capabilities.contains(&Capability::ToolRead));
        assert!(capabilities.contains(&Capability::ToolWrite));
        assert!(capabilities.contains(&Capability::UpdateRead));
        assert!(capabilities.contains(&Capability::DataRead));
        assert!(capabilities.contains(&Capability::DataWrite));
        assert!(capabilities.contains(&Capability::GatewayLifecycle));
        assert!(capabilities.contains(&Capability::StationRead));
        assert!(capabilities.contains(&Capability::StationWrite));
        assert!(capabilities.contains(&Capability::StationNetwork));
        assert!(
            !capabilities
                .iter()
                .any(|value| value.as_str() == "backup.restore")
        );
        assert!(capabilities.contains(&Capability::UpdateInstall));
        assert!(capabilities.contains(&Capability::NodeUpdateInstall));
        assert!(capabilities.contains(&Capability::NodeUpdateRelay));
        assert!(!capabilities.contains(&Capability::DataImport));
    }

    #[test]
    fn secret_detection_matches_nested_provider_fields() {
        assert!(contains_secret_write(&json!({
            "settingsConfig": {
                "env": { "ANTHROPIC_API_KEY": "sk-secret" }
            }
        })));
        assert!(contains_secret_write(&json!({
            "settingsConfig": {
                "headers": [["Authorization", "Bearer secret"]]
            }
        })));
        assert!(!contains_secret_write(&json!({
            "settingsConfig": {
                "apiKey": "******",
                "model": "claude"
            }
        })));
        assert!(common_config_may_contain_secret(
            "ANTHROPIC_API_KEY = \"secret\""
        ));
        assert!(!common_config_may_contain_secret(
            "model_reasoning_effort = \"high\""
        ));
    }

    #[test]
    fn revision_is_deterministic_and_sensitive_to_plan_changes() {
        assert_eq!(
            revision_for(&json!({"a": 1})),
            revision_for(&json!({"a": 1}))
        );
        assert_ne!(
            revision_for(&json!({"a": 1})),
            revision_for(&json!({"a": 2}))
        );
    }

    #[test]
    fn remote_values_mask_named_secrets_and_all_drift_payloads() {
        let value = json!({
            "public": "visible",
            "apiKey": "named-secret",
            "drift": {
                "conflicts": [{
                    "path": "env.CUSTOM_VALUE",
                    "live": "scalar-live-secret",
                    "incoming": {
                        "nested": "scalar-incoming-secret"
                    }
                }]
            }
        });

        let safe = remote_safe_value(&value);
        assert_eq!(safe["public"], "visible");
        assert_eq!(safe["apiKey"], "******");
        assert_eq!(safe["drift"]["conflicts"][0]["path"], "env.CUSTOM_VALUE");
        assert_eq!(safe["drift"]["conflicts"][0]["live"], "******");
        assert_eq!(safe["drift"]["conflicts"][0]["incoming"], "******");
        let encoded = serde_json::to_string(&safe).unwrap();
        assert!(!encoded.contains("named-secret"));
        assert!(!encoded.contains("scalar-live-secret"));
        assert!(!encoded.contains("scalar-incoming-secret"));
    }
}
