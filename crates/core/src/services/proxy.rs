//! Proxy service: lifecycle manager for the local streaming reverse proxy.
//!
//! Faithful port of cc-switch `services/proxy.rs`, restructured to drop Tauri
//! (`AppHandle`, `Emitter`, tray, events). Event emission and tray refresh
//! become host/UI concerns; the service keeps lifecycle and live-config behavior
//! in core.
//!
//! Responsibilities:
//! - start/stop the local axum proxy server bound to loopback,
//! - take over the selected app's live config (rewrite it to point at the local
//!   proxy, back up the real config to the DB `live_backup` table),
//! - restore the live config on stop (backup -> SSOT -> placeholder-cleanup),
//! - hot-switch the proxy target provider without restoring upstream live config.
//!
//! The format-transform request/response tiers live in `crate::proxy`.

use std::str::FromStr;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use tokio::sync::{OwnedMutexGuard, RwLock};

use crate::app_type::AppType;
use crate::apps::claude_desktop::ONE_M_CONTEXT_MARKER;
use crate::db::Database;
use crate::error::AppError;
use crate::model::Provider;
use crate::paths::{
    delete_file, get_claude_settings_path, get_home_dir, read_json_file, write_json_file,
    write_text_file,
};
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::providers::copilot_auth::CopilotAuthManager;
use crate::proxy::server::ProxyServer;
use crate::proxy::types::{ProxyConfig, ProxyServerInfo, ProxyStatus, ProxyTakeoverStatus};
use crate::proxy::PROXY_TOKEN_PLACEHOLDER;
use crate::services::provider::{
    build_effective_settings_with_common_config, sanitize_claude_settings_for_live,
    write_live_with_common_config,
};

// Codex config helpers (ported under crate::apps::*).
use crate::apps::codex as codex_config;

/// Claude live-config model-override env keys removed when taking over (the
/// proxy rewrites `*_MODEL` to stable role aliases mapped upstream).
const CLAUDE_MODEL_OVERRIDE_ENV_KEYS: [&str; 9] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_REASONING_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_SMALL_FAST_MODEL",
];

const CLAUDE_TAKEOVER_HAIKU_MODEL: &str = "claude-haiku-4-5";
const CLAUDE_TAKEOVER_SONNET_MODEL: &str = "claude-sonnet-4-6";
const CLAUDE_TAKEOVER_OPUS_MODEL: &str = "claude-opus-4-8";
const CLAUDE_ONE_M_MARKER_FOR_CLIENT: &str = "[1M]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeTakeoverAuthPolicy {
    PreserveExistingOrAuthToken,
    ManagedAccount { keep_auth_token: bool },
}

/// Per-app switch-lock registry plus the running proxy server handle.
pub struct ProxyService {
    db: Arc<Database>,
    copilot_auth: Arc<RwLock<CopilotAuthManager>>,
    codex_oauth: Arc<RwLock<CodexOAuthManager>>,
    server: Arc<RwLock<Option<ProxyServer>>>,
    switch_locks: Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

/// Outcome of a hot-switch.
#[derive(Debug, Clone, Copy, Default)]
pub struct HotSwitchOutcome {
    pub logical_target_changed: bool,
}

impl ProxyService {
    pub fn new(
        db: Arc<Database>,
        copilot_auth: Arc<RwLock<CopilotAuthManager>>,
        codex_oauth: Arc<RwLock<CodexOAuthManager>>,
    ) -> Self {
        Self {
            db,
            copilot_auth,
            codex_oauth,
            server: Arc::new(RwLock::new(None)),
            switch_locks: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    // ===================== switch lock =====================

    /// Acquire the per-app switch lock as an owned guard.
    pub async fn lock_switch_for_app(&self, app_type: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.switch_locks.write().await;
            map.entry(app_type.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    // ===================== claude takeover field writers =====================

    fn apply_claude_takeover_fields_for_provider(
        config: &mut Value,
        proxy_url: &str,
        provider: &Provider,
    ) {
        let auth_policy = if provider.uses_managed_account_auth() {
            ClaudeTakeoverAuthPolicy::ManagedAccount {
                keep_auth_token: !provider.is_github_copilot(),
            }
        } else {
            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken
        };
        let takeover_model_fields = if provider.uses_managed_account_auth() {
            Self::build_claude_takeover_model_fields(&provider.settings_config)
        } else {
            Self::build_claude_takeover_model_fields(config)
        };
        Self::apply_claude_takeover_fields_with_policy_and_models(
            config,
            proxy_url,
            auth_policy,
            takeover_model_fields,
        );
    }

    fn apply_claude_takeover_fields_with_policy(
        config: &mut Value,
        proxy_url: &str,
        auth_policy: ClaudeTakeoverAuthPolicy,
    ) {
        let takeover_model_fields = Self::build_claude_takeover_model_fields(config);
        Self::apply_claude_takeover_fields_with_policy_and_models(
            config,
            proxy_url,
            auth_policy,
            takeover_model_fields,
        );
    }

    fn apply_claude_takeover_fields_with_policy_and_models(
        config: &mut Value,
        proxy_url: &str,
        auth_policy: ClaudeTakeoverAuthPolicy,
        takeover_model_fields: Vec<(&'static str, String)>,
    ) {
        if !config.is_object() {
            *config = json!({});
        }
        let root = config.as_object_mut().expect("normalized object");
        let env = root.entry("env".to_string()).or_insert_with(|| json!({}));
        if !env.is_object() {
            *env = json!({});
        }
        let env = env.as_object_mut().expect("normalized env");
        env.insert("ANTHROPIC_BASE_URL".to_string(), json!(proxy_url));

        for key in CLAUDE_MODEL_OVERRIDE_ENV_KEYS {
            env.remove(key);
        }
        for (key, value) in takeover_model_fields {
            env.insert(key.to_string(), Value::String(value));
        }

        let token_keys = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ];

        match auth_policy {
            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken => {
                let mut replaced_any = false;
                for key in token_keys {
                    if env.contains_key(key) {
                        env.insert(key.to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
                        replaced_any = true;
                    }
                }
                if !replaced_any {
                    env.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        json!(PROXY_TOKEN_PLACEHOLDER),
                    );
                }
            }
            ClaudeTakeoverAuthPolicy::ManagedAccount { keep_auth_token } => {
                for key in token_keys {
                    env.remove(key);
                }
                env.insert(
                    "ANTHROPIC_API_KEY".to_string(),
                    json!(PROXY_TOKEN_PLACEHOLDER),
                );
                if keep_auth_token {
                    env.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        json!(PROXY_TOKEN_PLACEHOLDER),
                    );
                }
            }
        }
    }

    fn build_claude_takeover_model_fields(config: &Value) -> Vec<(&'static str, String)> {
        let Some(env) = config.get("env").and_then(Value::as_object) else {
            return Vec::new();
        };

        let default_model = Self::claude_env_string(env, "ANTHROPIC_MODEL");
        let small_fast_model = Self::claude_env_string(env, "ANTHROPIC_SMALL_FAST_MODEL");
        let haiku_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .or(small_fast_model)
            .or(default_model);
        let sonnet_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_SONNET_MODEL")
            .or(default_model)
            .or(small_fast_model);
        let opus_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_OPUS_MODEL")
            .or(default_model)
            .or(small_fast_model);

        let mut fields = Vec::with_capacity(6);
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            CLAUDE_TAKEOVER_HAIKU_MODEL,
            false,
            haiku_model,
        );
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            CLAUDE_TAKEOVER_SONNET_MODEL,
            true,
            sonnet_model,
        );
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            CLAUDE_TAKEOVER_OPUS_MODEL,
            true,
            opus_model,
        );
        fields
    }

    fn push_claude_takeover_role_fields(
        fields: &mut Vec<(&'static str, String)>,
        env: &Map<String, Value>,
        model_key: &'static str,
        name_key: &'static str,
        takeover_model: &'static str,
        supports_one_m: bool,
        upstream_model: Option<&str>,
    ) {
        let Some(upstream_model) = upstream_model else {
            return;
        };
        let mut client_model = takeover_model.to_string();
        if supports_one_m && Self::has_claude_one_m_marker(upstream_model) {
            client_model.push_str(CLAUDE_ONE_M_MARKER_FOR_CLIENT);
        }
        fields.push((model_key, client_model));

        let display_name = Self::claude_env_string(env, name_key)
            .map(str::to_string)
            .unwrap_or_else(|| Self::strip_claude_one_m_marker(upstream_model));
        if !display_name.is_empty() {
            fields.push((name_key, display_name));
        }
    }

    fn claude_env_string<'a>(env: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
        env.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn has_claude_one_m_marker(model: &str) -> bool {
        model
            .trim_end()
            .to_ascii_lowercase()
            .ends_with(ONE_M_CONTEXT_MARKER)
    }

    fn strip_claude_one_m_marker(model: &str) -> String {
        crate::proxy::model_mapper::strip_one_m_suffix_for_upstream(model)
            .trim()
            .to_string()
    }

    fn claude_provider_with_effective_settings(
        &self,
        provider: &Provider,
    ) -> Result<Provider, String> {
        let mut effective_provider = provider.clone();
        effective_provider.settings_config = build_effective_settings_with_common_config(
            self.db.as_ref(),
            &AppType::Claude,
            provider,
        )
        .map_err(|e| format!("build claude effective settings failed: {e}"))?;
        Ok(effective_provider)
    }

    // ===================== live sync while proxy active =====================

    pub async fn sync_claude_live_from_provider_while_proxy_active(
        &self,
        provider: &Provider,
    ) -> Result<(), String> {
        let effective_provider = self.claude_provider_with_effective_settings(provider)?;
        let mut effective_settings = effective_provider.settings_config.clone();
        let (proxy_url, _) = self.build_proxy_urls().await?;
        Self::apply_claude_takeover_fields_for_provider(
            &mut effective_settings,
            &proxy_url,
            &effective_provider,
        );
        self.write_claude_live(&effective_settings)?;
        Ok(())
    }

    pub async fn sync_codex_live_from_provider_while_proxy_active(
        &self,
        provider: &Provider,
    ) -> Result<(), String> {
        let existing_live = self.read_codex_live().ok();
        let mut effective_settings = build_effective_settings_with_common_config(
            self.db.as_ref(),
            &AppType::Codex,
            provider,
        )
        .map_err(|e| format!("build codex effective settings failed: {e}"))?;
        if let Some(existing_live) = existing_live.as_ref() {
            Self::preserve_codex_mcp_servers_from_existing_config(
                &mut effective_settings,
                existing_live,
            )?;
        }
        let (_, proxy_codex_base_url) = self.build_proxy_urls().await?;

        if let Some(auth) = effective_settings
            .get_mut("auth")
            .and_then(|v| v.as_object_mut())
        {
            auth.insert("OPENAI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
        } else if let Some(root) = effective_settings.as_object_mut() {
            root.insert(
                "auth".to_string(),
                json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
            );
        }

        let config_str = effective_settings
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let updated_config = Self::apply_codex_proxy_toml_config_for_provider(
            config_str,
            &proxy_codex_base_url,
            Some(provider),
        );
        effective_settings["config"] = json!(updated_config);
        Self::attach_codex_model_catalog_from_provider(&mut effective_settings, Some(provider));

        self.write_codex_takeover_live_for_provider(&effective_settings, Some(provider))?;
        Ok(())
    }

    // ===================== current provider helpers =====================

    fn get_current_provider_for_app(&self, app_type: &AppType) -> Result<Option<Provider>, String> {
        let Some(current_id) = crate::settings::get_effective_current_provider(&self.db, app_type)
            .map_err(|e| format!("get current provider for {app_type:?} failed: {e}"))?
        else {
            return Ok(None);
        };
        self.db
            .get_provider_by_id(&current_id, app_type.as_str())
            .map_err(|e| format!("read current provider for {app_type:?} failed: {e}"))
    }

    fn require_current_provider_for_app(&self, app_type: &AppType) -> Result<Provider, String> {
        self.get_current_provider_for_app(app_type)?.ok_or_else(|| {
            format!("{app_type:?} has no current provider; cannot take over live config")
        })
    }

    // ===================== server lifecycle =====================

    /// Start the proxy server (without taking over any live config).
    pub async fn start(&self) -> Result<ProxyServerInfo, String> {
        let mut global_config = self
            .db
            .get_global_proxy_config()
            .await
            .map_err(|e| format!("get global proxy config failed: {e}"))?;
        if !global_config.proxy_enabled {
            global_config.proxy_enabled = true;
            self.db
                .update_global_proxy_config(global_config.clone())
                .await
                .map_err(|e| format!("update global proxy switch failed: {e}"))?;
        }

        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("get proxy config failed: {e}"))?;

        if let Some(server) = self.server.read().await.as_ref() {
            let status = server.get_status().await;
            return Ok(ProxyServerInfo {
                address: status.address,
                port: status.port,
                started_at: chrono::Utc::now().to_rfc3339(),
            });
        }

        let server = ProxyServer::new(
            config.clone(),
            self.db.clone(),
            self.copilot_auth.clone(),
            self.codex_oauth.clone(),
        );
        let info = server
            .start()
            .await
            .map_err(|e| format!("start proxy server failed: {e}"))?;
        if let Err(e) = self
            .persist_ephemeral_listen_port_if_needed(&config, info.port)
            .await
        {
            let _ = server.stop().await;
            return Err(e);
        }

        *self.server.write().await = Some(server);
        log::info!("proxy server started: {}:{}", info.address, info.port);
        Ok(info)
    }

    async fn persist_ephemeral_listen_port_if_needed(
        &self,
        config: &ProxyConfig,
        actual_port: u16,
    ) -> Result<(), String> {
        if config.listen_port != 0 {
            return Ok(());
        }
        let mut resolved_config = config.clone();
        resolved_config.listen_port = actual_port;
        self.db
            .update_proxy_config(resolved_config)
            .await
            .map_err(|e| format!("persist dynamic proxy port failed: {e}"))
    }

    async fn start_before_takeover_if_ephemeral_port(&self) -> Result<bool, String> {
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("get proxy config failed: {e}"))?;
        if config.listen_port != 0 || self.is_running().await {
            return Ok(false);
        }
        self.start().await?;
        Ok(true)
    }

    /// Start the proxy server and take over all live configs.
    pub async fn start_with_takeover(&self) -> Result<ProxyServerInfo, String> {
        self.backup_live_configs().await?;

        if let Err(e) = self.sync_live_to_providers().await {
            if let Err(clean_err) = self.db.delete_all_live_backups().await {
                log::warn!("clear live backups failed: {clean_err}");
            }
            return Err(e);
        }

        let started_proxy_before_takeover =
            match self.start_before_takeover_if_ephemeral_port().await {
                Ok(started) => started,
                Err(e) => {
                    let _ = self.db.delete_all_live_backups().await;
                    return Err(e);
                }
            };

        if let Err(e) = self.db.set_live_takeover_active(true).await {
            let _ = self.db.delete_all_live_backups().await;
            if started_proxy_before_takeover {
                let _ = self.stop_inner().await;
            }
            return Err(format!("set takeover state failed: {e}"));
        }

        if let Err(e) = self.takeover_live_configs().await {
            log::error!("takeover live configs failed, attempting restore: {e}");
            match self.restore_live_configs().await {
                Ok(()) => {
                    let _ = self.db.set_live_takeover_active(false).await;
                    let _ = self.db.delete_all_live_backups().await;
                }
                Err(restore_err) => {
                    log::error!(
                        "restore failed, keeping backups for next-launch recovery: {restore_err}"
                    );
                }
            }
            if started_proxy_before_takeover {
                let _ = self.stop_inner().await;
            }
            return Err(e);
        }

        match self.start().await {
            Ok(info) => Ok(info),
            Err(e) => {
                log::error!("proxy start failed, attempting restore: {e}");
                match self.restore_live_configs().await {
                    Ok(()) => {
                        let _ = self.db.set_live_takeover_active(false).await;
                        let _ = self.db.delete_all_live_backups().await;
                    }
                    Err(restore_err) => {
                        log::error!("restore failed, keeping backups: {restore_err}");
                    }
                }
                if started_proxy_before_takeover {
                    let _ = self.stop_inner().await;
                }
                Err(e)
            }
        }
    }

    /// Per-app takeover status (read from proxy_config.enabled).
    pub async fn get_takeover_status(&self) -> Result<ProxyTakeoverStatus, String> {
        let claude = self
            .db
            .get_proxy_config_for_app("claude")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        let codex = self
            .db
            .get_proxy_config_for_app("codex")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        Ok(ProxyTakeoverStatus {
            claude,
            codex,
            opencode: false,
            openclaw: false,
        })
    }

    /// Enable/disable live takeover for one app.
    pub async fn set_takeover_for_app(&self, app_type: &str, enabled: bool) -> Result<(), String> {
        let app = AppType::from_str(app_type).map_err(|e| format!("invalid app type: {e}"))?;
        let app_type_str = app.as_str();
        let _guard = self.lock_switch_for_app(app_type_str).await;

        if enabled {
            if !self.is_running().await {
                self.start().await?;
            }

            let current_config = self
                .db
                .get_proxy_config_for_app(app_type_str)
                .await
                .map_err(|e| format!("get {app_type_str} config failed: {e}"))?;

            let mut restore_existing_backup_before_takeover = false;
            if current_config.enabled {
                let has_backup = match self.db.get_live_backup(app_type_str).await {
                    Ok(v) => v.is_some(),
                    Err(e) => {
                        log::warn!("read {app_type_str} backup failed (rebuilding takeover): {e}");
                        false
                    }
                };
                let live_matches_current_proxy =
                    match self.live_takeover_matches_current_proxy(&app).await {
                        Ok(value) => value,
                        Err(e) => {
                            log::warn!("detect {app_type_str} takeover config failed: {e}");
                            false
                        }
                    };
                if has_backup && live_matches_current_proxy {
                    return Ok(());
                }
                restore_existing_backup_before_takeover = has_backup;
                log::warn!(
                    "{app_type_str} marked taken-over but backup={has_backup} live_matches={live_matches_current_proxy}; re-taking over"
                );
            }

            if restore_existing_backup_before_takeover {
                self.restore_live_config_for_app_inner(&app).await?;
            } else {
                self.backup_live_config_strict(&app).await?;
                if let Err(e) = self.sync_live_to_provider(&app).await {
                    let _ = self.db.delete_live_backup(app_type_str).await;
                    return Err(e);
                }
            }

            if let Err(e) = self.takeover_live_config_strict(&app).await {
                log::error!("{app_type_str} takeover failed, attempting restore: {e}");
                match self.restore_live_config_for_app_inner(&app).await {
                    Ok(()) => {
                        let _ = self.db.delete_live_backup(app_type_str).await;
                    }
                    Err(restore_err) => {
                        log::error!("{app_type_str} restore failed, keeping backup: {restore_err}");
                    }
                }
                return Err(e);
            }

            let mut updated_config = self
                .db
                .get_proxy_config_for_app(app_type_str)
                .await
                .map_err(|e| format!("get {app_type_str} config failed: {e}"))?;
            updated_config.enabled = true;
            self.db
                .update_proxy_config_for_app(updated_config)
                .await
                .map_err(|e| format!("set {app_type_str} enabled failed: {e}"))?;

            let _ = self.db.set_live_takeover_active(true).await;

            // Tauri emitted a "proxy-official-warning" event here. Core has no
            // event bus, so the warning is logged and UI callers can inspect the
            // active provider category through the normal provider/status APIs.
            if let Ok(Some(current_id)) =
                crate::settings::get_effective_current_provider(&self.db, &app)
            {
                if let Ok(Some(provider)) = self.db.get_provider_by_id(&current_id, app_type_str) {
                    if provider.category.as_deref() == Some("official") {
                        log::warn!(
                            "[proxy] takeover enabled with official provider {} for {app_type_str}",
                            provider.name
                        );
                    }
                }
            }

            return Ok(());
        }

        // Disable takeover.
        let current_config = self
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| format!("get {app_type_str} config failed: {e}"))?;
        if !current_config.enabled {
            return Ok(());
        }

        self.restore_live_config_for_app_with_fallback_inner(&app)
            .await?;

        self.db
            .delete_live_backup(app_type_str)
            .await
            .map_err(|e| format!("delete {app_type_str} live backup failed: {e}"))?;

        let mut updated_config = self
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| format!("get {app_type_str} config failed: {e}"))?;
        updated_config.enabled = false;
        self.db
            .update_proxy_config_for_app(updated_config)
            .await
            .map_err(|e| format!("clear {app_type_str} enabled failed: {e}"))?;

        self.db
            .clear_provider_health_for_app(app_type_str)
            .await
            .map_err(|e| format!("clear {app_type_str} health failed: {e}"))?;

        let any_enabled = self
            .db
            .is_live_takeover_active()
            .await
            .map_err(|e| format!("check takeover state failed: {e}"))?;
        if !any_enabled {
            let _ = self.db.set_live_takeover_active(false).await;
            if self.is_running().await {
                let _ = self.stop_inner().await;
            }
        }

        Ok(())
    }

    // ===================== token sync =====================

    async fn sync_live_to_provider(&self, app_type: &AppType) -> Result<(), String> {
        let live_config = match app_type {
            AppType::Claude => self.read_claude_live()?,
            AppType::Codex => self.read_codex_live()?,
            _ => return Err("app does not support proxy".to_string()),
        };
        self.sync_live_config_to_provider(app_type, &live_config)
            .await
    }

    async fn sync_live_config_to_provider(
        &self,
        app_type: &AppType,
        live_config: &Value,
    ) -> Result<(), String> {
        match app_type {
            AppType::Claude => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::Claude)
                        .map_err(|e| format!("get claude current provider failed: {e}"))?;
                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "claude")
                    {
                        if let Some(env) = live_config.get("env").and_then(|v| v.as_object()) {
                            let token_pair = [
                                "ANTHROPIC_AUTH_TOKEN",
                                "ANTHROPIC_API_KEY",
                                "OPENROUTER_API_KEY",
                                "OPENAI_API_KEY",
                            ]
                            .into_iter()
                            .find_map(|key| {
                                env.get(key)
                                    .and_then(|v| v.as_str())
                                    .map(|s| (key, s.trim()))
                            })
                            .filter(|(_, token)| {
                                !token.is_empty() && *token != PROXY_TOKEN_PLACEHOLDER
                            });

                            if let Some((token_key, token)) = token_pair {
                                let env_obj = provider
                                    .settings_config
                                    .get_mut("env")
                                    .and_then(|v| v.as_object_mut());
                                match env_obj {
                                    Some(obj) => {
                                        if token_key == "ANTHROPIC_AUTH_TOKEN"
                                            || token_key == "ANTHROPIC_API_KEY"
                                        {
                                            let mut updated = false;
                                            if obj.contains_key("ANTHROPIC_AUTH_TOKEN") {
                                                obj.insert(
                                                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                                                    json!(token),
                                                );
                                                updated = true;
                                            }
                                            if obj.contains_key("ANTHROPIC_API_KEY") {
                                                obj.insert(
                                                    "ANTHROPIC_API_KEY".to_string(),
                                                    json!(token),
                                                );
                                                updated = true;
                                            }
                                            if !updated {
                                                obj.insert(token_key.to_string(), json!(token));
                                            }
                                        } else {
                                            obj.insert(token_key.to_string(), json!(token));
                                        }
                                    }
                                    None => {
                                        if provider.settings_config.is_null() {
                                            provider.settings_config = json!({});
                                        }
                                        if let Some(root) = provider.settings_config.as_object_mut()
                                        {
                                            root.insert(
                                                "env".to_string(),
                                                json!({ token_key: token }),
                                            );
                                        }
                                    }
                                }
                                if let Err(e) = self.db.update_provider_settings_config(
                                    "claude",
                                    &provider_id,
                                    &provider.settings_config,
                                ) {
                                    log::warn!("sync claude token to db failed: {e}");
                                }
                            }
                        }
                    }
                }
            }
            AppType::Codex => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::Codex)
                        .map_err(|e| format!("get codex current provider failed: {e}"))?;
                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "codex")
                    {
                        if let Some(token) = live_config
                            .get("auth")
                            .and_then(|v| v.get("OPENAI_API_KEY"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty() && *s != PROXY_TOKEN_PLACEHOLDER)
                        {
                            if let Some(auth_obj) = provider
                                .settings_config
                                .get_mut("auth")
                                .and_then(|v| v.as_object_mut())
                            {
                                auth_obj.insert("OPENAI_API_KEY".to_string(), json!(token));
                            } else {
                                if provider.settings_config.is_null() {
                                    provider.settings_config = json!({});
                                }
                                if let Some(root) = provider.settings_config.as_object_mut() {
                                    root.insert(
                                        "auth".to_string(),
                                        json!({ "OPENAI_API_KEY": token }),
                                    );
                                }
                            }
                            if let Err(e) = self.db.update_provider_settings_config(
                                "codex",
                                &provider_id,
                                &provider.settings_config,
                            ) {
                                log::warn!("sync codex token to db failed: {e}");
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn sync_live_to_providers(&self) -> Result<(), String> {
        if let Ok(live_config) = self.read_claude_live() {
            self.sync_live_config_to_provider(&AppType::Claude, &live_config)
                .await?;
        }
        if let Ok(live_config) = self.read_codex_live() {
            self.sync_live_config_to_provider(&AppType::Codex, &live_config)
                .await?;
        }
        Ok(())
    }

    // ===================== stop =====================

    async fn stop_inner(&self) -> Result<(), String> {
        if let Some(server) = self.server.write().await.take() {
            server
                .stop()
                .await
                .map_err(|e| format!("stop proxy server failed: {e}"))?;
            let mut global_config = self
                .db
                .get_global_proxy_config()
                .await
                .map_err(|e| format!("get global proxy config failed: {e}"))?;
            if global_config.proxy_enabled {
                global_config.proxy_enabled = false;
                if let Err(e) = self.db.update_global_proxy_config(global_config).await {
                    log::warn!("update global proxy switch failed: {e}");
                }
            }
            log::info!("proxy server stopped");
            Ok(())
        } else {
            Err("proxy server not running".to_string())
        }
    }

    /// Stop the proxy server (required signature: `Result<(), AppError>`).
    pub async fn stop(&self) -> Result<(), AppError> {
        match self.stop_inner().await {
            Ok(()) => Ok(()),
            // "not running" is not an error for the cleanup callers.
            Err(e) if e.contains("not running") => Ok(()),
            Err(e) => Err(AppError::Message(e)),
        }
    }

    /// Stop + restore live configs, keeping settings/takeover state for
    /// next-launch auto-recovery (used on normal program exit).
    pub async fn stop_with_restore_keep_state(&self) -> Result<(), AppError> {
        if let Err(e) = self.stop_inner().await {
            log::warn!("stop proxy server failed (continuing restore): {e}");
        }
        self.restore_live_configs()
            .await
            .map_err(AppError::Message)?;

        if let Ok(mut config) = self.db.get_proxy_config().await {
            config.live_takeover_active = false;
            let _ = self.db.update_proxy_config(config).await;
        }
        self.db
            .delete_all_live_backups()
            .await
            .map_err(|e| AppError::Message(format!("delete backups failed: {e}")))?;
        self.db
            .clear_all_provider_health()
            .await
            .map_err(|e| AppError::Message(format!("reset health failed: {e}")))?;

        log::info!("proxy stopped, live configs restored (state preserved)");
        Ok(())
    }

    /// Stop + restore live configs, clearing all takeover state (user-initiated).
    pub async fn stop_with_restore(&self) -> Result<(), String> {
        if let Err(e) = self.stop_inner().await {
            log::warn!("stop proxy server failed (continuing restore): {e}");
        }
        self.restore_live_configs().await?;
        self.db
            .set_live_takeover_active(false)
            .await
            .map_err(|e| format!("clear takeover state failed: {e}"))?;
        for app_type in ["claude", "codex"] {
            if let Ok(mut config) = self.db.get_proxy_config_for_app(app_type).await {
                if config.enabled {
                    config.enabled = false;
                    if let Err(e) = self.db.update_proxy_config_for_app(config).await {
                        log::warn!("clear {app_type} enabled failed: {e}");
                    }
                }
            }
        }
        self.db
            .delete_all_live_backups()
            .await
            .map_err(|e| format!("delete backups failed: {e}"))?;
        self.db
            .clear_all_provider_health()
            .await
            .map_err(|e| format!("reset health failed: {e}"))?;
        log::info!("proxy stopped, live configs restored");
        Ok(())
    }

    // ===================== backup =====================

    async fn backup_live_configs(&self) -> Result<(), String> {
        if let Ok(config) = self.read_claude_live() {
            if !Self::live_has_proxy_placeholder_for_app(&AppType::Claude, &config) {
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("serialize claude config failed: {e}"))?;
                self.db
                    .save_live_backup("claude", &json_str)
                    .await
                    .map_err(|e| format!("backup claude config failed: {e}"))?;
            }
        }
        if let Ok(config) = self.read_codex_live() {
            if !Self::live_has_proxy_placeholder_for_app(&AppType::Codex, &config) {
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("serialize codex config failed: {e}"))?;
                self.db
                    .save_live_backup("codex", &json_str)
                    .await
                    .map_err(|e| format!("backup codex config failed: {e}"))?;
            }
        }
        Ok(())
    }

    async fn backup_live_config_strict(&self, app_type: &AppType) -> Result<(), String> {
        let (app_type_str, config) = match app_type {
            AppType::Claude => ("claude", self.read_claude_live()?),
            AppType::Codex => ("codex", self.read_codex_live()?),
            _ => return Err("app does not support proxy".to_string()),
        };
        if Self::live_has_proxy_placeholder_for_app(app_type, &config) {
            log::warn!("{app_type_str} live already taken over; skip backup");
            return Ok(());
        }
        let json_str = serde_json::to_string(&config)
            .map_err(|e| format!("serialize {app_type_str} config failed: {e}"))?;
        self.db
            .save_live_backup(app_type_str, &json_str)
            .await
            .map_err(|e| format!("backup {app_type_str} config failed: {e}"))?;
        Ok(())
    }

    // ===================== proxy URL =====================

    async fn build_proxy_urls(&self) -> Result<(String, String), String> {
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("get proxy config failed: {e}"))?;

        let connect_host = match config.listen_address.as_str() {
            "0.0.0.0" => "127.0.0.1".to_string(),
            "::" => "::1".to_string(),
            _ => config.listen_address.clone(),
        };
        let connect_host_for_url = if connect_host.contains(':') && !connect_host.starts_with('[') {
            format!("[{connect_host}]")
        } else {
            connect_host
        };

        let mut listen_port = config.listen_port;
        if let Some(server) = self.server.read().await.as_ref() {
            let status = server.get_status().await;
            if status.running {
                listen_port = status.port;
            }
        }
        if listen_port == 0 {
            return Err("proxy listen port is 0 but server not running".to_string());
        }

        let proxy_origin = format!("http://{}:{}", connect_host_for_url, listen_port);
        let proxy_url = proxy_origin.clone();
        let proxy_codex_base_url = format!("{}/v1", proxy_origin.trim_end_matches('/'));
        Ok((proxy_url, proxy_codex_base_url))
    }

    // ===================== takeover writers =====================

    async fn takeover_live_configs(&self) -> Result<(), String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;

        if let Ok(mut live_config) = self.read_claude_live() {
            let claude_provider = self.require_current_provider_for_app(&AppType::Claude)?;
            let claude_provider = self.claude_provider_with_effective_settings(&claude_provider)?;
            Self::apply_claude_takeover_fields_for_provider(
                &mut live_config,
                &proxy_url,
                &claude_provider,
            );
            self.write_claude_live(&live_config)?;
            log::info!("Claude live taken over: {proxy_url}");
        }

        if let Ok(mut live_config) = self.read_codex_live() {
            if let Some(auth) = live_config.get_mut("auth").and_then(|v| v.as_object_mut()) {
                auth.insert("OPENAI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
            }
            let config_str = live_config
                .get("config")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let codex_provider = self
                .get_current_provider_for_app(&AppType::Codex)
                .ok()
                .flatten();
            let updated_config = Self::apply_codex_proxy_toml_config_for_provider(
                config_str,
                &proxy_codex_base_url,
                codex_provider.as_ref(),
            );
            live_config["config"] = json!(updated_config);
            Self::attach_codex_model_catalog_from_provider(
                &mut live_config,
                codex_provider.as_ref(),
            );
            self.write_codex_takeover_live_for_provider(&live_config, codex_provider.as_ref())?;
            log::info!("Codex live taken over: {proxy_codex_base_url}");
        }

        Ok(())
    }

    async fn takeover_live_config_strict(&self, app_type: &AppType) -> Result<(), String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        match app_type {
            AppType::Claude => {
                let mut live_config = self.read_claude_live()?;
                let claude_provider = self.require_current_provider_for_app(&AppType::Claude)?;
                let claude_provider =
                    self.claude_provider_with_effective_settings(&claude_provider)?;
                Self::apply_claude_takeover_fields_for_provider(
                    &mut live_config,
                    &proxy_url,
                    &claude_provider,
                );
                self.write_claude_live(&live_config)?;
            }
            AppType::Codex => {
                let mut live_config = self.read_codex_live()?;
                if let Some(auth) = live_config.get_mut("auth").and_then(|v| v.as_object_mut()) {
                    auth.insert("OPENAI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
                }
                let config_str = live_config
                    .get("config")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let codex_provider = self.require_current_provider_for_app(&AppType::Codex)?;
                let updated_config = Self::apply_codex_proxy_toml_config_for_provider(
                    config_str,
                    &proxy_codex_base_url,
                    Some(&codex_provider),
                );
                live_config["config"] = json!(updated_config);
                Self::attach_codex_model_catalog_from_provider(
                    &mut live_config,
                    Some(&codex_provider),
                );
                self.write_codex_takeover_live_for_provider(&live_config, Some(&codex_provider))?;
            }
            _ => return Err("app does not support proxy".to_string()),
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn takeover_live_config_best_effort(&self, app_type: &AppType) -> Result<(), String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        match app_type {
            AppType::Claude => {
                if let Ok(mut live_config) = self.read_claude_live() {
                    let claude_provider = self
                        .get_current_provider_for_app(&AppType::Claude)
                        .ok()
                        .flatten();
                    if let Some(provider) = claude_provider.as_ref() {
                        let provider = self.claude_provider_with_effective_settings(provider)?;
                        Self::apply_claude_takeover_fields_for_provider(
                            &mut live_config,
                            &proxy_url,
                            &provider,
                        );
                    } else {
                        Self::apply_claude_takeover_fields_with_policy(
                            &mut live_config,
                            &proxy_url,
                            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken,
                        );
                    }
                    let _ = self.write_claude_live(&live_config);
                }
            }
            AppType::Codex => {
                if let Ok(mut live_config) = self.read_codex_live() {
                    if let Some(auth) = live_config.get_mut("auth").and_then(|v| v.as_object_mut())
                    {
                        auth.insert("OPENAI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
                    }
                    let config_str = live_config
                        .get("config")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let codex_provider = self
                        .get_current_provider_for_app(&AppType::Codex)
                        .ok()
                        .flatten();
                    let updated_config = Self::apply_codex_proxy_toml_config_for_provider(
                        config_str,
                        &proxy_codex_base_url,
                        codex_provider.as_ref(),
                    );
                    live_config["config"] = json!(updated_config);
                    Self::attach_codex_model_catalog_from_provider(
                        &mut live_config,
                        codex_provider.as_ref(),
                    );
                    let _ = self.write_codex_takeover_live_for_provider(
                        &live_config,
                        codex_provider.as_ref(),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ===================== restore =====================

    async fn restore_live_config_for_app_inner(&self, app_type: &AppType) -> Result<(), String> {
        match app_type {
            AppType::Claude => {
                if let Ok(Some(backup)) = self.db.get_live_backup("claude").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("parse claude backup failed: {e}"))?;
                    self.write_claude_live(&config)?;
                }
            }
            AppType::Codex => {
                if let Ok(Some(backup)) = self.db.get_live_backup("codex").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("parse codex backup failed: {e}"))?;
                    self.write_codex_live(&config)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn restore_live_configs(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        for app_type in [AppType::Claude, AppType::Codex] {
            if let Err(e) = self
                .restore_live_config_for_app_with_fallback(&app_type)
                .await
            {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn restore_live_config_for_app_with_fallback(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        let _guard = self.lock_switch_for_app(app_type.as_str()).await;
        self.restore_live_config_for_app_with_fallback_inner(app_type)
            .await
    }

    async fn restore_live_config_for_app_with_fallback_inner(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        let app_type_str = app_type.as_str();

        let backup = self
            .db
            .get_live_backup(app_type_str)
            .await
            .map_err(|e| format!("get {app_type_str} live backup failed: {e}"))?;
        if let Some(backup) = backup {
            let config: Value = serde_json::from_str(&backup.original_config)
                .map_err(|e| format!("parse {app_type_str} backup failed: {e}"))?;
            if Self::live_has_proxy_placeholder_for_app(app_type, &config) {
                log::warn!(
                    "{app_type_str} backup itself is a proxy placeholder; using SSOT rebuild"
                );
            } else {
                self.write_live_config_for_app(app_type, &config)?;
                log::info!("{app_type_str} live restored from backup");
                return Ok(());
            }
        }

        if !self.detect_takeover_in_live_config_for_app(app_type) {
            return Ok(());
        }

        match self.restore_live_from_ssot_for_app(app_type) {
            Ok(true) => {
                log::info!("{app_type_str} live restored from SSOT (no backup)");
                return Ok(());
            }
            Ok(false) => {
                log::warn!("{app_type_str} backup missing and SSOT restore impossible; cleaning placeholders");
            }
            Err(e) => {
                log::error!("{app_type_str} SSOT restore failed; cleaning placeholders: {e}");
            }
        }

        self.cleanup_takeover_placeholders_in_live_for_app(app_type)?;
        log::info!("{app_type_str} takeover placeholders cleaned (no backup)");
        Ok(())
    }

    fn write_live_config_for_app(&self, app_type: &AppType, config: &Value) -> Result<(), String> {
        match app_type {
            AppType::Claude => self.write_claude_live(config),
            AppType::Codex => self.write_codex_live(config),
            _ => Err("app does not support proxy".to_string()),
        }
    }

    fn restore_live_from_ssot_for_app(&self, app_type: &AppType) -> Result<bool, String> {
        let current_id = crate::settings::get_effective_current_provider(&self.db, app_type)
            .map_err(|e| format!("get current provider for {app_type:?} failed: {e}"))?;
        let Some(current_id) = current_id else {
            return Ok(false);
        };
        let providers = self
            .db
            .get_all_providers(app_type.as_str())
            .map_err(|e| format!("read providers for {app_type:?} failed: {e}"))?;
        let Some(provider) = providers.get(&current_id) else {
            return Ok(false);
        };
        if Self::live_has_proxy_placeholder_for_app(app_type, &provider.settings_config) {
            log::warn!(
                "{app_type:?} current provider config has proxy placeholder; skip SSOT write"
            );
            return Ok(false);
        }
        write_live_with_common_config(self.db.as_ref(), app_type, provider)
            .map_err(|e| format!("write {app_type:?} live config failed: {e}"))?;
        Ok(true)
    }

    // ===================== takeover detection (required public API) =====================

    pub fn detect_takeover_in_live_config_for_app(&self, app_type: &AppType) -> bool {
        match app_type {
            AppType::Claude => match self.read_claude_live() {
                Ok(config) => Self::is_claude_live_taken_over(&config),
                Err(_) => false,
            },
            AppType::Codex => match self.read_codex_live() {
                Ok(config) => Self::is_codex_live_taken_over(&config),
                Err(_) => false,
            },
            _ => false,
        }
    }

    pub fn detect_takeover_in_live_configs(&self) -> bool {
        if let Ok(config) = self.read_claude_live() {
            if Self::is_claude_live_taken_over(&config) {
                return true;
            }
        }
        if let Ok(config) = self.read_codex_live() {
            if Self::is_codex_live_taken_over(&config) {
                return true;
            }
        }
        false
    }

    fn is_claude_live_taken_over(config: &Value) -> bool {
        let Some(env) = config.get("env").and_then(|v| v.as_object()) else {
            return false;
        };
        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ] {
            if env.get(key).and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER) {
                return true;
            }
        }
        false
    }

    fn codex_live_has_proxy_placeholder(config: &Value) -> bool {
        if config
            .get("auth")
            .and_then(|v| v.as_object())
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str())
            == Some(PROXY_TOKEN_PLACEHOLDER)
        {
            return true;
        }
        config
            .get("config")
            .and_then(|v| v.as_str())
            .and_then(codex_config::extract_codex_experimental_bearer_token)
            .as_deref()
            == Some(PROXY_TOKEN_PLACEHOLDER)
    }

    fn is_codex_live_taken_over(config: &Value) -> bool {
        Self::codex_live_has_proxy_placeholder(config)
    }

    fn live_has_proxy_placeholder_for_app(app_type: &AppType, config: &Value) -> bool {
        match app_type {
            AppType::Claude => Self::is_claude_live_taken_over(config),
            AppType::Codex => Self::codex_live_has_proxy_placeholder(config),
            _ => false,
        }
    }

    // ===================== takeover matching / cleanup =====================

    fn is_local_proxy_url(url: &str) -> bool {
        let url = url.trim();
        if !url.starts_with("http://") {
            return false;
        }
        let rest = &url["http://".len()..];
        rest.starts_with("127.0.0.1")
            || rest.starts_with("localhost")
            || rest.starts_with("0.0.0.0")
            || rest.starts_with("[::1]")
            || rest.starts_with("[::]")
            || rest.starts_with("::1")
            || rest.starts_with("::")
    }

    fn proxy_urls_match(actual: &str, expected: &str) -> bool {
        actual.trim().trim_end_matches('/') == expected.trim().trim_end_matches('/')
    }

    fn codex_config_has_base_url_matching(
        config_text: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> bool {
        let Ok(doc) = toml::from_str::<toml::Value>(config_text) else {
            return false;
        };
        let active_provider = doc
            .get("model_provider")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(provider_id) = active_provider {
            if doc
                .get("model_providers")
                .and_then(|value| value.get(provider_id))
                .and_then(|value| value.get("base_url"))
                .and_then(|value| value.as_str())
                .is_some_and(&predicate)
            {
                return true;
            }
        }
        doc.get("base_url")
            .and_then(|value| value.as_str())
            .is_some_and(predicate)
    }

    async fn live_takeover_matches_current_proxy(
        &self,
        app_type: &AppType,
    ) -> Result<bool, String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        match app_type {
            AppType::Claude => {
                let config = self.read_claude_live()?;
                let base_url_matches = config
                    .get("env")
                    .and_then(|value| value.get("ANTHROPIC_BASE_URL"))
                    .and_then(|value| value.as_str())
                    .is_some_and(|url| Self::proxy_urls_match(url, &proxy_url));
                Ok(Self::is_claude_live_taken_over(&config) && base_url_matches)
            }
            AppType::Codex => {
                let config = self.read_codex_live()?;
                let base_url_matches = config
                    .get("config")
                    .and_then(|value| value.as_str())
                    .is_some_and(|config_text| {
                        Self::codex_config_has_base_url_matching(config_text, |url| {
                            Self::proxy_urls_match(url, &proxy_codex_base_url)
                        })
                    });
                Ok(Self::codex_live_has_proxy_placeholder(&config) && base_url_matches)
            }
            _ => Ok(false),
        }
    }

    fn cleanup_takeover_placeholders_in_live_for_app(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        match app_type {
            AppType::Claude => self.cleanup_claude_takeover_placeholders_in_live(),
            AppType::Codex => self.cleanup_codex_takeover_placeholders_in_live(),
            _ => Ok(()),
        }
    }

    fn cleanup_claude_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let mut config = self.read_claude_live()?;
        let Some(env) = config.get_mut("env").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };
        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ] {
            if env.get(key).and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER) {
                env.remove(key);
            }
        }
        if env
            .get("ANTHROPIC_BASE_URL")
            .and_then(|v| v.as_str())
            .map(Self::is_local_proxy_url)
            .unwrap_or(false)
        {
            env.remove("ANTHROPIC_BASE_URL");
        }
        self.write_claude_live(&config)?;
        Ok(())
    }

    fn cleanup_codex_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let mut config = self.read_codex_live()?;
        if let Some(auth) = config.get_mut("auth").and_then(|v| v.as_object_mut()) {
            if auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER)
            {
                auth.remove("OPENAI_API_KEY");
            }
        }
        if let Some(cfg_str) = config.get("config").and_then(|v| v.as_str()) {
            let updated = Self::remove_local_toml_base_url(cfg_str);
            let updated =
                codex_config::remove_codex_experimental_bearer_token_if(&updated, |token| {
                    token == PROXY_TOKEN_PLACEHOLDER
                })
                .map_err(|e| format!("clean codex placeholders failed: {e}"))?;
            config["config"] = json!(updated);
        }
        self.write_codex_live(&config)?;
        Ok(())
    }

    fn remove_local_toml_base_url(toml_str: &str) -> String {
        codex_config::remove_codex_toml_base_url_if(toml_str, Self::is_local_proxy_url)
    }

    // ===================== crash recovery =====================

    pub async fn is_takeover_active(&self) -> Result<bool, String> {
        let status = self.get_takeover_status().await?;
        Ok(status.claude || status.codex)
    }

    pub async fn recover_from_crash(&self) -> Result<(), String> {
        self.restore_live_configs().await?;
        self.db
            .set_live_takeover_active(false)
            .await
            .map_err(|e| format!("clear takeover state failed: {e}"))?;
        self.db
            .delete_all_live_backups()
            .await
            .map_err(|e| format!("delete backups failed: {e}"))?;
        log::info!("recovered live configs from crash");
        Ok(())
    }

    // ===================== legacy gemini takeover restore =====================
    //
    // Gemini app support was removed. The normal restore/cleanup paths now only
    // iterate Claude + Codex, so a user who upgraded while a Gemini takeover was
    // active would otherwise keep `GEMINI_API_KEY=<placeholder>` and
    // `GOOGLE_GEMINI_BASE_URL` pointed at the (now dead) local proxy in
    // `~/.gemini/.env` forever, and the DB `live_backup` row for 'gemini' would
    // never be cleared. This LEGACY one-time path restores that stranded state
    // without reintroducing `AppType::Gemini`. It is keyed on the raw string
    // "gemini" and is a no-op when no 'gemini' backup row exists.

    /// Path to the legacy Gemini env file (`~/.gemini/.env`). Honors
    /// `CC_SWITCH_TEST_HOME` via `get_home_dir` for tests.
    fn legacy_gemini_env_path() -> std::path::PathBuf {
        get_home_dir().join(".gemini").join(".env")
    }

    /// Minimal private copy of the removed `write_gemini_live` logic: serialize
    /// the backed-up `{"env": { .. }}` JSON back to `~/.gemini/.env` (sorted
    /// `KEY=VALUE` lines) with the same 0700 dir / 0600 file permissions the
    /// deleted `apps/gemini.rs` used. Does NOT reintroduce the gemini module.
    fn write_legacy_gemini_env(config: &Value) -> Result<(), String> {
        let path = Self::legacy_gemini_env_path();

        let mut entries: Vec<(String, String)> = Vec::new();
        if let Some(env_obj) = config.get("env").and_then(|v| v.as_object()) {
            for (key, value) in env_obj {
                if let Some(val_str) = value.as_str() {
                    entries.push((key.clone(), val_str.to_string()));
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let content = entries
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create gemini dir failed: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(parent) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o700);
                    let _ = std::fs::set_permissions(parent, perms);
                }
            }
        }

        write_text_file(&path, &content).map_err(|e| format!("write gemini env failed: {e}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
        Ok(())
    }

    /// LEGACY one-time restore of a stranded Gemini takeover. Reads the
    /// string-keyed `live_backup` row for app 'gemini'; if present, writes the
    /// backed-up `~/.gemini/.env` back, then clears the backup row and any
    /// stale 'gemini' proxy_config takeover flag so it runs exactly once. A
    /// no-op when no backup row exists.
    pub async fn restore_legacy_gemini_takeover(&self) -> Result<(), String> {
        let backup = self
            .db
            .get_live_backup("gemini")
            .await
            .map_err(|e| format!("read gemini live backup failed: {e}"))?;
        let Some(backup) = backup else {
            // No stranded Gemini takeover to restore.
            return Ok(());
        };

        let config: Value = serde_json::from_str(&backup.original_config)
            .map_err(|e| format!("parse gemini backup failed: {e}"))?;

        let backup_is_placeholder = config
            .get("env")
            .and_then(|v| v.as_object())
            .and_then(|env| env.get("GEMINI_API_KEY"))
            .and_then(|v| v.as_str())
            == Some(PROXY_TOKEN_PLACEHOLDER);

        if backup_is_placeholder {
            // The backup itself is a proxy placeholder (should not happen given
            // backup guards, but be defensive): don't re-poison the live file.
            log::warn!(
                "legacy gemini backup is itself a proxy placeholder; clearing takeover without rewriting ~/.gemini/.env"
            );
        } else {
            Self::write_legacy_gemini_env(&config)?;
            log::info!("restored legacy gemini live config (~/.gemini/.env) from live backup");
        }

        // Clear the backup row so this restore runs exactly once.
        self.db
            .delete_live_backup("gemini")
            .await
            .map_err(|e| format!("delete gemini live backup failed: {e}"))?;

        // Clear any stale 'gemini' proxy_config takeover flag.
        if let Ok(mut gemini_config) = self.db.get_proxy_config_for_app("gemini").await {
            if gemini_config.enabled {
                gemini_config.enabled = false;
                if let Err(e) = self.db.update_proxy_config_for_app(gemini_config).await {
                    log::warn!("clear gemini takeover flag failed: {e}");
                } else {
                    log::info!("cleared stale gemini takeover flag");
                }
            }
        }

        Ok(())
    }

    // ===================== live backup from provider =====================

    pub async fn update_live_backup_from_provider(
        &self,
        app_type: &str,
        provider: &Provider,
    ) -> Result<(), String> {
        let _guard = self.lock_switch_for_app(app_type).await;
        self.update_live_backup_from_provider_inner(app_type, provider)
            .await
    }

    async fn update_live_backup_from_provider_inner(
        &self,
        app_type: &str,
        provider: &Provider,
    ) -> Result<(), String> {
        let app_type_enum =
            AppType::from_str(app_type).map_err(|_| format!("unknown app type: {app_type}"))?;
        let mut effective_settings =
            build_effective_settings_with_common_config(self.db.as_ref(), &app_type_enum, provider)
                .map_err(|e| format!("build {app_type} effective settings failed: {e}"))?;

        if matches!(app_type_enum, AppType::Codex) {
            let existing_backup_value = self
                .db
                .get_live_backup(app_type)
                .await
                .map_err(|e| format!("read {app_type} existing backup failed: {e}"))?
                .map(|backup| {
                    serde_json::from_str::<Value>(&backup.original_config)
                        .map_err(|e| format!("parse {app_type} existing backup failed: {e}"))
                })
                .transpose()?;

            if let Some(existing_value) = existing_backup_value.as_ref() {
                Self::preserve_codex_mcp_servers_from_existing_config(
                    &mut effective_settings,
                    existing_value,
                )?;
                Self::preserve_codex_oauth_auth_in_backup(&mut effective_settings, existing_value)?;
            }

            codex_config::apply_codex_unified_session_bucket_to_settings(
                provider.category.as_deref(),
                &mut effective_settings,
            )
            .map_err(|e| format!("inject unified session route failed: {e}"))?;
        }

        let backup_json = match app_type_enum {
            AppType::Claude | AppType::Codex => serde_json::to_string(&effective_settings)
                .map_err(|e| format!("serialize {app_type} config failed: {e}"))?,
            _ => return Err(format!("unknown app type: {app_type}")),
        };

        self.db
            .save_live_backup(app_type, &backup_json)
            .await
            .map_err(|e| format!("update {app_type} backup failed: {e}"))?;
        log::info!("updated {app_type} live backup (hot-switch)");
        Ok(())
    }

    // ===================== hot switch =====================

    /// Required signature: `anyhow::Result<()>`. Wraps the outcome-returning
    /// implementation (the call site discards the outcome).
    pub async fn hot_switch_provider_inner(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> anyhow::Result<()> {
        self.hot_switch_provider_inner_outcome(app_type, provider_id)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Hot-switch wrapper that acquires the per-app switch lock first.
    pub async fn hot_switch_provider(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<HotSwitchOutcome, String> {
        let _guard = self.lock_switch_for_app(app_type).await;
        self.hot_switch_provider_inner_outcome(app_type, provider_id)
            .await
    }

    async fn hot_switch_provider_inner_outcome(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<HotSwitchOutcome, String> {
        let app_type_enum =
            AppType::from_str(app_type).map_err(|_| format!("invalid app type: {app_type}"))?;
        let provider = self
            .db
            .get_provider_by_id(provider_id, app_type)
            .map_err(|e| format!("read provider failed: {e}"))?
            .ok_or_else(|| format!("provider not found: {provider_id}"))?;

        if provider.category.as_deref() == Some("official") {
            return Err("cannot switch to official provider during proxy takeover".to_string());
        }

        let logical_target_changed =
            crate::settings::get_effective_current_provider(&self.db, &app_type_enum)
                .map_err(|e| format!("read current provider failed: {e}"))?
                .as_deref()
                != Some(provider_id);

        let has_backup = self
            .db
            .get_live_backup(app_type_enum.as_str())
            .await
            .map_err(|e| format!("read {app_type} backup failed: {e}"))?
            .is_some();
        let live_taken_over = self.detect_takeover_in_live_config_for_app(&app_type_enum);
        let should_sync_backup = has_backup || live_taken_over;

        self.db
            .set_current_provider(app_type_enum.as_str(), provider_id)
            .map_err(|e| format!("update current provider failed: {e}"))?;
        crate::settings::set_current_provider(&app_type_enum, Some(provider_id))
            .map_err(|e| format!("update local current provider failed: {e}"))?;

        if should_sync_backup {
            self.update_live_backup_from_provider_inner(app_type, &provider)
                .await?;
            if matches!(app_type_enum, AppType::Claude) {
                self.sync_claude_live_from_provider_while_proxy_active(&provider)
                    .await?;
            } else if live_taken_over && matches!(app_type_enum, AppType::Codex) {
                self.sync_codex_live_from_provider_while_proxy_active(&provider)
                    .await?;
            }
        }

        if has_backup && !live_taken_over && matches!(app_type_enum, AppType::Codex) {
            let effective_settings = build_effective_settings_with_common_config(
                self.db.as_ref(),
                &AppType::Codex,
                &provider,
            )
            .map_err(|e| format!("build codex effective settings failed: {e}"))?;
            let auth = effective_settings
                .get("auth")
                .ok_or_else(|| "codex provider missing auth config".to_string())?;
            let config_str = effective_settings.get("config").and_then(|v| v.as_str());
            codex_config::write_codex_provider_live_with_catalog(
                &effective_settings,
                provider.category.as_deref(),
                auth,
                config_str,
            )
            .map_err(|e| format!("write codex config failed: {e}"))?;
        }

        Ok(HotSwitchOutcome {
            logical_target_changed,
        })
    }

    /// Switch the proxy target provider (hot-switch + log).
    pub async fn switch_proxy_target(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<(), String> {
        let outcome = self.hot_switch_provider(app_type, provider_id).await?;
        if outcome.logical_target_changed {
            log::info!("proxy: switched {app_type} target to {provider_id}");
        } else {
            log::debug!("proxy: {app_type} already aligned to {provider_id}");
        }
        Ok(())
    }

    // ===================== codex preserve helpers =====================

    fn preserve_codex_mcp_servers_from_existing_config(
        target_settings: &mut Value,
        existing_config: &Value,
    ) -> Result<(), String> {
        let target_obj = target_settings
            .as_object_mut()
            .ok_or_else(|| "codex backup must be a JSON object".to_string())?;
        let target_config = target_obj
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut target_doc = if target_config.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            target_config
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| format!("parse new codex config.toml failed: {e}"))?
        };

        let existing_config = existing_config
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if existing_config.trim().is_empty() {
            target_obj.insert("config".to_string(), json!(target_doc.to_string()));
            return Ok(());
        }

        let existing_doc = existing_config
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse existing codex backup failed: {e}"))?;

        if let Some(existing_mcp_servers) = existing_doc.get("mcp_servers") {
            match target_doc.get_mut("mcp_servers") {
                Some(target_mcp_servers) => {
                    if let (Some(target_table), Some(existing_table)) = (
                        target_mcp_servers.as_table_like_mut(),
                        existing_mcp_servers.as_table_like(),
                    ) {
                        for (server_id, server_item) in existing_table.iter() {
                            if target_table.get(server_id).is_none() {
                                target_table.insert(server_id, server_item.clone());
                            }
                        }
                    } else {
                        log::warn!("codex config has non-table mcp_servers; skip MCP merge");
                    }
                }
                None => {
                    target_doc["mcp_servers"] = existing_mcp_servers.clone();
                }
            }
        }

        target_obj.insert("config".to_string(), json!(target_doc.to_string()));
        Ok(())
    }

    fn preserve_codex_oauth_auth_in_backup(
        target_settings: &mut Value,
        existing_backup: &Value,
    ) -> Result<(), String> {
        if !crate::settings::preserve_codex_official_auth_on_switch() {
            return Ok(());
        }
        let Some(existing_auth) = existing_backup
            .get("auth")
            .filter(|auth| codex_config::codex_auth_has_oauth_login_material(auth))
            .cloned()
        else {
            return Ok(());
        };
        let Some(target_obj) = target_settings.as_object_mut() else {
            return Ok(());
        };
        let provider_auth = target_obj.get("auth").cloned().unwrap_or_else(|| json!({}));
        if let Some(config_text) = target_obj.get("config").and_then(|value| value.as_str()) {
            let live_config =
                codex_config::prepare_codex_provider_live_config(&provider_auth, config_text)
                    .map_err(|e| format!("update codex backup config failed: {e}"))?;
            target_obj.insert("config".to_string(), json!(live_config));
        }
        target_obj.insert("auth".to_string(), existing_auth);
        Ok(())
    }

    // ===================== codex toml helpers =====================

    fn update_toml_base_url(toml_str: &str, new_url: &str) -> String {
        codex_config::update_codex_toml_field(toml_str, "base_url", new_url)
            .unwrap_or_else(|_| toml_str.to_string())
    }

    fn apply_codex_proxy_toml_config_for_provider(
        toml_str: &str,
        proxy_url: &str,
        provider: Option<&Provider>,
    ) -> String {
        let updated = Self::update_toml_base_url(toml_str, proxy_url);
        let mut updated = codex_config::update_codex_toml_field(&updated, "wire_api", "responses")
            .unwrap_or(updated);
        if let Some(upstream_model) =
            provider.and_then(crate::proxy::model_mapper::codex_provider_upstream_model)
        {
            updated = codex_config::update_codex_toml_field(&updated, "model", &upstream_model)
                .unwrap_or(updated);
        }
        updated
    }

    fn attach_codex_model_catalog_from_provider(
        live_config: &mut Value,
        provider: Option<&Provider>,
    ) {
        let Some(provider) = provider else {
            return;
        };
        let model_catalog = provider
            .settings_config
            .get("modelCatalog")
            .cloned()
            .unwrap_or_else(|| json!({ "models": [] }));
        if let Some(root) = live_config.as_object_mut() {
            root.insert("modelCatalog".to_string(), model_catalog);
        }
    }

    // ===================== live read/write =====================

    fn read_claude_live(&self) -> Result<Value, String> {
        let path = get_claude_settings_path();
        if !path.exists() {
            return Err("claude config file does not exist".to_string());
        }
        let mut value: Value =
            read_json_file(&path).map_err(|e| format!("read claude config failed: {e}"))?;
        if value.is_null() {
            value = json!({});
        }
        if !value.is_object() {
            return Err(format!(
                "claude config root must be a JSON object, path: {}",
                path.display()
            ));
        }
        Ok(value)
    }

    fn write_claude_live(&self, config: &Value) -> Result<(), String> {
        let path = get_claude_settings_path();
        let settings = sanitize_claude_settings_for_live(config);
        write_json_file(&path, &settings).map_err(|e| format!("write claude config failed: {e}"))
    }

    fn read_codex_live(&self) -> Result<Value, String> {
        codex_config::read_codex_live_settings()
            .map_err(|e| format!("read codex live config failed: {e}"))
    }

    fn write_codex_live(&self, config: &Value) -> Result<(), String> {
        self.write_codex_live_verbatim(config)
    }

    fn write_codex_live_for_provider(
        &self,
        config: &Value,
        provider: Option<&Provider>,
    ) -> Result<(), String> {
        let Some(provider) = provider else {
            if crate::settings::preserve_codex_official_auth_on_switch() {
                if let (Some(auth), Some(config_str)) = (
                    config.get("auth"),
                    config.get("config").and_then(|v| v.as_str()),
                ) {
                    if auth.get("OPENAI_API_KEY").and_then(|v| v.as_str())
                        == Some(PROXY_TOKEN_PLACEHOLDER)
                    {
                        let live_config =
                            codex_config::prepare_codex_provider_live_config(auth, config_str)
                                .map_err(|e| format!("write codex config failed: {e}"))?;
                        codex_config::write_codex_live_config_atomic(Some(&live_config))
                            .map_err(|e| format!("write codex config failed: {e}"))?;
                        return Ok(());
                    }
                }
            }
            return self.write_codex_live_verbatim(config);
        };

        let auth = config
            .get("auth")
            .ok_or_else(|| "codex config missing auth field".to_string())?;
        let config_str = config.get("config").and_then(|v| v.as_str());
        codex_config::write_codex_provider_live_with_catalog(
            config,
            provider.category.as_deref(),
            auth,
            config_str,
        )
        .map_err(|e| format!("write codex config failed: {e}"))
    }

    fn codex_auth_has_proxy_placeholder(auth: &Value) -> bool {
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER)
    }

    fn write_codex_takeover_live_for_provider(
        &self,
        config: &Value,
        provider: Option<&Provider>,
    ) -> Result<(), String> {
        if crate::settings::preserve_codex_official_auth_on_switch() {
            if let Some(auth) = config
                .get("auth")
                .filter(|auth| Self::codex_auth_has_proxy_placeholder(auth))
            {
                let config_str = config.get("config").and_then(|v| v.as_str()).unwrap_or("");
                let prepared_config =
                    codex_config::prepare_codex_live_config_text_with_optional_catalog(
                        config, config_str,
                    )
                    .map_err(|e| format!("write codex config failed: {e}"))?;
                let live_config =
                    codex_config::prepare_codex_provider_live_config(auth, &prepared_config)
                        .map_err(|e| format!("write codex config failed: {e}"))?;
                codex_config::write_codex_live_config_atomic(Some(&live_config))
                    .map_err(|e| format!("write codex config failed: {e}"))?;
                return Ok(());
            }
        }
        self.write_codex_live_for_provider(config, provider)
    }

    fn write_codex_live_verbatim(&self, config: &Value) -> Result<(), String> {
        use codex_config::{get_codex_auth_path, get_codex_config_path};

        let auth = config.get("auth");
        let config_str = config.get("config").and_then(|v| v.as_str());

        let prepared_cfg = config_str
            .map(|cfg| {
                codex_config::prepare_codex_live_config_text_with_optional_catalog(config, cfg)
            })
            .transpose()
            .map_err(|e| format!("write codex config failed: {e}"))?;

        match (auth, prepared_cfg.as_deref()) {
            (Some(auth), Some(cfg)) => {
                let auth_path = get_codex_auth_path();
                if auth.as_object().is_some_and(|obj| obj.is_empty()) {
                    let _ = delete_file(&auth_path);
                    let config_path = get_codex_config_path();
                    write_text_file(&config_path, cfg)
                        .map_err(|e| format!("write codex config failed: {e}"))?;
                } else {
                    codex_config::write_codex_live_atomic(auth, Some(cfg))
                        .map_err(|e| format!("write codex config failed: {e}"))?;
                }
            }
            (Some(auth), None) => {
                let auth_path = get_codex_auth_path();
                write_json_file(&auth_path, auth)
                    .map_err(|e| format!("write codex auth failed: {e}"))?;
            }
            (None, Some(cfg)) => {
                let config_path = get_codex_config_path();
                write_text_file(&config_path, cfg)
                    .map_err(|e| format!("write codex config failed: {e}"))?;
            }
            (None, None) => {}
        }
        Ok(())
    }

    // ===================== status / config =====================

    pub async fn get_status(&self) -> Result<ProxyStatus, String> {
        if let Some(server) = self.server.read().await.as_ref() {
            Ok(server.get_status().await)
        } else {
            Ok(ProxyStatus {
                running: false,
                ..Default::default()
            })
        }
    }

    pub async fn get_config(&self) -> Result<ProxyConfig, String> {
        self.db
            .get_proxy_config()
            .await
            .map_err(|e| format!("get proxy config failed: {e}"))
    }

    pub async fn update_config(&self, config: &ProxyConfig) -> Result<(), String> {
        let previous = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("get proxy config failed: {e}"))?;

        let mut new_config = config.clone();
        new_config.live_takeover_active = previous.live_takeover_active;
        self.db
            .update_proxy_config(new_config.clone())
            .await
            .map_err(|e| format!("save proxy config failed: {e}"))?;

        let mut server_guard = self.server.write().await;
        if server_guard.is_none() {
            return Ok(());
        }

        let require_restart = new_config.listen_address != previous.listen_address
            || new_config.listen_port != previous.listen_port;

        if require_restart {
            if let Some(server) = server_guard.take() {
                server
                    .stop()
                    .await
                    .map_err(|e| format!("stop server before restart failed: {e}"))?;
            }
            let new_server = ProxyServer::new(
                new_config.clone(),
                self.db.clone(),
                self.copilot_auth.clone(),
                self.codex_oauth.clone(),
            );
            let info = new_server
                .start()
                .await
                .map_err(|e| format!("restart proxy server failed: {e}"))?;
            if let Err(e) = self
                .persist_ephemeral_listen_port_if_needed(&new_config, info.port)
                .await
            {
                let _ = new_server.stop().await;
                return Err(e);
            }
            *server_guard = Some(new_server);
            drop(server_guard);

            if let Ok(takeover) = self.get_takeover_status().await {
                if takeover.claude {
                    self.takeover_live_config_best_effort(&AppType::Claude)
                        .await?;
                }
                if takeover.codex {
                    self.takeover_live_config_best_effort(&AppType::Codex)
                        .await?;
                }
            }
            return Ok(());
        } else if let Some(server) = server_guard.as_ref() {
            server.apply_runtime_config(&new_config).await;
        }
        Ok(())
    }

    pub async fn is_running(&self) -> bool {
        self.server.read().await.is_some()
    }

    pub async fn update_circuit_breaker_configs(
        &self,
        config: crate::proxy::CircuitBreakerConfig,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server.update_circuit_breaker_configs(config).await;
        }
        Ok(())
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: crate::proxy::CircuitBreakerConfig,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .update_circuit_breaker_config_for_app(app_type, config)
                .await;
        }
        Ok(())
    }

    pub async fn reset_provider_circuit_breaker(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .reset_provider_circuit_breaker(provider_id, app_type)
                .await;
        }
        Ok(())
    }

    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::CircuitBreakerStats> {
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .get_circuit_breaker_stats(provider_id, app_type)
                .await
        } else {
            None
        }
    }
}

#[cfg(test)]
mod legacy_gemini_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_service(db: Arc<Database>, dir: std::path::PathBuf) -> ProxyService {
        let copilot_auth = Arc::new(RwLock::new(CopilotAuthManager::new(dir.clone())));
        let codex_oauth = Arc::new(RwLock::new(CodexOAuthManager::new(dir)));
        ProxyService::new(db, copilot_auth, codex_oauth)
    }

    #[tokio::test]
    async fn restore_legacy_gemini_takeover_restores_env_and_clears_backup() {
        let _guard = crate::test_support::env_lock();
        let temp = tempfile::tempdir().unwrap();
        let old_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        let old_home = std::env::var_os("HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        std::env::set_var("HOME", temp.path());

        // Simulate a stranded takeover: ~/.gemini/.env poisoned with the proxy
        // placeholder + a local-proxy base URL.
        let gemini_dir = temp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        let env_path = gemini_dir.join(".env");
        std::fs::write(
            &env_path,
            format!("GEMINI_API_KEY={PROXY_TOKEN_PLACEHOLDER}\nGOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8080"),
        )
        .unwrap();

        let db = Arc::new(Database::memory().unwrap());
        // Real backed-up config (what backup_live_configs would have stored).
        let backup = json!({
            "env": {
                "GEMINI_API_KEY": "real-secret-key",
                "GOOGLE_GEMINI_BASE_URL": "https://generativelanguage.googleapis.com"
            }
        });
        db.save_live_backup("gemini", &serde_json::to_string(&backup).unwrap())
            .await
            .unwrap();

        let service = make_service(db.clone(), temp.path().to_path_buf());
        service.restore_legacy_gemini_takeover().await.unwrap();

        // Live .env restored to the real config (no placeholder / no local proxy).
        let restored = std::fs::read_to_string(&env_path).unwrap();
        assert!(restored.contains("GEMINI_API_KEY=real-secret-key"), "{restored}");
        assert!(
            restored.contains("GOOGLE_GEMINI_BASE_URL=https://generativelanguage.googleapis.com"),
            "{restored}"
        );
        assert!(!restored.contains(PROXY_TOKEN_PLACEHOLDER), "{restored}");
        assert!(!restored.contains("127.0.0.1"), "{restored}");

        // Backup row cleared; second call is a no-op that leaves the file intact.
        assert!(db.get_live_backup("gemini").await.unwrap().is_none());
        service.restore_legacy_gemini_takeover().await.unwrap();
        let after = std::fs::read_to_string(&env_path).unwrap();
        assert_eq!(restored, after);
        assert!(db.get_live_backup("gemini").await.unwrap().is_none());

        match old_test_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[tokio::test]
    async fn restore_legacy_gemini_takeover_is_noop_without_backup() {
        let _guard = crate::test_support::env_lock();
        let temp = tempfile::tempdir().unwrap();
        let old_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        let old_home = std::env::var_os("HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        std::env::set_var("HOME", temp.path());

        let db = Arc::new(Database::memory().unwrap());
        let service = make_service(db.clone(), temp.path().to_path_buf());
        // No 'gemini' backup row -> must be a no-op, no ~/.gemini/.env created.
        service.restore_legacy_gemini_takeover().await.unwrap();
        assert!(!temp.path().join(".gemini").join(".env").exists());

        match old_test_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
