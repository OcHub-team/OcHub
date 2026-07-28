//! In-process, write-through usage cache for tray display.
//!
//! Ported from cc-switch `services/usage_cache.rs`. Not persisted; empties on
//! process restart and is refilled by the next usage query / tray hover.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::app_type::AppType;
use crate::model::UsageResult;
use crate::services::subscription::SubscriptionQuota;

#[derive(Default)]
pub struct UsageCache {
    subscription: RwLock<HashMap<AppType, SubscriptionQuota>>,
    script: RwLock<HashMap<(AppType, String), UsageResult>>,
}

impl UsageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_subscription(&self, app_type: AppType, quota: SubscriptionQuota) {
        if let Ok(mut w) = self.subscription.write() {
            w.insert(app_type, quota);
        }
    }

    pub fn put_script(&self, app_type: AppType, provider_id: String, result: UsageResult) {
        if let Ok(mut w) = self.script.write() {
            w.insert((app_type, provider_id), result);
        }
    }

    /// Expose a subscription snapshot by borrow, avoiding a deep clone.
    pub fn with_subscription<R>(
        &self,
        app_type: &AppType,
        f: impl FnOnce(&SubscriptionQuota) -> R,
    ) -> Option<R> {
        self.subscription
            .read()
            .ok()
            .and_then(|r| r.get(app_type).map(f))
    }

    /// Expose a usage-script result by borrow, avoiding a deep clone.
    pub fn with_script<R>(
        &self,
        app_type: &AppType,
        provider_id: &str,
        f: impl FnOnce(&UsageResult) -> R,
    ) -> Option<R> {
        self.script
            .read()
            .ok()
            .and_then(|r| r.get(&(*app_type, provider_id.to_string())).map(f))
    }

    pub fn invalidate_script(&self, app_type: &AppType, provider_id: &str) {
        let key = (*app_type, provider_id.to_string());
        if !self.script.read().is_ok_and(|r| r.contains_key(&key)) {
            return;
        }
        if let Ok(mut w) = self.script.write() {
            w.remove(&key);
        }
    }

    pub fn invalidate_subscription(&self, app_type: &AppType) {
        if !self
            .subscription
            .read()
            .is_ok_and(|r| r.contains_key(app_type))
        {
            return;
        }
        if let Ok(mut w) = self.subscription.write() {
            w.remove(app_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::subscription::CredentialStatus;

    fn fake_quota() -> SubscriptionQuota {
        SubscriptionQuota {
            tool: "claude".to_string(),
            credential_status: CredentialStatus::Valid,
            credential_message: None,
            success: true,
            tiers: vec![],
            extra_usage: None,
            error: None,
            queried_at: Some(0),
        }
    }

    fn fake_result() -> UsageResult {
        UsageResult {
            success: true,
            data: None,
            error: None,
        }
    }

    #[test]
    fn subscription_round_trip() {
        let cache = UsageCache::new();
        assert!(
            cache
                .with_subscription(&AppType::Claude, |q| q.success)
                .is_none()
        );
        cache.put_subscription(AppType::Claude, fake_quota());
        assert!(
            cache
                .with_subscription(&AppType::Claude, |q| q.success)
                .unwrap()
        );
        assert!(
            cache
                .with_subscription(&AppType::Codex, |q| q.success)
                .is_none()
        );
    }

    #[test]
    fn script_round_trip_and_invalidate() {
        let cache = UsageCache::new();
        assert!(
            cache
                .with_script(&AppType::Codex, "pid", |r| r.success)
                .is_none()
        );
        cache.put_script(AppType::Codex, "pid".to_string(), fake_result());
        assert!(
            cache
                .with_script(&AppType::Codex, "pid", |r| r.success)
                .is_some()
        );
        cache.invalidate_script(&AppType::Codex, "pid");
        assert!(
            cache
                .with_script(&AppType::Codex, "pid", |r| r.success)
                .is_none()
        );
    }

    #[test]
    fn script_keys_are_isolated_by_app_type() {
        let cache = UsageCache::new();
        cache.put_script(AppType::Claude, "same".to_string(), fake_result());
        assert!(
            cache
                .with_script(&AppType::Claude, "same", |r| r.success)
                .is_some()
        );
        assert!(
            cache
                .with_script(&AppType::Codex, "same", |r| r.success)
                .is_none()
        );
    }
}
