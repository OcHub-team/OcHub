//! Managed-account command surface.
//!
//! Faithful port of cc-switch `commands/auth.rs`, `commands/copilot.rs`, and
//! `commands/codex_oauth.rs`, with Tauri removed. The Tauri-managed
//! `CopilotAuthState` / `CodexOAuthState` are replaced by the managers held on
//! [`crate::app_state::AppState`]; each function takes `&AppState` (or the bare
//! manager) and returns `Result<_, String>` exactly as the original commands.
//!
//! Three account providers are supported:
//! - generic managed-account API (`auth_*`) dispatching by `auth_provider`
//! - GitHub Copilot OAuth (`copilot_*`)
//! - Codex / ChatGPT OAuth quota + models (`get_codex_oauth_*`)

use crate::app_state::AppState;
use crate::managed_auth::codex_oauth_auth::CodexOAuthError;
use crate::managed_auth::copilot_auth::{
    CopilotAuthError, CopilotAuthStatus, CopilotModel, CopilotUsageResponse, GitHubAccount,
    GitHubDeviceCodeResponse,
};
use crate::services::model_fetch::FetchedModel;
use crate::services::subscription::{query_codex_quota, CredentialStatus, SubscriptionQuota};

const AUTH_PROVIDER_GITHUB_COPILOT: &str = "github_copilot";
const AUTH_PROVIDER_CODEX_OAUTH: &str = "codex_oauth";

// ==================== Generic managed-account DTOs ====================

#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagedAuthAccount {
    pub id: String,
    pub provider: String,
    pub login: String,
    pub avatar_url: Option<String>,
    pub authenticated_at: i64,
    pub is_default: bool,
    pub github_domain: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagedAuthStatus {
    pub provider: String,
    pub authenticated: bool,
    pub default_account_id: Option<String>,
    pub migration_error: Option<String>,
    pub accounts: Vec<ManagedAuthAccount>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagedAuthDeviceCodeResponse {
    pub provider: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

fn ensure_auth_provider(auth_provider: &str) -> Result<&'static str, String> {
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => Ok(AUTH_PROVIDER_GITHUB_COPILOT),
        AUTH_PROVIDER_CODEX_OAUTH => Ok(AUTH_PROVIDER_CODEX_OAUTH),
        _ => Err(format!("Unsupported auth provider: {auth_provider}")),
    }
}

fn map_account(
    provider: &str,
    account: GitHubAccount,
    default_account_id: Option<&str>,
) -> ManagedAuthAccount {
    ManagedAuthAccount {
        is_default: default_account_id == Some(account.id.as_str()),
        id: account.id,
        provider: provider.to_string(),
        login: account.login,
        avatar_url: account.avatar_url,
        authenticated_at: account.authenticated_at,
        github_domain: account.github_domain,
    }
}

fn map_device_code_response(
    provider: &str,
    response: GitHubDeviceCodeResponse,
) -> ManagedAuthDeviceCodeResponse {
    ManagedAuthDeviceCodeResponse {
        provider: provider.to_string(),
        device_code: response.device_code,
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        expires_in: response.expires_in,
        interval: response.interval,
    }
}

// ==================== Generic managed-account API ====================

pub async fn auth_start_login(
    state: &AppState,
    auth_provider: &str,
    github_domain: Option<&str>,
) -> Result<ManagedAuthDeviceCodeResponse, String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.read().await;
            let response = auth_manager
                .start_device_flow(github_domain)
                .await
                .map_err(|e| e.to_string())?;
            Ok(map_device_code_response(auth_provider, response))
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.read().await;
            let response = auth_manager
                .start_device_flow()
                .await
                .map_err(|e| e.to_string())?;
            Ok(map_device_code_response(auth_provider, response))
        }
        _ => unreachable!(),
    }
}

pub async fn auth_poll_for_account(
    state: &AppState,
    auth_provider: &str,
    device_code: &str,
    github_domain: Option<&str>,
) -> Result<Option<ManagedAuthAccount>, String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.write().await;
            match auth_manager
                .poll_for_token(device_code, github_domain)
                .await
            {
                Ok(account) => {
                    let default_account_id = auth_manager.get_status().await.default_account_id;
                    Ok(account.map(|account| {
                        map_account(auth_provider, account, default_account_id.as_deref())
                    }))
                }
                Err(CopilotAuthError::AuthorizationPending) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.write().await;
            match auth_manager.poll_for_token(device_code).await {
                Ok(account) => {
                    let default_account_id = auth_manager.get_status().await.default_account_id;
                    Ok(account.map(|account| {
                        map_account(auth_provider, account, default_account_id.as_deref())
                    }))
                }
                Err(CodexOAuthError::AuthorizationPending) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }
        _ => unreachable!(),
    }
}

pub async fn auth_list_accounts(
    state: &AppState,
    auth_provider: &str,
) -> Result<Vec<ManagedAuthAccount>, String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.read().await;
            let status = auth_manager.get_status().await;
            let default_account_id = status.default_account_id.clone();
            Ok(status
                .accounts
                .into_iter()
                .map(|account| map_account(auth_provider, account, default_account_id.as_deref()))
                .collect())
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.read().await;
            let status = auth_manager.get_status().await;
            let default_account_id = status.default_account_id.clone();
            Ok(status
                .accounts
                .into_iter()
                .map(|account| map_account(auth_provider, account, default_account_id.as_deref()))
                .collect())
        }
        _ => unreachable!(),
    }
}

pub async fn auth_get_status(
    state: &AppState,
    auth_provider: &str,
) -> Result<ManagedAuthStatus, String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.read().await;
            let status = auth_manager.get_status().await;
            let default_account_id = status.default_account_id.clone();
            Ok(ManagedAuthStatus {
                provider: auth_provider.to_string(),
                authenticated: status.authenticated,
                default_account_id: default_account_id.clone(),
                migration_error: status.migration_error,
                accounts: status
                    .accounts
                    .into_iter()
                    .map(|account| {
                        map_account(auth_provider, account, default_account_id.as_deref())
                    })
                    .collect(),
            })
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.read().await;
            let status = auth_manager.get_status().await;
            let default_account_id = status.default_account_id.clone();
            Ok(ManagedAuthStatus {
                provider: auth_provider.to_string(),
                authenticated: status.authenticated,
                default_account_id: default_account_id.clone(),
                migration_error: None,
                accounts: status
                    .accounts
                    .into_iter()
                    .map(|account| {
                        map_account(auth_provider, account, default_account_id.as_deref())
                    })
                    .collect(),
            })
        }
        _ => unreachable!(),
    }
}

pub async fn auth_remove_account(
    state: &AppState,
    auth_provider: &str,
    account_id: &str,
) -> Result<(), String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.write().await;
            auth_manager
                .remove_account(account_id)
                .await
                .map_err(|e| e.to_string())
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.write().await;
            auth_manager
                .remove_account(account_id)
                .await
                .map_err(|e| e.to_string())
        }
        _ => unreachable!(),
    }
}

pub async fn auth_set_default_account(
    state: &AppState,
    auth_provider: &str,
    account_id: &str,
) -> Result<(), String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.write().await;
            auth_manager
                .set_default_account(account_id)
                .await
                .map_err(|e| e.to_string())
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.write().await;
            auth_manager
                .set_default_account(account_id)
                .await
                .map_err(|e| e.to_string())
        }
        _ => unreachable!(),
    }
}

pub async fn auth_logout(state: &AppState, auth_provider: &str) -> Result<(), String> {
    let auth_provider = ensure_auth_provider(auth_provider)?;
    match auth_provider {
        AUTH_PROVIDER_GITHUB_COPILOT => {
            let auth_manager = state.copilot_auth.write().await;
            auth_manager.clear_auth().await.map_err(|e| e.to_string())
        }
        AUTH_PROVIDER_CODEX_OAUTH => {
            let auth_manager = state.codex_oauth.write().await;
            auth_manager.clear_auth().await.map_err(|e| e.to_string())
        }
        _ => unreachable!(),
    }
}

// ==================== GitHub Copilot command surface ====================
// Ported from cc-switch `commands/copilot.rs`.

pub async fn copilot_start_device_flow(
    state: &AppState,
    github_domain: Option<&str>,
) -> Result<GitHubDeviceCodeResponse, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager
        .start_device_flow(github_domain)
        .await
        .map_err(|e| e.to_string())
}

/// Poll for OAuth token (backward-compatible: returns `true` when authorized).
pub async fn copilot_poll_for_auth(
    state: &AppState,
    device_code: &str,
    github_domain: Option<&str>,
) -> Result<bool, String> {
    let auth_manager = state.copilot_auth.write().await;
    match auth_manager
        .poll_for_token(device_code, github_domain)
        .await
    {
        Ok(Some(_account)) => {
            log::info!("[CopilotAuth] 用户已授权");
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(CopilotAuthError::AuthorizationPending) => Ok(false),
        Err(e) => {
            log::error!("[CopilotAuth] 轮询失败: {e}");
            Err(e.to_string())
        }
    }
}

/// Poll for OAuth token (multi-account: returns the newly added account).
pub async fn copilot_poll_for_account(
    state: &AppState,
    device_code: &str,
    github_domain: Option<&str>,
) -> Result<Option<GitHubAccount>, String> {
    let auth_manager = state.copilot_auth.write().await;
    match auth_manager
        .poll_for_token(device_code, github_domain)
        .await
    {
        Ok(account) => Ok(account),
        Err(CopilotAuthError::AuthorizationPending) => Ok(None),
        Err(e) => {
            log::error!("[CopilotAuth] 轮询失败: {e}");
            Err(e.to_string())
        }
    }
}

pub async fn copilot_list_accounts(state: &AppState) -> Result<Vec<GitHubAccount>, String> {
    let auth_manager = state.copilot_auth.read().await;
    Ok(auth_manager.list_accounts().await)
}

pub async fn copilot_remove_account(state: &AppState, account_id: &str) -> Result<(), String> {
    let auth_manager = state.copilot_auth.write().await;
    auth_manager
        .remove_account(account_id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn copilot_set_default_account(state: &AppState, account_id: &str) -> Result<(), String> {
    let auth_manager = state.copilot_auth.write().await;
    auth_manager
        .set_default_account(account_id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn copilot_get_auth_status(state: &AppState) -> Result<CopilotAuthStatus, String> {
    let auth_manager = state.copilot_auth.read().await;
    Ok(auth_manager.get_status().await)
}

pub async fn copilot_is_authenticated(state: &AppState) -> Result<bool, String> {
    let auth_manager = state.copilot_auth.read().await;
    Ok(auth_manager.is_authenticated().await)
}

pub async fn copilot_logout(state: &AppState) -> Result<(), String> {
    let auth_manager = state.copilot_auth.write().await;
    auth_manager.clear_auth().await.map_err(|e| e.to_string())
}

/// Get a valid Copilot token (backward-compatible: default account).
pub async fn copilot_get_token(state: &AppState) -> Result<String, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager
        .get_valid_token()
        .await
        .map_err(|e| e.to_string())
}

pub async fn copilot_get_token_for_account(
    state: &AppState,
    account_id: &str,
) -> Result<String, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager
        .get_valid_token_for_account(account_id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn copilot_get_models(state: &AppState) -> Result<Vec<CopilotModel>, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager.fetch_models().await.map_err(|e| e.to_string())
}

pub async fn copilot_get_models_for_account(
    state: &AppState,
    account_id: &str,
) -> Result<Vec<CopilotModel>, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager
        .fetch_models_for_account(account_id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn copilot_get_usage(state: &AppState) -> Result<CopilotUsageResponse, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager.fetch_usage().await.map_err(|e| e.to_string())
}

pub async fn copilot_get_usage_for_account(
    state: &AppState,
    account_id: &str,
) -> Result<CopilotUsageResponse, String> {
    let auth_manager = state.copilot_auth.read().await;
    auth_manager
        .fetch_usage_for_account(account_id)
        .await
        .map_err(|e| e.to_string())
}

// ==================== Codex / ChatGPT OAuth command surface ====================
// Ported from cc-switch `commands/codex_oauth.rs`.

/// Query Codex OAuth (ChatGPT Plus/Pro) subscription quota.
///
/// - Falls back to the manager's default account when `account_id` is `None`.
/// - Returns `not_found` when there is no account (the UI silently skips it).
pub async fn get_codex_oauth_quota(
    state: &AppState,
    account_id: Option<String>,
) -> Result<SubscriptionQuota, String> {
    let manager = state.codex_oauth.read().await;

    // Resolve account: explicit > default > none (not_found).
    let resolved = match account_id {
        Some(id) => Some(id),
        None => manager.default_account_id().await,
    };
    let Some(id) = resolved else {
        return Ok(SubscriptionQuota::not_found("codex_oauth"));
    };

    // Acquire (auto-refreshing) access_token.
    let token = match manager.get_valid_token_for_account(&id).await {
        Ok(t) => t,
        Err(e) => {
            return Ok(SubscriptionQuota::error(
                "codex_oauth",
                CredentialStatus::Expired,
                format!("Codex OAuth token unavailable: {e}"),
            ));
        }
    };

    Ok(query_codex_quota(
        &token,
        Some(&id),
        "codex_oauth",
        "Codex OAuth access token expired or rejected. Please re-login via OCHub.",
    )
    .await)
}

/// Get Codex OAuth (ChatGPT Plus/Pro) available models.
pub async fn get_codex_oauth_models(
    state: &AppState,
    account_id: Option<String>,
) -> Result<Vec<FetchedModel>, String> {
    let manager = state.codex_oauth.read().await;
    let resolved = match account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => manager.default_account_id().await,
    };
    let Some(id) = resolved else {
        return Err("No ChatGPT account available".to_string());
    };

    let token = manager
        .get_valid_token_for_account(&id)
        .await
        .map_err(|e| format!("Codex OAuth token unavailable: {e}"))?;

    crate::services::codex_oauth_models::fetch_models_with_token(&token, &id).await
}
