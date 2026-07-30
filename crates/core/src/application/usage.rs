use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

use crate::application::{
    Application, ApplicationError, ApplicationResult, PricingDefault, UsageFilter, UsageLimitItem,
};
use crate::services::pricing_catalog::{
    PricingCatalogEntry, PricingCatalogRefreshOutcome, PricingCatalogStatus,
    refresh_pricing_catalog,
};
use crate::services::session_usage::{DataSourceSummary, SessionSyncResult};
use crate::services::usage_stats::{
    DailyStats, LogFilters, ModelPricingInfo, ModelStats, PaginatedLogs, ProviderStats,
    RequestLogDetail, UsageSummary, UsageSummaryByApp,
};
use crate::{AppId, AppType};

impl Application {
    pub fn usage_summary(&self, filter: &UsageFilter) -> ApplicationResult<UsageSummary> {
        Ok(self.state.db.get_usage_summary(
            filter.start,
            filter.end,
            filter.app.as_deref(),
            filter.provider.as_deref(),
            filter.model.as_deref(),
        )?)
    }

    pub fn usage_by_app(&self, filter: &UsageFilter) -> ApplicationResult<Vec<UsageSummaryByApp>> {
        Ok(self.state.db.get_usage_summary_by_app(
            filter.start,
            filter.end,
            filter.provider.as_deref(),
            filter.model.as_deref(),
        )?)
    }

    pub fn usage_trend(
        &self,
        filter: &UsageFilter,
        interval: &str,
    ) -> ApplicationResult<Vec<DailyStats>> {
        let daily = self.state.db.get_daily_trends(
            filter.start,
            filter.end,
            filter.app.as_deref(),
            filter.provider.as_deref(),
            filter.model.as_deref(),
        )?;
        match interval {
            "day" => Ok(daily),
            "week" | "month" => aggregate_trends(daily, interval),
            other => Err(ApplicationError::InvalidInput(format!(
                "unsupported usage trend interval: {other}"
            ))),
        }
    }

    pub fn usage_provider_stats(
        &self,
        filter: &UsageFilter,
    ) -> ApplicationResult<Vec<ProviderStats>> {
        Ok(self.state.db.get_provider_stats(
            filter.start,
            filter.end,
            filter.app.as_deref(),
            filter.provider.as_deref(),
            filter.model.as_deref(),
        )?)
    }

    pub fn usage_model_stats(&self, filter: &UsageFilter) -> ApplicationResult<Vec<ModelStats>> {
        Ok(self.state.db.get_model_stats(
            filter.start,
            filter.end,
            filter.app.as_deref(),
            filter.provider.as_deref(),
            filter.model.as_deref(),
        )?)
    }

    pub fn usage_logs(
        &self,
        filters: &LogFilters,
        page: u32,
        page_size: u32,
    ) -> ApplicationResult<PaginatedLogs> {
        if page_size == 0 || page_size > 1_000 {
            return Err(ApplicationError::InvalidInput(
                "usage log page size must be between 1 and 1000".to_string(),
            ));
        }
        Ok(self.state.db.get_request_logs(filters, page, page_size)?)
    }

    pub fn usage_request(&self, id: &str) -> ApplicationResult<RequestLogDetail> {
        self.state
            .db
            .get_request_detail(id)?
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "usage-request",
                id: id.to_string(),
            })
    }

    pub fn usage_sources(&self) -> ApplicationResult<Vec<DataSourceSummary>> {
        Ok(crate::services::session_usage::get_data_source_breakdown(
            &self.state.db,
        )?)
    }

    pub fn usage_limits(
        &self,
        app: Option<&AppId>,
        provider_id: Option<&str>,
    ) -> ApplicationResult<Vec<UsageLimitItem>> {
        let apps = match app {
            Some(app) => vec![app.clone()],
            None => self
                .list_apps()?
                .into_iter()
                .filter(|app| app.supports_provider)
                .map(|app| AppId::parse(&app.id))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let mut result = Vec::new();
        for app in apps {
            for provider in self.list_providers(&app)? {
                if provider_id.is_some_and(|id| id != provider.id) {
                    continue;
                }
                result.push(UsageLimitItem {
                    app: app.to_string(),
                    provider_id: provider.id.clone(),
                    provider_name: provider.name,
                    status: self
                        .state
                        .db
                        .check_provider_limits(&provider.id, app.as_str())?,
                });
            }
        }
        if provider_id.is_some() && result.is_empty() {
            return Err(ApplicationError::NotFound {
                kind: "provider",
                id: provider_id.unwrap_or_default().to_string(),
            });
        }
        Ok(result)
    }

    pub fn sync_usage(&self, apps: &[AppId]) -> ApplicationResult<SessionSyncResult> {
        let targets = if apps.is_empty() {
            vec![AppType::Claude, AppType::Codex, AppType::OpenCode]
        } else {
            apps.iter()
                .map(|app| {
                    let app_type = AppType::from_app_id(app).ok_or_else(|| {
                        ApplicationError::CapabilityUnsupported {
                            app: app.to_string(),
                            capability: "usage.session-sync",
                        }
                    })?;
                    if !matches!(
                        app_type,
                        AppType::Claude | AppType::Codex | AppType::OpenCode
                    ) {
                        return Err(ApplicationError::CapabilityUnsupported {
                            app: app.to_string(),
                            capability: "usage.session-sync",
                        });
                    }
                    Ok(app_type)
                })
                .collect::<ApplicationResult<Vec<_>>>()?
        };

        let mut combined = SessionSyncResult {
            imported: 0,
            skipped: 0,
            files_scanned: 0,
            errors: Vec::new(),
        };
        for app in targets {
            let result = match app {
                AppType::Claude => {
                    crate::services::session_usage::sync_claude_session_logs(&self.state.db)
                }
                AppType::Codex => {
                    crate::services::session_usage_codex::sync_codex_usage(&self.state.db)
                }
                AppType::OpenCode => {
                    crate::services::session_usage_opencode::sync_opencode_usage(&self.state.db)
                }
                _ => unreachable!("targets were validated"),
            };
            match result {
                Ok(result) => merge_sync_result(&mut combined, result),
                Err(error) => combined.errors.push(format!("{app}: {error}")),
            }
        }
        Ok(combined)
    }

    pub async fn pricing_defaults(&self) -> ApplicationResult<Vec<PricingDefault>> {
        let mut defaults = Vec::new();
        for app in ["claude", "codex"] {
            defaults.push(PricingDefault {
                app: app.to_string(),
                multiplier: self.state.db.get_default_cost_multiplier(app).await?,
                model_source: self.state.db.get_pricing_model_source(app).await?,
            });
        }
        Ok(defaults)
    }

    pub async fn set_pricing_defaults(
        &self,
        defaults: &[PricingDefault],
    ) -> ApplicationResult<Vec<PricingDefault>> {
        for item in defaults {
            if !matches!(item.app.as_str(), "claude" | "codex") {
                return Err(ApplicationError::InvalidInput(format!(
                    "unsupported pricing defaults app: {}",
                    item.app
                )));
            }
            self.state
                .db
                .set_default_cost_multiplier(&item.app, &item.multiplier)
                .await?;
            self.state
                .db
                .set_pricing_model_source(&item.app, &item.model_source)
                .await?;
        }
        self.pricing_defaults().await
    }

    pub fn pricing_status(&self) -> ApplicationResult<PricingCatalogStatus> {
        Ok(self.state.db.get_pricing_catalog_status()?)
    }

    pub async fn refresh_pricing(
        &self,
        force: bool,
    ) -> ApplicationResult<PricingCatalogRefreshOutcome> {
        refresh_pricing_catalog(self.state.db.clone(), force)
            .await
            .map_err(|error| match error {
                crate::AppError::HttpStatus { .. } => ApplicationError::Core(error),
                other => ApplicationError::NetworkUnavailable(other.to_string()),
            })
    }

    pub fn list_pricing_catalog(
        &self,
        query: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> ApplicationResult<Vec<PricingCatalogEntry>> {
        Ok(self.state.db.list_pricing_catalog(query, limit, offset)?)
    }

    pub fn list_pricing_overrides(&self) -> ApplicationResult<Vec<ModelPricingInfo>> {
        Ok(self.state.db.get_model_pricing()?)
    }

    pub fn set_pricing_override(
        &self,
        model_id: &str,
        pricing: &ModelPricingInfo,
    ) -> ApplicationResult<ModelPricingInfo> {
        if pricing.model_id != model_id {
            return Err(ApplicationError::InvalidInput(format!(
                "pricing file modelId {} does not match --model {model_id}",
                pricing.model_id
            )));
        }
        self.state.db.update_model_pricing(pricing)?;
        self.list_pricing_overrides()?
            .into_iter()
            .find(|item| item.model_id == model_id)
            .ok_or_else(|| ApplicationError::NotFound {
                kind: "pricing-override",
                id: model_id.to_string(),
            })
    }

    pub fn remove_pricing_override(&self, model_id: &str) -> ApplicationResult<()> {
        if !self
            .list_pricing_overrides()?
            .iter()
            .any(|item| item.model_id == model_id)
        {
            return Err(ApplicationError::NotFound {
                kind: "pricing-override",
                id: model_id.to_string(),
            });
        }
        self.state.db.delete_model_pricing(model_id)?;
        Ok(())
    }

    pub fn backfill_pricing(&self) -> ApplicationResult<u64> {
        Ok(self.state.db.backfill_missing_usage_costs()?)
    }
}

fn merge_sync_result(target: &mut SessionSyncResult, value: SessionSyncResult) {
    target.imported += value.imported;
    target.skipped += value.skipped;
    target.files_scanned += value.files_scanned;
    target.errors.extend(value.errors);
}

fn aggregate_trends(daily: Vec<DailyStats>, interval: &str) -> ApplicationResult<Vec<DailyStats>> {
    let mut buckets = BTreeMap::<String, DailyStats>::new();
    for item in daily {
        let date = item.date.get(..10).unwrap_or(&item.date);
        let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|error| {
            ApplicationError::OperationFailed(format!(
                "invalid usage trend date {}: {error}",
                item.date
            ))
        })?;
        let key = if interval == "week" {
            let week = parsed.iso_week();
            format!("{}-W{:02}", week.year(), week.week())
        } else {
            format!("{:04}-{:02}", parsed.year(), parsed.month())
        };
        let bucket = buckets.entry(key.clone()).or_insert_with(|| DailyStats {
            date: key,
            request_count: 0,
            total_cost: "0.000000".to_string(),
            total_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
        });
        bucket.request_count += item.request_count;
        bucket.total_tokens += item.total_tokens;
        bucket.total_input_tokens += item.total_input_tokens;
        bucket.total_output_tokens += item.total_output_tokens;
        bucket.total_cache_creation_tokens += item.total_cache_creation_tokens;
        bucket.total_cache_read_tokens += item.total_cache_read_tokens;
        let current = Decimal::from_str(&bucket.total_cost).unwrap_or_default();
        let added = Decimal::from_str(&item.total_cost).unwrap_or_default();
        bucket.total_cost = format!("{:.6}", current + added);
    }
    Ok(buckets.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::aggregate_trends;
    use crate::services::usage_stats::DailyStats;

    #[test]
    fn aggregates_daily_usage_by_month() {
        let item = |date: &str, cost: &str| DailyStats {
            date: date.to_string(),
            request_count: 1,
            total_cost: cost.to_string(),
            total_tokens: 10,
            total_input_tokens: 4,
            total_output_tokens: 6,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
        };
        let result = aggregate_trends(
            vec![
                item("2026-07-01", "0.100000"),
                item("2026-07-02", "0.200000"),
            ],
            "month",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].date, "2026-07");
        assert_eq!(result[0].request_count, 2);
        assert_eq!(result[0].total_cost, "0.300000");
    }
}
