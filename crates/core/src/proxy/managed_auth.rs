//! Managed-account auth resolution for proxy forwarding.
//!
//! Provider adapters return placeholder auth for GitHub Copilot and Codex
//! OAuth. This module replaces those placeholders with fresh tokens from the
//! account managers held by [`ProxyState`], and exposes the small set of
//! account-scoped headers/endpoints the forwarders need.

use http::{HeaderName, HeaderValue};

use crate::model::Provider;
use crate::proxy::error::ProxyError;
use crate::proxy::providers::{AuthInfo, AuthStrategy};

use super::server::ProxyState;

#[derive(Debug, Clone)]
pub struct ResolvedAuth {
    pub auth: AuthInfo,
    pub codex_account_id: Option<String>,
    pub is_codex_oauth: bool,
}

pub async fn resolve_auth_info(
    state: &ProxyState,
    provider: &Provider,
    auth: AuthInfo,
) -> Result<ResolvedAuth, ProxyError> {
    match auth.strategy {
        AuthStrategy::GitHubCopilot => resolve_copilot_auth(state, provider).await,
        AuthStrategy::CodexOAuth => resolve_codex_oauth(state, provider).await,
        _ => Ok(ResolvedAuth {
            auth,
            codex_account_id: None,
            is_codex_oauth: false,
        }),
    }
}

pub async fn copilot_api_endpoint(state: &ProxyState, provider: &Provider) -> String {
    let account_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("github_copilot"));
    let manager = state.copilot_auth.read().await;
    match account_id.as_deref() {
        Some(id) => manager.get_api_endpoint(id).await,
        None => manager.get_default_api_endpoint().await,
    }
}

pub async fn codex_default_account_id(state: &ProxyState) -> Option<String> {
    state.codex_oauth.read().await.default_account_id().await
}

pub fn codex_oauth_session_headers(session_id: &str) -> Vec<(HeaderName, HeaderValue)> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Vec::new();
    }

    let mut headers = Vec::new();
    if let Ok(value) = HeaderValue::from_str(session_id) {
        headers.push((HeaderName::from_static("session_id"), value.clone()));
        headers.push((HeaderName::from_static("x-client-request-id"), value));
    }

    let window_id = format!("{session_id}:0");
    if let Ok(value) = HeaderValue::from_str(&window_id) {
        headers.push((HeaderName::from_static("x-codex-window-id"), value));
    }

    headers
}

async fn resolve_copilot_auth(
    state: &ProxyState,
    provider: &Provider,
) -> Result<ResolvedAuth, ProxyError> {
    let account_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("github_copilot"));
    let manager = state.copilot_auth.read().await;
    let token = match account_id.as_deref() {
        Some(id) => manager.get_valid_token_for_account(id).await,
        None => manager.get_valid_token().await,
    }
    .map_err(|error| ProxyError::AuthError(format!("GitHub Copilot 认证失败: {error}")))?;

    Ok(ResolvedAuth {
        auth: AuthInfo::new(token, AuthStrategy::GitHubCopilot),
        codex_account_id: None,
        is_codex_oauth: false,
    })
}

async fn resolve_codex_oauth(
    state: &ProxyState,
    provider: &Provider,
) -> Result<ResolvedAuth, ProxyError> {
    let account_id = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"));
    let manager = state.codex_oauth.read().await;
    let token = match account_id.as_deref() {
        Some(id) => manager.get_valid_token_for_account(id).await,
        None => manager.get_valid_token().await,
    }
    .map_err(|error| ProxyError::AuthError(format!("Codex OAuth 认证失败: {error}")))?;
    let codex_account_id = match account_id {
        Some(id) => Some(id),
        None => manager.default_account_id().await,
    };

    Ok(ResolvedAuth {
        auth: AuthInfo::new(token, AuthStrategy::CodexOAuth),
        codex_account_id,
        is_codex_oauth: true,
    })
}
