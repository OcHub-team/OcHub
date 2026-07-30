use serde::{Deserialize, Serialize};

/// Server-advertised Remote Nodes capabilities.
///
/// Capabilities are deliberately finer grained than "read" and "write" so a
/// future forced-command SSH key can enforce a meaningful policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "status.read")]
    StatusRead,
    #[serde(rename = "doctor.run")]
    DoctorRun,
    #[serde(rename = "app.read")]
    AppRead,
    #[serde(rename = "provider.read")]
    ProviderRead,
    #[serde(rename = "provider.write")]
    ProviderWrite,
    #[serde(rename = "gateway.read")]
    GatewayRead,
    #[serde(rename = "gateway.lifecycle")]
    GatewayLifecycle,
    #[serde(rename = "operation.read")]
    OperationRead,
    #[serde(rename = "daemon.lifecycle")]
    DaemonLifecycle,
}

impl Capability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StatusRead => "status.read",
            Self::DoctorRun => "doctor.run",
            Self::AppRead => "app.read",
            Self::ProviderRead => "provider.read",
            Self::ProviderWrite => "provider.write",
            Self::GatewayRead => "gateway.read",
            Self::GatewayLifecycle => "gateway.lifecycle",
            Self::OperationRead => "operation.read",
            Self::DaemonLifecycle => "daemon.lifecycle",
        }
    }
}
