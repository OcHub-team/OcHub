//! OcHub core library.
//!
//! A faithful Rust port of the cc-switch backend (`src-tauri/src`), restructured
//! to be UI- and transport-agnostic: no Tauri, no GPUI. The axum `ochub-server`
//! crate and the GPUI `ochub-app` crate both build on this.
//!
//! OcHub owns an independent `~/.ochub/` database and performs a one-time,
//! read-only import from cc-switch. Supported live-config writers target Claude,
//! Codex, OpenCode, OpenClaw, Hermes, and Claude Desktop.

pub mod app_id;
pub mod app_state;
pub mod app_store;
pub mod app_type;
pub mod autostart;
pub mod apps;
pub mod db;
pub mod deeplink;
pub mod error;
pub mod gateway;
pub mod http_client;
pub mod managed_auth;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod plugin;
pub mod provider_config;
pub mod services;
pub mod session_manager;
pub mod settings;
#[cfg(test)]
pub(crate) mod test_support;
pub mod usage_script;
pub mod usage_tracking;

pub use app_id::AppId;
pub use app_state::AppState;
pub use app_type::AppType;
pub use db::Database;
pub use deeplink::{
    import_mcp_from_deeplink, import_provider_from_deeplink, import_skill_from_deeplink,
    parse_deeplink_url, DeepLinkImportRequest, McpImportError, McpImportResult,
};
pub use error::{AppError, Result};
pub use model::{
    parse_custom_user_agent, AuthBinding, AuthBindingSource, ClaudeDesktopModelRoute,
    CodexChatReasoningConfig, Provider, ProviderManager, ProviderMeta, ProviderTestConfig,
    UsageData, UsageResult, UsageScript,
};
pub use services::{
    build_models_url_candidates, check_env_conflicts, delete_env_vars, fetch_models,
    has_codex_official_history_unify_backup,
    maybe_migrate_codex_official_history_to_unified_bucket,
    maybe_migrate_codex_provider_template_bucket,
    maybe_migrate_codex_third_party_history_provider_bucket,
    restore_codex_official_history_from_backups, restore_env_backup, BackupInfo,
    CodexHistoryProviderBucketMigrationOutcome, CodexOfficialHistoryRestoreOutcome,
    CodexProviderTemplateBucketMigrationOutcome, ConfigService, DailyStats, EndpointLatency,
    EnvConflict, FetchedModel, LogFilters, McpService, ModelStats, PaginatedLogs,
    ProviderLimitStatus, ProviderService, ProviderStats, RequestLogDetail, SkillService,
    SpeedtestService, SwitchResult, UsageCache, UsageSummary, UsageSummaryByApp,
};

// Authentication / managed-account subsystem (GitHub Copilot OAuth device flow,
// Codex/ChatGPT OAuth, generic managed accounts). Ported from cc-switch
// `commands/{auth,copilot,codex_oauth}.rs`.
pub use managed_auth::{
    CodexOAuthError, CodexOAuthManager, CodexOAuthStatus, CopilotAuthError, CopilotAuthManager,
    CopilotAuthStatus, CopilotModel, CopilotToken, CopilotUsageResponse, GitHubAccount,
    GitHubDeviceCodeResponse,
};
pub use services::auth::{ManagedAuthAccount, ManagedAuthDeviceCodeResponse, ManagedAuthStatus};
