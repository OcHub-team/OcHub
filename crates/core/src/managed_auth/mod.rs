//! Managed-account authentication independent of request routing.

pub mod codex_oauth_auth;
pub mod copilot_auth;
pub mod copilot_model_map;

pub use codex_oauth_auth::{CodexOAuthError, CodexOAuthManager, CodexOAuthStatus};
pub use copilot_auth::{
    CopilotAuthError, CopilotAuthManager, CopilotAuthStatus, CopilotModel, CopilotToken,
    CopilotUsageResponse, GitHubAccount, GitHubDeviceCodeResponse,
};
