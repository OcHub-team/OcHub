//! Official CLI credential vault for Claude Code and Kimi Code.
//!
//! OcHub never logs the user in. The user runs `claude /login` / `kimi login`.
//! Credentials belong to the **current official provider card**:
//! switch away → save the live slot onto that card; switch back → restore it.

mod claude;
mod kimi;
mod store;

use serde_json::Value;

use crate::app_type::AppType;
use crate::error::AppError;
use crate::model::Provider;

pub use store::official_auth_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialTool {
    Claude,
    Kimi,
}

impl OfficialTool {
    pub fn from_app(app_type: AppType) -> Option<Self> {
        match app_type {
            AppType::Claude => Some(Self::Claude),
            AppType::KimiCode => Some(Self::Kimi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Kimi => "kimi",
        }
    }

    pub fn seed_provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude-official",
            Self::Kimi => crate::db::dao::providers_seed::KIMI_CODE_OFFICIAL_PROVIDER_ID,
        }
    }

    pub fn app_type(self) -> AppType {
        match self {
            Self::Claude => AppType::Claude,
            Self::Kimi => AppType::KimiCode,
        }
    }
}

pub fn is_official_card(provider: &Provider) -> bool {
    provider.category.as_deref() == Some("official")
}

/// Save the CLI live slot onto `provider_id`'s catalog. Missing live is a no-op.
pub fn capture_live_to_card(tool: OfficialTool, provider_id: &str) -> Result<(), AppError> {
    let Some(blob) = read_live(tool)? else {
        return Ok(());
    };
    store::write_catalog(tool, provider_id, &blob)
}

/// Restore this official card into the CLI live slot.
///
/// A saved catalog is written back. A new official card with no catalog
/// **clears** the live slot so the CLI is logged out and the user can
/// `kimi login` / `claude /login` as a different account.
pub fn apply_card_to_live(tool: OfficialTool, provider_id: &str) -> Result<(), AppError> {
    match store::read_catalog(tool, provider_id)? {
        Some(blob) => write_live(tool, &blob),
        None => clear_live(tool),
    }
}

/// Restore `provider_id`'s catalog into the CLI live slot. Missing catalog is a no-op.
pub fn materialize_card_if_present(tool: OfficialTool, provider_id: &str) -> Result<(), AppError> {
    apply_card_to_live(tool, provider_id)
}

fn clear_live(tool: OfficialTool) -> Result<(), AppError> {
    match tool {
        OfficialTool::Claude => claude::clear_live(),
        OfficialTool::Kimi => kimi::clear_live(),
    }
}

pub fn delete_card_catalog(tool: OfficialTool, provider_id: &str) -> Result<(), AppError> {
    store::delete_catalog(tool, provider_id)
}

pub fn has_catalog(tool: OfficialTool, provider_id: &str) -> Result<bool, AppError> {
    Ok(store::read_catalog(tool, provider_id)?.is_some())
}

pub fn read_catalog(tool: OfficialTool, provider_id: &str) -> Result<Option<Value>, AppError> {
    store::read_catalog(tool, provider_id)
}

pub fn read_live(tool: OfficialTool) -> Result<Option<Value>, AppError> {
    match tool {
        OfficialTool::Claude => claude::read_live(),
        OfficialTool::Kimi => kimi::read_live(),
    }
}

fn write_live(tool: OfficialTool, blob: &Value) -> Result<(), AppError> {
    match tool {
        OfficialTool::Claude => claude::write_live(blob),
        OfficialTool::Kimi => kimi::write_live(blob),
    }
}

/// If the seed card has no catalog yet and the CLI already has official creds,
/// adopt those bytes onto the seed card. Does not change the current provider.
pub fn adopt_live_onto_seed_if_unbound(tool: OfficialTool) -> Result<bool, AppError> {
    let seed = tool.seed_provider_id();
    if has_catalog(tool, seed)? {
        return Ok(false);
    }
    let Some(blob) = read_live(tool)? else {
        return Ok(false);
    };
    if blob_looks_empty(&blob) {
        return Ok(false);
    }
    store::write_catalog(tool, seed, &blob)?;
    Ok(true)
}

pub fn live_looks_like_official_oauth(app_type: AppType, settings: &Value) -> bool {
    match app_type {
        AppType::KimiCode => kimi::settings_look_like_official(settings),
        AppType::Claude => claude::settings_look_like_official(settings),
        _ => false,
    }
}

pub fn access_token(blob: &Value, tool: OfficialTool) -> Option<String> {
    match tool {
        OfficialTool::Kimi => blob
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(str::to_string),
        OfficialTool::Claude => blob
            .get("claudeAiOauth")
            .or_else(|| blob.get("claude.ai_oauth"))
            .and_then(|entry| entry.get("accessToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(str::to_string),
    }
}

fn blob_looks_empty(blob: &Value) -> bool {
    match blob {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, remove_var, set_var};
    use serde_json::json;

    fn isolated_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = env_lock();
        let home = tempfile::tempdir().unwrap();
        set_var("OCHUB_TEST_HOME", home.path());
        crate::settings::reload_settings().ok();
        (home, guard)
    }

    fn cleanup() {
        remove_var("OCHUB_TEST_HOME");
        remove_var("KIMI_CODE_HOME");
        crate::settings::reload_settings().ok();
    }

    #[test]
    fn kimi_round_trip_binds_to_the_card_not_an_account_id() {
        let (_home, _guard) = isolated_home();
        let blob = json!({
            "access_token": "kimi-a",
            "refresh_token": "refresh-a",
            "expires_at": 9_999_999_999.0
        });
        write_live(OfficialTool::Kimi, &blob).unwrap();
        capture_live_to_card(OfficialTool::Kimi, "kimi-code-official").unwrap();

        let other = json!({
            "access_token": "kimi-b",
            "refresh_token": "refresh-b",
            "expires_at": 9_999_999_999.0
        });
        store::write_catalog(OfficialTool::Kimi, "card-b", &other).unwrap();
        materialize_card_if_present(OfficialTool::Kimi, "card-b").unwrap();

        let live = read_live(OfficialTool::Kimi).unwrap().unwrap();
        assert_eq!(live["access_token"], "kimi-b");
        let saved_a = read_catalog(OfficialTool::Kimi, "kimi-code-official")
            .unwrap()
            .unwrap();
        assert_eq!(saved_a["access_token"], "kimi-a");
        cleanup();
    }

    #[test]
    fn missing_catalog_clears_live() {
        let (_home, _guard) = isolated_home();
        let blob = json!({ "access_token": "keep-me", "refresh_token": "r" });
        write_live(OfficialTool::Kimi, &blob).unwrap();
        apply_card_to_live(OfficialTool::Kimi, "empty-card").unwrap();
        assert!(read_live(OfficialTool::Kimi).unwrap().is_none());
        cleanup();
    }

    #[test]
    fn adopt_seed_only_when_unbound() {
        let (_home, _guard) = isolated_home();
        write_live(
            OfficialTool::Kimi,
            &json!({ "access_token": "first", "refresh_token": "r" }),
        )
        .unwrap();
        assert!(adopt_live_onto_seed_if_unbound(OfficialTool::Kimi).unwrap());
        write_live(
            OfficialTool::Kimi,
            &json!({ "access_token": "second", "refresh_token": "r2" }),
        )
        .unwrap();
        assert!(!adopt_live_onto_seed_if_unbound(OfficialTool::Kimi).unwrap());
        let saved = read_catalog(OfficialTool::Kimi, OfficialTool::Kimi.seed_provider_id())
            .unwrap()
            .unwrap();
        assert_eq!(saved["access_token"], "first");
        cleanup();
    }

    #[test]
    fn claude_file_round_trip_under_test_home() {
        let (_home, _guard) = isolated_home();
        let blob = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat-a",
                "refreshToken": "sk-ant-ort-a",
                "expiresAt": 9_999_999_999_000u64
            }
        });
        write_live(OfficialTool::Claude, &blob).unwrap();
        capture_live_to_card(OfficialTool::Claude, "claude-official").unwrap();
        let other = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat-b",
                "refreshToken": "sk-ant-ort-b",
                "expiresAt": 9_999_999_999_000u64
            }
        });
        store::write_catalog(OfficialTool::Claude, "card-b", &other).unwrap();
        materialize_card_if_present(OfficialTool::Claude, "card-b").unwrap();
        let live = read_live(OfficialTool::Claude).unwrap().unwrap();
        assert_eq!(live["claudeAiOauth"]["accessToken"], "sk-ant-oat-b");
        cleanup();
    }

    #[test]
    fn switch_captures_outgoing_card_and_restores_incoming() {
        use std::sync::Arc;

        use crate::app_state::AppState;
        use crate::db::Database;
        use crate::model::Provider;
        use crate::services::provider::ProviderService;

        let (_home, _guard) = isolated_home();
        let state = AppState::new(Arc::new(Database::memory().unwrap()));
        state.db.init_default_official_providers().unwrap();
        state.db.init_official_quota_providers().unwrap();

        let mut card_b = Provider::with_id(
            "kimi-official-b".into(),
            "Kimi B".into(),
            json!({
                "default_model": "kimi-code/k3",
                "default_provider": "managed:kimi-code",
                "providers": {"managed:kimi-code": {
                    "type": "kimi",
                    "api_key": "",
                    "base_url": "https://api.kimi.com/coding/v1",
                    "oauth": {"storage": "file", "key": "oauth/kimi-code"}
                }},
                "models": {"kimi-code/k3": {
                    "provider": "managed:kimi-code",
                    "model": "k3",
                    "max_context_size": 1048576
                }}
            }),
            None,
        );
        card_b.category = Some("official".into());
        state
            .db
            .save_provider(AppType::KimiCode.as_str(), &card_b)
            .unwrap();

        write_live(
            OfficialTool::Kimi,
            &json!({ "access_token": "token-a", "refresh_token": "r-a" }),
        )
        .unwrap();
        crate::settings::set_current_provider(&AppType::KimiCode, Some("kimi-code-official"))
            .unwrap();
        state
            .db
            .set_current_provider(AppType::KimiCode.as_str(), "kimi-code-official")
            .unwrap();

        ProviderService::switch(&state, AppType::KimiCode, "kimi-official-b").unwrap();
        assert!(
            read_live(OfficialTool::Kimi).unwrap().is_none(),
            "a new official card must log the CLI out until the user logs in again"
        );
        assert_eq!(
            read_catalog(OfficialTool::Kimi, "kimi-code-official")
                .unwrap()
                .unwrap()["access_token"],
            "token-a"
        );

        write_live(
            OfficialTool::Kimi,
            &json!({ "access_token": "token-b", "refresh_token": "r-b" }),
        )
        .unwrap();
        ProviderService::switch(&state, AppType::KimiCode, "kimi-code-official").unwrap();
        assert_eq!(
            read_live(OfficialTool::Kimi).unwrap().unwrap()["access_token"],
            "token-a"
        );
        assert_eq!(
            read_catalog(OfficialTool::Kimi, "kimi-official-b")
                .unwrap()
                .unwrap()["access_token"],
            "token-b"
        );
        cleanup();
    }

    #[test]
    fn rejects_path_escape_in_provider_id() {
        let (_home, _guard) = isolated_home();
        let err = store::write_catalog(
            OfficialTool::Kimi,
            "../evil",
            &json!({ "access_token": "x" }),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("无效") || err.to_string().contains("invalid"),
            "{err}"
        );
        cleanup();
    }
}
