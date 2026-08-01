use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use ochub_core::services::update::headless::HeadlessPlatformEntry;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use super::store::{known_hosts_path, validate_scan_hostname};
use super::{RemoteClientError, RemoteHost};

const BOOTSTRAP_MARKER: &str = "OCHUB_BOOTSTRAP/1";
const BOOTSTRAP_INSTALLED_MARKER: &str = "OCHUB_BOOTSTRAP_INSTALLED/1";
const MAX_BOOTSTRAP_OUTPUT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapProbe {
    pub os: String,
    pub arch: String,
    pub home: PathBuf,
    pub existing_cli: Option<PathBuf>,
    pub existing_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootstrapInstallResult {
    pub executable: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshCommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl SshCommandSpec {
    pub(crate) fn for_remote(host: &RemoteHost) -> Result<Self, RemoteClientError> {
        host.validate()?;
        let mut args = base_args()?;
        args.push(host.ssh_alias.clone().into());
        // OpenSSH serializes the command tail for the remote login shell.
        // Every token below is fixed or validated to a shell-metacharacter-free
        // alphabet, so no user-controlled shell syntax can enter the command.
        args.extend([
            host.ochcli_path.clone().into(),
            "remote".into(),
            "serve".into(),
            "--stdio".into(),
        ]);
        Ok(Self {
            program: PathBuf::from("ssh"),
            args,
        })
    }

    pub(crate) fn for_node_update_receive(
        host: &RemoteHost,
        node_id: &str,
        version: &str,
        target: &str,
        entry: &HeadlessPlatformEntry,
    ) -> Result<Self, RemoteClientError> {
        host.validate()?;
        validate_update_token(node_id, "node id")?;
        validate_update_token(version, "version")?;
        validate_update_token(target, "target")?;
        validate_update_token(&entry.sha256, "sha256")?;
        validate_signature(&entry.signature)?;
        let mut args = base_args()?;
        args.push(host.ssh_alias.clone().into());
        args.extend([
            host.ochcli_path.clone().into(),
            "--json".into(),
            "--yes".into(),
            "node".into(),
            "update".into(),
            "receive".into(),
            "--version".into(),
            version.into(),
            "--target".into(),
            target.into(),
            "--signature".into(),
            entry.signature.clone().into(),
            "--sha256".into(),
            entry.sha256.clone().into(),
            "--size".into(),
            entry.size.to_string().into(),
            "--expected-node-id".into(),
            node_id.into(),
        ]);
        Ok(Self {
            program: PathBuf::from("ssh"),
            args,
        })
    }

    pub(crate) fn for_bootstrap_probe(host: &RemoteHost) -> Result<Self, RemoteClientError> {
        host.validate()?;
        let mut args = base_args()?;
        args.push(host.ssh_alias.clone().into());
        // Bootstrap is the one path that cannot invoke ochcli. This script is
        // a fixed literal with no host-derived interpolation and only performs
        // read-only platform/path discovery.
        args.push(
            r#"set -eu
ochub_cli=""
if command -v ochcli >/dev/null 2>&1; then
  ochub_cli="$(command -v ochcli)"
else
  for candidate in "$HOME/.local/bin/ochcli" /usr/local/bin/ochcli /usr/bin/ochcli; do
    if [ -x "$candidate" ]; then ochub_cli="$candidate"; break; fi
  done
fi
printf 'OCHUB_BOOTSTRAP/1\t%s\t%s\t%s\t%s\n' "$(uname -s)" "$(uname -m)" "$HOME" "$ochub_cli"
if [ -n "$ochub_cli" ]; then
  "$ochub_cli" --json version 2>/dev/null || "$ochub_cli" version 2>/dev/null || true
fi"#
            .into(),
        );
        Ok(Self {
            program: PathBuf::from("ssh"),
            args,
        })
    }

    pub(crate) fn for_bootstrap_install(
        host: &RemoteHost,
        entry: &HeadlessPlatformEntry,
    ) -> Result<Self, RemoteClientError> {
        host.validate()?;
        validate_update_token(&entry.sha256, "sha256")?;
        if entry.size == 0 || entry.size > ochub_core::services::update::headless::MAX_PAYLOAD_BYTES
        {
            return Err(RemoteClientError::InvalidHost(
                "bootstrap payload size is outside the allowed range".to_string(),
            ));
        }
        let script = BOOTSTRAP_INSTALL_SCRIPT
            .replace("__OCHUB_SIZE__", &entry.size.to_string())
            .replace("__OCHUB_SHA256__", &entry.sha256);
        let mut args = base_args()?;
        args.push(host.ssh_alias.clone().into());
        args.push(script.into());
        Ok(Self {
            program: PathBuf::from("ssh"),
            args,
        })
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

const BOOTSTRAP_INSTALL_SCRIPT: &str = r#"set -eu
umask 077
ochub_root="$HOME/.ochub/bootstrap"
mkdir -p "$ochub_root"
chmod 700 "$ochub_root"
ochub_tmp="$ochub_root/ochcli.$$"
ochub_log="$ochub_root/install.$$.log"
cleanup() { rm -f "$ochub_tmp" "$ochub_log"; }
trap cleanup EXIT HUP INT TERM
fail() { printf 'OCHUB_BOOTSTRAP_ERROR/1\t%s\n' "$1" >&2; exit "$2"; }
cat > "$ochub_tmp"
ochub_size="$(wc -c < "$ochub_tmp" | tr -d '[:space:]')"
[ "$ochub_size" = "__OCHUB_SIZE__" ] || fail SIZE_MISMATCH 71
if command -v sha256sum >/dev/null 2>&1; then
  ochub_hash="$(sha256sum "$ochub_tmp" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ochub_hash="$(shasum -a 256 "$ochub_tmp" | awk '{print $1}')"
else
  fail HASH_TOOL_MISSING 72
fi
[ "$ochub_hash" = "__OCHUB_SHA256__" ] || fail HASH_MISMATCH 73
chmod 700 "$ochub_tmp"
if ! "$ochub_tmp" --json version > /dev/null 2> "$ochub_log"; then
  cat "$ochub_log" >&2
  fail EXECUTABLE_INVALID 74
fi
if ! "$ochub_tmp" --json node install > "$ochub_log" 2>&1; then
  cat "$ochub_log" >&2
  fail INSTALL_FAILED 75
fi
ochub_managed="$HOME/.local/bin/ochcli"
if ! "$ochub_managed" --json version > /dev/null 2> "$ochub_log"; then
  cat "$ochub_log" >&2
  fail VERIFY_FAILED 76
fi
printf 'OCHUB_BOOTSTRAP_INSTALLED/1\t%s\n' "$ochub_managed""#;

fn base_args() -> Result<Vec<OsString>, RemoteClientError> {
    let mut args = vec![
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ClearAllForwardings=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
    ];
    if let Some(option) = known_hosts_option()? {
        args.push("-o".into());
        args.push(option.into());
    }
    Ok(args)
}

fn validate_update_token(value: &str, field: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RemoteClientError::InvalidHost(format!(
            "{field} contains characters that are unsafe for the fixed SSH update command"
        )));
    }
    Ok(())
}

fn validate_signature(value: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 16 * 1024
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return Err(RemoteClientError::InvalidHost(
            "release signature is not a safe base64 token".to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn relay_node_update(
    host: &RemoteHost,
    node_id: &str,
    version: &str,
    target: &str,
    entry: &HeadlessPlatformEntry,
    payload: &[u8],
) -> Result<Value, RemoteClientError> {
    if payload.len() as u64 != entry.size {
        return Err(RemoteClientError::Protocol(format!(
            "relayed update payload has {} bytes, expected {}",
            payload.len(),
            entry.size
        )));
    }
    let spec = SshCommandSpec::for_node_update_receive(host, node_id, version, target, entry)?;
    let mut child = spec.command().spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        RemoteClientError::Process("SSH update stdin was not created".to_string())
    })?;
    stdin.write_all(payload).await?;
    stdin.shutdown().await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(15 * 60), child.wait_with_output())
        .await
        .map_err(|_| RemoteClientError::Timeout("relayed node update".to_string()))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemoteClientError::Process(format!(
            "remote node update exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    if !envelope.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(RemoteClientError::Protocol(
            "remote node update returned an unsuccessful response".to_string(),
        ));
    }
    envelope.get("data").cloned().ok_or_else(|| {
        RemoteClientError::Protocol("remote update response has no data".to_string())
    })
}

pub(crate) async fn probe_bootstrap(
    host: &RemoteHost,
) -> Result<BootstrapProbe, RemoteClientError> {
    let spec = SshCommandSpec::for_bootstrap_probe(host)?;
    let output =
        run_bootstrap_command(&spec, None, Duration::from_secs(20), "SSH bootstrap probe").await?;
    if !output.status.success() {
        return Err(RemoteClientError::ssh_failure(
            format!("SSH bootstrap probe exited with {}", output.status),
            diagnostic_lines(&output.stderr),
            output.status.code(),
        ));
    }
    parse_bootstrap_probe(&output.stdout)
}

pub(crate) async fn install_bootstrap(
    host: &RemoteHost,
    entry: &HeadlessPlatformEntry,
    payload: &[u8],
) -> Result<BootstrapInstallResult, RemoteClientError> {
    ochub_core::services::update::headless::verify_payload(payload, entry)
        .map_err(|error| RemoteClientError::Protocol(error.to_string()))?;
    let spec = SshCommandSpec::for_bootstrap_install(host, entry)?;
    let output = run_bootstrap_command(
        &spec,
        Some(payload),
        Duration::from_secs(15 * 60),
        "SSH bootstrap installation",
    )
    .await?;
    if !output.status.success() {
        return Err(RemoteClientError::ssh_failure(
            format!("SSH bootstrap installation exited with {}", output.status),
            diagnostic_lines(&output.stderr),
            output.status.code(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let executable = stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix(BOOTSTRAP_INSTALLED_MARKER)
                .and_then(|value| value.strip_prefix('\t'))
        })
        .ok_or_else(|| {
            RemoteClientError::Protocol(
                "bootstrap installer completed without a success marker".to_string(),
            )
        })?;
    validate_remote_absolute_path(executable, "managed ochcli path")?;
    Ok(BootstrapInstallResult {
        executable: PathBuf::from(executable),
    })
}

fn parse_bootstrap_probe(stdout: &[u8]) -> Result<BootstrapProbe, RemoteClientError> {
    let stdout = String::from_utf8_lossy(stdout);
    let marker = stdout
        .lines()
        .find(|line| line.starts_with(BOOTSTRAP_MARKER))
        .ok_or_else(|| {
            RemoteClientError::Protocol(
                "SSH bootstrap probe returned no platform marker".to_string(),
            )
        })?;
    let mut fields = marker.split('\t');
    if fields.next() != Some(BOOTSTRAP_MARKER) {
        return Err(RemoteClientError::Protocol(
            "SSH bootstrap marker is invalid".to_string(),
        ));
    }
    let os = validate_bootstrap_token(fields.next().unwrap_or_default(), "operating system")?;
    let arch = validate_bootstrap_token(fields.next().unwrap_or_default(), "architecture")?;
    let home = fields.next().unwrap_or_default();
    validate_remote_absolute_path(home, "remote home directory")?;
    let existing_cli = fields.next().unwrap_or_default();
    let existing_cli = if existing_cli.is_empty() {
        None
    } else {
        validate_remote_absolute_path(existing_cli, "existing ochcli path")?;
        Some(PathBuf::from(existing_cli))
    };
    if fields.next().is_some() {
        return Err(RemoteClientError::Protocol(
            "SSH bootstrap marker has unexpected fields".to_string(),
        ));
    }
    let existing_version = stdout
        .split_once(marker)
        .and_then(|(_, remainder)| serde_json::from_str::<Value>(remainder.trim()).ok())
        .and_then(|value| {
            value
                .pointer("/data/version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    Ok(BootstrapProbe {
        os,
        arch,
        home: PathBuf::from(home),
        existing_cli,
        existing_version,
    })
}

fn validate_bootstrap_token(value: &str, field: &str) -> Result<String, RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RemoteClientError::Protocol(format!(
            "bootstrap {field} is invalid"
        )));
    }
    Ok(value.to_string())
}

fn validate_remote_absolute_path(value: &str, field: &str) -> Result<(), RemoteClientError> {
    if !value.starts_with('/')
        || value.len() > 4_096
        || value.chars().any(char::is_control)
        || value.split('/').any(|segment| segment == "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return Err(RemoteClientError::Protocol(format!(
            "bootstrap {field} is not a safe absolute path"
        )));
    }
    Ok(())
}

fn diagnostic_lines(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .take(200)
        .map(|line| line.chars().take(2_048).collect())
        .collect()
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct BootstrapCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn read_bounded<R>(mut reader: R) -> std::io::Result<BoundedBuffer>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_BOOTSTRAP_OUTPUT.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&chunk[..retained]);
        exceeded |= retained != count;
    }
    Ok(BoundedBuffer { bytes, exceeded })
}

async fn run_bootstrap_command(
    spec: &SshCommandSpec,
    payload: Option<&[u8]>,
    timeout: Duration,
    context: &str,
) -> Result<BootstrapCommandOutput, RemoteClientError> {
    let mut child = spec.command().spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        RemoteClientError::Process("SSH bootstrap stdin was not created".to_string())
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        RemoteClientError::Process("SSH bootstrap stdout was not created".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        RemoteClientError::Process("SSH bootstrap stderr was not created".to_string())
    })?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let completion = async {
        if let Some(payload) = payload {
            stdin.write_all(payload).await?;
        }
        stdin.shutdown().await?;
        drop(stdin);
        child.wait().await
    };
    let status = match tokio::time::timeout(timeout, completion).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(RemoteClientError::Timeout(context.to_string()));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| RemoteClientError::Process(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| RemoteClientError::Process(error.to_string()))??;
    if stdout.exceeded
        || stderr.exceeded
        || stdout.bytes.len().saturating_add(stderr.bytes.len()) > MAX_BOOTSTRAP_OUTPUT
    {
        return Err(RemoteClientError::Protocol(
            "SSH bootstrap output exceeded the safety limit".to_string(),
        ));
    }
    Ok(BootstrapCommandOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

fn known_hosts_option() -> Result<Option<String>, RemoteClientError> {
    let custom = known_hosts_path();
    if !custom.exists() {
        return Ok(None);
    }
    let user = ochub_core::paths::get_home_dir()
        .join(".ssh")
        .join("known_hosts");
    let values = if user.exists() {
        vec![user, custom]
    } else {
        vec![custom]
    };
    values
        .into_iter()
        .map(|path| quote_ssh_config_path(&path))
        .collect::<Result<Vec<_>, _>>()
        .map(|values| Some(format!("UserKnownHostsFile={}", values.join(" "))))
}

fn quote_ssh_config_path(path: &Path) -> Result<String, RemoteClientError> {
    let value = path.to_string_lossy();
    if value.contains(['\n', '\r', '\0']) {
        return Err(RemoteClientError::InvalidHost(
            "known_hosts path contains unsupported characters".to_string(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedHostKey {
    pub hostname: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
    pub known_hosts_line: String,
}

pub(crate) async fn scan_host_keys(
    hostname: &str,
    port: u16,
) -> Result<Vec<ScannedHostKey>, RemoteClientError> {
    validate_scan_hostname(hostname)?;
    if port == 0 {
        return Err(RemoteClientError::InvalidHost(
            "SSH port must be greater than zero".to_string(),
        ));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("ssh-keyscan")
            .args(["-T", "5", "-p", &port.to_string(), hostname])
            .output(),
    )
    .await
    .map_err(|_| RemoteClientError::Timeout("SSH host-key scan".to_string()))?
    .map_err(RemoteClientError::Io)?;
    if !output.status.success() && output.stdout.is_empty() {
        return Err(RemoteClientError::Process(format!(
            "ssh-keyscan exited with {}",
            output.status
        )));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| RemoteClientError::Protocol("ssh-keyscan returned non-UTF-8 data".into()))?;
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for raw in stdout.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(_host_field) = fields.next() else {
            continue;
        };
        let Some(key_type) = fields.next() else {
            continue;
        };
        let Some(encoded) = fields.next() else {
            continue;
        };
        if fields.next().is_some()
            || !matches!(
                key_type,
                "ssh-ed25519"
                    | "ecdsa-sha2-nistp256"
                    | "ecdsa-sha2-nistp384"
                    | "ecdsa-sha2-nistp521"
                    | "ssh-rsa"
            )
        {
            continue;
        }
        let key = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| {
                RemoteClientError::Protocol("ssh-keyscan returned an invalid public key".into())
            })?;
        let digest = Sha256::digest(key);
        let fingerprint = format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        );
        if seen.insert((key_type.to_string(), fingerprint.clone())) {
            keys.push(ScannedHostKey {
                hostname: hostname.to_string(),
                port,
                key_type: key_type.to_string(),
                fingerprint,
                known_hosts_line: line.to_string(),
            });
        }
    }
    if keys.is_empty() {
        return Err(RemoteClientError::Process(
            "no supported SSH host key was returned".to_string(),
        ));
    }
    Ok(keys)
}

/// Persist a key only after the caller has shown and confirmed its fingerprint.
pub(crate) fn trust_host_key(key: &ScannedHostKey) -> Result<(), RemoteClientError> {
    validate_scan_hostname(&key.hostname)?;
    let path = known_hosts_path();
    let mut lines = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !lines.iter().any(|line| line == &key.known_hosts_line) {
        lines.push(key.known_hosts_line.clone());
    }
    let content = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    ochub_core::paths::atomic_write(&path, content.as_bytes())
        .map_err(|error| RemoteClientError::Store(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            RemoteClientError::File {
                path: path.clone(),
                source,
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RemoteHost {
        RemoteHost {
            id: "node-1".to_string(),
            label: "Dev".to_string(),
            ssh_alias: "dev@example.test".to_string(),
            hostname: Some("example.test".to_string()),
            port: Some(22),
            remote_node_id: None,
            host_key_fingerprint: None,
            ochcli_path: "/usr/local/bin/ochcli".to_string(),
            tags: vec![],
            last_seen_at: None,
        }
    }

    #[test]
    fn ssh_command_is_batch_strict_and_has_no_shell_wrapper() {
        let spec = SshCommandSpec::for_remote(&host()).unwrap();
        assert_eq!(spec.program, PathBuf::from("ssh"));
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert_eq!(
            &args[args.len() - 4..],
            ["/usr/local/bin/ochcli", "remote", "serve", "--stdio"]
        );
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    #[test]
    fn relayed_update_command_is_fixed_and_tokenized() {
        let entry = HeadlessPlatformEntry {
            url: "https://github.com/OcHub-team/OcHub/releases/download/v1.0.0/ochcli".to_string(),
            signature: "YWJjZA==".to_string(),
            sha256: "a".repeat(64),
            size: 42,
        };
        let spec = SshCommandSpec::for_node_update_receive(
            &host(),
            "8f630f44-18ac-4ab4-bf99-6fcb705213ab",
            "1.0.0",
            "linux-x86_64",
            &entry,
        )
        .unwrap();
        let args = spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.ends_with(&[
            "--sha256".to_string(),
            "a".repeat(64),
            "--size".to_string(),
            "42".to_string(),
            "--expected-node-id".to_string(),
            "8f630f44-18ac-4ab4-bf99-6fcb705213ab".to_string(),
        ]));
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    #[test]
    fn bootstrap_probe_parses_platform_path_and_pretty_json_version() {
        let stdout = b"shell startup noise\nOCHUB_BOOTSTRAP/1\tLinux\tx86_64\t/home/alice\t/home/alice/.local/bin/ochcli\n{\n  \"ok\": true,\n  \"data\": { \"version\": \"1.2.3\" }\n}\n";
        let probe = parse_bootstrap_probe(stdout).unwrap();
        assert_eq!(probe.os, "Linux");
        assert_eq!(probe.arch, "x86_64");
        assert_eq!(probe.home, PathBuf::from("/home/alice"));
        assert_eq!(
            probe.existing_cli,
            Some(PathBuf::from("/home/alice/.local/bin/ochcli"))
        );
        assert_eq!(probe.existing_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn bootstrap_install_script_only_interpolates_validated_hash_and_size() {
        let entry = HeadlessPlatformEntry {
            url: "https://github.com/OcHub-team/OcHub/releases/download/v1.0.0/ochcli".to_string(),
            signature: "YWJjZA==".to_string(),
            sha256: "b".repeat(64),
            size: 42,
        };
        let spec = SshCommandSpec::for_bootstrap_install(&host(), &entry).unwrap();
        let script = spec.args.last().unwrap().to_string_lossy();
        assert!(script.contains(&"b".repeat(64)));
        assert!(script.contains("\"42\""));
        assert!(!script.contains("__OCHUB_"));
        assert!(script.contains("$HOME/.local/bin/ochcli"));
    }

    #[test]
    fn fingerprint_matches_openssh_sha256_shape() {
        let digest = Sha256::digest(b"public-key-blob");
        let fingerprint = format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        );
        assert!(fingerprint.starts_with("SHA256:"));
        assert!(!fingerprint.ends_with('='));
    }
}
