use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ochub_protocol::{
    Capability, EventFrame, Frame, GoodbyeFrame, HelloAckFrame, HelloFrame, PROTOCOL_MAX,
    PROTOCOL_MIN, PingFrame, PongFrame, ProtocolErrorFrame, RemoteError, RequestFrame,
    ResponseFrame, SCHEMA_VERSION, decode_frame, encode_frame,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use super::RemoteHost;
use super::ssh::SshCommandSpec;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DIAGNOSTIC_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteConnectionIssueKind {
    CliNotInstalled,
    NodeUpgradeRequired,
    DesktopUpgradeRequired,
    RemoteDisabled,
    AuthenticationFailed,
    HostKeyChanged,
    HostKeyUnknown,
    ConnectionRefused,
    ConnectionTimedOut,
    NetworkUnreachable,
    CliNotExecutable,
    ArchitectureMismatch,
    SystemIncompatible,
    ProtocolCorrupted,
    Unknown,
}

impl RemoteConnectionIssueKind {
    pub(crate) fn can_bootstrap(self) -> bool {
        matches!(
            self,
            Self::CliNotInstalled
                | Self::NodeUpgradeRequired
                | Self::CliNotExecutable
                | Self::ArchitectureMismatch
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteConnectionIssue {
    pub kind: RemoteConnectionIssueKind,
    pub detail: String,
    pub diagnostics: Vec<String>,
    pub exit_code: Option<i32>,
}

impl std::fmt::Display for RemoteConnectionIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteClientError {
    #[error("invalid remote host: {0}")]
    InvalidHost(String),
    #[error("remote protocol error: {0}")]
    Protocol(String),
    #[error("remote node does not advertise required capability {0}")]
    Capability(String),
    #[error("remote request timed out during {0}")]
    Timeout(String),
    #[error("SSH process failed: {0}")]
    Process(String),
    #[error("{0}")]
    Connection(RemoteConnectionIssue),
    #[error("remote node rejected the request [{code}]: {message}")]
    Remote {
        code: String,
        message: String,
        retryable: bool,
        details: Value,
    },
    #[error("remote host file {path} failed: {source}")]
    File {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("remote host store failed: {0}")]
    Store(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl RemoteClientError {
    pub(crate) fn ssh_failure(
        message: impl Into<String>,
        diagnostics: Vec<String>,
        exit_code: Option<i32>,
    ) -> Self {
        let message = message.into();
        Self::Connection(classify_connection_issue(&message, &diagnostics, exit_code))
    }

    pub(crate) fn connection_issue(&self) -> RemoteConnectionIssue {
        match self {
            Self::Connection(issue) => issue.clone(),
            Self::Timeout(message) => RemoteConnectionIssue {
                kind: RemoteConnectionIssueKind::ConnectionTimedOut,
                detail: message.clone(),
                diagnostics: Vec::new(),
                exit_code: None,
            },
            error => classify_connection_issue(&error.to_string(), &[], None),
        }
    }
}

impl From<ochub_protocol::ProtocolError> for RemoteClientError {
    fn from(error: ochub_protocol::ProtocolError) -> Self {
        Self::Protocol(error.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteRequestOptions {
    pub trace_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub expected_revision: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteResponse {
    pub data: Value,
    pub warnings: Vec<String>,
    pub revision: Option<String>,
    pub events: Vec<EventFrame>,
}

struct ConnectionIo {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub(crate) struct RemoteClient {
    host: RemoteHost,
    handshake: HelloAckFrame,
    io: Mutex<ConnectionIo>,
    unusable: AtomicBool,
}

impl RemoteClient {
    pub(crate) async fn connect(host: RemoteHost) -> Result<Arc<Self>, RemoteClientError> {
        host.validate()?;
        let spec = SshCommandSpec::for_remote(&host)?;
        let mut command = spec.command();
        command.stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| RemoteClientError::Process("SSH stdin was not created".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RemoteClientError::Process("SSH stdout was not created".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RemoteClientError::Process("SSH stderr was not created".to_string()))?;

        // Drain stderr before the protocol handshake. Most actionable startup
        // failures (missing ochcli, SSH authentication, host-key changes, old
        // subcommands) happen before the first stdout frame.
        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        spawn_stderr_drain(stderr, diagnostics.clone());

        let device_id = ochub_core::node_identity::load_or_create()
            .ok()
            .map(|identity| identity.node_id);
        if let Err(error) = write_frame(
            &mut stdin,
            &Frame::Hello(HelloFrame {
                protocol_min: PROTOCOL_MIN,
                protocol_max: PROTOCOL_MAX,
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                locale: None,
                device_id,
            }),
        )
        .await
        {
            return Err(finalize_connect_error(error, &mut child, &diagnostics).await);
        }
        let mut stdout = BufReader::new(stdout);
        let handshake =
            match tokio::time::timeout(CONNECT_TIMEOUT, read_handshake(&mut stdout)).await {
                Ok(Ok(handshake)) => handshake,
                Ok(Err(error)) => {
                    return Err(finalize_connect_error(error, &mut child, &diagnostics).await);
                }
                Err(_) => {
                    let error = RemoteClientError::Timeout("SSH protocol handshake".to_string());
                    return Err(finalize_connect_error(error, &mut child, &diagnostics).await);
                }
            };
        if handshake.schema_version != SCHEMA_VERSION {
            let kind = if handshake.schema_version < SCHEMA_VERSION {
                RemoteConnectionIssueKind::NodeUpgradeRequired
            } else {
                RemoteConnectionIssueKind::DesktopUpgradeRequired
            };
            return Err(RemoteClientError::Connection(RemoteConnectionIssue {
                kind,
                detail: format!(
                    "remote schema {}, desktop supports {}",
                    handshake.schema_version, SCHEMA_VERSION
                ),
                diagnostics: diagnostics.lock().await.iter().cloned().collect(),
                exit_code: None,
            }));
        }
        if let Some(expected) = &host.remote_node_id
            && expected != &handshake.node.id
        {
            return Err(RemoteClientError::InvalidHost(format!(
                "SSH endpoint returned node {}, expected {}",
                handshake.node.id, expected
            )));
        }

        Ok(Arc::new(Self {
            host,
            handshake,
            io: Mutex::new(ConnectionIo {
                child,
                stdin,
                stdout,
            }),
            unusable: AtomicBool::new(false),
        }))
    }

    pub(crate) fn host(&self) -> &RemoteHost {
        &self.host
    }

    pub(crate) fn handshake(&self) -> &HelloAckFrame {
        &self.handshake
    }

    pub(crate) fn require_capability(
        &self,
        capability: Capability,
    ) -> Result<(), RemoteClientError> {
        if self.handshake.capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(RemoteClientError::Capability(
                capability.as_str().to_string(),
            ))
        }
    }

    pub(crate) async fn request(
        &self,
        method: impl Into<String>,
        params: Value,
        options: RemoteRequestOptions,
    ) -> Result<RemoteResponse, RemoteClientError> {
        if self.unusable.load(Ordering::Acquire) {
            return Err(RemoteClientError::Process(
                "SSH protocol session is no longer usable; reconnect the node".to_string(),
            ));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = RequestFrame {
            protocol_version: self.handshake.protocol_version,
            request_id: request_id.clone(),
            method: method.into(),
            params,
            trace_id: options.trace_id,
            idempotency_key: options.idempotency_key,
            expected_revision: options.expected_revision,
        };
        let timeout = options.timeout.unwrap_or(REQUEST_TIMEOUT);
        match tokio::time::timeout(timeout, self.request_inner(request_id, request)).await {
            Ok(result) => result,
            Err(_) => {
                // Cancelling a read leaves the eventual response queued on
                // stdout. Reusing that stream would associate it with the next
                // request, so fail closed and force a fresh SSH handshake.
                self.unusable.store(true, Ordering::Release);
                self.abort_session().await;
                Err(RemoteClientError::Timeout("remote request".to_string()))
            }
        }
    }

    async fn request_inner(
        &self,
        request_id: String,
        request: RequestFrame,
    ) -> Result<RemoteResponse, RemoteClientError> {
        let mut io = self.io.lock().await;
        if let Some(status) = io.child.try_wait()? {
            return Err(RemoteClientError::Process(format!(
                "SSH process already exited with {status}"
            )));
        }
        write_frame(&mut io.stdin, &Frame::Request(request)).await?;
        let mut events = Vec::new();
        loop {
            let frame = read_frame(&mut io.stdout).await?;
            match frame {
                Frame::Response(response) if response.request_id == request_id => {
                    return response_result(response, events);
                }
                Frame::Event(event) if event.request_id == request_id => events.push(event),
                Frame::Ping(PingFrame { timestamp }) => {
                    write_frame(&mut io.stdin, &Frame::Pong(PongFrame { timestamp })).await?;
                }
                Frame::ProtocolError(error) => return Err(protocol_error(error)),
                Frame::Goodbye(goodbye) => {
                    return Err(RemoteClientError::Process(format!(
                        "remote node closed the connection: {}",
                        goodbye.reason
                    )));
                }
                _ => {
                    return Err(RemoteClientError::Protocol(
                        "received an unrelated or unexpected remote frame".to_string(),
                    ));
                }
            }
        }
    }

    pub(crate) async fn close(&self) -> Result<(), RemoteClientError> {
        self.unusable.store(true, Ordering::Release);
        let mut io = self.io.lock().await;
        let _ = write_frame(
            &mut io.stdin,
            &Frame::Goodbye(GoodbyeFrame {
                reason: "desktop-disconnect".to_string(),
            }),
        )
        .await;
        if io.child.try_wait()?.is_none() {
            io.child.kill().await?;
        }
        let _ = io.child.wait().await;
        Ok(())
    }

    async fn abort_session(&self) {
        let mut io = self.io.lock().await;
        if io.child.try_wait().ok().flatten().is_none() {
            let _ = io.child.kill().await;
        }
        let _ = io.child.wait().await;
    }
}

async fn write_frame(stdin: &mut ChildStdin, frame: &Frame) -> Result<(), RemoteClientError> {
    let bytes = encode_frame(frame)?;
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_handshake(
    stdout: &mut BufReader<ChildStdout>,
) -> Result<HelloAckFrame, RemoteClientError> {
    match read_frame(stdout).await? {
        Frame::HelloAck(ack)
            if ack.protocol_version >= PROTOCOL_MIN && ack.protocol_version <= PROTOCOL_MAX =>
        {
            Ok(ack)
        }
        Frame::HelloAck(ack) => Err(RemoteClientError::Protocol(format!(
            "server selected unsupported protocol {}",
            ack.protocol_version
        ))),
        Frame::ProtocolError(error) => Err(protocol_error(error)),
        _ => Err(RemoteClientError::Protocol(
            "remote node did not return helloAck".to_string(),
        )),
    }
}

async fn read_frame(stdout: &mut BufReader<ChildStdout>) -> Result<Frame, RemoteClientError> {
    let mut line = Vec::new();
    let count = stdout.read_until(b'\n', &mut line).await?;
    if count == 0 {
        return Err(RemoteClientError::Process(
            "SSH connection closed without a protocol frame".to_string(),
        ));
    }
    Ok(decode_frame(&line)?)
}

fn response_result(
    response: ResponseFrame,
    events: Vec<EventFrame>,
) -> Result<RemoteResponse, RemoteClientError> {
    if let Some(error) = response.error {
        return Err(remote_error(error));
    }
    if !response.ok {
        return Err(RemoteClientError::Protocol(
            "remote response was unsuccessful without an error body".to_string(),
        ));
    }
    Ok(RemoteResponse {
        data: response.data,
        warnings: response.warnings,
        revision: response.revision,
        events,
    })
}

fn remote_error(error: RemoteError) -> RemoteClientError {
    RemoteClientError::Remote {
        code: error.code,
        message: error.message,
        retryable: error.retryable,
        details: error.details,
    }
}

fn protocol_error(error: ProtocolErrorFrame) -> RemoteClientError {
    if error.code == "PROTOCOL_INCOMPATIBLE" {
        let server_min = error.details.get("serverMin").and_then(Value::as_u64);
        let server_max = error.details.get("serverMax").and_then(Value::as_u64);
        let kind = if server_max.is_some_and(|server_max| server_max < u64::from(PROTOCOL_MIN)) {
            RemoteConnectionIssueKind::NodeUpgradeRequired
        } else if server_min.is_some_and(|server_min| server_min > u64::from(PROTOCOL_MAX)) {
            RemoteConnectionIssueKind::DesktopUpgradeRequired
        } else {
            RemoteConnectionIssueKind::ProtocolCorrupted
        };
        return RemoteClientError::Connection(RemoteConnectionIssue {
            kind,
            detail: format!("{}: {}", error.code, error.message),
            diagnostics: Vec::new(),
            exit_code: None,
        });
    }
    RemoteClientError::Protocol(format!("{}: {}", error.code, error.message))
}

async fn finalize_connect_error(
    error: RemoteClientError,
    child: &mut Child,
    diagnostics: &Arc<Mutex<VecDeque<String>>>,
) -> RemoteClientError {
    let mut exit_code = child
        .try_wait()
        .ok()
        .flatten()
        .and_then(|status| status.code());
    if exit_code.is_none() && !matches!(error, RemoteClientError::Timeout(_)) {
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_millis(500), child.wait()).await
        {
            exit_code = status.code();
        }
    } else if matches!(error, RemoteClientError::Timeout(_)) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    tokio::task::yield_now().await;
    let lines = diagnostics.lock().await.iter().cloned().collect::<Vec<_>>();
    if let RemoteClientError::Connection(mut issue) = error {
        if issue.diagnostics.is_empty() {
            issue.diagnostics = lines;
        }
        issue.exit_code = issue.exit_code.or(exit_code);
        return RemoteClientError::Connection(issue);
    }
    RemoteClientError::Connection(classify_connection_issue(
        &error.to_string(),
        &lines,
        exit_code,
    ))
}

fn classify_connection_issue(
    message: &str,
    diagnostics: &[String],
    exit_code: Option<i32>,
) -> RemoteConnectionIssue {
    let detail = diagnostics
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| message.to_string());
    let haystack = format!("{message}\n{}", diagnostics.join("\n")).to_ascii_lowercase();
    let contains = |needle: &str| haystack.contains(needle);
    let kind = if (contains("ochcli") && contains("command not found"))
        || (contains("ochcli") && contains("no such file or directory"))
        || contains("'ochcli' is not recognized")
    {
        RemoteConnectionIssueKind::CliNotInstalled
    } else if contains("unrecognized subcommand 'remote'")
        || contains("unrecognized subcommand `remote`")
        || contains("unexpected argument 'remote'")
        || contains("found argument 'remote' which wasn't expected")
    {
        RemoteConnectionIssueKind::NodeUpgradeRequired
    } else if contains("remote access is disabled") {
        RemoteConnectionIssueKind::RemoteDisabled
    } else if contains("remote host identification has changed")
        || contains("offending ") && contains("host key")
    {
        RemoteConnectionIssueKind::HostKeyChanged
    } else if contains("host key verification failed") {
        RemoteConnectionIssueKind::HostKeyUnknown
    } else if contains("permission denied (publickey")
        || contains("too many authentication failures")
        || contains("no supported authentication methods available")
    {
        RemoteConnectionIssueKind::AuthenticationFailed
    } else if contains("connection refused") {
        RemoteConnectionIssueKind::ConnectionRefused
    } else if contains("operation timed out")
        || contains("connection timed out")
        || contains("protocol handshake") && contains("timed out")
    {
        RemoteConnectionIssueKind::ConnectionTimedOut
    } else if contains("no route to host")
        || contains("network is unreachable")
        || contains("could not resolve hostname")
        || contains("name or service not known")
    {
        RemoteConnectionIssueKind::NetworkUnreachable
    } else if contains("exec format error") || contains("bad cpu type in executable") {
        RemoteConnectionIssueKind::ArchitectureMismatch
    } else if contains("glibc_")
        || contains("version `glibc_")
        || contains("cannot open shared object file")
    {
        RemoteConnectionIssueKind::SystemIncompatible
    } else if contains("ochcli") && contains("permission denied") {
        RemoteConnectionIssueKind::CliNotExecutable
    } else if contains("invalid frame")
        || contains("without a protocol frame")
        || contains("did not return helloack")
        || contains("json error")
    {
        RemoteConnectionIssueKind::ProtocolCorrupted
    } else {
        RemoteConnectionIssueKind::Unknown
    };
    RemoteConnectionIssue {
        kind,
        detail,
        diagnostics: diagnostics.to_vec(),
        exit_code,
    }
}

fn spawn_stderr_drain(
    stderr: tokio::process::ChildStderr,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let Ok(count) = reader.read_line(&mut line).await else {
                break;
            };
            if count == 0 {
                break;
            }
            let mut diagnostics = diagnostics.lock().await;
            if diagnostics.len() >= MAX_DIAGNOSTIC_LINES {
                diagnostics.pop_front();
            }
            diagnostics.push_back(line.trim_end().chars().take(2_048).collect());
        }
    });
}

#[cfg(test)]
mod issue_tests {
    use super::*;

    #[test]
    fn missing_cli_is_not_reported_as_a_generic_protocol_failure() {
        let issue = classify_connection_issue(
            "SSH connection closed without a protocol frame",
            &["bash: line 1: ochcli: command not found".to_string()],
            Some(127),
        );
        assert_eq!(issue.kind, RemoteConnectionIssueKind::CliNotInstalled);
        assert_eq!(issue.exit_code, Some(127));
        assert!(issue.detail.contains("command not found"));
    }

    #[test]
    fn common_ssh_failures_have_stable_categories() {
        let cases = [
            (
                "root@example: Permission denied (publickey).",
                RemoteConnectionIssueKind::AuthenticationFailed,
            ),
            (
                "ssh: connect to host example port 22: Connection refused",
                RemoteConnectionIssueKind::ConnectionRefused,
            ),
            (
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
                RemoteConnectionIssueKind::HostKeyChanged,
            ),
            (
                "ochcli: /lib64/libc.so.6: version `GLIBC_2.39' not found",
                RemoteConnectionIssueKind::SystemIncompatible,
            ),
            (
                "error: unrecognized subcommand 'remote'",
                RemoteConnectionIssueKind::NodeUpgradeRequired,
            ),
            (
                "remote access is disabled by the device policy",
                RemoteConnectionIssueKind::RemoteDisabled,
            ),
            (
                "bash: /tmp/ochcli: cannot execute binary file: Exec format error",
                RemoteConnectionIssueKind::ArchitectureMismatch,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(classify_connection_issue(message, &[], None).kind, expected);
        }
    }

    #[test]
    fn bootstrap_is_only_offered_for_recoverable_cli_failures() {
        assert!(RemoteConnectionIssueKind::CliNotInstalled.can_bootstrap());
        assert!(RemoteConnectionIssueKind::NodeUpgradeRequired.can_bootstrap());
        assert!(RemoteConnectionIssueKind::CliNotExecutable.can_bootstrap());
        assert!(RemoteConnectionIssueKind::ArchitectureMismatch.can_bootstrap());
        assert!(!RemoteConnectionIssueKind::AuthenticationFailed.can_bootstrap());
        assert!(!RemoteConnectionIssueKind::HostKeyChanged.can_bootstrap());
        assert!(!RemoteConnectionIssueKind::SystemIncompatible.can_bootstrap());
    }
}
