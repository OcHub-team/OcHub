use std::collections::HashSet;

use serde_json::{Value, json};
use url::Url;

use crate::application::{Application, ApplicationError, ApplicationResult, ProviderDetails};
use crate::provider_config::FormValues;
use crate::services::{
    ConfigService, ProviderService, ProviderSortUpdate, SpeedtestService, balance, model_fetch,
};
use crate::{AppId, AppType, Provider, UsageResult, UsageScript};

impl Application {
    pub fn sort_providers(
        &self,
        app: &AppId,
        ids: &[String],
    ) -> ApplicationResult<Vec<crate::application::ProviderListItem>> {
        let app_type = builtin_app(app, "provider.sort")?;
        let existing = self
            .state
            .db
            .get_all_providers(app_type.as_str())?
            .into_keys()
            .collect::<HashSet<_>>();
        let requested = ids.iter().cloned().collect::<HashSet<_>>();
        if requested.len() != ids.len() || requested != existing {
            return Err(ApplicationError::InvalidInput(
                "provider sort must contain every provider id exactly once".to_string(),
            ));
        }
        ProviderService::update_sort_order(
            &self.state,
            app_type,
            ids.iter()
                .enumerate()
                .map(|(sort_index, id)| ProviderSortUpdate {
                    id: id.clone(),
                    sort_index,
                })
                .collect(),
        )?;
        self.list_providers(app)
    }

    pub fn copy_provider(
        &self,
        source_app: &AppId,
        target_app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<ProviderDetails> {
        if source_app == target_app {
            return Err(ApplicationError::InvalidInput(
                "source and target applications must differ".to_string(),
            ));
        }
        let source_plugin =
            crate::plugin::get_plugin(source_app).ok_or_else(|| ApplicationError::NotFound {
                kind: "app",
                id: source_app.to_string(),
            })?;
        let target_plugin =
            crate::plugin::get_plugin(target_app).ok_or_else(|| ApplicationError::NotFound {
                kind: "app",
                id: target_app.to_string(),
            })?;
        let source_codec = source_plugin.provider_config().ok_or_else(|| {
            ApplicationError::CapabilityUnsupported {
                app: source_app.to_string(),
                capability: "provider.copy",
            }
        })?;
        let target_codec = target_plugin.provider_config().ok_or_else(|| {
            ApplicationError::CapabilityUnsupported {
                app: target_app.to_string(),
                capability: "provider.copy",
            }
        })?;
        let source = self
            .state
            .db
            .get_provider_by_id(provider_id, source_app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.to_string(),
            })?;
        if self
            .state
            .db
            .get_provider_by_id(provider_id, target_app.as_str())?
            .is_some()
        {
            return Err(ApplicationError::AlreadyExists {
                kind: "provider",
                id: provider_id.to_string(),
            });
        }

        let source_values = source_codec.decode(&source.settings_config, source.meta.as_ref());
        let target_fields = target_codec
            .schema()
            .into_iter()
            .flat_map(|section| section.fields)
            .map(|field| field.id)
            .collect::<HashSet<_>>();
        let mut values: FormValues = target_codec.decode(&Value::Null, None);
        for (key, value) in source_values {
            if target_fields.contains(&key) {
                values.insert(key, value);
            }
        }
        let issues = target_codec.validate_for_category(&values, None);
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == crate::provider_config::Severity::Error)
            .map(|issue| {
                json!({
                    "field": issue.field,
                    "message": issue.message
                })
            })
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(ApplicationError::ValidationFailed {
                message: format!(
                    "provider {} cannot be converted from {} to {}",
                    provider_id, source_app, target_app
                ),
                details: json!({ "issues": errors }),
            });
        }
        let encoded = target_codec.encode(&values, &Value::Null, None);
        let target = Provider {
            id: source.id.clone(),
            name: source.name.clone(),
            settings_config: encoded.settings_config,
            website_url: source.website_url.clone(),
            category: None,
            created_at: Some(chrono::Utc::now().timestamp()),
            sort_index: None,
            notes: Some(format!("Copied from {}:{}", source_app, source.id)),
            meta: encoded.meta,
            icon: source.icon.clone(),
            icon_color: source.icon_color.clone(),
        };
        let target_type = builtin_app(target_app, "provider.copy")?;
        ProviderService::add(&self.state, target_type, target, false)?;
        self.get_provider(target_app, provider_id, false)
    }

    pub async fn provider_test(
        &self,
        app: &AppId,
        provider_id: &str,
        timeout_secs: Option<u64>,
    ) -> ApplicationResult<Value> {
        let (_, provider) = self.provider_with_type(app, provider_id)?;
        let base_url = provider.resolve_usage_base_url(&builtin_app(app, "provider.test")?);
        let mut results = SpeedtestService::test_endpoints(vec![base_url], timeout_secs).await?;
        Ok(serde_json::to_value(results.pop())
            .map_err(|source| crate::AppError::JsonSerialize { source })?)
    }

    pub async fn provider_speed_test(
        &self,
        app: &AppId,
        provider_id: &str,
        timeout_secs: Option<u64>,
    ) -> ApplicationResult<Value> {
        let (app_type, provider) = self.provider_with_type(app, provider_id)?;
        let mut urls = vec![provider.resolve_usage_base_url(&app_type)];
        urls.extend(
            ProviderService::get_custom_endpoints(&self.state, app_type, provider_id)?
                .into_iter()
                .map(|endpoint| endpoint.url),
        );
        urls.retain(|url| !url.trim().is_empty());
        urls.sort();
        urls.dedup();
        Ok(
            serde_json::to_value(SpeedtestService::test_endpoints(urls, timeout_secs).await?)
                .map_err(|source| crate::AppError::JsonSerialize { source })?,
        )
    }

    pub async fn provider_models(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<Value> {
        let (app_type, provider) = self.provider_with_type(app, provider_id)?;
        let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
        let is_full_url = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.is_full_url)
            .unwrap_or(false);
        let user_agent = provider
            .meta
            .as_ref()
            .map(|meta| meta.custom_user_agent_header())
            .transpose()
            .map_err(|error| ApplicationError::InvalidInput(error.to_string()))?
            .flatten();
        let models = model_fetch::fetch_models(&base_url, &api_key, is_full_url, None, user_agent)
            .await
            .map_err(map_network_error)?;
        Ok(serde_json::to_value(models)
            .map_err(|source| crate::AppError::JsonSerialize { source })?)
    }

    pub async fn provider_balance(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<UsageResult> {
        let (app_type, provider) = self.provider_with_type(app, provider_id)?;
        let (base_url, api_key) = provider.resolve_usage_credentials(&app_type);
        balance::get_balance(&base_url, &api_key)
            .await
            .map_err(map_network_error)
    }

    pub async fn provider_quota(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<UsageResult> {
        let app_type = builtin_app(app, "provider.quota")?;
        self.get_provider(app, provider_id, false)?;
        Ok(ProviderService::query_usage(&self.state, app_type, provider_id).await?)
    }

    pub async fn run_provider_usage_script(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<UsageResult> {
        self.provider_quota(app, provider_id).await
    }

    pub async fn test_provider_usage_script(
        &self,
        app: &AppId,
        provider_id: &str,
        script: &UsageScript,
    ) -> ApplicationResult<UsageResult> {
        let app_type = builtin_app(app, "provider.usage-script.test")?;
        Ok(ProviderService::test_usage_script(
            &self.state,
            app_type,
            provider_id,
            &script.code,
            script.timeout.unwrap_or(10),
            script.api_key.as_deref(),
            script.base_url.as_deref(),
            script.access_token.as_deref(),
            script.user_id.as_deref(),
            script.template_type.as_deref(),
        )
        .await?)
    }

    pub fn provider_endpoints(&self, app: &AppId, provider_id: &str) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "provider.endpoint")?;
        self.get_provider(app, provider_id, false)?;
        Ok(serde_json::to_value(ProviderService::get_custom_endpoints(
            &self.state,
            app_type,
            provider_id,
        )?)
        .map_err(|source| crate::AppError::JsonSerialize { source })?)
    }

    pub fn add_provider_endpoint(
        &self,
        app: &AppId,
        provider_id: &str,
        url: &str,
    ) -> ApplicationResult<Value> {
        validate_http_url(url)?;
        let app_type = builtin_app(app, "provider.endpoint")?;
        self.get_provider(app, provider_id, false)?;
        ProviderService::add_custom_endpoint(&self.state, app_type, provider_id, url.to_string())?;
        self.provider_endpoints(app, provider_id)
    }

    pub fn remove_provider_endpoint(
        &self,
        app: &AppId,
        provider_id: &str,
        url: &str,
    ) -> ApplicationResult<Value> {
        let app_type = builtin_app(app, "provider.endpoint")?;
        self.get_provider(app, provider_id, false)?;
        ProviderService::remove_custom_endpoint(
            &self.state,
            app_type,
            provider_id,
            url.to_string(),
        )?;
        self.provider_endpoints(app, provider_id)
    }

    pub fn open_provider_terminal(
        &self,
        app: &AppId,
        provider_id: &str,
        cwd: Option<String>,
    ) -> ApplicationResult<()> {
        builtin_app(app, "provider.terminal")?;
        crate::session_manager::provider_terminal::open_provider_terminal(
            &self.state,
            app.as_str(),
            provider_id,
            cwd,
        )
        .map_err(|error| {
            if error.to_ascii_lowercase().contains("unsupported") {
                ApplicationError::PlatformUnsupported(error)
            } else {
                ApplicationError::OperationFailed(error)
            }
        })?;
        Ok(())
    }

    pub fn common_config(&self, app: &AppId) -> ApplicationResult<Option<String>> {
        ensure_common_config_app(app)?;
        Ok(ConfigService::get_common_config_snippet(
            &self.state,
            app.as_str(),
        )?)
    }

    pub fn set_common_config(&self, app: &AppId, snippet: String) -> ApplicationResult<()> {
        ensure_common_config_app(app)?;
        ConfigService::set_common_config_snippet(&self.state, app.as_str(), snippet)?;
        Ok(())
    }

    pub fn extract_common_config(&self, app: &AppId) -> ApplicationResult<String> {
        let app_type = builtin_app(app, "config.common.extract")?;
        ensure_common_config_app(app)?;
        Ok(ConfigService::extract_common_config_snippet(
            &self.state,
            app_type,
            None,
        )?)
    }

    pub fn apply_common_config(
        &self,
        app: &AppId,
        provider_ids: &[String],
    ) -> ApplicationResult<Vec<String>> {
        let app_type = builtin_app(app, "config.common.apply")?;
        ensure_common_config_app(app)?;
        let mut providers = self.state.db.get_all_providers(app.as_str())?;
        let targets = if provider_ids.is_empty() {
            providers.keys().cloned().collect::<Vec<_>>()
        } else {
            provider_ids.to_vec()
        };
        for id in &targets {
            let provider = providers
                .get_mut(id)
                .ok_or_else(|| ApplicationError::NotFound {
                    kind: "provider",
                    id: id.clone(),
                })?;
            provider
                .meta
                .get_or_insert_with(Default::default)
                .common_config_enabled = Some(true);
            self.state.db.save_provider(app.as_str(), provider)?;
        }
        ProviderService::sync_current_provider_for_app(&self.state, app_type)?;
        Ok(targets)
    }

    fn provider_with_type(
        &self,
        app: &AppId,
        provider_id: &str,
    ) -> ApplicationResult<(AppType, Provider)> {
        let app_type = builtin_app(app, "provider.network")?;
        let provider = self
            .state
            .db
            .get_provider_by_id(provider_id, app.as_str())?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.to_string(),
            })?;
        Ok((app_type, provider))
    }
}

fn builtin_app(app: &AppId, capability: &'static str) -> ApplicationResult<AppType> {
    AppType::from_app_id(app).ok_or_else(|| ApplicationError::CapabilityUnsupported {
        app: app.to_string(),
        capability,
    })
}

fn ensure_common_config_app(app: &AppId) -> ApplicationResult<()> {
    if matches!(app.as_str(), "claude" | "codex" | "opencode" | "openclaw") {
        Ok(())
    } else {
        Err(ApplicationError::CapabilityUnsupported {
            app: app.to_string(),
            capability: "config.common",
        })
    }
}

fn validate_http_url(value: &str) -> ApplicationResult<()> {
    let url = Url::parse(value)
        .map_err(|error| ApplicationError::InvalidInput(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ApplicationError::InvalidInput(
            "endpoint must be an absolute http(s) URL".to_string(),
        ));
    }
    Ok(())
}

fn map_network_error(error: String) -> ApplicationError {
    if error.contains("HTTP 4") || error.contains("HTTP 5") {
        ApplicationError::UpstreamRejected(error)
    } else {
        ApplicationError::NetworkUnavailable(error)
    }
}
