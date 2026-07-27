//! Transport-neutral application use cases shared by the GUI, CLI and server.
//!
//! This layer deliberately speaks in open [`AppId`](crate::AppId) values and
//! serializable DTOs.  Adapters decide how to render results or collect user
//! input; business operations and safety decisions stay here.

mod advanced;
mod apps;
mod auth;
mod declarative;
mod deeplink;
mod dto;
mod error;
mod gateway;
mod mcp;
mod migration;
mod plugins;
mod provider_ops;
mod providers;
mod sessions;
mod settings;
mod skills;
mod sync;
mod system;
mod theme;
mod update;
mod usage;

use std::sync::Arc;

use crate::{AppState, Database};

pub use declarative::{DeclarativeAction, DeclarativeDocument, DeclarativePlan};
pub use dto::{
    AppModeDto, AppSummary, ConfigFieldDto, ConfigFieldKindDto, ConfigSchemaDto, ConfigSectionDto,
    DoctorCheck, DoctorReport, OperationOutcome, PluginDetails, PluginSummary, ProviderDetails,
    ProviderListItem, ProviderSwitchPlan, StatusSummary, UsageFilter, UsageLimitItem,
};
pub use error::{ApplicationError, ApplicationResult};
pub use gateway::GatewayStation;
pub use providers::{redact_json, ProviderSwitchPolicy};
pub use skills::{parse_skill_repo_spec, parse_skill_source};

/// Options controlling one application-layer runtime.
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    /// Run idempotent startup discovery and seeding.
    pub bootstrap: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self { bootstrap: true }
    }
}

/// A transport-neutral handle to all OcHub use cases.
#[derive(Clone)]
pub struct Application {
    state: Arc<AppState>,
}

impl Application {
    /// Open the configured OcHub data store without importing cc-switch.
    pub fn open(options: OpenOptions) -> ApplicationResult<Self> {
        crate::app_store::refresh_app_config_dir_override();
        let db = Arc::new(Database::init()?);
        let state = Arc::new(AppState::new(db));
        if options.bootstrap {
            state.bootstrap();
        } else {
            for error in crate::plugin::load_and_register_user_plugins() {
                log::warn!(
                    "failed to load user plugin {}: {}",
                    error.path,
                    error.message
                );
            }
        }
        Ok(Self { state })
    }

    /// Build the facade around an already-owned in-process state.
    pub fn from_state(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    pub async fn status(&self) -> ApplicationResult<StatusSummary> {
        let apps = self.list_apps()?;
        let gateway = self.state.gateway.status().await;
        Ok(StatusSummary {
            version: env!("CARGO_PKG_VERSION").to_string(),
            data_dir: crate::paths::get_app_config_dir()
                .to_string_lossy()
                .into_owned(),
            database_path: crate::paths::get_database_path()
                .to_string_lossy()
                .into_owned(),
            enabled_apps: apps.iter().filter(|app| app.enabled).count(),
            registered_apps: apps.len(),
            gateway,
        })
    }
}
