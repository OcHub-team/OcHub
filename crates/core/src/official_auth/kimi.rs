use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::store::write_secret_json;
use crate::apps::kimi_code::get_kimi_code_config_dir;
use crate::error::AppError;

pub fn live_path() -> PathBuf {
    get_kimi_code_config_dir()
        .join("credentials")
        .join("kimi-code.json")
}

pub fn read_live() -> Result<Option<Value>, AppError> {
    with_live_lock(read_live_unlocked)
}

pub fn write_live(blob: &Value) -> Result<(), AppError> {
    with_live_lock(|| {
        write_secret_json(&live_path(), blob)?;
        Ok(())
    })
}

fn read_live_unlocked() -> Result<Option<Value>, AppError> {
    let path = live_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value = serde_json::from_str(&text).map_err(|error| AppError::json(&path, error))?;
    Ok(Some(value))
}

pub fn settings_look_like_official(settings: &Value) -> bool {
    let Some(providers) = settings.get("providers").and_then(Value::as_object) else {
        return false;
    };
    providers.values().any(|provider| {
        let key = provider
            .get("oauth")
            .and_then(|oauth| oauth.get("key"))
            .and_then(Value::as_str);
        let api_key = provider
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        key == Some("oauth/kimi-code") && api_key.is_empty()
    })
}

/// Best-effort compatible lock: sentinel file + exclusive `{sentinel}.lock` dir.
fn with_live_lock<T>(op: impl FnOnce() -> Result<T, AppError>) -> Result<T, AppError> {
    if std::env::var_os("KIMI_DISABLE_OAUTH_LOCK").is_some() {
        return op();
    }

    let home = get_kimi_code_config_dir();
    let oauth_dir = home.join("oauth");
    fs::create_dir_all(&oauth_dir).map_err(|error| AppError::io(&oauth_dir, error))?;
    let sentinel = oauth_dir.join("kimi-code");
    if sentinel.is_dir() {
        return Err(AppError::localized(
            "official_auth.kimi.lock.sentinel_is_dir",
            "Kimi OAuth 锁路径被建成了目录，无法与 CLI 共用",
            "Kimi OAuth lock sentinel is a directory; cannot share the lock with the CLI",
        ));
    }
    if !sentinel.exists() {
        fs::write(&sentinel, b"").map_err(|error| AppError::io(&sentinel, error))?;
    }

    let lock_dir = oauth_dir.join("kimi-code.lock");
    acquire_lock_dir(&lock_dir)?;
    let result = op();
    let _ = fs::remove_dir_all(&lock_dir);
    result
}

fn acquire_lock_dir(lock_dir: &Path) -> Result<(), AppError> {
    const STALE: Duration = Duration::from_secs(10);
    const RETRIES: u32 = 120;

    for attempt in 0..RETRIES {
        match fs::create_dir(lock_dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(lock_dir, STALE) {
                    let _ = fs::remove_dir_all(lock_dir);
                    continue;
                }
                let sleep_ms = 500 + u64::from(attempt % 2) * 250;
                thread::sleep(Duration::from_millis(sleep_ms));
            }
            Err(error) => return Err(AppError::io(lock_dir, error)),
        }
    }
    Err(AppError::localized(
        "official_auth.kimi.lock.timeout",
        "等待 Kimi 凭据锁超时",
        "timed out waiting for the Kimi credential lock",
    ))
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age >= stale)
        .unwrap_or(false)
}
