//! Usage script execution
//!
//! Handles executing and formatting usage query results via the JS
//! usage-script engine (`crate::usage_script::execute_usage_script`).

use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::model::{Provider, UsageData, UsageResult, UsageScript};
use crate::settings;
use crate::usage_script;

const TEMPLATE_TYPE_GITHUB_COPILOT: &str = "github_copilot";
const TEMPLATE_TYPE_TOKEN_PLAN: &str = "token_plan";
const TEMPLATE_TYPE_BALANCE: &str = "balance";
const TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION: &str = "official_subscription";
const COPILOT_UNIT_PREMIUM: &str = "requests";

/// Execute usage script and format result (private helper method)
pub(crate) async fn execute_and_format_usage_result(
    script_code: &str,
    api_key: &str,
    base_url: &str,
    timeout: u64,
    access_token: Option<&str>,
    user_id: Option<&str>,
    template_type: Option<&str>,
) -> Result<UsageResult, AppError> {
    match usage_script::execute_usage_script(
        script_code,
        api_key,
        base_url,
        timeout,
        access_token,
        user_id,
        template_type,
    )
    .await
    {
        Ok(data) => {
            let usage_list: Vec<UsageData> = if data.is_array() {
                serde_json::from_value(data).map_err(|e| {
                    AppError::localized(
                        "usage_script.data_format_error",
                        format!("数据格式错误: {e}"),
                        format!("Data format error: {e}"),
                    )
                })?
            } else {
                let single: UsageData = serde_json::from_value(data).map_err(|e| {
                    AppError::localized(
                        "usage_script.data_format_error",
                        format!("数据格式错误: {e}"),
                        format!("Data format error: {e}"),
                    )
                })?;
                vec![single]
            };

            Ok(UsageResult {
                success: true,
                data: Some(usage_list),
                error: None,
            })
        }
        Err(err) => {
            let lang = settings::get_settings()
                .language
                .unwrap_or_else(|| "zh".to_string());

            let msg = match err {
                AppError::Localized { zh, en, .. } => {
                    if lang == "en" {
                        en
                    } else {
                        zh
                    }
                }
                other => other.to_string(),
            };

            Ok(UsageResult {
                success: false,
                data: None,
                error: Some(msg),
            })
        }
    }
}

/// Resolve `(api_key, base_url)` for the JS-script path: explicit non-empty
/// script values win, otherwise fall back to the provider's stored config via
/// `Provider::resolve_usage_credentials`.
fn resolve_script_credentials(
    app_type: &AppType,
    provider: &Provider,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> (String, String) {
    let (provider_base_url, provider_api_key) = provider.resolve_usage_credentials(app_type);

    let api_key = api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or(provider_api_key);

    let base_url = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_owned())
        .unwrap_or(provider_base_url);

    (api_key, base_url)
}

fn resolve_native_credentials(app_type: &AppType, provider: Option<&Provider>) -> (String, String) {
    provider
        .map(|p| p.resolve_usage_credentials(app_type))
        .unwrap_or_default()
}

fn resolve_coding_plan_credentials(
    app_type: &AppType,
    provider: Option<&Provider>,
    usage_script: Option<&UsageScript>,
) -> (String, String) {
    let is_zenmux = usage_script
        .and_then(|s| s.coding_plan_provider.as_deref())
        .map(|provider| provider.eq_ignore_ascii_case("zenmux"))
        .unwrap_or(false);

    if !is_zenmux {
        return resolve_native_credentials(app_type, provider);
    }

    let script_base_url = usage_script
        .and_then(|s| s.base_url.as_deref())
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let script_api_key = usage_script
        .and_then(|s| s.api_key.as_deref())
        .unwrap_or("")
        .to_string();

    if !script_base_url.is_empty() && !script_api_key.is_empty() {
        return (script_base_url, script_api_key);
    }

    let native = resolve_native_credentials(app_type, provider);
    if !native.0.is_empty() && !native.1.is_empty() {
        native
    } else {
        (script_base_url, script_api_key)
    }
}

fn subscription_quota_to_usage_result(
    quota: crate::services::subscription::SubscriptionQuota,
    zenmux_extra: bool,
) -> UsageResult {
    if !quota.success {
        return UsageResult {
            success: false,
            data: None,
            error: quota.error.or(quota.credential_message),
        };
    }

    let plan_label = if zenmux_extra {
        quota
            .credential_message
            .as_deref()
            .and_then(|msg| msg.split(' ').next())
            .map(|tier| format!("ZenMux·{}", tier.to_uppercase()))
    } else {
        None
    };
    let mut first_tier = true;

    let data: Vec<UsageData> = quota
        .tiers
        .iter()
        .map(|tier| {
            let total = 100.0;
            let used = tier.utilization;
            let remaining = total - used;
            let extra = if zenmux_extra {
                let mut extra_json = serde_json::json!({
                    "resetsAt": tier.resets_at,
                });
                if let Some(v) = tier.used_value_usd {
                    extra_json["usedValueUsd"] = serde_json::json!(v);
                }
                if let Some(v) = tier.max_value_usd {
                    extra_json["maxValueUsd"] = serde_json::json!(v);
                }
                if first_tier {
                    if let Some(ref label) = plan_label {
                        extra_json["planLabel"] = serde_json::json!(label);
                    }
                    first_tier = false;
                }
                Some(extra_json.to_string())
            } else {
                tier.resets_at.clone()
            };

            UsageData {
                plan_name: Some(tier.name.clone()),
                remaining: Some(remaining),
                total: Some(total),
                used: Some(used),
                unit: Some("%".to_string()),
                is_valid: Some(true),
                invalid_message: None,
                extra,
            }
        })
        .collect();

    UsageResult {
        success: true,
        data: if data.is_empty() { None } else { Some(data) },
        error: None,
    }
}

async fn query_official_card_usage(
    state: &AppState,
    tool: crate::official_auth::OfficialTool,
    provider_id: &str,
) -> Result<UsageResult, AppError> {
    let current = crate::settings::get_effective_current_provider(&state.db, &tool.app_type())?;
    let blob = if current.as_deref() == Some(provider_id) {
        crate::official_auth::read_live(tool)?
            .or(crate::official_auth::read_catalog(tool, provider_id)?)
    } else {
        crate::official_auth::read_catalog(tool, provider_id)?
    };
    let Some(blob) = blob else {
        return Ok(UsageResult {
            success: false,
            data: None,
            error: Some("请先在终端登录该官方账号后再查询额度".to_string()),
        });
    };
    let Some(token) = crate::official_auth::access_token(&blob, tool) else {
        return Ok(UsageResult {
            success: false,
            data: None,
            error: Some("该官方卡没有可用的 access token".to_string()),
        });
    };
    let quota = crate::services::subscription::get_subscription_quota_with_token(
        tool.app_type().as_str(),
        &token,
    )
    .await
    .map_err(|e| AppError::Message(format!("Failed to query subscription quota: {e}")))?;
    Ok(subscription_quota_to_usage_result(quota, false))
}

/// Query provider usage (using saved script configuration)
pub async fn query_usage(
    state: &AppState,
    app_type: AppType,
    provider_id: &str,
) -> Result<UsageResult, AppError> {
    let providers = state.db.get_all_providers(app_type.as_str())?;
    let provider = providers.get(provider_id).ok_or_else(|| {
        AppError::localized(
            "provider.not_found",
            format!("供应商不存在: {provider_id}"),
            format!("Provider not found: {provider_id}"),
        )
    })?;
    let usage_script = provider.meta.as_ref().and_then(|m| m.usage_script.as_ref());
    let template_type = usage_script
        .and_then(|s| s.template_type.as_deref())
        .unwrap_or("");

    // Official login providers use credentials owned by their CLI instead of
    // a provider API key. Keep this native route independent of usage_script:
    // seeded official cards have no script metadata, and a user should not
    // need to configure one merely to read the account's remaining percent.
    if provider.category.as_deref() == Some("official")
        && matches!(
            app_type,
            AppType::Claude | AppType::Codex | AppType::KimiCode | AppType::GrokBuild
        )
    {
        if let Some(tool) = crate::official_auth::OfficialTool::from_app(app_type) {
            return query_official_card_usage(state, tool, provider_id).await;
        }
        let quota = crate::services::subscription::get_subscription_quota(app_type.as_str())
            .await
            .map_err(|e| AppError::Message(format!("Failed to query subscription quota: {e}")))?;
        return Ok(subscription_quota_to_usage_result(quota, false));
    }

    if template_type == TEMPLATE_TYPE_GITHUB_COPILOT {
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|m| m.managed_account_id_for(TEMPLATE_TYPE_GITHUB_COPILOT));
        let auth_manager = state.copilot_auth.read().await;
        let usage = match account_id.as_deref() {
            Some(account_id) => auth_manager.fetch_usage_for_account(account_id).await,
            None => auth_manager.fetch_usage().await,
        }
        .map_err(|e| AppError::Message(format!("Failed to fetch Copilot usage: {e}")))?;
        let premium = &usage.quota_snapshots.premium_interactions;
        let used = premium.entitlement - premium.remaining;

        return Ok(UsageResult {
            success: true,
            data: Some(vec![UsageData {
                plan_name: Some(usage.copilot_plan),
                remaining: Some(premium.remaining as f64),
                total: Some(premium.entitlement as f64),
                used: Some(used as f64),
                unit: Some(COPILOT_UNIT_PREMIUM.to_string()),
                is_valid: Some(true),
                invalid_message: None,
                extra: Some(format!("Reset: {}", usage.quota_reset_date)),
            }]),
            error: None,
        });
    }

    if template_type == TEMPLATE_TYPE_TOKEN_PLAN {
        let (base_url, api_key) =
            resolve_coding_plan_credentials(&app_type, Some(provider), usage_script);
        let quota = crate::services::coding_plan::get_coding_plan_quota(&base_url, &api_key)
            .await
            .map_err(|e| AppError::Message(format!("Failed to query coding plan: {e}")))?;
        let has_usd = quota
            .tiers
            .first()
            .map(|t| t.used_value_usd.is_some())
            .unwrap_or(false);
        return Ok(subscription_quota_to_usage_result(quota, has_usd));
    }

    if template_type == TEMPLATE_TYPE_BALANCE {
        let (base_url, api_key) = resolve_native_credentials(&app_type, Some(provider));
        return crate::services::balance::get_balance(&base_url, &api_key)
            .await
            .map_err(|e| AppError::Message(format!("Failed to query balance: {e}")));
    }

    if template_type == TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION {
        if !usage_script.map(|s| s.enabled).unwrap_or(false) {
            return Ok(UsageResult {
                success: false,
                data: None,
                error: Some("Usage query is disabled".to_string()),
            });
        }
        let quota = crate::services::subscription::get_subscription_quota(app_type.as_str())
            .await
            .map_err(|e| AppError::Message(format!("Failed to query subscription quota: {e}")))?;
        return Ok(subscription_quota_to_usage_result(quota, false));
    }

    let (script_code, timeout, api_key, base_url, access_token, user_id, template_type) = {
        let usage_script = usage_script.ok_or_else(|| {
            AppError::localized(
                "provider.usage.script.missing",
                "未配置用量查询脚本",
                "Usage script is not configured",
            )
        })?;
        if !usage_script.enabled {
            return Err(AppError::localized(
                "provider.usage.disabled",
                "用量查询未启用",
                "Usage query is disabled",
            ));
        }

        // Get credentials: prioritize UsageScript values, fallback to provider config
        let (api_key, base_url) = resolve_script_credentials(
            &app_type,
            provider,
            usage_script.api_key.as_deref(),
            usage_script.base_url.as_deref(),
        );

        (
            usage_script.code.clone(),
            usage_script.timeout.unwrap_or(10),
            api_key,
            base_url,
            usage_script.access_token.clone(),
            usage_script.user_id.clone(),
            usage_script.template_type.clone(),
        )
    };

    execute_and_format_usage_result(
        &script_code,
        &api_key,
        &base_url,
        timeout,
        access_token.as_deref(),
        user_id.as_deref(),
        template_type.as_deref(),
    )
    .await
}

/// Test usage script (using temporary script content, not saved)
#[allow(clippy::too_many_arguments)]
pub async fn test_usage_script(
    state: &AppState,
    app_type: AppType,
    provider_id: &str,
    script_code: &str,
    timeout: u64,
    api_key: Option<&str>,
    base_url: Option<&str>,
    access_token: Option<&str>,
    user_id: Option<&str>,
    template_type: Option<&str>,
) -> Result<UsageResult, AppError> {
    let providers = state.db.get_all_providers(app_type.as_str())?;
    let provider = providers.get(provider_id).ok_or_else(|| {
        AppError::localized(
            "provider.not_found",
            format!("供应商不存在: {provider_id}"),
            format!("Provider not found: {provider_id}"),
        )
    })?;

    // Resolve like the real query so testing matches what a saved script does.
    let (api_key, base_url) = resolve_script_credentials(&app_type, provider, api_key, base_url);

    execute_and_format_usage_result(
        script_code,
        &api_key,
        &base_url,
        timeout,
        access_token,
        user_id,
        template_type,
    )
    .await
}

/// Validate UsageScript configuration (boundary checks)
pub(crate) fn validate_usage_script(script: &UsageScript) -> Result<(), AppError> {
    // Validate auto query interval (0-1440 minutes, max 24 hours)
    if let Some(interval) = script.auto_query_interval
        && interval > 1440
    {
        return Err(AppError::localized(
            "usage_script.interval_too_large",
            format!("自动查询间隔不能超过 1440 分钟（24小时），当前值: {interval}"),
            format!(
                "Auto query interval cannot exceed 1440 minutes (24 hours), current: {interval}"
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_coding_plan_credentials, resolve_native_credentials, resolve_script_credentials,
    };
    use crate::app_type::AppType;
    use crate::model::{Provider, UsageScript};
    use serde_json::json;

    fn provider_with_settings(settings_config: serde_json::Value) -> Provider {
        Provider::with_id(
            "provider-1".to_string(),
            "Provider".to_string(),
            settings_config,
            None,
        )
    }

    #[test]
    fn script_values_override_provider_credentials() {
        let provider = provider_with_settings(json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "provider-key",
                "ANTHROPIC_BASE_URL": "https://provider.example.com/"
            }
        }));

        let (api_key, base_url) = resolve_script_credentials(
            &AppType::Claude,
            &provider,
            Some(" script-key "),
            Some(" https://script.example.com/ "),
        );
        assert_eq!(api_key, "script-key");
        assert_eq!(base_url, "https://script.example.com");
    }

    #[test]
    fn empty_script_values_fall_back_to_provider_credentials() {
        let provider = provider_with_settings(json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "provider-key",
                "ANTHROPIC_BASE_URL": "https://provider.example.com/"
            }
        }));

        let (api_key, base_url) =
            resolve_script_credentials(&AppType::Claude, &provider, Some(""), None);
        assert_eq!(api_key, "provider-key");
        assert_eq!(base_url, "https://provider.example.com");
    }

    #[test]
    fn codex_fallback_reads_auth_and_config_toml() {
        let provider = provider_with_settings(json!({
            "auth": {
                "OPENAI_API_KEY": "openai-key"
            },
            "config": r#"model_provider = "azure"

[model_providers.azure]
base_url = "https://azure.example.com/v1/"

[model_providers.other]
base_url = "https://other.example.com/v1"
"#
        }));

        let (api_key, base_url) =
            resolve_script_credentials(&AppType::Codex, &provider, None, None);
        assert_eq!(api_key, "openai-key");
        assert_eq!(base_url, "https://azure.example.com/v1");
    }

    fn usage_script(
        coding_plan_provider: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> UsageScript {
        UsageScript {
            enabled: true,
            language: "javascript".to_string(),
            code: String::new(),
            timeout: Some(10),
            api_key: api_key.map(str::to_string),
            base_url: base_url.map(str::to_string),
            access_token: None,
            user_id: None,
            template_type: Some("token_plan".to_string()),
            auto_query_interval: None,
            coding_plan_provider: coding_plan_provider.map(str::to_string),
        }
    }

    #[test]
    fn native_usage_credentials_delegate_to_provider_for_codex() {
        let provider = Provider::with_id(
            "test".to_string(),
            "Test".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "sk-codex" },
                "config": "model_provider = \"deepseek\"\n\
                           [model_providers.deepseek]\n\
                           base_url = \"https://api.deepseek.com\"\n",
            }),
            None,
        );
        let (base_url, api_key) = resolve_native_credentials(&AppType::Codex, Some(&provider));
        assert_eq!(base_url, "https://api.deepseek.com");
        assert_eq!(api_key, "sk-codex");
    }

    #[test]
    fn native_usage_credentials_missing_provider_yields_empty() {
        let (base_url, api_key) = resolve_native_credentials(&AppType::Codex, None);
        assert!(base_url.is_empty());
        assert!(api_key.is_empty());
    }

    #[test]
    fn zenmux_coding_plan_uses_script_credentials_first() {
        let provider = provider_with_settings(json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://provider.zenmux.example/v1",
                "ANTHROPIC_AUTH_TOKEN": "sk-provider"
            }
        }));
        let script = usage_script(
            Some("zenmux"),
            Some("https://script.zenmux.example/api/usage/"),
            Some("sk-script"),
        );

        let (base_url, api_key) =
            resolve_coding_plan_credentials(&AppType::Claude, Some(&provider), Some(&script));

        assert_eq!(base_url, "https://script.zenmux.example/api/usage");
        assert_eq!(api_key, "sk-script");
    }

    #[test]
    fn zenmux_coding_plan_falls_back_to_provider_credentials() {
        let provider = provider_with_settings(json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://provider.zenmux.example/v1",
                "ANTHROPIC_AUTH_TOKEN": "sk-provider"
            }
        }));
        let script = usage_script(Some("zenmux"), Some("https://script.zenmux.example"), None);

        let (base_url, api_key) =
            resolve_coding_plan_credentials(&AppType::Claude, Some(&provider), Some(&script));

        assert_eq!(base_url, "https://provider.zenmux.example/v1");
        assert_eq!(api_key, "sk-provider");
    }
}
