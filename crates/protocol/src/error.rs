use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("remote protocol frame is empty")]
    EmptyFrame,
    #[error("remote protocol frame is {actual} bytes; maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error(
        "no compatible remote protocol version; client supports {client_min}..={client_max}, server supports {server_min}..={server_max}"
    )]
    VersionIncompatible {
        client_min: u32,
        client_max: u32,
        server_min: u32,
        server_max: u32,
    },
    #[error("invalid remote protocol value: {0}")]
    InvalidValue(String),
    #[error("remote protocol JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Stable error body returned by a remote node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: Value,
}
