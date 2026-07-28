//! App-config-dir override store.
//!
//! cc-switch stored this in a Tauri plugin-store file (`app_paths.json`). We
//! reimplement it as a plain flat-JSON file at a *fixed* bootstrap location
//! (`~/.cc-switch/app_paths.json`) — it can't live inside the directory it
//! points at. The on-disk shape (`{"app_config_dir_override": "<path>"}`) is
//! the same flat object Tauri's store produced, so existing values still load.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use crate::error::AppError;

const STORE_KEY_APP_CONFIG_DIR: &str = "app_config_dir_override";

static APP_CONFIG_DIR_OVERRIDE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

fn override_cache() -> &'static RwLock<Option<PathBuf>> {
    APP_CONFIG_DIR_OVERRIDE.get_or_init(|| RwLock::new(None))
}

fn update_cached_override(value: Option<PathBuf>) {
    if let Ok(mut guard) = override_cache().write() {
        *guard = value;
    }
}

/// Fixed bootstrap path for the override file. Never subject to the override.
fn store_path() -> PathBuf {
    crate::paths::get_home_dir()
        .join(".ochub")
        .join("app_paths.json")
}

fn read_store_object() -> serde_json::Map<String, Value> {
    let path = store_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return serde_json::Map::new();
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

/// Get the cached override (populated by [`refresh_app_config_dir_override`]).
pub fn get_app_config_dir_override() -> Option<PathBuf> {
    override_cache().read().ok()?.clone()
}

fn read_override_from_store() -> Option<PathBuf> {
    let map = read_store_object();
    match map.get(STORE_KEY_APP_CONFIG_DIR) {
        Some(Value::String(path_str)) => {
            let path_str = path_str.trim();
            if path_str.is_empty() {
                return None;
            }
            let path = resolve_path(path_str);
            if !path.exists() {
                log::warn!(
                    "app_paths.json 中配置的 app_config_dir 不存在: {path:?}，将使用默认路径。"
                );
                return None;
            }
            log::info!("使用 app_paths.json 中的 app_config_dir: {path:?}");
            Some(path)
        }
        Some(_) => {
            log::warn!("app_paths.json 中的 {STORE_KEY_APP_CONFIG_DIR} 类型不正确，应为字符串");
            None
        }
        None => None,
    }
}

/// Re-read the override file and refresh the cache.
pub fn refresh_app_config_dir_override() -> Option<PathBuf> {
    let value = read_override_from_store();
    update_cached_override(value.clone());
    value
}

/// Persist (or clear) the app_config_dir override and refresh the cache.
pub fn set_app_config_dir_to_store(path: Option<&str>) -> Result<(), AppError> {
    let mut map = read_store_object();
    match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(trimmed) => {
            map.insert(
                STORE_KEY_APP_CONFIG_DIR.to_string(),
                Value::String(trimmed.to_string()),
            );
            log::info!("已将 app_config_dir 写入 app_paths.json: {trimmed}");
        }
        None => {
            map.remove(STORE_KEY_APP_CONFIG_DIR);
            log::info!("已从 app_paths.json 中删除 app_config_dir 配置");
        }
    }

    let path = store_path();
    crate::paths::write_json_file(&path, &Value::Object(map))?;
    refresh_app_config_dir_override();
    Ok(())
}

/// Resolve a `~`-prefixed path.
fn resolve_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if let Some(stripped) = raw.strip_prefix("~\\")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(raw)
}
