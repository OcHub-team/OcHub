//! Global application state. Ported from cc-switch `store.rs`.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::app_type::AppType;
use crate::db::Database;
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::providers::copilot_auth::CopilotAuthManager;
use crate::services::provider::ProviderService;
use crate::services::{ProxyService, UsageCache};

/// Drive a future to completion on the current thread without a Tokio runtime.
///
/// Only sound for futures that never register a real waker / return `Pending`
/// pending external IO (used here for synchronous, std-mutex-guarded DB work).
/// This lets `bootstrap` run the same code whether or not it is called from
/// inside a Tokio runtime, where `block_in_place` / a nested `Runtime` panic.
fn block_on_sync<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop_raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(std::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Global application state shared across the switching layer and writers.
pub struct AppState {
    pub db: Arc<Database>,
    pub proxy_service: ProxyService,
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
        let proxy_service =
            ProxyService::new(db.clone(), copilot_auth.clone(), codex_oauth.clone());

        Self {
            db,
            proxy_service,
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
    /// Order matters: import existing live configs as the `default` provider
    /// (set current) *before* seeding official presets, so switching to an
    /// official preset never clobbers the user's original live config.
    pub fn bootstrap(&self) {
        match self.db.init_default_skill_repos() {
            Ok(count) if count > 0 => log::info!("seeded {count} default skill repositories"),
            Ok(_) => {}
            Err(e) => log::warn!("failed to seed default skill repos: {e}"),
        }

        for app_type in AppType::all().filter(|t| !t.is_additive_mode()) {
            if ProviderService::should_import_default_config_on_startup(self, &app_type)
                .unwrap_or(false)
            {
                match ProviderService::import_default_config(self, app_type) {
                    Ok(true) => {
                        log::info!("imported live config for {} as default", app_type.as_str())
                    }
                    Ok(false) => {}
                    Err(e) => {
                        log::debug!("no live config to import for {}: {e}", app_type.as_str())
                    }
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

        // LEGACY one-time recovery: restore a stranded Gemini takeover left over
        // from before Gemini app support was removed (the normal restore paths
        // only iterate Claude + Codex, so an active pre-upgrade Gemini takeover
        // would otherwise poison `~/.gemini/.env` forever). No-op when there is
        // no 'gemini' live-backup row.
        //
        // Driven with a runtime-independent poll because `bootstrap` runs both
        // outside a Tokio runtime (GPUI app) and inside one (`#[tokio::main]`
        // server / current-thread `#[tokio::test]`), where the usual bridges
        // (`block_in_place`, a nested `Runtime`) panic. The restore future only
        // touches std-mutex-guarded DB access and synchronous fs writes, so it
        // never yields and completes on the first poll.
        if let Err(e) = block_on_sync(self.proxy_service.restore_legacy_gemini_takeover()) {
            log::warn!("legacy gemini takeover restore failed: {e}");
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
