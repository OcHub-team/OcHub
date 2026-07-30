use std::sync::Arc;

use ochub_core::AppId;
use ochub_core::application::{
    AppSummary, Application, ApplicationError, DoctorReport, ProviderDetails, ProviderListItem,
    ProviderSwitchPlan, ProviderSwitchPolicy, StatusSummary,
};
use ochub_core::gateway::GatewayStatus;
use ochub_core::runtime::journal::OperationRecord;
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
    Remote(#[from] RemoteClientError),
    #[error("remote response has an invalid shape: {0}")]
    Response(#[from] serde_json::Error),
    #[error("workspace state changed after the plan was created")]
    Conflict,
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
                Ok(response.data)
            }
            _ => Err(WorkspaceBackendError::Conflict),
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
}

fn policy_name(policy: ProviderSwitchPolicy) -> &'static str {
    match policy {
        ProviderSwitchPolicy::Abort => "abort",
        ProviderSwitchPolicy::Preserve => "preserve",
        ProviderSwitchPolicy::Discard => "discard",
    }
}

fn revision_for<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let digest = Sha256::digest(serde_json::to_vec(value)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}
