//! Provider reachability checks.
//!
//! Ported from cc-switch `services/stream_check.rs`. This probes the provider
//! base URL only: any HTTP response counts as reachable, while DNS/connect/TLS
//! and timeout failures count as failed. It deliberately does not validate
//! model names or credentials.

use std::time::Instant;

use reqwest::header::HeaderValue;
use reqwest::Client;

use crate::app_type::AppType;
use crate::db::stream_check_types::{HealthStatus, StreamCheckConfig, StreamCheckResult};
use crate::error::AppError;
use crate::model::Provider;

pub struct StreamCheckService;

impl StreamCheckService {
    pub async fn check_with_retry(
        app_type: &AppType,
        provider: &Provider,
        config: &StreamCheckConfig,
        base_url_override: Option<String>,
    ) -> Result<StreamCheckResult, AppError> {
        let effective = Self::merge_provider_config(provider, config);

        let mut last_result: Option<StreamCheckResult> = None;
        for attempt in 0..=effective.max_retries {
            let start = Instant::now();
            let result = Self::check_once(
                app_type,
                provider,
                &effective,
                base_url_override.clone(),
                start,
            )
            .await?;

            if result.success {
                return Ok(StreamCheckResult {
                    retry_count: attempt,
                    ..result
                });
            }

            if Self::should_retry(&result.message) && attempt < effective.max_retries {
                last_result = Some(result);
                continue;
            }

            return Ok(StreamCheckResult {
                retry_count: attempt,
                ..result
            });
        }

        Ok(last_result.unwrap_or_else(|| StreamCheckResult {
            status: HealthStatus::Failed,
            success: false,
            message: "Check failed".to_string(),
            response_time_ms: None,
            http_status: None,
            model_used: String::new(),
            tested_at: chrono::Utc::now().timestamp(),
            retry_count: effective.max_retries,
            error_category: None,
        }))
    }

    fn merge_provider_config(provider: &Provider, global: &StreamCheckConfig) -> StreamCheckConfig {
        let test_config = provider
            .meta
            .as_ref()
            .and_then(|m| m.test_config.as_ref())
            .filter(|tc| tc.enabled);

        match test_config {
            Some(tc) => StreamCheckConfig {
                timeout_secs: tc.timeout_secs.unwrap_or(global.timeout_secs),
                max_retries: tc.max_retries.unwrap_or(global.max_retries),
                degraded_threshold_ms: tc
                    .degraded_threshold_ms
                    .unwrap_or(global.degraded_threshold_ms),
            },
            None => global.clone(),
        }
    }

    async fn check_once(
        app_type: &AppType,
        provider: &Provider,
        config: &StreamCheckConfig,
        base_url_override: Option<String>,
        start: Instant,
    ) -> Result<StreamCheckResult, AppError> {
        let base_url = match base_url_override {
            Some(url) => url,
            None => Self::resolve_base_url(app_type, provider)?,
        };

        let client = crate::http_client::get();
        let timeout = std::time::Duration::from_secs(config.timeout_secs);
        let user_agent = Self::custom_user_agent(provider);

        let result = Self::probe_reachability(&client, &base_url, timeout, user_agent).await;
        let response_time = start.elapsed().as_millis() as u64;
        Ok(Self::build_result(
            result,
            response_time,
            config.degraded_threshold_ms,
        ))
    }

    fn resolve_base_url(app_type: &AppType, provider: &Provider) -> Result<String, AppError> {
        match app_type {
            AppType::OpenCode => {
                let npm = Self::extract_opencode_npm(provider);
                Self::resolve_opencode_base_url(provider, npm.as_deref())
            }
            AppType::OpenClaw => Self::extract_openclaw_base_url(provider),
            AppType::Hermes => Self::extract_hermes_base_url(provider),
            AppType::Claude | AppType::ClaudeDesktop => Self::extract_claude_base_url(provider),
            AppType::Codex => Self::extract_codex_base_url(provider),
        }
    }

    fn extract_claude_base_url(provider: &Provider) -> Result<String, AppError> {
        if provider.is_codex_oauth() {
            return Ok("https://chatgpt.com/backend-api/codex".to_string());
        }
        Self::first_non_empty([
            provider
                .settings_config
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(|value| value.as_str()),
            provider
                .settings_config
                .get("base_url")
                .and_then(|value| value.as_str()),
            provider
                .settings_config
                .get("baseURL")
                .and_then(|value| value.as_str()),
            provider
                .settings_config
                .get("apiEndpoint")
                .and_then(|value| value.as_str()),
        ])
        .ok_or_else(|| AppError::Config("Claude 供应商缺少 Base URL".to_string()))
    }

    fn extract_codex_base_url(provider: &Provider) -> Result<String, AppError> {
        let direct = Self::first_non_empty([
            provider
                .settings_config
                .get("base_url")
                .and_then(|value| value.as_str()),
            provider
                .settings_config
                .get("baseURL")
                .and_then(|value| value.as_str()),
            provider
                .settings_config
                .pointer("/config/base_url")
                .and_then(|value| value.as_str()),
        ]);
        direct
            .or_else(|| {
                provider
                    .settings_config
                    .get("config")
                    .and_then(|value| value.as_str())
                    .and_then(crate::apps::codex::extract_codex_base_url)
                    .map(|value| value.trim_end_matches('/').to_string())
            })
            .ok_or_else(|| AppError::Config("Codex 供应商缺少 Base URL".to_string()))
    }

    fn first_non_empty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
        values
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/').to_string())
    }

    async fn probe_reachability(
        client: &Client,
        base_url: &str,
        timeout: std::time::Duration,
        custom_ua: Option<HeaderValue>,
    ) -> Result<u16, AppError> {
        let url = base_url.trim();
        if url.is_empty() {
            return Err(AppError::Message("base_url is empty".to_string()));
        }

        let mut req = client
            .get(url)
            .timeout(timeout)
            .header("accept", "*/*")
            .header("accept-encoding", "identity");
        if let Some(ua) = custom_ua {
            req = req.header("user-agent", ua);
        }

        match req.send().await {
            Ok(resp) => Ok(resp.status().as_u16()),
            Err(err) => Err(Self::map_request_error(err)),
        }
    }

    fn build_result(
        result: Result<u16, AppError>,
        response_time: u64,
        degraded_threshold_ms: u64,
    ) -> StreamCheckResult {
        let tested_at = chrono::Utc::now().timestamp();
        match result {
            Ok(status) => StreamCheckResult {
                status: Self::determine_status(response_time, degraded_threshold_ms),
                success: true,
                message: "Reachable".to_string(),
                response_time_ms: Some(response_time),
                http_status: Some(status),
                model_used: String::new(),
                tested_at,
                retry_count: 0,
                error_category: None,
            },
            Err(err) => StreamCheckResult {
                status: HealthStatus::Failed,
                success: false,
                message: err.to_string(),
                response_time_ms: Some(response_time),
                http_status: None,
                model_used: String::new(),
                tested_at,
                retry_count: 0,
                error_category: None,
            },
        }
    }

    fn determine_status(latency_ms: u64, threshold: u64) -> HealthStatus {
        if latency_ms <= threshold {
            HealthStatus::Operational
        } else {
            HealthStatus::Degraded
        }
    }

    fn should_retry(message: &str) -> bool {
        let lower = message.to_lowercase();
        lower.contains("timeout") || lower.contains("abort") || lower.contains("timed out")
    }

    fn map_request_error(err: reqwest::Error) -> AppError {
        if err.is_timeout() {
            AppError::Message("Request timeout".to_string())
        } else if err.is_connect() {
            AppError::Message(format!("Connection failed: {err}"))
        } else {
            AppError::Message(err.to_string())
        }
    }

    fn custom_user_agent(provider: &Provider) -> Option<HeaderValue> {
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.custom_user_agent_header().ok().flatten())
    }

    fn extract_openclaw_base_url(provider: &Provider) -> Result<String, AppError> {
        provider
            .settings_config
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::localized(
                    "openclaw_base_url_missing",
                    "OpenClaw 供应商缺少 baseUrl",
                    "OpenClaw provider is missing `baseUrl`",
                )
            })
    }

    fn extract_hermes_base_url(provider: &Provider) -> Result<String, AppError> {
        provider
            .settings_config
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::localized(
                    "hermes_base_url_missing",
                    "Hermes 供应商缺少 base_url",
                    "Hermes provider is missing `base_url`",
                )
            })
    }

    fn resolve_opencode_base_url(
        provider: &Provider,
        npm: Option<&str>,
    ) -> Result<String, AppError> {
        if let Some(explicit) = Self::extract_opencode_base_url(provider) {
            return Ok(explicit);
        }

        let fallback = match npm {
            Some("@ai-sdk/openai") => Some("https://api.openai.com/v1"),
            Some("@ai-sdk/anthropic") => Some("https://api.anthropic.com"),
            Some("@ai-sdk/google") => Some("https://generativelanguage.googleapis.com"),
            _ => None,
        };

        fallback.map(str::to_string).ok_or_else(|| {
            AppError::localized(
                "opencode_base_url_missing",
                "OpenCode 供应商缺少 options.baseURL，且当前 SDK 包没有默认端点",
                "OpenCode provider is missing `options.baseURL` and the SDK package has no default endpoint",
            )
        })
    }

    fn extract_opencode_base_url(provider: &Provider) -> Option<String> {
        provider
            .settings_config
            .get("options")
            .and_then(|v| v.get("baseURL"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn extract_opencode_npm(provider: &Provider) -> Option<String> {
        provider
            .settings_config
            .get("npm")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderMeta, ProviderTestConfig};

    fn provider(settings_config: serde_json::Value) -> Provider {
        Provider::with_id(
            "test".to_string(),
            "Test".to_string(),
            settings_config,
            None,
        )
    }

    #[test]
    fn any_http_status_is_reachable() {
        for status in [200u16, 401, 403, 404, 429, 500, 503] {
            let result = StreamCheckService::build_result(Ok(status), 100, 1500);
            assert!(result.success);
            assert_eq!(result.status, HealthStatus::Operational);
            assert_eq!(result.http_status, Some(status));
        }
    }

    #[test]
    fn slow_reachable_response_is_degraded() {
        let result = StreamCheckService::build_result(Ok(200), 3000, 1500);
        assert!(result.success);
        assert_eq!(result.status, HealthStatus::Degraded);
    }

    #[test]
    fn provider_config_overrides_global_config_when_enabled() {
        let global = StreamCheckConfig::default();
        let mut p = provider(serde_json::json!({}));
        p.meta = Some(ProviderMeta {
            test_config: Some(ProviderTestConfig {
                enabled: true,
                timeout_secs: Some(20),
                degraded_threshold_ms: Some(3000),
                max_retries: None,
            }),
            ..Default::default()
        });

        let merged = StreamCheckService::merge_provider_config(&p, &global);
        assert_eq!(merged.timeout_secs, 20);
        assert_eq!(merged.degraded_threshold_ms, 3000);
        assert_eq!(merged.max_retries, global.max_retries);
    }

    #[test]
    fn opencode_base_url_uses_explicit_url_before_sdk_default() {
        let p = provider(serde_json::json!({
            "npm": "@ai-sdk/openai",
            "options": { "baseURL": "https://proxy.local/v1", "apiKey": "k" },
        }));
        let url = StreamCheckService::resolve_opencode_base_url(&p, Some("@ai-sdk/openai"))
            .expect("explicit baseURL");
        assert_eq!(url, "https://proxy.local/v1");
    }
}
