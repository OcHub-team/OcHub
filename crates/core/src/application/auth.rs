use serde_json::{Value, json};

use crate::application::{Application, ApplicationError, ApplicationResult};
use crate::model::{AuthBinding, AuthBindingSource};
use crate::services::auth;
use crate::{AppId, AppType};

const COPILOT: &str = "github_copilot";
const CODEX: &str = "codex_oauth";

impl Application {
    pub async fn managed_auth_status(&self, provider: &str) -> ApplicationResult<Value> {
        to_value(
            auth::auth_get_status(&self.state, provider)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn managed_auth_login(
        &self,
        provider: &str,
        github_domain: Option<&str>,
    ) -> ApplicationResult<Value> {
        to_value(
            auth::auth_start_login(&self.state, provider, github_domain)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn managed_auth_poll(
        &self,
        provider: &str,
        device_code: &str,
        github_domain: Option<&str>,
    ) -> ApplicationResult<Value> {
        let account =
            auth::auth_poll_for_account(&self.state, provider, device_code, github_domain)
                .await
                .map_err(map_auth_error)?;
        Ok(json!({
            "pending": account.is_none(),
            "account": account
        }))
    }

    pub async fn managed_auth_accounts(&self, provider: &str) -> ApplicationResult<Value> {
        to_value(
            auth::auth_list_accounts(&self.state, provider)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn set_default_managed_auth_account(
        &self,
        provider: &str,
        account_id: &str,
    ) -> ApplicationResult<()> {
        auth::auth_set_default_account(&self.state, provider, account_id)
            .await
            .map_err(map_auth_error)
    }

    pub async fn remove_managed_auth_account(
        &self,
        provider: &str,
        account_id: &str,
    ) -> ApplicationResult<()> {
        auth::auth_remove_account(&self.state, provider, account_id)
            .await
            .map_err(map_auth_error)
    }

    pub async fn logout_managed_auth(&self, provider: &str) -> ApplicationResult<()> {
        auth::auth_logout(&self.state, provider)
            .await
            .map_err(map_auth_error)
    }

    pub async fn copilot_token(&self, account_id: Option<&str>) -> ApplicationResult<String> {
        match account_id {
            Some(account_id) => auth::copilot_get_token_for_account(&self.state, account_id)
                .await
                .map_err(map_auth_error),
            None => auth::copilot_get_token(&self.state)
                .await
                .map_err(map_auth_error),
        }
    }

    pub async fn copilot_models(&self, account_id: Option<&str>) -> ApplicationResult<Value> {
        match account_id {
            Some(account_id) => to_value(
                auth::copilot_get_models_for_account(&self.state, account_id)
                    .await
                    .map_err(map_auth_error)?,
            ),
            None => to_value(
                auth::copilot_get_models(&self.state)
                    .await
                    .map_err(map_auth_error)?,
            ),
        }
    }

    pub async fn copilot_usage(&self, account_id: Option<&str>) -> ApplicationResult<Value> {
        match account_id {
            Some(account_id) => to_value(
                auth::copilot_get_usage_for_account(&self.state, account_id)
                    .await
                    .map_err(map_auth_error)?,
            ),
            None => to_value(
                auth::copilot_get_usage(&self.state)
                    .await
                    .map_err(map_auth_error)?,
            ),
        }
    }

    pub async fn codex_oauth_models(&self, account_id: Option<String>) -> ApplicationResult<Value> {
        to_value(
            auth::get_codex_oauth_models(&self.state, account_id)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn codex_oauth_quota(&self, account_id: Option<String>) -> ApplicationResult<Value> {
        to_value(
            auth::get_codex_oauth_quota(&self.state, account_id)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn list_auth_bindings(&self) -> ApplicationResult<Vec<Value>> {
        let mut bindings = Vec::new();
        for app in AppType::all() {
            for provider in self.state.db.get_all_providers(app.as_str())?.into_values() {
                if let Some(binding) = provider.meta.and_then(|meta| meta.auth_binding) {
                    bindings.push(json!({
                        "app": app.as_str(),
                        "providerId": provider.id,
                        "providerName": provider.name,
                        "binding": binding
                    }));
                }
            }
        }
        Ok(bindings)
    }

    pub async fn set_auth_binding(
        &self,
        app: &AppId,
        provider_id: &str,
        account_id: &str,
    ) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "auth.binding")?;
        let mut provider = self
            .state
            .db
            .get_provider_by_id(provider_id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.to_string(),
            })?;
        let auth_provider = resolve_account_provider(&self.state, account_id).await?;
        provider
            .meta
            .get_or_insert_with(Default::default)
            .auth_binding = Some(AuthBinding {
            source: AuthBindingSource::ManagedAccount,
            auth_provider: Some(auth_provider.to_string()),
            account_id: Some(account_id.to_string()),
        });
        self.state.db.save_provider(app_type.as_str(), &provider)?;
        Ok(json!({
            "app": app,
            "providerId": provider_id,
            "accountId": account_id,
            "authProvider": auth_provider
        }))
    }

    pub fn remove_auth_binding(&self, app: &AppId, provider_id: &str) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "auth.binding")?;
        let mut provider = self
            .state
            .db
            .get_provider_by_id(provider_id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.to_string(),
            })?;
        if let Some(meta) = provider.meta.as_mut() {
            meta.auth_binding = None;
            meta.github_account_id = None;
        }
        self.state.db.save_provider(app_type.as_str(), &provider)?;
        Ok(json!({
            "app": app,
            "providerId": provider_id,
            "removed": true
        }))
    }

    pub async fn subscription_quota(&self, tool: &str) -> ApplicationResult<Value> {
        to_value(
            crate::services::subscription::get_subscription_quota(tool)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub async fn coding_plan_quota(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "quota.coding-plan")?;
        let provider = self
            .state
            .db
            .get_provider_by_id(provider_id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.to_string(),
            })?;
        let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
        to_value(
            crate::services::coding_plan::get_coding_plan_quota(&base_url, &api_key)
                .await
                .map_err(map_auth_error)?,
        )
    }

    pub fn claude_desktop_status(&self) -> ApplicationResult<Value> {
        to_value(crate::apps::claude_desktop::get_status(&self.state.db)?)
    }

    pub fn ensure_claude_desktop_official(&self) -> ApplicationResult<bool> {
        Ok(self
            .state
            .db
            .ensure_official_seed_by_id("claude-desktop-official", AppType::ClaudeDesktop)?)
    }

    pub fn import_claude_desktop_from_claude(&self) -> ApplicationResult<usize> {
        let claude = self.state.db.get_all_providers(AppType::Claude.as_str())?;
        let desktop = self
            .state
            .db
            .get_all_providers(AppType::ClaudeDesktop.as_str())?;
        let mut imported = 0;
        for provider in claude.values() {
            if desktop.contains_key(&provider.id)
                || !crate::apps::claude_desktop::is_compatible_direct_provider(provider)
                || crate::apps::claude_desktop::validate_direct_provider(provider).is_err()
            {
                continue;
            }
            self.state
                .db
                .save_provider(AppType::ClaudeDesktop.as_str(), provider)?;
            imported += 1;
        }
        let _ = self.ensure_claude_desktop_official()?;
        Ok(imported)
    }
}

async fn resolve_account_provider(
    state: &crate::AppState,
    account_id: &str,
) -> ApplicationResult<&'static str> {
    let copilot = auth::auth_list_accounts(state, COPILOT)
        .await
        .map_err(map_auth_error)?
        .into_iter()
        .any(|account| account.id == account_id);
    let codex = auth::auth_list_accounts(state, CODEX)
        .await
        .map_err(map_auth_error)?
        .into_iter()
        .any(|account| account.id == account_id);
    match (copilot, codex) {
        (true, false) => Ok(COPILOT),
        (false, true) => Ok(CODEX),
        (false, false) => Err(ApplicationError::NotFound {
            kind: "managed-auth-account",
            id: account_id.to_string(),
        }),
        (true, true) => Err(ApplicationError::InvalidInput(format!(
            "account id {account_id} exists in more than one auth provider"
        ))),
    }
}

fn builtin_app(app: &AppId, capability: &'static str) -> ApplicationResult<AppType> {
    AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
        app: app.to_string(),
        capability,
    })
}

fn to_value(value: impl serde::Serialize) -> ApplicationResult<Value> {
    serde_json::to_value(value)
        .map_err(|source| ApplicationError::Core(crate::AppError::JsonSerialize { source }))
}

fn map_auth_error(error: String) -> ApplicationError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("pending") {
        ApplicationError::UpstreamRejected(error)
    } else if lower.contains("network")
        || lower.contains("request")
        || lower.contains("connect")
        || lower.contains("timeout")
    {
        ApplicationError::NetworkUnavailable(error)
    } else if lower.contains("not found") || lower.contains("not authenticated") {
        ApplicationError::NotFound {
            kind: "managed-auth",
            id: error,
        }
    } else {
        ApplicationError::UpstreamRejected(error)
    }
}
