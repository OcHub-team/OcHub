use std::process::ExitCode;

use ochub_core::application::ApplicationError;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Application(#[from] ApplicationError),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("{0}")]
    Core(#[from] ochub_core::AppError),

    #[error("{message}")]
    Remote {
        code: String,
        message: String,
        retryable: bool,
        details: serde_json::Value,
        exit_code: u8,
    },
}

impl CliError {
    pub fn code(&self) -> &str {
        match self {
            Self::Application(error) => error.code(),
            Self::InvalidInput(_) | Self::Json(_) | Self::Yaml(_) => "INVALID_ARGUMENT",
            Self::Io(error) if error.kind() == std::io::ErrorKind::NotFound => "NOT_FOUND",
            Self::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                "PERMISSION_DENIED"
            }
            Self::Io(_) | Self::Core(_) => "INTERNAL",
            Self::Remote { code, .. } => code,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::Application(error) if error.retryable())
            || matches!(
                self,
                Self::Remote {
                    retryable: true,
                    ..
                }
            )
    }

    pub fn details(&self) -> serde_json::Value {
        match self {
            Self::Application(error) => error.details(),
            Self::Io(error) => serde_json::json!({ "kind": format!("{:?}", error.kind()) }),
            Self::Remote { details, .. } => details.clone(),
            _ => serde_json::Value::Null,
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.exit_code_u8())
    }

    pub fn exit_code_u8(&self) -> u8 {
        if let Self::Remote { exit_code, .. } = self {
            return *exit_code;
        }
        match self.code() {
            "INVALID_ARGUMENT" | "VALIDATION_FAILED" => 2,
            "NOT_FOUND" => 3,
            "CONFIG_DRIFT" | "RESOURCE_CONFLICT" | "OWNER_CONFLICT" => 4,
            "PERMISSION_DENIED" | "PATH_UNSAFE" => 5,
            "NETWORK_UNAVAILABLE" | "UPSTREAM_REJECTED" => 6,
            "DEPENDENCY_MISSING" | "PLATFORM_UNSUPPORTED" | "CAPABILITY_UNSUPPORTED" => 7,
            "PARTIAL_FAILURE" => 8,
            "CANCELLED" => 9,
            "RUNTIME_UNAVAILABLE" => 10,
            _ => 1,
        }
    }
}
