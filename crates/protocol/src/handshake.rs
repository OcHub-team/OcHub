use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProtocolError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDescriptor {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDescriptor {
    pub persistent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    #[serde(default)]
    pub gateway: Value,
}

pub fn negotiate_protocol(
    client_min: u32,
    client_max: u32,
    server_min: u32,
    server_max: u32,
) -> Result<u32, ProtocolError> {
    if client_min == 0 || server_min == 0 || client_min > client_max || server_min > server_max {
        return Err(ProtocolError::InvalidValue(
            "protocol ranges must be non-zero and ordered".to_string(),
        ));
    }
    let lower = client_min.max(server_min);
    let upper = client_max.min(server_max);
    if lower > upper {
        return Err(ProtocolError::VersionIncompatible {
            client_min,
            client_max,
            server_min,
            server_max,
        });
    }
    Ok(upper)
}
