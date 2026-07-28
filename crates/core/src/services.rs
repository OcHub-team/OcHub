//! Service layer: provider switching, OMO config, the
//! tray usage cache, cloud sync (WebDAV/S3), and provider network utilities.
//! Ported from cc-switch `services/`.
//!
//! Session usage sync services are also ported; Tauri event fan-out points are
//! replaced with log output or explicit API calls.

pub mod apps;
pub mod auth;
pub mod balance;
pub mod codex_history_migration;
pub mod codex_oauth_models;
pub mod coding_plan;
pub mod config;
pub mod env;
pub mod mcp;
pub mod model_fetch;
pub mod omo;
pub mod pricing_catalog;
pub mod provider;
pub mod s3;
pub mod s3_auto_sync;
pub mod s3_sync;
pub mod session_usage;
pub mod session_usage_codex;
pub mod session_usage_opencode;
pub mod skill;
pub mod speedtest;
pub mod sql_helpers;
pub mod subscription;
pub mod sync_protocol;
pub mod update;
pub mod usage_cache;
pub mod usage_stats;
pub mod webdav;
pub mod webdav_auto_sync;
pub mod webdav_sync;

pub use codex_history_migration::{
    CodexHistoryProviderBucketMigrationOutcome, CodexOfficialHistoryRestoreOutcome,
    CodexProviderTemplateBucketMigrationOutcome, has_codex_official_history_unify_backup,
    maybe_migrate_codex_official_history_to_unified_bucket,
    maybe_migrate_codex_provider_template_bucket,
    maybe_migrate_codex_third_party_history_provider_bucket,
    restore_codex_official_history_from_backups,
};
pub use config::ConfigService;
pub use env::{
    BackupInfo, EnvConflict, check_env_conflicts, delete_env_vars, restore_env_backup,
    restore_from_backup,
};
pub use mcp::McpService;
pub use model_fetch::{FetchedModel, build_models_url_candidates, fetch_models};
pub use omo::OmoService;
pub use pricing_catalog::{
    MissingPricingModel, PricingCatalogRefreshKind, PricingCatalogRefreshOutcome,
    PricingCatalogStatus,
};
pub use provider::{ProviderService, ProviderSortUpdate, SwitchResult};
pub use skill::SkillService;
pub use speedtest::{EndpointLatency, SpeedtestService};
pub use update::{UpdateCheckResult, latest_release_url};
pub use usage_cache::UsageCache;
pub use usage_stats::{
    DailyStats, LogFilters, ModelStats, PaginatedLogs, ProviderLimitStatus, ProviderStats,
    RequestLogDetail, UsageSummary, UsageSummaryByApp,
};
