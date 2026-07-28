use crate::AppError;
use crate::services::provider::LiveDrift;

pub type ApplicationResult<T> = Result<T, ApplicationError>;

/// Stable application-layer failures.  Transports map these variants to their
/// own status/exit codes without parsing localized display strings.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    #[error("{0}")]
    Core(#[from] AppError),

    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("{kind} already exists: {id}")]
    AlreadyExists { kind: &'static str, id: String },

    #[error("validation failed: {message}")]
    ValidationFailed {
        message: String,
        details: serde_json::Value,
    },

    #[error("capability unsupported for {app}: {capability}")]
    CapabilityUnsupported {
        app: String,
        capability: &'static str,
    },

    #[error("live configuration changed outside OcHub for {app}: {path}")]
    ConfigDrift {
        app: String,
        path: String,
        drift: Box<LiveDrift>,
    },

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("required dependency is unavailable: {0}")]
    DependencyMissing(String),

    #[error("network operation failed: {0}")]
    NetworkUnavailable(String),

    #[error("upstream operation was rejected: {0}")]
    UpstreamRejected(String),

    #[error("operation failed: {0}")]
    OperationFailed(String),

    #[error("platform capability is unavailable: {0}")]
    PlatformUnsupported(String),

    #[error("operation completed with failures: {message}")]
    PartialFailure {
        message: String,
        details: serde_json::Value,
    },

    #[error("another OcHub runtime owns this data store: {0}")]
    OwnerConflict(String),

    #[error("OcHub runtime is unavailable: {0}")]
    RuntimeUnavailable(String),

    #[error("runtime protocol is incompatible: {0}")]
    ProtocolIncompatible(String),

    #[error("resource conflict: {0}")]
    ResourceConflict(String),

    #[error("an interrupted operation requires recovery: {0}")]
    RecoveryRequired(String),
}

impl ApplicationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Core(AppError::InvalidInput(_)) => "INVALID_ARGUMENT",
            Self::Core(AppError::AppDisabled(_)) => "CAPABILITY_UNSUPPORTED",
            Self::Core(AppError::Io { .. } | AppError::IoContext { .. }) => "PERMISSION_DENIED",
            Self::Core(AppError::HttpStatus { .. }) => "UPSTREAM_REJECTED",
            Self::Core(_) => "INTERNAL",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::AlreadyExists { .. } => "ALREADY_EXISTS",
            Self::ValidationFailed { .. } => "VALIDATION_FAILED",
            Self::CapabilityUnsupported { .. } => "CAPABILITY_UNSUPPORTED",
            Self::ConfigDrift { .. } => "CONFIG_DRIFT",
            Self::InvalidInput(_) => "INVALID_ARGUMENT",
            Self::DependencyMissing(_) => "DEPENDENCY_MISSING",
            Self::NetworkUnavailable(_) => "NETWORK_UNAVAILABLE",
            Self::UpstreamRejected(_) => "UPSTREAM_REJECTED",
            Self::OperationFailed(_) => "INTERNAL",
            Self::PlatformUnsupported(_) => "PLATFORM_UNSUPPORTED",
            Self::PartialFailure { .. } => "PARTIAL_FAILURE",
            Self::OwnerConflict(_) => "OWNER_CONFLICT",
            Self::RuntimeUnavailable(_) => "RUNTIME_UNAVAILABLE",
            Self::ProtocolIncompatible(_) => "PROTOCOL_INCOMPATIBLE",
            Self::ResourceConflict(_) => "RESOURCE_CONFLICT",
            Self::RecoveryRequired(_) => "RECOVERY_REQUIRED",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Core(AppError::HttpStatus {
                status: 429 | 500..=599,
                ..
            }) | Self::NetworkUnavailable(_)
        )
    }

    pub fn details(&self) -> serde_json::Value {
        match self {
            Self::NotFound { kind, id } => {
                serde_json::json!({ "kind": kind, "id": id })
            }
            Self::AlreadyExists { kind, id } => {
                serde_json::json!({ "kind": kind, "id": id })
            }
            Self::ValidationFailed { details, .. } => details.clone(),
            Self::CapabilityUnsupported { app, capability } => {
                serde_json::json!({ "app": app, "capability": capability })
            }
            Self::ConfigDrift { app, path, drift } => {
                serde_json::json!({ "app": app, "path": path, "drift": drift })
            }
            Self::DependencyMissing(dependency) => {
                serde_json::json!({ "dependency": dependency })
            }
            Self::PartialFailure { details, .. } => details.clone(),
            Self::OwnerConflict(message)
            | Self::RuntimeUnavailable(message)
            | Self::ProtocolIncompatible(message)
            | Self::ResourceConflict(message)
            | Self::RecoveryRequired(message) => {
                serde_json::json!({ "reason": message })
            }
            _ => serde_json::Value::Null,
        }
    }

    /// Map errors produced by the external `skills` CLI and skills.sh into the
    /// stable application error vocabulary.
    pub(crate) fn from_skill_error(error: anyhow::Error) -> Self {
        let message = error.to_string();
        if message.contains("\"code\":\"NPX_MISSING\"") {
            Self::DependencyMissing(message)
        } else if message.contains("\"code\":\"UNSUPPORTED_AGENT\"") {
            Self::InvalidInput(message)
        } else if message.contains("\"code\":\"CLI_TIMEOUT\"")
            || message.contains("timed out")
            || message.contains("error sending request")
            || message.contains("connection")
        {
            Self::NetworkUnavailable(message)
        } else if message.contains("\"code\":\"CLI_")
            || message.contains("\"code\":\"INSTALL_")
            || message.contains("\"code\":\"DISCOVER_")
        {
            Self::UpstreamRejected(message)
        } else {
            Self::OperationFailed(message)
        }
    }
}
