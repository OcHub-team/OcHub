use serde::{Deserialize, Serialize};

use crate::ProtocolError;

pub mod methods {
    pub const STATUS_READ: &str = "status.read";
    pub const DOCTOR_RUN: &str = "doctor.run";
    pub const APP_LIST: &str = "app.list";
    pub const PROVIDER_LIST: &str = "provider.list";
    pub const PROVIDER_GET: &str = "provider.get";
    pub const PROVIDER_SWITCH_PLAN: &str = "provider.switch.plan";
    pub const PROVIDER_SWITCH_APPLY: &str = "provider.switch.apply";
    pub const GATEWAY_STATUS: &str = "gateway.status";
    pub const GATEWAY_START: &str = "gateway.start";
    pub const GATEWAY_STOP: &str = "gateway.stop";
    pub const OPERATION_LIST: &str = "operation.list";
    pub const OPERATION_INSPECT: &str = "operation.inspect";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSwitchParams {
    pub app: String,
    pub provider_id: String,
    #[serde(default = "default_drift_policy")]
    pub on_drift: String,
}

fn default_drift_policy() -> String {
    "abort".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPlanParams {
    pub plan_id: String,
}

pub fn require_non_empty<'a>(value: &'a str, field: &str) -> Result<&'a str, ProtocolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ProtocolError::InvalidValue(format!(
            "{field} must not be empty"
        )));
    }
    Ok(value)
}

pub fn validate_request_id(value: &str) -> Result<(), ProtocolError> {
    let value = require_non_empty(value, "requestId")?;
    if value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(ProtocolError::InvalidValue(
            "requestId must contain 1 to 128 safe ASCII characters".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_reject_whitespace_and_shell_metacharacters() {
        assert!(validate_request_id("request-1").is_ok());
        assert!(validate_request_id("trace:desktop/node").is_ok());
        assert!(validate_request_id("contains space").is_err());
        assert!(validate_request_id("$(command)").is_err());
    }
}
