//! Stable identity for one OcHub data directory.
//!
//! The node id is not an authentication credential. SSH host-key validation
//! authenticates the machine; this id lets a desktop recognize the same OcHub
//! node after its hostname, address or SSH alias changes.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AppError;

pub const NODE_IDENTITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentity {
    pub schema_version: u32,
    pub node_id: String,
    pub created_at: String,
}

pub fn identity_path() -> PathBuf {
    crate::paths::get_app_config_dir().join("node.json")
}

pub fn load_or_create() -> Result<NodeIdentity, AppError> {
    load_or_create_at(&identity_path())
}

fn load_or_create_at(path: &Path) -> Result<NodeIdentity, AppError> {
    if path.exists() {
        let identity = crate::paths::read_json_file::<NodeIdentity>(path)?;
        validate(&identity)?;
        enforce_private_permissions(path)?;
        return Ok(identity);
    }
    let identity = NodeIdentity {
        schema_version: NODE_IDENTITY_SCHEMA_VERSION,
        node_id: uuid::Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    crate::paths::write_json_file(path, &identity)?;
    enforce_private_permissions(path)?;
    Ok(identity)
}

fn validate(identity: &NodeIdentity) -> Result<(), AppError> {
    if identity.schema_version != NODE_IDENTITY_SCHEMA_VERSION {
        return Err(AppError::Config(format!(
            "node identity schema {} is incompatible with supported schema {}",
            identity.schema_version, NODE_IDENTITY_SCHEMA_VERSION
        )));
    }
    uuid::Uuid::parse_str(&identity.node_id)
        .map_err(|_| AppError::Config("node identity contains an invalid nodeId".to_string()))?;
    if identity.created_at.trim().is_empty() {
        return Err(AppError::Config(
            "node identity contains an empty createdAt".to_string(),
        ));
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
    fn identity_is_stable_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node.json");
        let first = load_or_create_at(&path).unwrap();
        let second = load_or_create_at(&path).unwrap();
        assert_eq!(first, second);
        uuid::Uuid::parse_str(&first.node_id).unwrap();
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
    fn incompatible_or_invalid_identity_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node.json");
        crate::paths::write_json_file(
            &path,
            &NodeIdentity {
                schema_version: 2,
                node_id: "not-a-uuid".to_string(),
                created_at: String::new(),
            },
        )
        .unwrap();
        assert!(load_or_create_at(&path).is_err());
    }
}
