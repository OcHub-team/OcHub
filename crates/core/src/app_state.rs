//! Global application state. Ported from cc-switch `store.rs`.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::app_type::AppType;
use crate::db::Database;
use crate::managed_auth::codex_oauth_auth::CodexOAuthManager;
use crate::managed_auth::copilot_auth::CopilotAuthManager;
use crate::services::provider::ProviderService;
use crate::services::UsageCache;

/// Global application state shared across the switching layer and writers.
pub struct AppState {
    pub db: Arc<Database>,
    /// Local relay gateway (standing multi-dialect server + channel routing).
    pub gateway: Arc<crate::gateway::GatewayService>,
    pub usage_cache: Arc<UsageCache>,
    /// GitHub Copilot OAuth manager (managed-account device flow + token cache).
    /// Mirrors cc-switch's Tauri-managed `CopilotAuthState`.
    pub copilot_auth: Arc<RwLock<CopilotAuthManager>>,
    /// Codex / ChatGPT OAuth manager (managed-account device flow + refresh).
    /// Mirrors cc-switch's Tauri-managed `CodexOAuthState`.
    pub codex_oauth: Arc<RwLock<CodexOAuthManager>>,
}

impl AppState {
    /// Create a new application state from a shared database handle.
    pub fn new(db: Arc<Database>) -> Self {
        // Account stores persist under the app config dir as
        // `copilot_auth.json` / `codex_oauth_auth.json`, matching cc-switch
        // (managers load synchronously from disk on construction).
        let auth_dir = crate::paths::get_app_config_dir();
        let copilot_auth = Arc::new(RwLock::new(CopilotAuthManager::new(auth_dir.clone())));
        let codex_oauth = Arc::new(RwLock::new(CodexOAuthManager::new(auth_dir)));
        let gateway = Arc::new(crate::gateway::GatewayService::new(db.clone()));

        Self {
            db,
            gateway,
            usage_cache: Arc::new(UsageCache::new()),
            copilot_auth,
            codex_oauth,
        }
    }

    /// First-launch / every-launch seeding, mirroring the cc-switch `lib.rs`
    /// bootstrap. Idempotent: each step is internally guarded (table-empty
    /// checks, "already has providers" checks), so it is safe to run on every
    /// startup.
    ///
    /// Order matters: discover existing live configs *before* seeding official
    /// presets, so switching to an official preset never clobbers the user's
    /// original live config. Discovery is repeated on every startup and only
    /// adds provider ids that OcHub does not already manage.
    pub fn bootstrap(&self) {
        if let Err(error) = crate::settings::enable_extended_managed_apps_once() {
            log::warn!("failed to enable newly supported managed apps: {error}");
        }

        match self.db.init_default_skill_repos() {
            Ok(count) if count > 0 => log::info!("seeded {count} default skill repositories"),
            Ok(_) => {}
            Err(e) => log::warn!("failed to seed default skill repos: {e}"),
        }

        // Register user manifest plugins before the provider import loop so any
        // manifest app participates in startup import like the built-ins.
        for err in crate::plugin::load_and_register_user_plugins() {
            log::warn!("failed to load user plugin {}: {}", err.path, err.message);
        }

        for app_type in AppType::all().filter(crate::plugin::registry::is_app_type_enabled) {
            match ProviderService::auto_import_live_providers(self, app_type) {
                Ok(count) if count > 0 => log::info!(
                    "automatically discovered {count} provider(s) from {}",
                    app_type.as_str()
                ),
                Ok(_) => {}
                Err(e) => {
                    log::debug!(
                        "no live providers to discover for {}: {e}",
                        app_type.as_str()
                    )
                }
            }
        }

        match self.db.init_default_official_providers() {
            Ok(count) if count > 0 => log::info!("seeded {count} official provider(s)"),
            Ok(_) => {}
            Err(e) => log::warn!("failed to seed official providers: {e}"),
        }

        match self.db.backfill_missing_usage_costs() {
            Ok(count) if count > 0 => {
                log::info!("backfilled historical usage costs for {count} row(s)")
            }
            Ok(_) => {}
            Err(e) => log::warn!("failed to backfill historical usage costs: {e}"),
        }

        // A v2 installation may have exited while the retired local proxy had
        // rewritten Claude/Codex/Gemini live files. Reapply the current
        // provider once after migration so no tool remains pointed at a dead
        // loopback listener. Keep the flag on failure for the next launch.
        if self
            .db
            .get_bool_flag("legacy_proxy_cleanup_pending")
            .unwrap_or(false)
        {
            match crate::services::provider::live::restore_live_after_legacy_local_routing(self) {
                Ok(()) => {
                    if let Err(error) = self.db.set_setting("legacy_proxy_cleanup_pending", "false")
                    {
                        log::warn!("failed to finish legacy routing cleanup: {error}");
                    } else {
                        log::info!("restored live configs after removing legacy local routing");
                    }
                }
                Err(error) => {
                    log::warn!("legacy local-routing cleanup will retry next launch: {error}")
                }
            }
        }

        let db_for_codex_history_migration = self.db.clone();
        let _ = std::thread::Builder::new()
            .name("codex-history-migrations".into())
            .spawn(move || {
                match crate::services::codex_history_migration::maybe_migrate_codex_third_party_history_provider_bucket(
                    &db_for_codex_history_migration,
                ) {
                    Ok(outcome) => {
                        if let Some(reason) = outcome.skipped_reason {
                            log::debug!(
                                "Codex history provider bucket migration skipped: {reason}"
                            );
                        } else {
                            log::info!(
                                "Codex history provider bucket migration completed: sources={}, jsonl_files={}, state_rows={}",
                                outcome.source_provider_ids.len(),
                                outcome.migrated_jsonl_files,
                                outcome.migrated_state_rows
                            );
                        }
                    }
                    Err(e) => log::warn!(
                        "Codex history provider bucket migration failed: {e}"
                    ),
                }

                match crate::services::codex_history_migration::maybe_migrate_codex_provider_template_bucket(
                    &db_for_codex_history_migration,
                ) {
                    Ok(outcome) => {
                        if let Some(reason) = outcome.skipped_reason {
                            log::debug!(
                                "Codex provider template bucket migration skipped: {reason}"
                            );
                        } else if !outcome.migrated_provider_ids.is_empty() {
                            log::info!(
                                "Codex provider template bucket migration completed: providers={}",
                                outcome.migrated_provider_ids.len()
                            );
                        }
                    }
                    Err(e) => log::warn!(
                        "Codex provider template bucket migration failed: {e}"
                    ),
                }

                match crate::services::codex_history_migration::maybe_migrate_codex_official_history_to_unified_bucket() {
                    Ok(outcome) => {
                        if let Some(reason) = outcome.skipped_reason {
                            log::debug!(
                                "Codex official history unify migration skipped: {reason}"
                            );
                        } else {
                            log::info!(
                                "Codex official history unify migration completed: jsonl_files={}, state_rows={}",
                                outcome.migrated_jsonl_files,
                                outcome.migrated_state_rows
                            );
                        }
                    }
                    Err(e) => log::warn!(
                        "Codex official history unify migration failed: {e}"
                    ),
                }
            });
    }
}
