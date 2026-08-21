//! Device-local policy for `ochcli remote serve`.
//!
//! The policy does not pretend to sandbox an ordinary SSH account: a user with
//! a shell can run local commands directly. It becomes a real authorization
//! boundary when the SSH key is restricted to the OcHub forced command.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AppError;

pub const REMOTE_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemotePolicy {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "enabled")]
    pub allow_write: bool,
    #[serde(default = "enabled")]
    pub allow_gateway_lifecycle: bool,
    #[serde(default = "enabled")]
    pub allow_daemon_lifecycle: bool,
    /// Legacy `remote.toml` key. Secret writes now follow `allow_write`.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    allow_secrets_write: bool,
    #[serde(default)]
    pub allow_backup_restore: bool,
    #[serde(default = "enabled")]
    pub allow_update_install: bool,
}

const fn schema_version() -> u32 {
    REMOTE_POLICY_SCHEMA_VERSION
}

const fn enabled() -> bool {
    true
}

impl Default for RemotePolicy {
    fn default() -> Self {
        Self {
            schema_version: REMOTE_POLICY_SCHEMA_VERSION,
            enabled: true,
            allow_write: true,
            allow_gateway_lifecycle: true,
            allow_daemon_lifecycle: true,
            allow_secrets_write: false,
            allow_backup_restore: false,
            allow_update_install: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePolicyStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub policy: RemotePolicy,
}

pub fn policy_path() -> PathBuf {
    crate::paths::get_app_config_dir().join("remote.toml")
}

pub fn load() -> Result<RemotePolicy, AppError> {
    load_at(&policy_path())
}

pub fn status() -> Result<RemotePolicyStatus, AppError> {
    let path = policy_path();
    Ok(RemotePolicyStatus {
        exists: path.exists(),
        policy: load_at(&path)?,
        path,
    })
}

pub fn validate_file() -> Result<RemotePolicyStatus, AppError> {
    status()
}

fn load_at(path: &Path) -> Result<RemotePolicy, AppError> {
    if !path.exists() {
        return Ok(RemotePolicy::default());
    }
    let source = fs::read_to_string(path).map_err(|error| AppError::io(path, error))?;
    let policy: RemotePolicy = toml::from_str(&source)
        .map_err(|error| AppError::Config(format!("invalid {}: {error}", path.display())))?;
    validate(&policy)?;
    enforce_private_permissions(path)?;
    Ok(policy)
}

fn validate(policy: &RemotePolicy) -> Result<(), AppError> {
    if policy.schema_version != REMOTE_POLICY_SCHEMA_VERSION {
        return Err(AppError::Config(format!(
            "remote policy schema {} is incompatible with supported schema {}",
            policy.schema_version, REMOTE_POLICY_SCHEMA_VERSION
        )));
    }
    if !policy.enabled
        && (policy.allow_write
            || policy.allow_gateway_lifecycle
            || policy.allow_daemon_lifecycle
            || policy.allow_backup_restore
            || policy.allow_update_install)
    {
        // This is allowed, because keeping the subordinate choices makes
        // disabling and re-enabling remote access reversible.
    }
    Ok(())
}

fn enforce_private_permissions(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::io(path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_policy_uses_safe_remote_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let policy = load_at(&directory.path().join("remote.toml")).unwrap();
        assert!(policy.enabled);
        assert!(policy.allow_write);
        assert!(!policy.allow_backup_restore);
        assert!(policy.allow_update_install);
    }

    #[test]
    fn update_install_defaults_on_but_can_be_disabled_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote.toml");

        fs::write(&path, "schemaVersion = 1\n").unwrap();
        assert!(load_at(&path).unwrap().allow_update_install);

        fs::write(&path, "schemaVersion = 1\nallowUpdateInstall = false\n").unwrap();
        assert!(!load_at(&path).unwrap().allow_update_install);
    }

    #[test]
    fn rejects_unknown_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote.toml");
        fs::write(&path, "unknown = true\n").unwrap();
        assert!(load_at(&path).is_err());
    }

    #[test]
    fn accepts_legacy_allow_secrets_write_field() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote.toml");
        fs::write(
            &path,
            "schemaVersion = 1\nallowWrite = false\nallowSecretsWrite = true\n",
        )
        .unwrap();
        let policy = load_at(&path).unwrap();
        assert!(!policy.allow_write);
        let shown = serde_json::to_value(&policy).unwrap();
        assert!(shown.get("allowSecretsWrite").is_none());
    }
}
