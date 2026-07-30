use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::RemoteClientError;

const REMOTE_HOSTS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteHost {
    pub id: String,
    pub label: String,
    pub ssh_alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_fingerprint: Option<String>,
    #[serde(default = "default_ochcli_path")]
    pub ochcli_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
}

fn default_ochcli_path() -> String {
    "ochcli".to_string()
}

impl RemoteHost {
    pub(crate) fn validate(&self) -> Result<(), RemoteClientError> {
        validate_label(&self.label)?;
        validate_ssh_alias(&self.ssh_alias)?;
        validate_ochcli_path(&self.ochcli_path)?;
        if self.id.trim().is_empty() || self.id.len() > 128 {
            return Err(RemoteClientError::InvalidHost(
                "connection id must contain 1 to 128 characters".to_string(),
            ));
        }
        if let Some(hostname) = &self.hostname {
            validate_scan_hostname(hostname)?;
        }
        if self
            .remote_node_id
            .as_deref()
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_err())
        {
            return Err(RemoteClientError::InvalidHost(
                "remote node id is not a UUID".to_string(),
            ));
        }
        if self.tags.len() > 64
            || self.tags.iter().any(|tag| {
                tag.trim().is_empty() || tag.len() > 64 || tag.chars().any(char::is_control)
            })
        {
            return Err(RemoteClientError::InvalidHost(
                "tags must contain 1 to 64 printable characters".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteHostsDocument {
    schema_version: u32,
    #[serde(default)]
    hosts: Vec<RemoteHost>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteHostStore {
    hosts: Vec<RemoteHost>,
}

impl RemoteHostStore {
    pub(crate) fn load() -> Result<Self, RemoteClientError> {
        Self::load_at(&remote_hosts_path())
    }

    fn load_at(path: &Path) -> Result<Self, RemoteClientError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path).map_err(|source| RemoteClientError::File {
            path: path.to_path_buf(),
            source,
        })?;
        let document: RemoteHostsDocument = serde_json::from_slice(&bytes)?;
        if document.schema_version != REMOTE_HOSTS_SCHEMA_VERSION {
            return Err(RemoteClientError::InvalidHost(format!(
                "remote hosts schema {} is incompatible with supported schema {}",
                document.schema_version, REMOTE_HOSTS_SCHEMA_VERSION
            )));
        }
        for host in &document.hosts {
            host.validate()?;
        }
        let mut ids = std::collections::HashSet::new();
        if document
            .hosts
            .iter()
            .any(|host| !ids.insert(host.id.clone()))
        {
            return Err(RemoteClientError::InvalidHost(
                "remote hosts file contains duplicate connection ids".to_string(),
            ));
        }
        enforce_private_permissions(path)?;
        Ok(Self {
            hosts: document.hosts,
        })
    }

    pub(crate) fn hosts(&self) -> &[RemoteHost] {
        &self.hosts
    }

    pub(crate) fn get(&self, id: &str) -> Option<&RemoteHost> {
        self.hosts.iter().find(|host| host.id == id)
    }

    pub(crate) fn upsert(&mut self, host: RemoteHost) -> Result<(), RemoteClientError> {
        host.validate()?;
        if let Some(existing) = self
            .hosts
            .iter_mut()
            .find(|existing| existing.id == host.id)
        {
            *existing = host;
        } else {
            self.hosts.push(host);
        }
        self.hosts.sort_by_key(|host| host.label.to_lowercase());
        self.save()
    }

    pub(crate) fn remove(&mut self, id: &str) -> Result<bool, RemoteClientError> {
        let before = self.hosts.len();
        self.hosts.retain(|host| host.id != id);
        let removed = before != self.hosts.len();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<(), RemoteClientError> {
        self.save_at(&remote_hosts_path())
    }

    fn save_at(&self, path: &Path) -> Result<(), RemoteClientError> {
        let document = RemoteHostsDocument {
            schema_version: REMOTE_HOSTS_SCHEMA_VERSION,
            hosts: self.hosts.clone(),
        };
        ochub_core::paths::write_json_file(path, &document)
            .map_err(|error| RemoteClientError::Store(error.to_string()))?;
        enforce_private_permissions(path)
    }
}

pub(crate) fn remote_hosts_path() -> PathBuf {
    ochub_core::paths::get_home_dir()
        .join(".ochub")
        .join("remote-hosts.json")
}

pub(crate) fn known_hosts_path() -> PathBuf {
    ochub_core::paths::get_home_dir()
        .join(".ochub")
        .join("ssh")
        .join("known_hosts")
}

pub(crate) fn validate_ssh_alias(value: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':' | b'%' | b'[' | b']')
        })
    {
        return Err(RemoteClientError::InvalidHost(
            "SSH alias must contain 1 to 255 safe hostname characters and may not start with '-'"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_ochcli_path(value: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 1_024
        || value.starts_with('-')
        || value.split('/').any(|segment| segment == "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(RemoteClientError::InvalidHost(
            "ochcli path must be a command name or absolute path without whitespace or '..'"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_scan_hostname(value: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b':' | b'%' | b'[' | b']')
        })
    {
        return Err(RemoteClientError::InvalidHost(
            "hostname contains unsupported characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_label(value: &str) -> Result<(), RemoteClientError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 80 || value.chars().any(char::is_control) {
        return Err(RemoteClientError::InvalidHost(
            "label must contain 1 to 80 printable characters".to_string(),
        ));
    }
    Ok(())
}

fn enforce_private_permissions(path: &Path) -> Result<(), RemoteClientError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            RemoteClientError::File {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, label: &str) -> RemoteHost {
        RemoteHost {
            id: id.to_string(),
            label: label.to_string(),
            ssh_alias: "user@example.test".to_string(),
            hostname: Some("example.test".to_string()),
            port: Some(22),
            remote_node_id: None,
            host_key_fingerprint: None,
            ochcli_path: "ochcli".to_string(),
            tags: vec!["dev".to_string()],
            last_seen_at: None,
        }
    }

    #[test]
    fn store_round_trips_and_sorts_hosts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("remote-hosts.json");
        let mut store = RemoteHostStore::default();
        store.hosts.push(host("b", "Zulu"));
        store.hosts.push(host("a", "Alpha"));
        store
            .hosts
            .sort_by(|left, right| left.label.cmp(&right.label));
        store.save_at(&path).unwrap();
        let loaded = RemoteHostStore::load_at(&path).unwrap();
        assert_eq!(loaded.hosts()[0].label, "Alpha");
        assert_eq!(loaded.hosts()[1].label, "Zulu");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_option_injection_and_shell_paths() {
        assert!(validate_ssh_alias("-oProxyCommand=bad").is_err());
        assert!(validate_ssh_alias("user@example.test").is_ok());
        assert!(validate_ochcli_path("/usr/local/bin/ochcli").is_ok());
        assert!(validate_ochcli_path("ochcli;rm").is_err());
        assert!(validate_ochcli_path("../ochcli").is_err());
    }
}
