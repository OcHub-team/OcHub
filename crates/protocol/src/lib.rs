//! Versioned, transport-neutral protocol for OcHub Remote Nodes.
//!
//! The wire format is UTF-8 JSON Lines. SSH supplies authentication,
//! confidentiality and integrity; this crate defines only the application
//! frames carried over that channel.

mod capability;
mod error;
mod frame;
mod handshake;
mod operation;

pub use capability::Capability;
pub use error::{ProtocolError, RemoteError};
pub use frame::{
    CancelFrame, EventFrame, Frame, GoodbyeFrame, HelloAckFrame, HelloFrame, PingFrame, PongFrame,
    ProtocolErrorFrame, RequestFrame, ResponseFrame,
};
pub use handshake::{NodeDescriptor, RuntimeDescriptor, negotiate_protocol};
pub use operation::{
    ApplyPlanParams, ProviderSwitchParams, methods, require_non_empty, validate_request_id,
};

/// First protocol version implemented by this build.
pub const PROTOCOL_MIN: u32 = 1;
/// Newest protocol version implemented by this build.
pub const PROTOCOL_MAX: u32 = 1;
/// Schema version for stable DTOs nested in protocol responses.
pub const SCHEMA_VERSION: u32 = 1;
/// Maximum serialized size of one JSONL frame.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Serialize a frame and append the JSONL delimiter.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = serde_json::to_vec(frame)?;
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            maximum: MAX_FRAME_SIZE,
        });
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode one complete JSONL frame.
pub fn decode_frame(bytes: &[u8]) -> Result<Frame, ProtocolError> {
    if bytes.len() > MAX_FRAME_SIZE + 1 {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            maximum: MAX_FRAME_SIZE,
        });
    }
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(ProtocolError::EmptyFrame);
    }
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hello_round_trips_as_camel_case_jsonl() {
        let frame = Frame::Hello(HelloFrame {
            protocol_min: 1,
            protocol_max: 1,
            client_version: "0.5.0".to_string(),
            locale: Some("zh-CN".to_string()),
            device_id: Some("desktop-1".to_string()),
        });
        let bytes = encode_frame(&frame).unwrap();
        assert!(bytes.ends_with(b"\n"));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "hello");
        assert_eq!(value["protocolMin"], 1);
        assert_eq!(decode_frame(&bytes).unwrap(), frame);
    }

    #[test]
    fn response_error_contract_is_stable() {
        let frame = Frame::Response(ResponseFrame {
            protocol_version: 1,
            request_id: "request-1".to_string(),
            ok: false,
            data: serde_json::Value::Null,
            warnings: vec![],
            error: Some(RemoteError {
                code: "NOT_FOUND".to_string(),
                message: "missing".to_string(),
                retryable: false,
                details: json!({"kind": "provider"}),
            }),
            revision: None,
        });
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "response");
        assert_eq!(value["error"]["code"], "NOT_FOUND");
        assert_eq!(value["requestId"], "request-1");
    }

    #[test]
    fn negotiates_highest_shared_version() {
        assert_eq!(negotiate_protocol(1, 3, 1, 2).unwrap(), 2);
        assert!(negotiate_protocol(3, 4, 1, 2).is_err());
    }

    #[test]
    fn rejects_empty_or_oversized_frames() {
        assert!(matches!(
            decode_frame(b" \n"),
            Err(ProtocolError::EmptyFrame)
        ));
        let oversized = vec![b'x'; MAX_FRAME_SIZE + 2];
        assert!(matches!(
            decode_frame(&oversized),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }
}
