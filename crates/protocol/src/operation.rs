use serde::{Deserialize, Serialize};

use crate::ProtocolError;

pub mod methods {
    pub const STATUS_READ: &str = "status.read";
    pub const DOCTOR_RUN: &str = "doctor.run";
    pub const APP_LIST: &str = "app.list";
    pub const APP_GET: &str = "app.get";
    pub const APP_SCHEMA: &str = "app.schema";
    pub const APP_SET_ENABLED: &str = "app.setEnabled";
    pub const PROVIDER_LIST: &str = "provider.list";
    pub const PROVIDER_GET: &str = "provider.get";
    pub const PROVIDER_CREATE: &str = "provider.create";
    pub const PROVIDER_UPDATE: &str = "provider.update";
    pub const PROVIDER_DELETE: &str = "provider.delete";
    pub const PROVIDER_DUPLICATE: &str = "provider.duplicate";
    pub const PROVIDER_SORT: &str = "provider.sort";
    pub const PROVIDER_COPY: &str = "provider.copy";
    pub const PROVIDER_SEED_OFFICIAL: &str = "provider.seedOfficial";
    pub const PROVIDER_IMPORT_LIVE: &str = "provider.importLive";
    pub const PROVIDER_SYNC_LIVE: &str = "provider.syncLive";
    pub const PROVIDER_ADD_TO_LIVE: &str = "provider.addToLive";
    pub const PROVIDER_REMOVE_FROM_LIVE: &str = "provider.removeFromLive";
    pub const PROVIDER_TEST: &str = "provider.test";
    pub const PROVIDER_SPEED_TEST: &str = "provider.speedTest";
    pub const PROVIDER_MODELS: &str = "provider.models";
    pub const PROVIDER_BALANCE: &str = "provider.balance";
    pub const PROVIDER_QUOTA: &str = "provider.quota";
    pub const PROVIDER_ENDPOINT_LIST: &str = "provider.endpoint.list";
    pub const PROVIDER_ENDPOINT_ADD: &str = "provider.endpoint.add";
    pub const PROVIDER_ENDPOINT_REMOVE: &str = "provider.endpoint.remove";
    pub const PROVIDER_COMMON_GET: &str = "provider.common.get";
    pub const PROVIDER_COMMON_SET: &str = "provider.common.set";
    pub const PROVIDER_COMMON_EXTRACT: &str = "provider.common.extract";
    pub const PROVIDER_COMMON_APPLY: &str = "provider.common.apply";
    pub const MCP_LIST: &str = "mcp.list";
    pub const MCP_GET: &str = "mcp.get";
    pub const MCP_UPSERT: &str = "mcp.upsert";
    pub const MCP_DELETE: &str = "mcp.delete";
    pub const MCP_SET_APP: &str = "mcp.setApp";
    pub const MCP_SYNC_ALL: &str = "mcp.syncAll";
    pub const MCP_IMPORT: &str = "mcp.import";
    pub const SKILL_LIST: &str = "skill.list";
    pub const SKILL_GET: &str = "skill.get";
    pub const SKILL_SEARCH: &str = "skill.search";
    pub const SKILL_DISCOVER: &str = "skill.discover";
    pub const SKILL_INSTALL: &str = "skill.install";
    pub const SKILL_UNINSTALL: &str = "skill.uninstall";
    pub const SKILL_CHECK_ALL: &str = "skill.checkAll";
    pub const SKILL_UPDATE: &str = "skill.update";
    pub const SKILL_UPDATE_ALL: &str = "skill.updateAll";
    pub const SKILL_SET_APP: &str = "skill.setApp";
    pub const SKILL_REPO_LIST: &str = "skill.repo.list";
    pub const SKILL_REPO_UPSERT: &str = "skill.repo.upsert";
    pub const SKILL_REPO_DELETE: &str = "skill.repo.delete";
    pub const SKILL_REPO_CATALOG: &str = "skill.repo.catalog";
    pub const USAGE_SUMMARY: &str = "usage.summary";
    pub const USAGE_SOURCES: &str = "usage.sources";
    pub const USAGE_BY_APP: &str = "usage.byApp";
    pub const USAGE_TREND: &str = "usage.trend";
    pub const USAGE_PROVIDERS: &str = "usage.providers";
    pub const USAGE_MODELS: &str = "usage.models";
    pub const USAGE_LOGS: &str = "usage.logs";
    pub const USAGE_GET: &str = "usage.get";
    pub const USAGE_SYNC: &str = "usage.sync";
    pub const USAGE_LIMITS: &str = "usage.limits";
    pub const PRICING_STATUS: &str = "pricing.status";
    pub const PRICING_REFRESH: &str = "pricing.refresh";
    pub const PRICING_OVERRIDE_LIST: &str = "pricing.override.list";
    pub const PRICING_OVERRIDE_SET: &str = "pricing.override.set";
    pub const PRICING_OVERRIDE_DELETE: &str = "pricing.override.delete";
    pub const PRICING_DEFAULTS_GET: &str = "pricing.defaults.get";
    pub const PRICING_DEFAULTS_SET: &str = "pricing.defaults.set";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_GET: &str = "session.get";
    pub const SESSION_DELETE: &str = "session.delete";
    pub const SESSION_SEARCH: &str = "session.search";
    pub const SESSION_INDEX_STATUS: &str = "session.index.status";
    pub const SESSION_INDEX_BUILD: &str = "session.index.build";
    pub const SESSION_INDEX_MAINTAIN: &str = "session.index.maintain";
    pub const SESSION_INDEX_DELETE: &str = "session.index.delete";
    pub const PROXY_GET: &str = "proxy.get";
    pub const PROXY_SET: &str = "proxy.set";
    pub const PROXY_TEST: &str = "proxy.test";
    pub const SETTINGS_LIST: &str = "settings.list";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const SETTINGS_UNSET: &str = "settings.unset";
    pub const SYNC_STATUS: &str = "sync.status";
    pub const SYNC_CONFIGURE: &str = "sync.configure";
    pub const SYNC_TEST: &str = "sync.test";
    pub const SYNC_UPLOAD: &str = "sync.upload";
    pub const SYNC_DOWNLOAD: &str = "sync.download";
    pub const SYNC_REMOTE_INFO: &str = "sync.remoteInfo";
    pub const BACKUP_LIST: &str = "backup.list";
    pub const BACKUP_CREATE: &str = "backup.create";
    pub const BACKUP_RENAME: &str = "backup.rename";
    pub const BACKUP_RESTORE: &str = "backup.restore";
    pub const BACKUP_DELETE: &str = "backup.delete";
    pub const BACKUP_EXPORT_SQL: &str = "backup.exportSql";
    pub const BACKUP_IMPORT_SQL: &str = "backup.importSql";
    pub const BACKUP_POLICY_GET: &str = "backup.policy.get";
    pub const BACKUP_POLICY_SET: &str = "backup.policy.set";
    pub const TOOL_VERSIONS: &str = "tool.versions";
    pub const TOOL_PROBE: &str = "tool.probe";
    pub const TOOL_INSTALL: &str = "tool.install";
    pub const TOOL_UPDATE: &str = "tool.update";
    pub const TOOL_ADVANCED_READ: &str = "tool.advanced.read";
    pub const TOOL_ADVANCED_WRITE: &str = "tool.advanced.write";
    pub const UPDATE_STATUS: &str = "update.status";
    pub const UPDATE_CHECK: &str = "update.check";
    pub const UPDATE_INSTALL: &str = "update.install";
    pub const DATA_DIR_SHOW: &str = "dataDir.show";
    pub const DATA_DIR_SET: &str = "dataDir.set";
    pub const DATA_DIR_RESET: &str = "dataDir.reset";
    pub const MIGRATE_CCSWITCH_DETECT: &str = "migrate.ccswitch.detect";
    pub const MIGRATE_CCSWITCH_PLAN: &str = "migrate.ccswitch.plan";
    pub const MIGRATE_CCSWITCH_IMPORT: &str = "migrate.ccswitch.import";
    pub const PROVIDER_SWITCH_PLAN: &str = "provider.switch.plan";
    pub const PROVIDER_SWITCH_APPLY: &str = "provider.switch.apply";
    pub const GATEWAY_STATUS: &str = "gateway.status";
    pub const GATEWAY_START: &str = "gateway.start";
    pub const GATEWAY_STOP: &str = "gateway.stop";
    pub const GATEWAY_CONNECTION_INFO: &str = "gateway.connectionInfo";
    pub const STATION_LIST: &str = "station.list";
    pub const STATION_GET: &str = "station.get";
    pub const STATION_CREATE: &str = "station.create";
    pub const STATION_UPDATE: &str = "station.update";
    pub const STATION_DELETE: &str = "station.delete";
    pub const STATION_SET_ENABLED: &str = "station.setEnabled";
    pub const STATION_PROBE: &str = "station.probe";
    pub const STATION_MODELS: &str = "station.models";
    pub const STATION_DETECT_DIALECTS: &str = "station.detectDialects";
    pub const STATION_FETCH_MODELS: &str = "station.fetchModels";
    pub const STATION_TEST_ENDPOINT: &str = "station.testEndpoint";
    pub const STATION_SELECT: &str = "station.select";
    pub const STATION_APPLY: &str = "station.apply";
    pub const STATION_DISCONNECT: &str = "station.disconnect";
    pub const STATION_CONNECTION_INFO: &str = "station.connectionInfo";
    pub const STATION_IMPORT_PROVIDER: &str = "station.importProvider";
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCreateParams {
    pub app: String,
    pub provider: serde_json::Value,
    #[serde(default)]
    pub add_to_live: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderUpdateParams {
    pub app: String,
    pub provider_id: String,
    /// JSON Merge Patch applied to the stored provider.
    pub patch: serde_json::Value,
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
