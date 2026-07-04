//! RouteDeck core library.
//!
//! A faithful Rust port of the cc-switch backend (`src-tauri/src`), restructured
//! to be UI- and transport-agnostic: no Tauri, no GPUI. The axum `routedeck-server`
//! crate and the GPUI `routedeck-app` crate both build on this.
//!
//! Persistence and live-config paths are kept byte-compatible with cc-switch
//! (`~/.cc-switch/`, `~/.claude`, `~/.codex`, `~/.gemini`, …) so RouteDeck is
//! a drop-in backend replacement.

pub mod app_state;
pub mod app_store;
pub mod app_type;
pub mod apps;
pub mod db;
pub mod deeplink;
pub mod error;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod prompt;
pub mod provider_config;
pub mod proxy;
pub mod services;
pub mod session_manager;
pub mod settings;
#[cfg(test)]
pub(crate) mod test_support;
pub mod usage_script;

pub use app_state::AppState;
pub use app_type::AppType;
pub use db::{Database, FailoverQueueItem};
pub use deeplink::{
    import_mcp_from_deeplink, import_prompt_from_deeplink, import_provider_from_deeplink,
    import_skill_from_deeplink, parse_deeplink_url, DeepLinkImportRequest, McpImportError,
    McpImportResult,
};
pub use error::{AppError, Result};
pub use model::{
    parse_custom_user_agent, AuthBinding, AuthBindingSource, ClaudeDesktopMode,
    ClaudeDesktopModelRoute, CodexChatReasoningConfig, Provider, ProviderManager, ProviderMeta,
    ProviderTestConfig, UniversalProvider, UsageData, UsageResult, UsageScript,
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
    EnvConflict, FetchedModel, LogFilters, McpService, ModelStats, PaginatedLogs, PromptService,
    ProviderLimitStatus, ProviderService, ProviderStats, ProxyService, RequestLogDetail,
    SkillService, SpeedtestService, SwitchResult, UsageCache, UsageSummary, UsageSummaryByApp,
    WorkspaceService,
};

// Authentication / managed-account subsystem (GitHub Copilot OAuth device flow,
// Codex/ChatGPT OAuth, generic managed accounts). Ported from cc-switch
// `commands/{auth,copilot,codex_oauth}.rs` + `proxy/providers/*`.
pub use proxy::providers::{
    AuthInfo, AuthStrategy, CodexOAuthError, CodexOAuthManager, CodexOAuthStatus, CopilotAuthError,
    CopilotAuthManager, CopilotAuthStatus, CopilotModel, CopilotToken, CopilotUsageResponse,
    GitHubAccount, GitHubDeviceCodeResponse,
};
pub use services::auth::{ManagedAuthAccount, ManagedAuthDeviceCodeResponse, ManagedAuthStatus};
