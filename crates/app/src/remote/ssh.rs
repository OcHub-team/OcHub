use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use super::store::{known_hosts_path, validate_scan_hostname};
use super::{RemoteClientError, RemoteHost};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshCommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl SshCommandSpec {
    pub(crate) fn for_remote(host: &RemoteHost) -> Result<Self, RemoteClientError> {
        host.validate()?;
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
