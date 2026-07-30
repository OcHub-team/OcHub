use std::sync::Arc;

use ochub_core::AppId;
use ochub_core::Provider;
use ochub_core::application::{
    AppSummary, Application, ApplicationError, ConfigSchemaDto, DoctorReport, GatewayStation,
    PricingDefault, ProviderDetails, ProviderListItem, ProviderSwitchPlan, ProviderSwitchPolicy,
    StatusSummary, UsageFilter,
};
use ochub_core::db::import_ccswitch::{DetectedSource, ImportReport};
use ochub_core::db::{BackupEntry, InstalledSkill, McpServer, SkillRepo};
use ochub_core::gateway::GatewayStatus;
use ochub_core::gateway::apply::ApplyResult;
use ochub_core::gateway::types::{Dialect, GatewayAppModelPolicy, GatewayEndpointTestResult};
use ochub_core::runtime::journal::OperationRecord;
use ochub_core::services::session_usage::{DataSourceSummary, SessionSyncResult};
use ochub_core::services::skill::{
    DiscoverableSkill, Skill, SkillUninstallResult, SkillUpdateInfo, SkillsShSearchResult,
};
use ochub_core::services::usage_stats::{
    DailyStats, LogFilters, ModelPricingInfo, ModelStats, PaginatedLogs, ProviderStats,
    RequestLogDetail, UsageSummary, UsageSummaryByApp,
};
use ochub_core::services::{PricingCatalogRefreshOutcome, PricingCatalogStatus};
use ochub_core::session_index::{IndexStats, MaintenanceOutcome, SearchHit, SyncOutcome};
use ochub_core::session_manager::{
    DeleteSessionOutcome, SessionMessage, SessionMeta, ToolInstallationReport, ToolVersion,
};
use ochub_core::settings::{ProxySettings, S3SyncSettings, WebDavSyncSettings};
use ochub_protocol::{Capability, methods};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{RemoteClient, RemoteClientError, RemoteRequestOptions};

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspaceBackendError {
    #[error(transparent)]
    Local(#[from] ApplicationError),
    #[error(transparent)]
    Remote(Box<RemoteClientError>),
    #[error("remote response has an invalid shape: {0}")]
    Response(#[from] serde_json::Error),
    #[error("workspace state changed after the plan was created")]
    Conflict,
}

impl From<RemoteClientError> for WorkspaceBackendError {
    fn from(value: RemoteClientError) -> Self {
        Self::Remote(Box::new(value))
    }
}

#[derive(Clone)]
pub(crate) enum WorkspaceBackend {
    // Constructed as each existing local-only view is migrated to this
    // boundary; Remote Nodes is the first consumer.
    #[allow(dead_code)]
    Local(Arc<Application>),
    Remote(Arc<RemoteClient>),
}

#[derive(Debug, Clone)]
pub(crate) enum ProviderSwitchHandle {
    Local {
        app: AppId,
        provider_id: String,
        policy: ProviderSwitchPolicy,
        revision: String,
        plan: ProviderSwitchPlan,
    },
    Remote {
        plan_id: String,
        revision: String,
        plan: ProviderSwitchPlan,
    },
}

impl ProviderSwitchHandle {
    pub(crate) fn plan(&self) -> &ProviderSwitchPlan {
        match self {
            Self::Local { plan, .. } | Self::Remote { plan, .. } => plan,
        }
    }

    pub(crate) fn revision(&self) -> &str {
        match self {
            Self::Local { revision, .. } | Self::Remote { revision, .. } => revision,
        }
    }
}

#[allow(dead_code)]
impl WorkspaceBackend {
    #[allow(dead_code)]
    pub(crate) fn local(state: Arc<ochub_core::AppState>) -> Self {
        Self::Local(Arc::new(Application::from_state(state)))
    }

    pub(crate) fn remote(client: Arc<RemoteClient>) -> Self {
        Self::Remote(client)
    }

    #[allow(dead_code)]
    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    pub(crate) async fn status(&self) -> Result<StatusSummary, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.status().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::StatusRead)?;
                let response = client
                    .request(methods::STATUS_READ, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_apps(&self) -> Result<Vec<AppSummary>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_apps()?),
            Self::Remote(client) => {
                client.require_capability(Capability::AppRead)?;
                let response = client
                    .request(methods::APP_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_app_enabled(
        &self,
        app: &AppId,
        enabled: bool,
    ) -> Result<AppSummary, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_app_enabled(app, enabled).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::AppWrite)?;
                let response = client
                    .request(
                        methods::APP_SET_ENABLED,
                        json!({ "app": app.as_str(), "enabled": enabled }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn app_schema(
        &self,
        app: &AppId,
    ) -> Result<ConfigSchemaDto, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.app_schema(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::AppRead)?;
                let response = client
                    .request(methods::APP_SCHEMA, app_params(app), Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_mcp_servers(&self) -> Result<Vec<McpServer>, WorkspaceBackendError> {
        let values = match self {
            Self::Local(application) => application.list_mcp_servers(true)?,
            Self::Remote(client) => {
                client.require_capability(Capability::McpRead)?;
                client
                    .request(methods::MCP_LIST, Value::Null, Default::default())
                    .await?
                    .data
                    .as_array()
                    .cloned()
                    .ok_or_else(|| {
                        WorkspaceBackendError::from(RemoteClientError::Protocol(
                            "mcp.list response must be an array".to_string(),
                        ))
                    })?
            }
        };
        values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(WorkspaceBackendError::Response)
    }

    pub(crate) async fn upsert_mcp_server(
        &self,
        original_id: Option<&str>,
        server: McpServer,
    ) -> Result<McpServer, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(serde_json::from_value(
                application.upsert_mcp_server(server)?,
            )?),
            Self::Remote(client) => {
                client.require_capability(Capability::McpWrite)?;
                let response = client
                    .request(
                        methods::MCP_UPSERT,
                        json!({
                            "server": server,
                            "originalId": original_id
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_mcp_server(&self, id: &str) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.delete_mcp_server(id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::McpWrite)?;
                client
                    .request(methods::MCP_DELETE, json!({ "id": id }), mutation_options())
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn set_mcp_app(
        &self,
        id: &str,
        app: &AppId,
        enabled: bool,
    ) -> Result<McpServer, WorkspaceBackendError> {
        let value = match self {
            Self::Local(application) => application.set_mcp_app_enabled(id, app, enabled)?,
            Self::Remote(client) => {
                client.require_capability(Capability::McpWrite)?;
                client
                    .request(
                        methods::MCP_SET_APP,
                        json!({
                            "id": id,
                            "app": app.as_str(),
                            "enabled": enabled
                        }),
                        mutation_options(),
                    )
                    .await?
                    .data
            }
        };
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) async fn sync_all_mcp_servers(&self) -> Result<usize, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.sync_all_mcp_servers()?),
            Self::Remote(client) => {
                client.require_capability(Capability::McpWrite)?;
                let response = client
                    .request(methods::MCP_SYNC_ALL, Value::Null, mutation_options())
                    .await?;
                Ok(response
                    .data
                    .get("synced")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize)
            }
        }
    }

    pub(crate) async fn import_mcp_from_app(
        &self,
        app: &AppId,
    ) -> Result<usize, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.import_mcp_from_app(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::McpWrite)?;
                let response = client
                    .request(methods::MCP_IMPORT, app_params(app), mutation_options())
                    .await?;
                Ok(response
                    .data
                    .get("imported")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize)
            }
        }
    }

    pub(crate) async fn list_installed_skills(
        &self,
    ) -> Result<Vec<InstalledSkill>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_installed_skills()?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillRead)?;
                let response = client
                    .request(methods::SKILL_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn search_skills(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SkillsShSearchResult, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.search_skills(query, limit, offset).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillNetwork)?;
                let response = client
                    .request(
                        methods::SKILL_SEARCH,
                        json!({ "query": query, "limit": limit, "offset": offset }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn discover_skills(
        &self,
    ) -> Result<Vec<DiscoverableSkill>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.discover_skills(None).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillNetwork)?;
                let response = client
                    .request(methods::SKILL_DISCOVER, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn install_skill(
        &self,
        skill: &DiscoverableSkill,
        app: &AppId,
    ) -> Result<InstalledSkill, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.install_skill(skill, app).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(
                        methods::SKILL_INSTALL,
                        json!({ "skill": skill, "app": app.as_str() }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn uninstall_skill(
        &self,
        id: &str,
    ) -> Result<SkillUninstallResult, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.uninstall_skill(id).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(
                        methods::SKILL_UNINSTALL,
                        json!({ "id": id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn check_skill_updates(
        &self,
    ) -> Result<Vec<SkillUpdateInfo>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.check_skill_updates().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillNetwork)?;
                let response = client
                    .request(methods::SKILL_CHECK_ALL, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn update_skill(
        &self,
        id: &str,
    ) -> Result<InstalledSkill, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.update_skill(id).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(
                        methods::SKILL_UPDATE,
                        json!({ "id": id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn update_all_skills(
        &self,
    ) -> Result<Vec<InstalledSkill>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.update_all_skills().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(methods::SKILL_UPDATE_ALL, Value::Null, mutation_options())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_skill_app(
        &self,
        id: &str,
        app: &AppId,
        enabled: bool,
    ) -> Result<InstalledSkill, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.set_skill_app_enabled(id, app, enabled).await?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(
                        methods::SKILL_SET_APP,
                        json!({ "id": id, "app": app.as_str(), "enabled": enabled }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_skill_repos(&self) -> Result<Vec<SkillRepo>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_skill_repos()?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillRead)?;
                let response = client
                    .request(methods::SKILL_REPO_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn upsert_skill_repo(
        &self,
        original_id: Option<&str>,
        repo: SkillRepo,
    ) -> Result<SkillRepo, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.save_skill_repo(repo)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                let response = client
                    .request(
                        methods::SKILL_REPO_UPSERT,
                        json!({ "repo": repo, "originalId": original_id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_skill_repo(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.delete_skill_repo(owner, name)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SkillWrite)?;
                client
                    .request(
                        methods::SKILL_REPO_DELETE,
                        json!({ "id": format!("{owner}/{name}") }),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn skill_repo_catalog(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Vec<Skill>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let repo = application.get_skill_repo(owner, name)?;
                Ok(application.skill_catalog(Some(repo)).await?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::SkillNetwork)?;
                let response = client
                    .request(
                        methods::SKILL_REPO_CATALOG,
                        json!({ "id": format!("{owner}/{name}") }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_summary(
        &self,
        filter: &UsageFilter,
    ) -> Result<UsageSummary, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_summary(filter)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_SUMMARY,
                        usage_filter_params(filter),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_by_app(
        &self,
        filter: &UsageFilter,
    ) -> Result<Vec<UsageSummaryByApp>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_by_app(filter)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_BY_APP,
                        usage_filter_params(filter),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_trend(
        &self,
        filter: &UsageFilter,
    ) -> Result<Vec<DailyStats>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_trend(filter, "day")?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let mut params = usage_filter_params(filter);
                params["interval"] = json!("day");
                let response = client
                    .request(methods::USAGE_TREND, params, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_provider_stats(
        &self,
        filter: &UsageFilter,
    ) -> Result<Vec<ProviderStats>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_provider_stats(filter)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_PROVIDERS,
                        usage_filter_params(filter),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_model_stats(
        &self,
        filter: &UsageFilter,
    ) -> Result<Vec<ModelStats>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_model_stats(filter)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_MODELS,
                        usage_filter_params(filter),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_logs(
        &self,
        filters: &LogFilters,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedLogs, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_logs(filters, page, page_size)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_LOGS,
                        json!({
                            "from": filters.start_date,
                            "to": filters.end_date,
                            "app": filters.app_type,
                            "provider": filters.provider_name,
                            "model": filters.model,
                            "status": filters.status_code,
                            "page": page,
                            "pageSize": page_size
                        }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_request(
        &self,
        request_id: &str,
    ) -> Result<RequestLogDetail, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_request(request_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::USAGE_GET,
                        json!({ "requestId": request_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn usage_sources(
        &self,
    ) -> Result<Vec<DataSourceSummary>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.usage_sources()?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(methods::USAGE_SOURCES, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn sync_usage(
        &self,
        apps: &[AppId],
    ) -> Result<SessionSyncResult, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.sync_usage(apps)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageWrite)?;
                let response = client
                    .request(
                        methods::USAGE_SYNC,
                        json!({ "apps": apps.iter().map(AppId::as_str).collect::<Vec<_>>() }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn pricing_status(
        &self,
    ) -> Result<PricingCatalogStatus, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.pricing_status()?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(methods::PRICING_STATUS, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn refresh_pricing(
        &self,
        force: bool,
    ) -> Result<PricingCatalogRefreshOutcome, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.refresh_pricing(force).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageNetwork)?;
                let response = client
                    .request(
                        methods::PRICING_REFRESH,
                        json!({ "force": force }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_pricing_overrides(
        &self,
    ) -> Result<Vec<ModelPricingInfo>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_pricing_overrides()?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::PRICING_OVERRIDE_LIST,
                        Value::Null,
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_pricing_override(
        &self,
        pricing: ModelPricingInfo,
    ) -> Result<ModelPricingInfo, WorkspaceBackendError> {
        let model_id = pricing.model_id.clone();
        match self {
            Self::Local(application) => Ok(application.set_pricing_override(&model_id, &pricing)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageWrite)?;
                let response = client
                    .request(
                        methods::PRICING_OVERRIDE_SET,
                        json!({ "modelId": model_id, "pricing": pricing }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_pricing_override(
        &self,
        model_id: &str,
    ) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.remove_pricing_override(model_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageWrite)?;
                client
                    .request(
                        methods::PRICING_OVERRIDE_DELETE,
                        json!({ "modelId": model_id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn pricing_defaults(
        &self,
    ) -> Result<Vec<PricingDefault>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.pricing_defaults().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageRead)?;
                let response = client
                    .request(
                        methods::PRICING_DEFAULTS_GET,
                        Value::Null,
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_pricing_defaults(
        &self,
        defaults: Vec<PricingDefault>,
    ) -> Result<Vec<PricingDefault>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_pricing_defaults(&defaults).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::UsageWrite)?;
                let response = client
                    .request(
                        methods::PRICING_DEFAULTS_SET,
                        json!({ "defaults": defaults }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_sessions(
        &self,
        app: Option<&AppId>,
        query: Option<&str>,
    ) -> Result<Vec<SessionMeta>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let apps = app.into_iter().cloned().collect::<Vec<_>>();
                Ok(application.list_sessions(&apps, query)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::SessionRead)?;
                let response = client
                    .request(
                        methods::SESSION_LIST,
                        json!({
                            "app": app.map(AppId::as_str),
                            "query": query
                        }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn get_session_messages(
        &self,
        app: &AppId,
        id: &str,
    ) -> Result<(SessionMeta, Vec<SessionMessage>), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.get_session_messages(app, id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionRead)?;
                let response = client
                    .request(
                        methods::SESSION_GET,
                        json!({ "app": app.as_str(), "id": id }),
                        Default::default(),
                    )
                    .await?;
                #[derive(Deserialize)]
                struct Response {
                    session: SessionMeta,
                    messages: Vec<SessionMessage>,
                }
                let response: Response = serde_json::from_value(response.data)?;
                Ok((response.session, response.messages))
            }
        }
    }

    pub(crate) async fn delete_session(
        &self,
        app: &AppId,
        id: &str,
    ) -> Result<DeleteSessionOutcome, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.delete_session(app, id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionWrite)?;
                let response = client
                    .request(
                        methods::SESSION_DELETE,
                        json!({ "app": app.as_str(), "id": id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn session_index_status(
        &self,
    ) -> Result<Option<IndexStats>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.session_index_status()?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionRead)?;
                let response = client
                    .request(
                        methods::SESSION_INDEX_STATUS,
                        Value::Null,
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data["stats"].clone())?)
            }
        }
    }

    pub(crate) async fn search_session_index(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.search_session_index(query, limit)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionRead)?;
                let response = client
                    .request(
                        methods::SESSION_SEARCH,
                        json!({ "query": query, "limit": limit }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn build_session_index(&self) -> Result<SyncOutcome, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.sync_session_index()?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionWrite)?;
                let response = client
                    .request(
                        methods::SESSION_INDEX_BUILD,
                        Value::Null,
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn maintain_session_index(
        &self,
        budget_seconds: u64,
    ) -> Result<MaintenanceOutcome, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application
                .maintain_session_index(std::time::Duration::from_secs(budget_seconds))?),
            Self::Remote(client) => {
                client.require_capability(Capability::SessionWrite)?;
                let response = client
                    .request(
                        methods::SESSION_INDEX_MAINTAIN,
                        json!({ "budgetSeconds": budget_seconds }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_session_index(&self) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                application.delete_session_index()?;
                Ok(())
            }
            Self::Remote(client) => {
                client.require_capability(Capability::SessionWrite)?;
                client
                    .request(
                        methods::SESSION_INDEX_DELETE,
                        Value::Null,
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn doctor(&self) -> Result<DoctorReport, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.doctor(false).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::DoctorRun)?;
                let response = client
                    .request(
                        methods::DOCTOR_RUN,
                        json!({ "network": false }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_operations(
        &self,
    ) -> Result<Vec<OperationRecord>, WorkspaceBackendError> {
        match self {
            Self::Local(_) => Ok(ochub_core::runtime::journal::list_operations()?),
            Self::Remote(client) => {
                client.require_capability(Capability::OperationRead)?;
                let response = client
                    .request(methods::OPERATION_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_providers(
        &self,
        app: &AppId,
    ) -> Result<Vec<ProviderListItem>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_providers(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderRead)?;
                let response = client
                    .request(
                        methods::PROVIDER_LIST,
                        json!({ "app": app.as_str() }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn get_provider(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> Result<ProviderDetails, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.get_provider(app, provider_id, false)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderRead)?;
                let response = client
                    .request(
                        methods::PROVIDER_GET,
                        json!({ "app": app.as_str(), "providerId": provider_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn create_provider(
        &self,
        app: &AppId,
        provider: Provider,
        add_to_live: bool,
    ) -> Result<ProviderDetails, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.add_provider(app, provider, add_to_live)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_CREATE,
                        json!({
                            "app": app.as_str(),
                            "provider": provider,
                            "addToLive": add_to_live
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn update_provider(
        &self,
        app: &AppId,
        provider_id: &str,
        patch: Value,
    ) -> Result<ProviderDetails, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let mut provider = serde_json::to_value(
                    application.get_provider(app, provider_id, true)?.provider,
                )?;
                merge_json_patch(&mut provider, &patch);
                Ok(application.update_provider(
                    app,
                    provider_id,
                    serde_json::from_value(provider)?,
                )?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_UPDATE,
                        json!({
                            "app": app.as_str(),
                            "providerId": provider_id,
                            "patch": patch
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_provider(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.delete_provider(app, provider_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                client
                    .request(
                        methods::PROVIDER_DELETE,
                        provider_params(app, provider_id),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn duplicate_provider(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> Result<ProviderDetails, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.duplicate_provider(app, provider_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_DUPLICATE,
                        provider_params(app, provider_id),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn sort_providers(
        &self,
        app: &AppId,
        ids: Vec<String>,
    ) -> Result<Vec<ProviderListItem>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.sort_providers(app, &ids)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_SORT,
                        json!({ "app": app.as_str(), "ids": ids }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn copy_provider(
        &self,
        from_app: &AppId,
        to_app: &AppId,
        provider_id: &str,
    ) -> Result<ProviderDetails, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.copy_provider(from_app, to_app, provider_id)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_COPY,
                        json!({
                            "providerId": provider_id,
                            "fromApp": from_app.as_str(),
                            "toApp": to_app.as_str()
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn import_live_providers(
        &self,
        app: &AppId,
    ) -> Result<usize, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.import_live_providers(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_IMPORT_LIVE,
                        app_params(app),
                        mutation_options(),
                    )
                    .await?;
                Ok(response
                    .data
                    .get("imported")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize)
            }
        }
    }

    pub(crate) async fn seed_official_provider(
        &self,
        app: &AppId,
    ) -> Result<bool, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.seed_official_provider(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_SEED_OFFICIAL,
                        app_params(app),
                        mutation_options(),
                    )
                    .await?;
                Ok(response
                    .data
                    .get("created")
                    .and_then(Value::as_bool)
                    .unwrap_or(false))
            }
        }
    }

    pub(crate) async fn sync_live_provider(
        &self,
        app: &AppId,
    ) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.sync_live_provider(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                client
                    .request(
                        methods::PROVIDER_SYNC_LIVE,
                        app_params(app),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn set_provider_live(
        &self,
        app: &AppId,
        provider_id: &str,
        enabled: bool,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) if enabled => Ok(serde_json::to_value(
                application.add_provider_to_live(app, provider_id)?,
            )?),
            Self::Local(application) => {
                application.remove_provider_from_live(app, provider_id)?;
                Ok(json!({ "removedFromLive": true }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        if enabled {
                            methods::PROVIDER_ADD_TO_LIVE
                        } else {
                            methods::PROVIDER_REMOVE_FROM_LIVE
                        },
                        provider_params(app, provider_id),
                        mutation_options(),
                    )
                    .await?;
                let mut data = response.data;
                if !response.warnings.is_empty()
                    && let Some(object) = data.as_object_mut()
                {
                    object.insert(
                        "warnings".to_string(),
                        serde_json::to_value(response.warnings)?,
                    );
                }
                Ok(data)
            }
        }
    }

    pub(crate) async fn provider_network_operation(
        &self,
        method: &'static str,
        app: &AppId,
        provider_id: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let value = match method {
                    methods::PROVIDER_TEST => serde_json::to_value(
                        application.provider_test(app, provider_id, None).await?,
                    )?,
                    methods::PROVIDER_SPEED_TEST => serde_json::to_value(
                        application
                            .provider_speed_test(app, provider_id, None)
                            .await?,
                    )?,
                    methods::PROVIDER_MODELS => {
                        serde_json::to_value(application.provider_models(app, provider_id).await?)?
                    }
                    methods::PROVIDER_BALANCE => {
                        serde_json::to_value(application.provider_balance(app, provider_id).await?)?
                    }
                    methods::PROVIDER_QUOTA => {
                        serde_json::to_value(application.provider_quota(app, provider_id).await?)?
                    }
                    _ => {
                        return Err(WorkspaceBackendError::from(RemoteClientError::Protocol(
                            format!("unsupported provider network method {method}"),
                        )));
                    }
                };
                Ok(value)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderNetwork)?;
                let response = client
                    .request(
                        method,
                        provider_params(app, provider_id),
                        Default::default(),
                    )
                    .await?;
                let mut data = response.data;
                if !response.warnings.is_empty()
                    && let Some(object) = data.as_object_mut()
                {
                    object.insert(
                        "warnings".to_string(),
                        serde_json::to_value(response.warnings)?,
                    );
                }
                Ok(data)
            }
        }
    }

    pub(crate) async fn provider_endpoints(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.provider_endpoints(app, provider_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderRead)?;
                let response = client
                    .request(
                        methods::PROVIDER_ENDPOINT_LIST,
                        provider_params(app, provider_id),
                        Default::default(),
                    )
                    .await?;
                Ok(response.data)
            }
        }
    }

    pub(crate) async fn mutate_provider_endpoint(
        &self,
        app: &AppId,
        provider_id: &str,
        url: &str,
        add: bool,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) if add => {
                Ok(application.add_provider_endpoint(app, provider_id, url)?)
            }
            Self::Local(application) => {
                Ok(application.remove_provider_endpoint(app, provider_id, url)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        if add {
                            methods::PROVIDER_ENDPOINT_ADD
                        } else {
                            methods::PROVIDER_ENDPOINT_REMOVE
                        },
                        json!({
                            "app": app.as_str(),
                            "providerId": provider_id,
                            "url": url
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(response.data)
            }
        }
    }

    pub(crate) async fn common_config(
        &self,
        app: &AppId,
    ) -> Result<Option<String>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.common_config(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderRead)?;
                let response = client
                    .request(
                        methods::PROVIDER_COMMON_GET,
                        app_params(app),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_common_config(
        &self,
        app: &AppId,
        snippet: String,
    ) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_common_config(app, snippet)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                client
                    .request(
                        methods::PROVIDER_COMMON_SET,
                        json!({ "app": app.as_str(), "snippet": snippet }),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn extract_common_config(
        &self,
        app: &AppId,
    ) -> Result<String, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.extract_common_config(app)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderRead)?;
                let response = client
                    .request(
                        methods::PROVIDER_COMMON_EXTRACT,
                        app_params(app),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn apply_common_config(
        &self,
        app: &AppId,
        provider_ids: Vec<String>,
    ) -> Result<Vec<String>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.apply_common_config(app, &provider_ids)?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_COMMON_APPLY,
                        json!({
                            "app": app.as_str(),
                            "providerIds": provider_ids
                        }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn plan_provider_switch(
        &self,
        app: &AppId,
        provider_id: &str,
        policy: ProviderSwitchPolicy,
    ) -> Result<ProviderSwitchHandle, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let plan = application.preview_provider_switch(app, provider_id)?;
                let revision = revision_for(&plan)?;
                Ok(ProviderSwitchHandle::Local {
                    app: app.clone(),
                    provider_id: provider_id.to_string(),
                    policy,
                    revision,
                    plan,
                })
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ProviderWrite)?;
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Envelope {
                    plan_id: String,
                    revision: String,
                    plan: ProviderSwitchPlan,
                }
                let response = client
                    .request(
                        methods::PROVIDER_SWITCH_PLAN,
                        json!({
                            "app": app.as_str(),
                            "providerId": provider_id,
                            "onDrift": policy_name(policy)
                        }),
                        Default::default(),
                    )
                    .await?;
                for warning in &response.warnings {
                    log::warn!("[Remote] provider switch plan warning: {warning}");
                }
                for event in &response.events {
                    log::debug!("[Remote] provider switch event: {:?}", event);
                }
                let envelope: Envelope = serde_json::from_value(response.data)?;
                if response.revision.as_deref() != Some(envelope.revision.as_str()) {
                    return Err(WorkspaceBackendError::Conflict);
                }
                Ok(ProviderSwitchHandle::Remote {
                    plan_id: envelope.plan_id,
                    revision: envelope.revision,
                    plan: envelope.plan,
                })
            }
        }
    }

    pub(crate) async fn apply_provider_switch(
        &self,
        handle: ProviderSwitchHandle,
    ) -> Result<Value, WorkspaceBackendError> {
        match (self, handle) {
            (
                Self::Local(application),
                ProviderSwitchHandle::Local {
                    app,
                    provider_id,
                    policy,
                    revision,
                    ..
                },
            ) => {
                let refreshed = application.preview_provider_switch(&app, &provider_id)?;
                if revision_for(&refreshed)? != revision {
                    return Err(WorkspaceBackendError::Conflict);
                }
                let result = application.switch_provider(&app, &provider_id, policy)?;
                Ok(json!({
                    "applied": true,
                    "app": app.as_str(),
                    "providerId": provider_id,
                    "drift": result.drift,
                    "warnings": result.warnings
                }))
            }
            (
                Self::Remote(client),
                ProviderSwitchHandle::Remote {
                    plan_id, revision, ..
                },
            ) => {
                client.require_capability(Capability::ProviderWrite)?;
                let response = client
                    .request(
                        methods::PROVIDER_SWITCH_APPLY,
                        json!({ "planId": plan_id }),
                        RemoteRequestOptions {
                            idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
                            expected_revision: Some(revision),
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut data = response.data;
                if !response.warnings.is_empty()
                    && let Some(object) = data.as_object_mut()
                {
                    object.insert(
                        "warnings".to_string(),
                        serde_json::to_value(response.warnings)?,
                    );
                }
                Ok(data)
            }
            _ => Err(WorkspaceBackendError::Conflict),
        }
    }

    pub(crate) async fn proxy_settings(&self) -> Result<ProxySettings, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.proxy_settings(true)),
            Self::Remote(client) => {
                client.require_capability(Capability::ProxyRead)?;
                let response = client
                    .request(methods::PROXY_GET, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn settings(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.settings(true)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SettingsRead)?;
                Ok(client
                    .request(methods::SETTINGS_LIST, Value::Null, Default::default())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn setting(&self, path: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.get_setting(path, true)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SettingsRead)?;
                Ok(client
                    .request(
                        methods::SETTINGS_GET,
                        json!({ "path": path }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn set_setting(
        &self,
        path: &str,
        value: Value,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_setting(path, value)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SettingsWrite)?;
                Ok(client
                    .request(
                        methods::SETTINGS_SET,
                        json!({ "path": path, "value": value }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn unset_setting(&self, path: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.unset_setting(path)?),
            Self::Remote(client) => {
                client.require_capability(Capability::SettingsWrite)?;
                Ok(client
                    .request(
                        methods::SETTINGS_UNSET,
                        json!({ "path": path }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn sync_status(&self, backend: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match backend {
                "webdav" => Ok(application.webdav_sync_status(false)?),
                "s3" => Ok(application.s3_sync_status(false)?),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::SyncRead)?;
                Ok(client
                    .request(
                        methods::SYNC_STATUS,
                        json!({ "backend": backend }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn configure_sync(
        &self,
        backend: &str,
        settings: Value,
        clear_secret: bool,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match backend {
                "webdav" => Ok(application.configure_webdav_sync(
                    serde_json::from_value::<WebDavSyncSettings>(settings)?,
                    !clear_secret,
                )?),
                "s3" => Ok(application.configure_s3_sync(
                    serde_json::from_value::<S3SyncSettings>(settings)?,
                    !clear_secret,
                )?),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::SyncWrite)?;
                Ok(client
                    .request(
                        methods::SYNC_CONFIGURE,
                        json!({
                            "backend": backend,
                            "settings": settings,
                            "clearSecret": clear_secret
                        }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn test_sync(
        &self,
        backend: &str,
        settings: Option<Value>,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match (backend, settings) {
                ("webdav", Some(settings)) => Ok(application
                    .test_webdav_sync_settings(serde_json::from_value(settings)?)
                    .await?),
                ("webdav", None) => Ok(application.test_webdav_sync().await?),
                ("s3", Some(settings)) => Ok(application
                    .test_s3_sync_settings(serde_json::from_value(settings)?)
                    .await?),
                ("s3", None) => Ok(application.test_s3_sync().await?),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::SyncNetwork)?;
                Ok(client
                    .request(
                        methods::SYNC_TEST,
                        json!({ "backend": backend, "settings": settings }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn upload_sync(&self, backend: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match backend {
                "webdav" => Ok(application.upload_webdav_sync().await?),
                "s3" => Ok(application.upload_s3_sync().await?),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::SyncWrite)?;
                Ok(client
                    .request(
                        methods::SYNC_UPLOAD,
                        json!({ "backend": backend }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn download_sync(
        &self,
        backend: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match backend {
                "webdav" => Ok(application.download_webdav_sync().await?.data),
                "s3" => Ok(application.download_s3_sync().await?.data),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::BackupRestore)?;
                Ok(client
                    .request(
                        methods::SYNC_DOWNLOAD,
                        json!({ "backend": backend }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn sync_remote_info(
        &self,
        backend: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match backend {
                "webdav" => Ok(application.webdav_remote_info().await?),
                "s3" => Ok(application.s3_remote_info().await?),
                _ => Err(ApplicationError::InvalidInput("unknown sync backend".into()).into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::SyncNetwork)?;
                Ok(client
                    .request(
                        methods::SYNC_REMOTE_INFO,
                        json!({ "backend": backend }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn list_backups(&self) -> Result<Vec<BackupEntry>, WorkspaceBackendError> {
        match self {
            Self::Local(_) => ochub_core::Database::list_backups()
                .map_err(ApplicationError::from)
                .map_err(WorkspaceBackendError::from),
            Self::Remote(client) => {
                client.require_capability(Capability::BackupRead)?;
                let response = client
                    .request(methods::BACKUP_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn create_backup(
        &self,
        name: Option<&str>,
    ) -> Result<String, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let mut filename = application
                    .state()
                    .db
                    .create_backup_file()
                    .map_err(ApplicationError::from)?;
                if let Some(name) = name {
                    filename = ochub_core::Database::rename_backup(&filename, name)
                        .map_err(ApplicationError::from)?;
                }
                Ok(filename)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::BackupWrite)?;
                let response = client
                    .request(
                        methods::BACKUP_CREATE,
                        json!({ "name": name }),
                        mutation_options(),
                    )
                    .await?;
                response.data["filename"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or(WorkspaceBackendError::from(RemoteClientError::Protocol(
                        "backup.create response is missing filename".to_string(),
                    )))
            }
        }
    }

    pub(crate) async fn rename_backup(
        &self,
        id: &str,
        name: &str,
    ) -> Result<String, WorkspaceBackendError> {
        match self {
            Self::Local(_) => ochub_core::Database::rename_backup(id, name)
                .map_err(ApplicationError::from)
                .map_err(WorkspaceBackendError::from),
            Self::Remote(client) => {
                client.require_capability(Capability::BackupWrite)?;
                let response = client
                    .request(
                        methods::BACKUP_RENAME,
                        json!({ "id": id, "name": name }),
                        mutation_options(),
                    )
                    .await?;
                response.data["filename"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or(WorkspaceBackendError::from(RemoteClientError::Protocol(
                        "backup.rename response is missing filename".to_string(),
                    )))
            }
        }
    }

    pub(crate) async fn restore_backup(&self, id: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let safety = application
                    .state()
                    .db
                    .restore_from_backup(id)
                    .map_err(ApplicationError::from)?;
                Ok(json!({ "restored": id, "safetyBackup": safety }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::BackupRestore)?;
                Ok(client
                    .request(
                        methods::BACKUP_RESTORE,
                        json!({ "id": id }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn delete_backup(&self, id: &str) -> Result<(), WorkspaceBackendError> {
        match self {
            Self::Local(_) => {
                ochub_core::Database::delete_backup(id).map_err(ApplicationError::from)?;
                Ok(())
            }
            Self::Remote(client) => {
                client.require_capability(Capability::BackupWrite)?;
                client
                    .request(
                        methods::BACKUP_DELETE,
                        json!({ "id": id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn export_sql(&self, path: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                application
                    .state()
                    .db
                    .export_sql(std::path::Path::new(path))
                    .map_err(ApplicationError::from)?;
                Ok(json!({ "path": path }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::BackupWrite)?;
                Ok(client
                    .request(
                        methods::BACKUP_EXPORT_SQL,
                        json!({ "path": path }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn import_sql(&self, path: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let safety_backup = application
                    .state()
                    .db
                    .import_sql(std::path::Path::new(path))
                    .map_err(ApplicationError::from)?;
                let sync_warning = ochub_core::services::ProviderService::sync_current_to_live(
                    application.state(),
                )
                .err()
                .map(|error| error.to_string());
                Ok(json!({
                    "imported": path,
                    "safetyBackup": safety_backup,
                    "syncWarning": sync_warning
                }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::BackupRestore)?;
                Ok(client
                    .request(
                        methods::BACKUP_IMPORT_SQL,
                        json!({ "path": path }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn tool_versions(
        &self,
        tools: Option<Vec<String>>,
    ) -> Result<Vec<ToolVersion>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.tool_versions(tools).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::ToolRead)?;
                let response = client
                    .request(
                        methods::TOOL_VERSIONS,
                        json!({ "tools": tools.unwrap_or_default() }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn probe_tool(
        &self,
        tool: &str,
    ) -> Result<Vec<ToolInstallationReport>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.probe_tools(vec![tool.to_string()])?),
            Self::Remote(client) => {
                client.require_capability(Capability::ToolRead)?;
                let response = client
                    .request(
                        methods::TOOL_PROBE,
                        json!({ "tool": tool }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn run_tool_lifecycle(
        &self,
        tool: &str,
        action: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                application
                    .run_tool_lifecycle(vec![tool.to_string()], action)
                    .await?;
                Ok(json!({ "action": action, "tool": tool, "completed": true }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::ToolWrite)?;
                let method = match action {
                    "install" => methods::TOOL_INSTALL,
                    "update" => methods::TOOL_UPDATE,
                    _ => {
                        return Err(ApplicationError::InvalidInput(
                            "tool action must be install or update".into(),
                        )
                        .into());
                    }
                };
                Ok(client
                    .request(method, json!({ "tool": tool }), mutation_options())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn advanced_tool_read(
        &self,
        action: &str,
        params: Value,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match action {
                "env.scan" => Ok(serde_json::to_value(
                    application.scan_environment_conflicts(false)?,
                )?),
                "omo.localFile" => Ok(application.omo_local_file(false)?),
                "omoSlim.localFile" => Ok(application.omo_local_file(true)?),
                "claude.mcp.config" => Ok(application.claude_mcp_config(false)?),
                "claude.mcp.validatePaths" => Ok(application.validate_claude_mcp_paths()?),
                "claude.mcp.validateCommand" => {
                    let command = required_string(&params, "command")?;
                    Ok(json!({
                        "command": command,
                        "valid": ochub_core::mcp::validate_command_in_path(command)
                            .map_err(ApplicationError::from)?
                    }))
                }
                "codex.history.status" => Ok(application.codex_history_status()?),
                "openclaw.health" => Ok(application.openclaw_health()?),
                "openclaw.defaultModel" => Ok(application.openclaw_default_model()?),
                "openclaw.env" => Ok(application.openclaw_env(false)?),
                "openclaw.tools" => Ok(application.openclaw_tools()?),
                "hermes.models" => Ok(application.hermes_models()?),
                "hermes.memory.status" => Ok(application.hermes_memory_status()?),
                "hermes.memory.limits" => Ok(application.hermes_memory_limits()?),
                "hermes.memory.read" => Ok(application.read_hermes_memory("memory")?),
                "hermes.user.read" => Ok(application.read_hermes_memory("user")?),
                _ => Err(ApplicationError::InvalidInput(format!(
                    "unsupported advanced read action: {action}"
                ))
                .into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::ToolRead)?;
                Ok(client
                    .request(
                        methods::TOOL_ADVANCED_READ,
                        json!({ "action": action, "params": params }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn advanced_tool_write(
        &self,
        action: &str,
        params: Value,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => match action {
                "env.clean" => {
                    Ok(application.clean_environment_conflict(required_string(&params, "id")?)?)
                }
                "env.restore" => {
                    Ok(application.restore_environment_backup(required_string(&params, "id")?)?)
                }
                "omo.disable" => Ok(application.disable_omo(false)?),
                "omoSlim.disable" => Ok(application.disable_omo(true)?),
                "claude.plugin.apply" => Ok(application.apply_claude_plugin(false)?),
                "claude.plugin.restore" => Ok(application.restore_claude_plugin()?),
                "claude.onboarding.skip" => Ok(application.set_claude_onboarding(true)?),
                "claude.onboarding.clear" => Ok(application.set_claude_onboarding(false)?),
                "codex.history.restore" => Ok(application.restore_codex_history()?),
                "openclaw.defaultModel.set" => {
                    Ok(application.set_openclaw_default_model(params["value"].clone())?)
                }
                "openclaw.env.set" => Ok(application.set_openclaw_env(params["value"].clone())?),
                "openclaw.tools.set" => {
                    Ok(application.set_openclaw_tools(params["value"].clone())?)
                }
                "hermes.models.set" => Ok(application.set_hermes_models(params["value"].clone())?),
                "hermes.memory.write" => Ok(application
                    .write_hermes_memory("memory", required_string(&params, "content")?)?),
                "hermes.user.write" => Ok(application
                    .write_hermes_memory("user", required_string(&params, "content")?)?),
                "hermes.memory.enable" | "hermes.memory.disable" => {
                    Ok(application
                        .set_hermes_memory_enabled("memory", action.ends_with(".enable"))?)
                }
                "hermes.user.enable" | "hermes.user.disable" => {
                    Ok(application
                        .set_hermes_memory_enabled("user", action.ends_with(".enable"))?)
                }
                _ => Err(ApplicationError::InvalidInput(format!(
                    "unsupported advanced write action: {action}"
                ))
                .into()),
            },
            Self::Remote(client) => {
                client.require_capability(Capability::ToolWrite)?;
                Ok(client
                    .request(
                        methods::TOOL_ADVANCED_WRITE,
                        json!({ "action": action, "params": params }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn update_status(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.update_status()?),
            Self::Remote(client) => {
                client.require_capability(Capability::UpdateRead)?;
                Ok(client
                    .request(methods::UPDATE_STATUS, Value::Null, Default::default())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn check_for_update(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.check_for_update().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::UpdateRead)?;
                Ok(client
                    .request(methods::UPDATE_CHECK, Value::Null, Default::default())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn install_update(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.install_update().await?),
            Self::Remote(client) => {
                client.require_capability(Capability::UpdateInstall)?;
                Ok(client
                    .request(methods::UPDATE_INSTALL, Value::Null, mutation_options())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn data_dir_status(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.data_dir_status()?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataRead)?;
                Ok(client
                    .request(methods::DATA_DIR_SHOW, Value::Null, Default::default())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn set_data_dir(&self, path: &str) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_data_dir(std::path::Path::new(path))?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataWrite)?;
                Ok(client
                    .request(
                        methods::DATA_DIR_SET,
                        json!({ "path": path }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn reset_data_dir(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.reset_data_dir()?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataWrite)?;
                Ok(client
                    .request(methods::DATA_DIR_RESET, Value::Null, mutation_options())
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn detect_ccswitch(
        &self,
    ) -> Result<Option<DetectedSource>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.detect_ccswitch()?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataRead)?;
                let response = client
                    .request(
                        methods::MIGRATE_CCSWITCH_DETECT,
                        Value::Null,
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data["source"].clone())?)
            }
        }
    }

    pub(crate) async fn plan_ccswitch_import(&self) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.plan_ccswitch_import()?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataRead)?;
                Ok(client
                    .request(
                        methods::MIGRATE_CCSWITCH_PLAN,
                        Value::Null,
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn import_ccswitch(&self) -> Result<ImportReport, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.import_ccswitch()?),
            Self::Remote(client) => {
                client.require_capability(Capability::DataImport)?;
                let response = client
                    .request(
                        methods::MIGRATE_CCSWITCH_IMPORT,
                        Value::Null,
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_proxy_settings(
        &self,
        proxy: &ProxySettings,
    ) -> Result<ProxySettings, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.set_proxy_settings(proxy.clone())?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProxyWrite)?;
                let response = client
                    .request(
                        methods::PROXY_SET,
                        json!({ "proxy": proxy }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn test_proxy_settings(
        &self,
        proxy: &ProxySettings,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.test_proxy_settings(proxy.clone()).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::ProxyNetwork)?;
                Ok(client
                    .request(
                        methods::PROXY_TEST,
                        json!({ "proxy": proxy }),
                        Default::default(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn gateway_status(&self) -> Result<GatewayStatus, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(serde_json::from_value(application.gateway_status().await?)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::GatewayRead)?;
                let response = client
                    .request(methods::GATEWAY_STATUS, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn set_gateway_running(
        &self,
        running: bool,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) if running => {
                Ok(serde_json::to_value(application.start_gateway().await?)
                    .map_err(WorkspaceBackendError::Response)?)
            }
            Self::Local(application) => {
                application.stop_gateway().await?;
                Ok(json!({ "stopped": true }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::GatewayLifecycle)?;
                let response = client
                    .request(
                        if running {
                            methods::GATEWAY_START
                        } else {
                            methods::GATEWAY_STOP
                        },
                        Value::Null,
                        RemoteRequestOptions {
                            idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(response.data)
            }
        }
    }

    pub(crate) async fn gateway_connection_info(
        &self,
    ) -> Result<ApplyResult, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(serde_json::from_value(
                application.gateway_connection_info(None)?,
            )?),
            Self::Remote(client) => {
                client.require_capability(Capability::GatewayLifecycle)?;
                let response = client
                    .request(
                        methods::GATEWAY_CONNECTION_INFO,
                        Value::Null,
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn import_provider_as_station(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> Result<ochub_core::gateway::types::GatewayChannel, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.import_provider_as_gateway_channel(app, provider_id)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                let response = client
                    .request(
                        methods::STATION_IMPORT_PROVIDER,
                        json!({ "app": app.as_str(), "providerId": provider_id }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn list_stations(&self) -> Result<Vec<GatewayStation>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.list_gateway_stations()?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationRead)?;
                let response = client
                    .request(methods::STATION_LIST, Value::Null, Default::default())
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn get_station(
        &self,
        station_id: &str,
    ) -> Result<GatewayStation, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.get_gateway_station(station_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationRead)?;
                let response = client
                    .request(
                        methods::STATION_GET,
                        json!({ "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn create_station(
        &self,
        station: &GatewayStation,
    ) -> Result<GatewayStation, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.save_gateway_station(station.clone())?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                let response = client
                    .request(
                        methods::STATION_CREATE,
                        json!({ "station": station }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn update_station(
        &self,
        station_id: &str,
        patch: Value,
    ) -> Result<GatewayStation, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                let current = application.get_gateway_station(station_id)?;
                let mut value = serde_json::to_value(current)?;
                merge_json_patch(&mut value, &patch);
                value["id"] = Value::String(station_id.to_string());
                Ok(application.save_gateway_station(serde_json::from_value(value)?)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                let response = client
                    .request(
                        methods::STATION_UPDATE,
                        json!({ "stationId": station_id, "patch": patch }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn delete_station(
        &self,
        station_id: &str,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                application.delete_gateway_station(station_id)?;
                Ok(json!({ "id": station_id, "deleted": true }))
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                Ok(client
                    .request(
                        methods::STATION_DELETE,
                        json!({ "stationId": station_id }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn set_station_enabled(
        &self,
        station_id: &str,
        enabled: bool,
    ) -> Result<GatewayStation, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.set_gateway_station_enabled(station_id, enabled)?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                let response = client
                    .request(
                        methods::STATION_SET_ENABLED,
                        json!({ "stationId": station_id, "enabled": enabled }),
                        mutation_options(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn probe_station(
        &self,
        station_id: &str,
    ) -> Result<Vec<GatewayEndpointTestResult>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.probe_gateway_station(station_id).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationNetwork)?;
                let response = client
                    .request(
                        methods::STATION_PROBE,
                        json!({ "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn detect_station_dialects(
        &self,
        url: &str,
        api_key: &str,
        station_id: Option<&str>,
    ) -> Result<Vec<Dialect>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.probe_gateway_dialects(url, api_key).await?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationNetwork)?;
                let response = client
                    .request(
                        methods::STATION_DETECT_DIALECTS,
                        json!({ "url": url, "apiKey": api_key, "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn fetch_station_models(
        &self,
        url: &str,
        api_key: &str,
        station_id: Option<&str>,
    ) -> Result<Vec<String>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.gateway_endpoint_models(url, api_key).await?)
            }
            Self::Remote(client) => {
                client.require_capability(Capability::StationNetwork)?;
                let response = client
                    .request(
                        methods::STATION_FETCH_MODELS,
                        json!({ "url": url, "apiKey": api_key, "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn test_station_endpoint(
        &self,
        url: &str,
        api_key: &str,
        station_id: Option<&str>,
    ) -> Result<GatewayEndpointTestResult, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.test_gateway_endpoint(url, api_key).await?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationNetwork)?;
                let response = client
                    .request(
                        methods::STATION_TEST_ENDPOINT,
                        json!({ "url": url, "apiKey": api_key, "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn station_models(
        &self,
        station_id: &str,
    ) -> Result<Vec<String>, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.gateway_station_models(station_id)?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationRead)?;
                let response = client
                    .request(
                        methods::STATION_MODELS,
                        json!({ "stationId": station_id }),
                        Default::default(),
                    )
                    .await?;
                Ok(serde_json::from_value(response.data)?)
            }
        }
    }

    pub(crate) async fn select_station(
        &self,
        station_id: &str,
        app: &AppId,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(serde_json::to_value(
                application.select_gateway_station(station_id, app)?,
            )?),
            Self::Remote(_) => {
                self.station_app_mutation(
                    methods::STATION_SELECT,
                    json!({ "stationId": station_id, "app": app.as_str() }),
                )
                .await
            }
        }
    }

    pub(crate) async fn apply_station(
        &self,
        station_id: &str,
        app: &AppId,
        policy: GatewayAppModelPolicy,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(serde_json::to_value(
                application.apply_gateway_station(station_id, app, Some(policy))?,
            )?),
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                Ok(client
                    .request(
                        methods::STATION_APPLY,
                        json!({
                            "stationId": station_id,
                            "app": app.as_str(),
                            "policy": policy
                        }),
                        mutation_options(),
                    )
                    .await?
                    .data)
            }
        }
    }

    pub(crate) async fn disconnect_station(
        &self,
        app: &AppId,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => Ok(application.disconnect_gateway_from_app(app)?),
            Self::Remote(_) => {
                self.station_app_mutation(
                    methods::STATION_DISCONNECT,
                    json!({ "app": app.as_str() }),
                )
                .await
            }
        }
    }

    pub(crate) async fn station_connection_info(
        &self,
        station_id: &str,
        app: &AppId,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Local(application) => {
                Ok(application.gateway_station_connection_info(station_id, app)?)
            }
            Self::Remote(_) => {
                self.station_app_mutation(
                    methods::STATION_CONNECTION_INFO,
                    json!({ "stationId": station_id, "app": app.as_str() }),
                )
                .await
            }
        }
    }

    async fn station_app_mutation(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, WorkspaceBackendError> {
        match self {
            Self::Remote(client) => {
                client.require_capability(Capability::StationWrite)?;
                Ok(client
                    .request(method, params, mutation_options())
                    .await?
                    .data)
            }
            Self::Local(_) => Err(WorkspaceBackendError::Conflict),
        }
    }
}

fn policy_name(policy: ProviderSwitchPolicy) -> &'static str {
    match policy {
        ProviderSwitchPolicy::Abort => "abort",
        ProviderSwitchPolicy::Preserve => "preserve",
        ProviderSwitchPolicy::Discard => "discard",
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ApplicationError> {
    value[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApplicationError::InvalidInput(format!("{field} is required")))
}

fn revision_for<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let digest = Sha256::digest(serde_json::to_vec(value)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[allow(dead_code)]
fn app_params(app: &AppId) -> Value {
    json!({ "app": app.as_str() })
}

#[allow(dead_code)]
fn provider_params(app: &AppId, provider_id: &str) -> Value {
    json!({ "app": app.as_str(), "providerId": provider_id })
}

fn usage_filter_params(filter: &UsageFilter) -> Value {
    json!({
        "from": filter.start,
        "to": filter.end,
        "app": filter.app,
        "provider": filter.provider,
        "model": filter.model
    })
}

#[allow(dead_code)]
fn mutation_options() -> RemoteRequestOptions {
    RemoteRequestOptions {
        idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    }
}

#[allow(dead_code)]
fn merge_json_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Default::default());
    }
    let target = target
        .as_object_mut()
        .expect("target initialized as a JSON object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_json_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}
