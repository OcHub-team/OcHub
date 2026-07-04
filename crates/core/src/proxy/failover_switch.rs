//! Failover-switch manager: dedup control for failover-triggered provider
//! switches. Ported from cc-switch `proxy/failover_switch.rs`.
//!
//! Tauri-specific side effects (tray-menu refresh and `provider-switched` event
//! emission) are replaced by updating the shared DB/local settings current
//! provider pointers; GPUI and HTTP callers read the new state through the
//! normal status/provider APIs.

use crate::app_type::AppType;
use crate::db::Database;
use crate::error::AppError;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Failover-switch manager.
#[derive(Clone)]
pub struct FailoverSwitchManager {
    /// In-flight switches (key = `"app_type:provider_id"`).
    pending_switches: Arc<RwLock<HashSet<String>>>,
    db: Arc<Database>,
}

impl FailoverSwitchManager {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            pending_switches: Arc::new(RwLock::new(HashSet::new())),
            db,
        }
    }

    /// Attempt a failover switch.
    ///
    /// Returns `Ok(true)` if the switch was executed, `Ok(false)` if a matching
    /// switch is already in-flight (skipped) or the app is not proxy-enabled.
    pub async fn try_switch(
        &self,
        app_type: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        let switch_key = format!("{app_type}:{provider_id}");

        {
            let mut pending = self.pending_switches.write().await;
            if pending.contains(&switch_key) {
                log::debug!(
                    "[Failover] switch already in flight, skipping: {app_type} -> {provider_id}"
                );
                return Ok(false);
            }
            pending.insert(switch_key.clone());
        }

        let result = self.do_switch(app_type, provider_id, provider_name).await;

        {
            let mut pending = self.pending_switches.write().await;
            pending.remove(&switch_key);
        }

        result
    }

    async fn do_switch(
        &self,
        app_type: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        let app_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.enabled,
            Err(e) => {
                log::warn!("[FO-002] cannot read {app_type} config: {e}, skipping switch");
                return Ok(false);
            }
        };

        if !app_enabled {
            log::debug!("[Failover] {app_type} proxy not enabled, skipping switch");
            return Ok(false);
        }

        log::info!("[FO-001] failover switch: {app_type} -> {provider_name}");

        let app_type_enum = AppType::from_str(app_type)
            .map_err(|_| AppError::InvalidInput(format!("invalid app type: {app_type}")))?;
        let provider = self
            .db
            .get_provider_by_id(provider_id, app_type)?
            .ok_or_else(|| AppError::InvalidInput(format!("provider not found: {provider_id}")))?;
        if provider.category.as_deref() == Some("official") {
            return Err(AppError::InvalidInput(
                "cannot switch to official provider during proxy takeover".to_string(),
            ));
        }

        let changed = crate::settings::get_effective_current_provider(&self.db, &app_type_enum)?
            .as_deref()
            != Some(provider_id);

        self.db.set_current_provider(app_type, provider_id)?;
        crate::settings::set_current_provider(&app_type_enum, Some(provider_id))?;

        Ok(changed)
    }
}
