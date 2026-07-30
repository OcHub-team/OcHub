//! SSH stdio bridge for OcHub Remote Nodes.
//!
//! This module intentionally exposes typed, allowlisted methods. It never
//! accepts a shell command or arbitrary argv from the remote client.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser as _;
use fs2::FileExt as _;
use ochub_core::application::{Application, OpenOptions as ApplicationOpenOptions, redact_json};
use ochub_core::runtime::{IpcError, OwnerGuard, OwnerKind};
use ochub_protocol::{
    ApplyPlanParams, Capability, Frame, HelloAckFrame, HelloFrame, MAX_FRAME_SIZE, NodeDescriptor,
    PROTOCOL_MAX, PROTOCOL_MIN, PingFrame, PongFrame, ProtocolErrorFrame, RemoteError,
    RequestFrame, ResponseFrame, RuntimeDescriptor, SCHEMA_VERSION, decode_frame, encode_frame,
    methods, negotiate_protocol, validate_request_id,
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
    device_id: Option<String>,
    node_id: String,
}

impl RemoteSession {
    fn new(
        protocol_version: u32,
        policy: ochub_core::remote_policy::RemotePolicy,
        execution: RemoteExecution,
        device_id: Option<String>,
        node_id: String,
    ) -> Self {
        Self {
            protocol_version,
            policy,
            execution,
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
        let argv = match argv_for_request(&request) {
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
        self.execute_response(request, argv, None).await
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

fn argv_for_request(request: &RequestFrame) -> Result<Vec<String>, String> {
    match request.method.as_str() {
        methods::STATUS_READ => expect_empty_params(&request.params).map(|_| vec!["status".into()]),
        methods::DOCTOR_RUN => {
            #[derive(Deserialize)]
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
        methods::GATEWAY_STATUS => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "status".into()])
        }
        methods::GATEWAY_START => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "start".into()])
        }
        methods::GATEWAY_STOP => {
            expect_empty_params(&request.params).map(|_| vec!["gateway".into(), "stop".into()])
        }
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
        methods::APP_LIST => Some(Capability::AppRead),
        methods::PROVIDER_LIST | methods::PROVIDER_GET => Some(Capability::ProviderRead),
        methods::PROVIDER_SWITCH_PLAN | methods::PROVIDER_SWITCH_APPLY => {
            Some(Capability::ProviderWrite)
        }
        methods::GATEWAY_STATUS => Some(Capability::GatewayRead),
        methods::GATEWAY_START | methods::GATEWAY_STOP => Some(Capability::GatewayLifecycle),
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
        Capability::GatewayRead,
        Capability::OperationRead,
    ]);
    if policy.allow_write {
        values.insert(Capability::ProviderWrite);
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

    fn request(method: &str, params: Value) -> RequestFrame {
        RequestFrame {
            protocol_version: 1,
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
    }

    #[test]
    fn policy_capabilities_keep_high_risk_features_out_by_default() {
        let capabilities = capabilities(&ochub_core::remote_policy::RemotePolicy::default());
        assert!(capabilities.contains(&Capability::ProviderWrite));
        assert!(capabilities.contains(&Capability::GatewayLifecycle));
        assert!(
            !capabilities
                .iter()
                .any(|value| value.as_str() == "backup.restore")
        );
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
