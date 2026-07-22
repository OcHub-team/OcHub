//! S3 auto-sync queue (the `notify_db_changed` consumer).
//!
//! Ported from cc-switch `services/s3_auto_sync.rs`. Tauri removed: status
//! updates are persisted in settings and surfaced through GPUI/API polling; the
//! worker is spawned on the ambient tokio runtime instead of
//! `tauri::async_runtime`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use crate::error::AppError;
use crate::services::s3_sync;
use crate::settings::{self, S3SyncSettings};

const AUTO_SYNC_DEBOUNCE_MS: u64 = 1000;
pub(crate) const MAX_AUTO_SYNC_WAIT_MS: u64 = 10_000;

static DB_CHANGE_TX: OnceLock<Sender<String>> = OnceLock::new();
static AUTO_SYNC_SUPPRESS_DEPTH: AtomicUsize = AtomicUsize::new(0);

// Suppression guard is used by download/import paths to avoid auto-sync feedback
// loops while applying a downloaded snapshot.
#[allow(dead_code)]
pub(crate) struct AutoSyncSuppressionGuard;

impl AutoSyncSuppressionGuard {
    #[allow(dead_code)]
    pub fn new() -> Self {
        AUTO_SYNC_SUPPRESS_DEPTH.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for AutoSyncSuppressionGuard {
    fn drop(&mut self) {
        let _ =
            AUTO_SYNC_SUPPRESS_DEPTH.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            });
    }
}

pub(crate) fn is_auto_sync_suppressed() -> bool {
    AUTO_SYNC_SUPPRESS_DEPTH.load(Ordering::SeqCst) > 0
}

pub fn should_trigger_for_table(table: &str) -> bool {
    let normalized = table.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "providers"
            | "provider_endpoints"
            | "mcp_servers"
            | "prompts"
            | "skills"
            | "skill_repos"
            | "settings"
            | "proxy_config"
    )
}

pub(crate) fn enqueue_change_signal(tx: &Sender<String>, table: &str) -> bool {
    match tx.try_send(table.to_string()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => false,
    }
}

pub(crate) fn auto_sync_wait_duration(started_at: Instant, now: Instant) -> Option<Duration> {
    let max_wait = Duration::from_millis(MAX_AUTO_SYNC_WAIT_MS);
    let debounce = Duration::from_millis(AUTO_SYNC_DEBOUNCE_MS);
    let elapsed = now.saturating_duration_since(started_at);
    if elapsed >= max_wait {
        return None;
    }
    Some(debounce.min(max_wait - elapsed))
}

fn should_run_auto_sync(settings: Option<&S3SyncSettings>) -> bool {
    let Some(sync) = settings else {
        return false;
    };
    sync.enabled && sync.auto_sync
}

fn persist_auto_sync_error(settings: &mut S3SyncSettings, error: &AppError) {
    settings.status.last_error = Some(error.to_string());
    settings.status.last_error_source = Some("auto".to_string());
    let _ = settings::update_s3_sync_status(settings.status.clone());
}

// cc-switch emitted a `s3-sync-status-updated` Tauri event here. OCHUB
// persists status in settings; GPUI and the HTTP API expose explicit refresh
// actions instead of using a Tauri event bus.
fn report_auto_sync_status(status: &str, error: Option<&str>) {
    match error {
        Some(message) => log::debug!("[S3][AutoSync] status={status} error={message}"),
        None => log::debug!("[S3][AutoSync] status={status}"),
    }
}

async fn run_auto_sync_upload(db: &crate::db::Database) -> Result<(), AppError> {
    let mut settings = settings::get_s3_sync_settings();
    if !should_run_auto_sync(settings.as_ref()) {
        return Ok(());
    }

    let mut sync_settings = match settings.take() {
        Some(value) => value,
        None => return Ok(()),
    };

    let result = s3_sync::run_with_sync_lock(s3_sync::upload(db, &mut sync_settings)).await;
    match result {
        Ok(_) => {
            report_auto_sync_status("success", None);
            Ok(())
        }
        Err(err) => {
            persist_auto_sync_error(&mut sync_settings, &err);
            report_auto_sync_status("error", Some(&err.to_string()));
            Err(err)
        }
    }
}

pub fn notify_db_changed(table: &str) {
    if is_auto_sync_suppressed() {
        return;
    }
    if !should_trigger_for_table(table) {
        return;
    }
    let Some(tx) = DB_CHANGE_TX.get() else {
        return;
    };
    let _ = enqueue_change_signal(tx, table);
}

/// Start the auto-sync worker. Must be called from within a tokio runtime.
pub fn start_worker(db: Arc<crate::db::Database>) {
    if DB_CHANGE_TX.get().is_some() {
        return;
    }

    // Buffer size 1 is enough: we only need "dirty" signals, not every event.
    let (tx, rx) = channel::<String>(1);
    if DB_CHANGE_TX.set(tx).is_err() {
        return;
    }

    // cc-switch used `tauri::async_runtime::spawn`. We rely on the ambient Tokio
    // runtime; callers must invoke `start_worker` from within one.
    tokio::spawn(async move {
        run_worker_loop(db, rx).await;
    });
}

async fn run_worker_loop(db: Arc<crate::db::Database>, mut rx: Receiver<String>) {
    while let Some(first_table) = rx.recv().await {
        let started_at = Instant::now();
        let mut merged_count = 1usize;

        while let Some(wait_for) = auto_sync_wait_duration(started_at, Instant::now()) {
            let timeout = tokio::time::timeout(wait_for, rx.recv()).await;

            match timeout {
                Ok(Some(_)) => merged_count += 1,
                Ok(None) => return,
                Err(_) => break,
            }
        }

        log::debug!(
            "[S3][AutoSync] Triggered by table={first_table}, merged_changes={merged_count}"
        );

        if let Err(err) = run_auto_sync_upload(&db).await {
            log::warn!("[S3][AutoSync] Upload failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auto_sync_wait_duration, enqueue_change_signal, is_auto_sync_suppressed,
        should_run_auto_sync, should_trigger_for_table, AutoSyncSuppressionGuard,
        MAX_AUTO_SYNC_WAIT_MS,
    };
    use crate::settings::S3SyncSettings;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::channel;

    #[test]
    fn should_trigger_sync_for_config_tables_only() {
        assert!(should_trigger_for_table("providers"));
        assert!(should_trigger_for_table("settings"));
        assert!(!should_trigger_for_table("proxy_request_logs"));
        assert!(!should_trigger_for_table("provider_health"));
    }

    #[test]
    fn suppression_guard_enables_and_restores_state() {
        assert!(!is_auto_sync_suppressed());
        {
            let _guard = AutoSyncSuppressionGuard::new();
            assert!(is_auto_sync_suppressed());
        }
        assert!(!is_auto_sync_suppressed());
    }

    #[test]
    fn max_wait_caps_flush_latency_for_continuous_events() {
        let started = Instant::now();
        let later = started + Duration::from_millis(MAX_AUTO_SYNC_WAIT_MS + 1);
        assert!(auto_sync_wait_duration(started, later).is_none());
    }

    #[tokio::test]
    async fn enqueue_change_signal_drops_when_channel_is_full() {
        let (tx, _rx) = channel::<String>(1);
        assert!(enqueue_change_signal(&tx, "providers"));
        assert!(!enqueue_change_signal(&tx, "providers"));
    }

    #[test]
    fn should_run_auto_sync_requires_enabled_and_auto_sync_flag() {
        assert!(!should_run_auto_sync(None));

        let disabled = S3SyncSettings {
            enabled: false,
            auto_sync: true,
            ..S3SyncSettings::default()
        };
        assert!(!should_run_auto_sync(Some(&disabled)));

        let auto_sync_off = S3SyncSettings {
            enabled: true,
            auto_sync: false,
            ..S3SyncSettings::default()
        };
        assert!(!should_run_auto_sync(Some(&auto_sync_off)));

        let enabled = S3SyncSettings {
            enabled: true,
            auto_sync: true,
            ..S3SyncSettings::default()
        };
        assert!(should_run_auto_sync(Some(&enabled)));
    }

    #[test]
    fn service_layer_does_not_depend_on_commands_layer() {
        let source = include_str!("s3_auto_sync.rs");
        let needle = ["crate", "commands", ""].join("::");
        assert!(
            !source.contains(&needle),
            "services layer should not depend on commands layer"
        );
    }
}
