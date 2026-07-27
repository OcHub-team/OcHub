use std::path::{Path, PathBuf};
use std::time::Duration;

use ochub_core::application::ApplicationError;
use ochub_core::runtime::{self, IpcRequest, IpcResponse, OwnerRecord, PROTOCOL_VERSION};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::command::Cli;
use crate::error::CliError;
use crate::output::Output;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub async fn try_execute(cli: &Cli, output: &Output) -> Result<bool, CliError> {
    let owner = runtime::active_owner()?;
    if cli.direct {
        if let Some(owner) = owner {
            return Err(ApplicationError::OwnerConflict(format!(
                "{} owner pid {} is active at {}",
                owner_kind(&owner),
                owner.pid,
                owner.endpoint
            ))
            .into());
        }
        return Ok(false);
    }

    let Some(owner) = owner else {
        if cli.socket.is_some() {
            return Err(ApplicationError::RuntimeUnavailable(
                "--socket was provided but no runtime owner holds the owner lock".to_string(),
            )
            .into());
        }
        return Ok(false);
    };
    validate_owner(&owner)?;
    let current_data_dir = resolved_data_dir();
    if normalize_path(Path::new(&owner.data_dir)) != normalize_path(&current_data_dir) {
        return Err(ApplicationError::OwnerConflict(format!(
            "owner data directory is {}, requested {}",
            owner.data_dir,
            current_data_dir.display()
        ))
        .into());
    }

    let argv = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string().map_err(|_| {
                CliError::InvalidInput(
                    "runtime RPC does not support non-Unicode command arguments".to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let response = request(
        cli.socket.as_deref(),
        cli.timeout,
        "execute",
        json!({ "argv": argv }),
    )
    .await?;
    apply_response(response, output)?;
    Ok(true)
}

pub async fn ping(socket: Option<&Path>, timeout_secs: u64) -> Result<IpcResponse, CliError> {
    request(socket, timeout_secs, "ping", Value::Null).await
}

pub async fn shutdown(socket: Option<&Path>, timeout_secs: u64) -> Result<IpcResponse, CliError> {
    request(socket, timeout_secs, "shutdown", Value::Null).await
}

pub fn owner_status() -> Result<Option<OwnerRecord>, CliError> {
    Ok(runtime::active_owner()?)
}

async fn request(
    socket_override: Option<&Path>,
    timeout_secs: u64,
    operation: &str,
    params: Value,
) -> Result<IpcResponse, CliError> {
    if timeout_secs == 0 {
        return Err(CliError::InvalidInput(
            "--timeout must be at least 1 second".to_string(),
        ));
    }
    let socket = resolve_socket(socket_override)?;
    let request = IpcRequest {
        frame_type: "request".to_string(),
        protocol_version: PROTOCOL_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        operation: operation.to_string(),
        params,
    };
    let timeout = Duration::from_secs(timeout_secs);
    tokio::time::timeout(timeout, request_on_socket(&socket, &request))
        .await
        .map_err(|_| {
            CliError::Application(ApplicationError::RuntimeUnavailable(format!(
                "runtime request timed out after {timeout_secs}s"
            )))
        })?
}

fn resolve_socket(socket_override: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = socket_override {
        return Ok(path.to_path_buf());
    }
    if let Some(owner) = runtime::active_owner()? {
        if let Some(path) = owner.endpoint.strip_prefix("unix:") {
            return Ok(PathBuf::from(path));
        }
        if owner.endpoint.starts_with(r"\\.\pipe\") {
            return Ok(PathBuf::from(owner.endpoint));
        }
    }
    Ok(runtime::socket_path())
}

#[cfg(unix)]
async fn request_on_socket(socket: &Path, request: &IpcRequest) -> Result<IpcResponse, CliError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| {
            ApplicationError::RuntimeUnavailable(format!(
                "cannot connect to {}: {error}",
                socket.display()
            ))
        })?;
    let (read, mut write) = stream.into_split();
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.shutdown().await?;

    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    if line.len() > MAX_FRAME_SIZE {
        return Err(CliError::InvalidInput(
            "runtime response exceeds the maximum frame size".to_string(),
        ));
    }
    if line.trim().is_empty() {
        return Err(ApplicationError::RuntimeUnavailable(
            "runtime closed the connection without a response".to_string(),
        )
        .into());
    }
    let response: IpcResponse = serde_json::from_str(&line)?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(ApplicationError::ProtocolIncompatible(format!(
            "client protocol {}, owner protocol {}",
            PROTOCOL_VERSION, response.protocol_version
        ))
        .into());
    }
    Ok(response)
}

#[cfg(windows)]
async fn request_on_socket(_socket: &Path, _request: &IpcRequest) -> Result<IpcResponse, CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "Windows named-pipe runtime transport is not available in this build".to_string(),
    )
    .into())
}

pub fn apply_response(response: IpcResponse, output: &Output) -> Result<(), CliError> {
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| ochub_core::runtime::IpcError {
                code: "INTERNAL".to_string(),
                message: "runtime returned an error without details".to_string(),
                retryable: false,
                details: Value::Null,
                exit_code: 1,
            });
        return Err(CliError::Remote {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
            details: error.details,
            exit_code: error.exit_code,
        });
    }
    output.mark_owner();
    output.success(&response.data, &response.warnings)
}

fn validate_owner(owner: &OwnerRecord) -> Result<(), CliError> {
    if owner.protocol_version != PROTOCOL_VERSION {
        return Err(ApplicationError::ProtocolIncompatible(format!(
            "client protocol {}, owner protocol {}",
            PROTOCOL_VERSION, owner.protocol_version
        ))
        .into());
    }
    Ok(())
}

fn resolved_data_dir() -> PathBuf {
    ochub_core::app_store::refresh_app_config_dir_override();
    ochub_core::paths::get_app_config_dir()
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn owner_kind(owner: &OwnerRecord) -> &'static str {
    match owner.kind {
        runtime::OwnerKind::Gui => "GUI",
        runtime::OwnerKind::Daemon => "daemon",
        runtime::OwnerKind::Foreground => "foreground",
    }
}
