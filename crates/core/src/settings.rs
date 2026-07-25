//! Device-level settings (`~/.ochub/settings.json`). Not synced with the
//! database, so multiple devices operate independently under cloud sync.
//! Ported from cc-switch `settings.rs`.

use std::fs;
#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::app_type::AppType;
use crate::error::AppError;

// ---------------------------------------------------------------------------
// Skill sync enums (kept here; re-exported from the skill service later)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncMethod {
    /// Auto: prefer symlink, fall back to copy.
    #[default]
    Auto,
    Symlink,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillStorageLocation {
    /// OcHub-managed directory (`~/.ochub/skills/`).
    #[default]
    #[serde(rename = "ochub", alias = "cc_switch")]
    Ochub,
    /// Unified Agent Skills dir (`~/.agents/skills/`).
    Unified,
}

/// How the selected theme family chooses between its required light and dark palettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Follow the native window appearance.
    #[default]
    System,
    /// Always use the family's light palette.
    Light,
    /// Always use the family's dark palette.
    Dark,
}

fn default_theme_family() -> String {
    "ochub".to_string()
}

/// Custom endpoint record (stored in `provider.meta.custom_endpoints`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEndpoint {
    pub url: String,
    pub added_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
}

fn default_true() -> bool {
    true
}

/// Deserialize the per-app enabled map, normalizing legacy id spellings
/// (`claudeDesktop` / `claude_desktop` → `claude-desktop`). Accepts the legacy
/// `visibleApps` object unchanged — its keys are already app-id strings.
fn deserialize_enabled_apps<'de, D>(
    deserializer: D,
) -> Result<Option<std::collections::BTreeMap<String, bool>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<std::collections::BTreeMap<String, bool>>::deserialize(deserializer)?;
    Ok(raw.map(|map| {
        map.into_iter()
            .map(|(key, value)| {
                let key = match crate::app_id::AppId::parse(&key) {
                    Ok(id) => id.as_str().to_string(),
                    // Preserve unknown/invalid keys rather than dropping user data.
                    Err(_) => key,
                };
                (key, value)
            })
            .collect()
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebDavSyncStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_local_manifest_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_manifest_hash: Option<String>,
}

fn default_remote_root() -> String {
    "ochub-sync".to_string()
}
fn default_profile() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavSyncSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_remote_root")]
    pub remote_root: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub status: WebDavSyncStatus,
}

impl Default for WebDavSyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_sync: false,
            base_url: String::new(),
            username: String::new(),
            password: String::new(),
            remote_root: default_remote_root(),
            profile: default_profile(),
            status: WebDavSyncStatus::default(),
        }
    }
}

impl WebDavSyncSettings {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.base_url.trim().is_empty() {
            return Err(AppError::localized(
                "webdav.base_url.required",
                "WebDAV 地址不能为空",
                "WebDAV URL is required.",
            ));
        }
        if self.username.trim().is_empty() {
            return Err(AppError::localized(
                "webdav.username.required",
                "WebDAV 用户名不能为空",
                "WebDAV username is required.",
            ));
        }
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.base_url = self.base_url.trim().to_string();
        self.username = self.username.trim().to_string();
        self.remote_root = self.remote_root.trim().to_string();
        self.profile = self.profile.trim().to_string();
        if self.remote_root.is_empty() {
            self.remote_root = default_remote_root();
        }
        if self.profile.is_empty() {
            self.profile = default_profile();
        }
    }

    fn is_empty(&self) -> bool {
        self.base_url.is_empty() && self.username.is_empty() && self.password.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3SyncSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_remote_root")]
    pub remote_root: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub status: WebDavSyncStatus,
}

impl Default for S3SyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_sync: false,
            region: String::new(),
            bucket: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            endpoint: String::new(),
            remote_root: default_remote_root(),
            profile: default_profile(),
            status: WebDavSyncStatus::default(),
        }
    }
}

impl S3SyncSettings {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.bucket.trim().is_empty() {
            return Err(AppError::localized(
                "s3.bucket.required",
                "S3 存储桶不能为空",
                "S3 bucket is required.",
            ));
        }
        if self.region.trim().is_empty() {
            return Err(AppError::localized(
                "s3.region.required",
                "S3 区域不能为空",
                "S3 region is required.",
            ));
        }
        if self.access_key_id.trim().is_empty() {
            return Err(AppError::localized(
                "s3.access_key_id.required",
                "S3 Access Key ID 不能为空",
                "S3 Access Key ID is required.",
            ));
        }
        if self.secret_access_key.trim().is_empty() {
            return Err(AppError::localized(
                "s3.secret_access_key.required",
                "S3 Secret Access Key 不能为空",
                "S3 Secret Access Key is required.",
            ));
        }
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.region = self.region.trim().to_string();
        self.bucket = self.bucket.trim().to_string();
        self.access_key_id = self.access_key_id.trim().to_string();
        self.endpoint = self.endpoint.trim().to_string();
        self.remote_root = self.remote_root.trim().to_string();
        self.profile = self.profile.trim().to_string();
        if self.remote_root.is_empty() {
            self.remote_root = default_remote_root();
        }
        if self.profile.is_empty() {
            self.profile = default_profile();
        }
    }

    fn is_empty(&self) -> bool {
        self.bucket.is_empty()
            && self.region.is_empty()
            && self.access_key_id.is_empty()
            && self.secret_access_key.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LocalMigrations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_third_party_history_provider_bucket_v1:
        Option<CodexThirdPartyHistoryProviderBucketMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_provider_template_v1: Option<CodexProviderTemplateMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_official_history_unify_v1: Option<CodexOfficialHistoryUnifyMigration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extended_managed_apps_v1: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexThirdPartyHistoryProviderBucketMigration {
    pub completed_at: String,
    pub target_provider_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_provider_ids: Vec<String>,
    #[serde(default)]
    pub migrated_jsonl_files: usize,
    #[serde(default)]
    pub migrated_state_rows: usize,
    #[serde(default)]
    pub scanned_history_files: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexProviderTemplateMigration {
    pub completed_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrated_provider_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOfficialHistoryUnifyMigration {
    pub completed_at: String,
    pub target_provider_id: String,
    #[serde(default)]
    pub migrated_jsonl_files: usize,
    #[serde(default)]
    pub migrated_state_rows: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_config_dir: Option<String>,
}

/// Device-level settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default = "default_true")]
    pub show_in_tray: bool,
    #[serde(default = "default_true")]
    pub minimize_to_tray_on_close: bool,
    #[serde(default)]
    pub enable_claude_plugin_integration: bool,
    #[serde(default)]
    pub skip_claude_onboarding: bool,
    #[serde(default)]
    pub launch_on_startup: bool,
    #[serde(default)]
    pub silent_startup: bool,
    /// Check GitHub for a newer release shortly after launch and daily after.
    /// On by default: a stale desktop app is how users end up on known bugs.
    #[serde(default = "default_true")]
    pub auto_update_check: bool,
    /// A version the user dismissed. Suppresses the notification for that
    /// version only, so the next release still gets through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_update_version: Option<String>,
    /// Unix seconds of the last completed check, used to space them out across
    /// restarts rather than checking on every launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_check_confirmed: Option<bool>,
    #[serde(default)]
    pub preserve_codex_official_auth_on_switch: bool,
    #[serde(default)]
    pub unify_codex_session_history: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unify_codex_migrate_existing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_notice_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_config_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sync_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default = "default_theme_family")]
    pub theme_family: String,
    #[serde(default)]
    pub theme_mode: ThemeMode,

    /// Per-app enabled map keyed by app id. Missing key → the plugin's
    /// `enabled_by_default()`. Reads the legacy `visibleApps` object via alias.
    #[serde(
        default,
        alias = "visibleApps",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_enabled_apps"
    )]
    pub enabled_apps: Option<std::collections::BTreeMap<String, bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grokbuild_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openclaw_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hermes_config_dir: Option<String>,

    /// Config-dir overrides for manifest apps, keyed by app id. Built-in apps
    /// keep their dedicated `*_config_dir` fields above; manifest-driven plugins
    /// (which have no static field) read their override from this map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_config_dirs: Option<std::collections::BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_claude_desktop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_codex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_grokbuild: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_gemini: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_opencode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_openclaw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_hermes: Option<String>,

    #[serde(default)]
    pub skill_sync_method: SyncMethod,
    #[serde(default)]
    pub skill_storage_location: SkillStorageLocation,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_sync: Option<WebDavSyncSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_sync: Option<S3SyncSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webdav_backup: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_interval_hours: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_retain_count: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_terminal: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_migrations: Option<LocalMigrations>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            show_in_tray: true,
            minimize_to_tray_on_close: true,
            enable_claude_plugin_integration: false,
            skip_claude_onboarding: false,
            launch_on_startup: false,
            silent_startup: false,
            auto_update_check: true,
            skipped_update_version: None,
            last_update_check_at: None,
            usage_confirmed: None,
            stream_check_confirmed: None,
            preserve_codex_official_auth_on_switch: false,
            unify_codex_session_history: false,
            unify_codex_migrate_existing: None,
            first_run_notice_confirmed: None,
            common_config_confirmed: None,
            auto_sync_confirmed: None,
            language: None,
            theme_family: default_theme_family(),
            theme_mode: ThemeMode::default(),
            enabled_apps: None,
            claude_config_dir: None,
            codex_config_dir: None,
            grokbuild_config_dir: None,
            gemini_config_dir: None,
            opencode_config_dir: None,
            openclaw_config_dir: None,
            hermes_config_dir: None,
            app_config_dirs: None,
            current_provider_claude: None,
            current_provider_claude_desktop: None,
            current_provider_codex: None,
            current_provider_grokbuild: None,
            current_provider_gemini: None,
            current_provider_opencode: None,
            current_provider_openclaw: None,
            current_provider_hermes: None,
            skill_sync_method: SyncMethod::default(),
            skill_storage_location: SkillStorageLocation::default(),
            webdav_sync: None,
            s3_sync: None,
            webdav_backup: None,
            backup_interval_hours: None,
            backup_retain_count: None,
            preferred_terminal: None,
            local_migrations: None,
        }
    }
}

impl AppSettings {
    /// Explicit enabled state for an app id, if the user ever toggled it.
    /// `None` means "use the plugin's default".
    pub fn app_enabled(&self, id: &str) -> Option<bool> {
        self.enabled_apps.as_ref()?.get(id).copied()
    }

    pub fn set_app_enabled(&mut self, id: &str, enabled: bool) {
        self.enabled_apps
            .get_or_insert_with(Default::default)
            .insert(id.to_string(), enabled);
    }

    /// Config-dir override for a manifest app id, if the user set one.
    ///
    /// Trims the value, treats empty as unset, and expands a leading `~/`
    /// (or a bare `~`) via [`crate::paths::get_home_dir`] so tests honoring
    /// `OCHUB_TEST_HOME` resolve correctly.
    pub fn app_config_dir_override(&self, id: &str) -> Option<PathBuf> {
        let raw = self.app_config_dirs.as_ref()?.get(id)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed == "~" {
            return Some(crate::paths::get_home_dir());
        }
        if let Some(stripped) = trimmed.strip_prefix("~/") {
            return Some(crate::paths::get_home_dir().join(stripped));
        }
        Some(PathBuf::from(trimmed))
    }

    fn settings_path() -> Option<PathBuf> {
        Some(
            crate::paths::get_home_dir()
                .join(".ochub")
                .join("settings.json"),
        )
    }

    fn normalize_one(field: &mut Option<String>) {
        *field = field
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }

    fn normalize_paths(&mut self) {
        Self::normalize_one(&mut self.claude_config_dir);
        Self::normalize_one(&mut self.codex_config_dir);
        Self::normalize_one(&mut self.grokbuild_config_dir);
        Self::normalize_one(&mut self.gemini_config_dir);
        Self::normalize_one(&mut self.opencode_config_dir);
        Self::normalize_one(&mut self.openclaw_config_dir);
        Self::normalize_one(&mut self.hermes_config_dir);

        // An *enabled* destination is a user decision, so it survives even
        // before any credentials exist — dropping it here would make picking a
        // sync destination and then filling in the form impossible, because the
        // choice would vanish on the way to disk. Only a never-configured,
        // never-enabled block is discarded.
        if let Some(sync) = &mut self.webdav_sync {
            sync.normalize();
            if sync.is_empty() && !sync.enabled {
                self.webdav_sync = None;
            }
        }
        if let Some(s3) = &mut self.s3_sync {
            s3.normalize();
            if s3.is_empty() && !s3.enabled {
                self.s3_sync = None;
            }
        }
    }

    fn load_from_file() -> Self {
        let Some(path) = Self::settings_path() else {
            return Self::default();
        };
        if let Ok(content) = fs::read_to_string(&path) {
            match serde_json::from_str::<AppSettings>(&content) {
                Ok(mut settings) => {
                    settings.normalize_paths();
                    settings
                }
                Err(err) => {
                    log::warn!(
                        "解析设置文件失败，将使用默认设置。路径: {}, 错误: {}",
                        path.display(),
                        err
                    );
                    Self::default()
                }
            }
        } else {
            Self::default()
        }
    }
}

fn save_settings_file(settings: &AppSettings) -> Result<(), AppError> {
    let mut normalized = settings.clone();
    normalized.normalize_paths();
    let Some(path) = AppSettings::settings_path() else {
        return Err(AppError::Config("无法获取用户主目录".to_string()));
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let json = serde_json::to_string_pretty(&normalized)
        .map_err(|e| AppError::JsonSerialize { source: e })?;

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| AppError::io(&path, e))?;
        file.write_all(json.as_bytes())
            .map_err(|e| AppError::io(&path, e))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&path, json).map_err(|e| AppError::io(&path, e))?;
    }
    Ok(())
}

static SETTINGS_STORE: OnceLock<RwLock<AppSettings>> = OnceLock::new();

fn settings_store() -> &'static RwLock<AppSettings> {
    SETTINGS_STORE.get_or_init(|| RwLock::new(AppSettings::load_from_file()))
}

// ===== 备份策略管理函数 =====

/// Get the effective auto-backup interval in hours (default 24).
///
/// Ported from cc-switch `settings.rs`.
pub fn effective_backup_interval_hours() -> u32 {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .backup_interval_hours
        .unwrap_or(24)
}

/// Get the effective backup retain count (default 10, minimum 1).
///
/// Ported from cc-switch `settings.rs`.
pub fn effective_backup_retain_count() -> usize {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .backup_retain_count
        .map(|n| (n as usize).max(1))
        .unwrap_or(10)
}

fn resolve_override_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if let Some(stripped) = raw.strip_prefix("~\\") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(raw)
}

pub fn get_settings() -> AppSettings {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .clone()
}

/// Read one app-enabled override without cloning the full settings graph.
///
/// Plugin checks are common UI metadata lookups; routing them through
/// [`get_settings`] used to clone sync credentials, migration state, path
/// overrides, and every other setting for a single boolean.
pub(crate) fn app_enabled_override(id: &str) -> Option<bool> {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .app_enabled(id)
}

/// Snapshot only the enabled-app map for one-pass plugin filtering.
pub(crate) fn enabled_apps_snapshot() -> Option<std::collections::BTreeMap<String, bool>> {
    settings_store()
        .read()
        .unwrap_or_else(|e| {
            log::warn!("设置锁已毒化，使用恢复值: {e}");
            e.into_inner()
        })
        .enabled_apps
        .clone()
}

/// Settings with secrets redacted, for sending to the UI.
pub fn get_settings_for_frontend() -> AppSettings {
    let mut settings = get_settings();
    if let Some(sync) = &mut settings.webdav_sync {
        sync.password.clear();
    }
    if let Some(s3) = &mut settings.s3_sync {
        s3.secret_access_key.clear();
    }
    settings.webdav_backup = None;
    settings
}

pub fn update_settings(mut new_settings: AppSettings) -> Result<(), AppError> {
    new_settings.normalize_paths();
    save_settings_file(&new_settings)?;
    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    *guard = new_settings;
    Ok(())
}

pub fn mutate_settings<F>(mutator: F) -> Result<(), AppError>
where
    F: FnOnce(&mut AppSettings),
{
    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    let mut next = guard.clone();
    mutator(&mut next);
    next.normalize_paths();
    save_settings_file(&next)?;
    *guard = next;
    Ok(())
}

/// One-time rollout for clients that became first-class managed apps after the
/// initial four-app release. Once marked, later user toggles are preserved.
pub fn enable_extended_managed_apps_once() -> Result<(), AppError> {
    if get_settings()
        .local_migrations
        .as_ref()
        .and_then(|migrations| migrations.extended_managed_apps_v1)
        .unwrap_or(false)
    {
        return Ok(());
    }

    mutate_settings(|settings| {
        for app_id in ["grokbuild", "openclaw", "hermes"] {
            settings.set_app_enabled(app_id, true);
        }
        settings
            .local_migrations
            .get_or_insert_with(Default::default)
            .extended_managed_apps_v1 = Some(true);
    })
}

/// Reload settings from disk into the in-memory cache.
pub fn reload_settings() -> Result<(), AppError> {
    let fresh = AppSettings::load_from_file();
    let mut guard = settings_store().write().unwrap_or_else(|e| {
        log::warn!("设置锁已毒化，使用恢复值: {e}");
        e.into_inner()
    });
    *guard = fresh;
    Ok(())
}

macro_rules! override_getter {
    ($name:ident, $field:ident) => {
        pub fn $name() -> Option<PathBuf> {
            let settings = settings_store().read().ok()?;
            settings.$field.as_ref().map(|p| resolve_override_path(p))
        }
    };
}

override_getter!(get_claude_override_dir, claude_config_dir);
override_getter!(get_codex_override_dir, codex_config_dir);
override_getter!(get_grokbuild_override_dir, grokbuild_config_dir);
override_getter!(get_opencode_override_dir, opencode_config_dir);
override_getter!(get_openclaw_override_dir, openclaw_config_dir);
override_getter!(get_hermes_override_dir, hermes_config_dir);

pub fn preserve_codex_official_auth_on_switch() -> bool {
    get_settings().preserve_codex_official_auth_on_switch
}

pub fn unify_codex_session_history() -> bool {
    get_settings().unify_codex_session_history
}

pub fn unify_codex_migrate_existing_requested() -> bool {
    get_settings().unify_codex_migrate_existing.unwrap_or(false)
}

pub fn clear_codex_unify_migrate_existing() -> Result<(), AppError> {
    mutate_settings(|settings| {
        settings.unify_codex_migrate_existing = None;
    })
}

// ----- per-app current provider (device-level) -----

pub fn get_current_provider(app_type: &AppType) -> Option<String> {
    let settings = settings_store().read().ok()?;
    match app_type {
        AppType::Claude => settings.current_provider_claude.clone(),
        AppType::ClaudeDesktop => settings.current_provider_claude_desktop.clone(),
        AppType::Codex => settings.current_provider_codex.clone(),
        AppType::GrokBuild => settings.current_provider_grokbuild.clone(),
        AppType::OpenCode => settings.current_provider_opencode.clone(),
        AppType::OpenClaw => settings.current_provider_openclaw.clone(),
        AppType::Hermes => settings.current_provider_hermes.clone(),
    }
}

pub fn set_current_provider(app_type: &AppType, id: Option<&str>) -> Result<(), AppError> {
    let id_owned = id.map(|s| s.to_string());
    mutate_settings(|settings| match app_type {
        AppType::Claude => settings.current_provider_claude = id_owned.clone(),
        AppType::ClaudeDesktop => settings.current_provider_claude_desktop = id_owned.clone(),
        AppType::Codex => settings.current_provider_codex = id_owned.clone(),
        AppType::GrokBuild => settings.current_provider_grokbuild = id_owned.clone(),
        AppType::OpenCode => settings.current_provider_opencode = id_owned.clone(),
        AppType::OpenClaw => settings.current_provider_openclaw = id_owned.clone(),
        AppType::Hermes => settings.current_provider_hermes = id_owned.clone(),
    })
}

/// Resolve the effective current provider id for an app, validating against the DB.
///
/// Ported verbatim from cc-switch `settings.rs::get_effective_current_provider`.
///
/// 1. Read the device-level current provider id from local settings.
/// 2. Validate it exists in the database.
/// 3. If missing, clear the local setting and fall back to `db.get_current_provider`.
pub fn get_effective_current_provider(
    db: &crate::db::Database,
    app_type: &AppType,
) -> Result<Option<String>, AppError> {
    // 1. Read from local settings
    if let Some(local_id) = get_current_provider(app_type) {
        // 2. Validate it exists in the database
        let providers = db.get_all_providers(app_type.as_str())?;
        if providers.contains_key(&local_id) {
            return Ok(Some(local_id));
        }

        // 3. Not present: clear the local setting
        log::warn!(
            "本地 settings 中的供应商 {} ({}) 在数据库中不存在，将清理并 fallback 到数据库",
            local_id,
            app_type.as_str()
        );
        let _ = set_current_provider(app_type, None);
    }

    // Fallback to the database is_current flag
    db.get_current_provider(app_type.as_str())
}

pub fn get_skill_sync_method() -> SyncMethod {
    get_settings().skill_sync_method
}

pub fn get_skill_storage_location() -> SkillStorageLocation {
    get_settings().skill_storage_location
}

pub fn set_skill_storage_location(location: SkillStorageLocation) -> Result<(), AppError> {
    mutate_settings(|settings| {
        settings.skill_storage_location = location;
    })
}

// ----- codex local-migration markers -----

pub fn is_codex_third_party_history_provider_bucket_migrated() -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|m| m.codex_third_party_history_provider_bucket_v1.as_ref())
        .is_some_and(|m| m.scanned_history_files)
}

pub fn mark_codex_third_party_history_provider_bucket_migrated(
    migration: CodexThirdPartyHistoryProviderBucketMigration,
) -> Result<(), AppError> {
    mutate_settings(|settings| {
        settings
            .local_migrations
            .get_or_insert_with(Default::default)
            .codex_third_party_history_provider_bucket_v1 = Some(migration);
    })
}

pub fn is_codex_provider_template_migrated() -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|m| m.codex_provider_template_v1.as_ref())
        .is_some()
}

pub fn mark_codex_provider_template_migrated(
    migration: CodexProviderTemplateMigration,
) -> Result<(), AppError> {
    mutate_settings(|settings| {
        settings
            .local_migrations
            .get_or_insert_with(Default::default)
            .codex_provider_template_v1 = Some(migration);
    })
}

pub fn is_codex_official_history_unify_migrated_for_dir(codex_dir: &str) -> bool {
    get_settings()
        .local_migrations
        .as_ref()
        .and_then(|m| m.codex_official_history_unify_v1.as_ref())
        .is_some_and(|m| m.codex_config_dir.as_deref() == Some(codex_dir))
}

pub fn mark_codex_official_history_unify_migrated_if_enabled(
    migration: CodexOfficialHistoryUnifyMigration,
) -> Result<bool, AppError> {
    let mut written = false;
    mutate_settings(|settings| {
        if settings.unify_codex_session_history
            && settings.unify_codex_migrate_existing.unwrap_or(false)
        {
            settings
                .local_migrations
                .get_or_insert_with(Default::default)
                .codex_official_history_unify_v1 = Some(migration);
            written = true;
        }
    })?;
    Ok(written)
}

pub fn clear_codex_official_history_unify_migration() -> Result<(), AppError> {
    mutate_settings(|settings| {
        if let Some(migrations) = settings.local_migrations.as_mut() {
            migrations.codex_official_history_unify_v1 = None;
        }
    })
}

// ===== WebDAV sync settings management =====

/// 读取 WebDAV 同步设置
pub fn get_webdav_sync_settings() -> Option<WebDavSyncSettings> {
    settings_store().read().ok()?.webdav_sync.clone()
}

/// 保存 WebDAV 同步设置
pub fn set_webdav_sync_settings(settings: Option<WebDavSyncSettings>) -> Result<(), AppError> {
    mutate_settings(|current| {
        current.webdav_sync = settings;
    })
}

/// 仅更新 WebDAV 同步状态，避免覆写 credentials/root/profile 等字段
pub fn update_webdav_sync_status(status: WebDavSyncStatus) -> Result<(), AppError> {
    mutate_settings(|current| {
        if let Some(sync) = current.webdav_sync.as_mut() {
            sync.status = status;
        }
    })
}

// ===== S3 sync settings management =====

pub fn get_s3_sync_settings() -> Option<S3SyncSettings> {
    settings_store().read().ok()?.s3_sync.clone()
}

pub fn set_s3_sync_settings(settings: Option<S3SyncSettings>) -> Result<(), AppError> {
    mutate_settings(|current| {
        current.s3_sync = settings;
    })
}

pub fn update_s3_sync_status(status: WebDavSyncStatus) -> Result<(), AppError> {
    mutate_settings(|current| {
        if let Some(s3) = current.s3_sync.as_mut() {
            s3.status = status;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_visible_apps_deserializes_into_enabled_map() {
        let json = r#"{"visibleApps":{"claude":true,"claude-desktop":false,"codex":true,"gemini":false,"opencode":true,"openclaw":true,"hermes":true}}"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        let map = settings.enabled_apps.as_ref().expect("map populated");
        assert_eq!(map.get("claude-desktop"), Some(&false));
        assert_eq!(map.get("gemini"), Some(&false));
        assert_eq!(map.get("hermes"), Some(&true));
        assert_eq!(map.len(), 7);

        // Values carry over under the new key on the next write.
        let out = serde_json::to_string(&settings).unwrap();
        assert!(out.contains("enabledApps"));
        assert!(!out.contains("visibleApps"));
    }

    #[test]
    fn legacy_claude_desktop_key_variants_normalize() {
        for key in ["claudeDesktop", "claude_desktop"] {
            let json = format!(r#"{{"visibleApps":{{"{key}":false}}}}"#);
            let settings: AppSettings = serde_json::from_str(&json).unwrap();
            let map = settings.enabled_apps.as_ref().unwrap();
            assert_eq!(map.get("claude-desktop"), Some(&false), "key {key}");
        }
    }

    #[test]
    fn app_enabled_missing_is_none() {
        let settings = AppSettings::default();
        assert_eq!(settings.app_enabled("hermes"), None);
        assert_eq!(settings.app_enabled("claude"), None);

        let mut settings = settings;
        settings.set_app_enabled("hermes", true);
        assert_eq!(settings.app_enabled("hermes"), Some(true));
        assert_eq!(settings.app_enabled("claude"), None);
    }

    #[test]
    fn enabled_apps_new_key_round_trips() {
        let json = r#"{"enabledApps":{"claude":false,"my-app":true}}"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.app_enabled("claude"), Some(false));
        assert_eq!(settings.app_enabled("my-app"), Some(true));
    }

    #[test]
    fn legacy_settings_default_to_the_ochub_system_theme() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.theme_family, "ochub");
        assert_eq!(settings.theme_mode, ThemeMode::System);

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""themeFamily":"ochub""#));
        assert!(json.contains(r#""themeMode":"system""#));
    }

    #[test]
    fn skill_storage_location_rebrands_and_accepts_legacy_value() {
        assert_eq!(SkillStorageLocation::default(), SkillStorageLocation::Ochub);
        assert_eq!(
            serde_json::to_string(&SkillStorageLocation::Ochub).unwrap(),
            r#""ochub""#
        );
        assert_eq!(
            serde_json::from_str::<SkillStorageLocation>(r#""cc_switch""#).unwrap(),
            SkillStorageLocation::Ochub
        );
    }

    #[test]
    fn sync_defaults_use_ochub_remote_root() {
        assert_eq!(WebDavSyncSettings::default().remote_root, "ochub-sync");
        assert_eq!(S3SyncSettings::default().remote_root, "ochub-sync");
    }
}
