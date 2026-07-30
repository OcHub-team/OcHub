use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Capability, NodeDescriptor, RemoteError, RuntimeDescriptor};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Frame {
    #[serde(rename = "hello")]
    Hello(HelloFrame),
    #[serde(rename = "helloAck")]
    HelloAck(HelloAckFrame),
    #[serde(rename = "request")]
    Request(RequestFrame),
    #[serde(rename = "response")]
    Response(ResponseFrame),
    #[serde(rename = "event")]
    Event(EventFrame),
    #[serde(rename = "cancel")]
    Cancel(CancelFrame),
    #[serde(rename = "ping")]
    Ping(PingFrame),
    #[serde(rename = "pong")]
    Pong(PongFrame),
    #[serde(rename = "protocolError")]
    ProtocolError(ProtocolErrorFrame),
    #[serde(rename = "goodbye")]
    Goodbye(GoodbyeFrame),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloFrame {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub client_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAckFrame {
    pub protocol_version: u32,
    pub schema_version: u32,
    pub server_version: String,
    pub node: NodeDescriptor,
    pub runtime: RuntimeDescriptor,
    pub capabilities: Vec<Capability>,
    pub max_frame_size: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFrame {
    pub protocol_version: u32,
    pub request_id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseFrame {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RemoteError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFrame {
    pub protocol_version: u32,
    pub request_id: String,
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelFrame {
    pub protocol_version: u32,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingFrame {
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PongFrame {
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolErrorFrame {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoodbyeFrame {
    pub reason: String,
}
