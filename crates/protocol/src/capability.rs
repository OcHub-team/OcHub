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
    #[serde(rename = "app.write")]
    AppWrite,
    #[serde(rename = "provider.read")]
    ProviderRead,
    #[serde(rename = "provider.write")]
    ProviderWrite,
    #[serde(rename = "provider.network")]
    ProviderNetwork,
    #[serde(rename = "mcp.read")]
    McpRead,
    #[serde(rename = "mcp.write")]
    McpWrite,
    #[serde(rename = "skill.read")]
    SkillRead,
    #[serde(rename = "skill.write")]
    SkillWrite,
    #[serde(rename = "skill.network")]
    SkillNetwork,
    #[serde(rename = "usage.read")]
    UsageRead,
    #[serde(rename = "usage.write")]
    UsageWrite,
    #[serde(rename = "usage.network")]
    UsageNetwork,
    #[serde(rename = "session.read")]
    SessionRead,
    #[serde(rename = "session.write")]
    SessionWrite,
    #[serde(rename = "proxy.read")]
    ProxyRead,
    #[serde(rename = "proxy.write")]
    ProxyWrite,
    #[serde(rename = "proxy.network")]
    ProxyNetwork,
    #[serde(rename = "settings.read")]
    SettingsRead,
    #[serde(rename = "settings.write")]
    SettingsWrite,
    #[serde(rename = "sync.read")]
    SyncRead,
    #[serde(rename = "sync.write")]
    SyncWrite,
    #[serde(rename = "sync.network")]
    SyncNetwork,
    #[serde(rename = "backup.read")]
    BackupRead,
    #[serde(rename = "backup.write")]
    BackupWrite,
    #[serde(rename = "backup.restore")]
    BackupRestore,
    #[serde(rename = "tool.read")]
    ToolRead,
    #[serde(rename = "tool.write")]
    ToolWrite,
    #[serde(rename = "update.read")]
    UpdateRead,
    #[serde(rename = "update.install")]
    UpdateInstall,
    #[serde(rename = "node.update.read")]
    NodeUpdateRead,
    #[serde(rename = "node.update.install")]
    NodeUpdateInstall,
    #[serde(rename = "node.update.relay")]
    NodeUpdateRelay,
    #[serde(rename = "data.read")]
    DataRead,
    #[serde(rename = "data.write")]
    DataWrite,
    #[serde(rename = "data.import")]
    DataImport,
    #[serde(rename = "gateway.read")]
    GatewayRead,
    #[serde(rename = "gateway.lifecycle")]
    GatewayLifecycle,
    #[serde(rename = "station.read")]
    StationRead,
    #[serde(rename = "station.write")]
    StationWrite,
    #[serde(rename = "station.network")]
    StationNetwork,
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
            Self::AppWrite => "app.write",
            Self::ProviderRead => "provider.read",
            Self::ProviderWrite => "provider.write",
            Self::ProviderNetwork => "provider.network",
            Self::McpRead => "mcp.read",
            Self::McpWrite => "mcp.write",
            Self::SkillRead => "skill.read",
            Self::SkillWrite => "skill.write",
            Self::SkillNetwork => "skill.network",
            Self::UsageRead => "usage.read",
            Self::UsageWrite => "usage.write",
            Self::UsageNetwork => "usage.network",
            Self::SessionRead => "session.read",
            Self::SessionWrite => "session.write",
            Self::ProxyRead => "proxy.read",
            Self::ProxyWrite => "proxy.write",
            Self::ProxyNetwork => "proxy.network",
            Self::SettingsRead => "settings.read",
            Self::SettingsWrite => "settings.write",
            Self::SyncRead => "sync.read",
            Self::SyncWrite => "sync.write",
            Self::SyncNetwork => "sync.network",
            Self::BackupRead => "backup.read",
            Self::BackupWrite => "backup.write",
            Self::BackupRestore => "backup.restore",
            Self::ToolRead => "tool.read",
            Self::ToolWrite => "tool.write",
            Self::UpdateRead => "update.read",
            Self::UpdateInstall => "update.install",
            Self::NodeUpdateRead => "node.update.read",
            Self::NodeUpdateInstall => "node.update.install",
            Self::NodeUpdateRelay => "node.update.relay",
            Self::DataRead => "data.read",
            Self::DataWrite => "data.write",
            Self::DataImport => "data.import",
            Self::GatewayRead => "gateway.read",
            Self::GatewayLifecycle => "gateway.lifecycle",
            Self::StationRead => "station.read",
            Self::StationWrite => "station.write",
            Self::StationNetwork => "station.network",
            Self::OperationRead => "operation.read",
            Self::DaemonLifecycle => "daemon.lifecycle",
        }
    }
}
