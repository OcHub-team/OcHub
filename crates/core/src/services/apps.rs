//! App enable/disable service.
//!
//! Disabling an app leaves its live config files untouched, with one
//! exception: if the app is currently in proxy-takeover state, the original
//! config is restored (and the backup dropped) first, so the tool is never
//! left pointing at a proxy that no longer serves it. Data (providers, usage
//! history) is retained; re-enabling is just the settings flip.

use crate::app_id::AppId;
use crate::app_state::AppState;
use crate::app_type::AppType;
use crate::error::AppError;
use crate::plugin;

pub async fn set_app_enabled(state: &AppState, id: &AppId, enabled: bool) -> Result<(), AppError> {
    let target = plugin::get_plugin(id)
        .ok_or_else(|| AppError::InvalidInput(format!("未知的应用类型: {id}")))?;

    if !enabled {
        // Keep at least one app enabled.
        let remaining = plugin::enabled_plugins()
            .iter()
            .filter(|p| p.id() != id)
            .count();
        if plugin::is_app_enabled(target.as_ref()) && remaining == 0 {
            return Err(AppError::InvalidInput("至少保留一个启用的应用".to_string()));
        }

        // Takeover check: restore the original live config before letting go.
        if target.proxy().is_some_and(|p| p.supports_takeover) {
            if let Some(app_type) = AppType::from_app_id(id) {
                let has_backup = state
                    .db
                    .get_live_backup(app_type.as_str())
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                let taken_over = state
                    .proxy_service
                    .detect_takeover_in_live_config_for_app(&app_type);
                if has_backup || taken_over {
                    state
                        .proxy_service
                        .set_takeover_for_app(app_type.as_str(), false)
                        .await
                        .map_err(|e| AppError::Message(format!("停用前还原代理接管失败: {e}")))?;
                }
            }
        }
    }

    crate::settings::mutate_settings(|settings| {
        settings.set_app_enabled(id.as_str(), enabled);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::sync::Arc;

    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    fn test_home() -> HomeGuard {
        let lock = crate::test_support::env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("OCHUB_TEST_HOME", dir.path());
        crate::settings::reload_settings().unwrap();
        HomeGuard {
            _lock: lock,
            _dir: dir,
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            std::env::remove_var("OCHUB_TEST_HOME");
            let _ = crate::settings::reload_settings();
        }
    }

    #[tokio::test]
    async fn set_app_enabled_persists_and_enforces_min_one() {
        let _home = test_home();
        let db = Arc::new(Database::memory().unwrap());
        let state = AppState::new(db);

        let codex = AppId::from_static("codex");
        set_app_enabled(&state, &codex, false).await.unwrap();
        assert_eq!(
            crate::settings::get_settings().app_enabled("codex"),
            Some(false)
        );
        assert!(!plugin::is_app_type_enabled(&AppType::Codex));

        // Re-enable works.
        set_app_enabled(&state, &codex, true).await.unwrap();
        assert!(plugin::is_app_type_enabled(&AppType::Codex));

        // Unknown app id rejected.
        let bogus = AppId::parse("no-such-app").unwrap();
        assert!(set_app_enabled(&state, &bogus, false).await.is_err());

        // Disabling everything but one is fine; disabling the last enabled app fails.
        for id in ["claude", "claude-desktop", "gemini", "opencode", "openclaw"] {
            set_app_enabled(&state, &AppId::parse(id).unwrap(), false)
                .await
                .unwrap();
        }
        // codex is now the only enabled app (hermes is off by default).
        let err = set_app_enabled(&state, &codex, false).await.unwrap_err();
        assert!(err.to_string().contains("至少保留一个"), "{err}");
    }
}
