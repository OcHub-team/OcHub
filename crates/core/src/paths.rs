//! Path resolution + deterministic JSON / atomic file IO.
//! Ported from cc-switch `config.rs`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::AppError;

/// Resolve the user home directory, honoring `OCHUB_TEST_HOME` for tests.
///
/// On Windows we deliberately use `dirs::home_dir()` (real profile) rather than
/// `$HOME`, which third-party tools (Git/Cygwin/MSYS) may inject.
pub fn get_home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("OCHUB_TEST_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    dirs::home_dir().unwrap_or_else(|| {
        log::warn!("无法获取用户主目录，回退到当前目录");
        PathBuf::from(".")
    })
}

/// Claude Code config directory (`~/.claude`, or the override).
pub fn get_claude_config_dir() -> PathBuf {
    if let Some(custom) = crate::settings::get_claude_override_dir() {
        return custom;
    }
    get_home_dir().join(".claude")
}

/// Default Claude MCP config path (`~/.claude.json`).
pub fn get_default_claude_mcp_path() -> PathBuf {
    get_home_dir().join(".claude.json")
}

fn derive_mcp_path_from_override(dir: &Path) -> Option<PathBuf> {
    let file_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())?
        .trim()
        .to_string();
    if file_name.is_empty() {
        return None;
    }
    let parent = dir.parent().unwrap_or_else(|| Path::new(""));
    Some(parent.join(format!("{file_name}.json")))
}

/// Claude MCP config path; lives next to the override dir when set.
pub fn get_claude_mcp_path() -> PathBuf {
    if let Some(custom_dir) = crate::settings::get_claude_override_dir() {
        if let Some(path) = derive_mcp_path_from_override(&custom_dir) {
            return path;
        }
    }
    get_default_claude_mcp_path()
}

/// Claude Code primary settings file path.
pub fn get_claude_settings_path() -> PathBuf {
    let dir = get_claude_config_dir();
    let settings = dir.join("settings.json");
    if settings.exists() {
        return settings;
    }
    let legacy = dir.join("claude.json");
    if legacy.exists() {
        return legacy;
    }
    settings
}

/// App config directory (`~/.ochub`, or the store override).
pub fn get_app_config_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OCHUB_DATA_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Some(custom) = crate::app_store::get_app_config_dir_override() {
        return custom;
    }

    get_home_dir().join(".ochub")
}

/// App config file path (`~/.ochub/config.json`, legacy import source).
pub fn get_app_config_path() -> PathBuf {
    get_app_config_dir().join("config.json")
}

/// SQLite database path (`~/.ochub/ochub.db`).
pub fn get_database_path() -> PathBuf {
    get_app_config_dir().join("ochub.db")
}

/// Legacy cc-switch data directory (`~/.cc-switch`) — one-time import source.
/// OcHub never writes here.
pub fn get_legacy_ccswitch_dir() -> PathBuf {
    get_home_dir().join(".cc-switch")
}

/// Legacy cc-switch SQLite database path (`~/.cc-switch/cc-switch.db`).
pub fn get_legacy_ccswitch_database_path() -> PathBuf {
    get_legacy_ccswitch_dir().join("cc-switch.db")
}

/// Legacy cc-switch JSON config path (`~/.cc-switch/config.json`), used by
/// cc-switch before it moved to SQLite. Second choice as an import source:
/// the database carries everything this file does and more.
pub fn get_legacy_ccswitch_config_path() -> PathBuf {
    get_legacy_ccswitch_dir().join("config.json")
}

/// A path with the home directory written as `~`, for display.
///
/// Paths under the home directory are most of what OcHub shows, and the
/// absolute form spends its first thirty characters saying nothing while
/// pushing the part that identifies the file out of a narrow container.
pub fn abbreviate_home(path: &Path) -> String {
    let home = get_home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Sanitize a provider name into a filesystem-safe lowercase string.
pub fn sanitize_provider_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            _ => c,
        })
        .collect::<String>()
        .to_lowercase()
}

pub fn get_provider_config_path(provider_id: &str, provider_name: Option<&str>) -> PathBuf {
    let base_name = provider_name
        .map(sanitize_provider_name)
        .unwrap_or_else(|| sanitize_provider_name(provider_id));
    get_claude_config_dir().join(format!("settings-{base_name}.json"))
}

/// Read a JSON config file into a typed value.
pub fn read_json_file<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!("文件不存在: {}", path.display())));
    }
    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;
    serde_json::from_str(&content).map_err(|e| AppError::json(path, e))
}

/// Recursively sort object keys for deterministic serialization.
pub fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted_map = Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted_map.insert(key.clone(), sort_json_keys(&map[key]));
            }
            Value::Object(sorted_map)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

/// Write a JSON config file with keys sorted (deterministic output).
pub fn write_json_file<T: Serialize>(path: &Path, data: &T) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    let value = serde_json::to_value(data).map_err(|e| AppError::JsonSerialize { source: e })?;
    let sorted_value = sort_json_keys(&value);
    let json = serde_json::to_string_pretty(&sorted_value)
        .map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(path, json.as_bytes())
}

/// Atomic write for plain text (TOML / YAML / arbitrary text).
pub fn write_text_file(path: &Path, data: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    atomic_write(path, data.as_bytes())
}

/// Atomic write: write to a temp file then rename to avoid half-written state.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("无效的路径".to_string()))?;
    let mut tmp = parent.to_path_buf();
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::Config("无效的文件名".to_string()))?
        .to_string_lossy()
        .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    tmp.push(format!("{file_name}.tmp.{ts}"));

    {
        let mut f = fs::File::create(&tmp).map_err(|e| AppError::io(&tmp, e))?;
        f.write_all(data).map_err(|e| AppError::io(&tmp, e))?;
        f.flush().map_err(|e| AppError::io(&tmp, e))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let perm = meta.permissions().mode();
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(perm));
        }
    }

    #[cfg(windows)]
    {
        if path.exists() {
            let _ = fs::remove_file(path);
        }
    }

    fs::rename(&tmp, path).map_err(|e| AppError::IoContext {
        context: format!("原子替换失败: {} -> {}", tmp.display(), path.display()),
        source: e,
    })?;
    Ok(())
}

pub fn copy_file(from: &Path, to: &Path) -> Result<(), AppError> {
    fs::copy(from, to).map_err(|e| AppError::IoContext {
        context: format!("复制文件失败 ({} -> {})", from.display(), to.display()),
        source: e,
    })?;
    Ok(())
}

pub fn delete_file(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        fs::remove_file(path).map_err(|e| AppError::io(path, e))?;
    }
    Ok(())
}

/// Existence + path of a config file.
#[derive(Serialize, Deserialize)]
pub struct ConfigStatus {
    pub exists: bool,
    pub path: String,
}

pub fn get_claude_config_status() -> ConfigStatus {
    let path = get_claude_settings_path();
    ConfigStatus {
        exists: path.exists(),
        path: path.to_string_lossy().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_data_dir_has_precedence_over_store_and_home() {
        let _guard = crate::test_support::env_lock();
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::env::set_var("OCHUB_TEST_HOME", temp.path());
        std::env::set_var("OCHUB_DATA_DIR", &data);
        assert_eq!(get_app_config_dir(), data);
        std::env::remove_var("OCHUB_DATA_DIR");
        std::env::remove_var("OCHUB_TEST_HOME");
    }

    #[test]
    fn derive_mcp_path_preserves_folder_name() {
        let dir = PathBuf::from("/tmp/profile/.claude");
        assert_eq!(
            derive_mcp_path_from_override(&dir).unwrap(),
            PathBuf::from("/tmp/profile/.claude.json")
        );
    }

    #[test]
    fn derive_mcp_path_root_returns_none() {
        assert!(derive_mcp_path_from_override(&PathBuf::from("/")).is_none());
    }

    #[test]
    fn sort_json_keys_is_deterministic() {
        let mut a = Map::new();
        a.insert("z".into(), serde_json::json!(1));
        a.insert("a".into(), serde_json::json!(2));
        let mut b = Map::new();
        b.insert("a".into(), serde_json::json!(2));
        b.insert("z".into(), serde_json::json!(1));
        assert_eq!(
            serde_json::to_string(&sort_json_keys(&Value::Object(a))).unwrap(),
            serde_json::to_string(&sort_json_keys(&Value::Object(b))).unwrap(),
        );
    }

    #[test]
    fn abbreviate_home_replaces_only_the_home_prefix() {
        let home = get_home_dir();
        assert_eq!(
            abbreviate_home(&home.join(".cc-switch/config.json")),
            "~/.cc-switch/config.json"
        );
        assert_eq!(
            abbreviate_home(Path::new("/opt/shared/config.json")),
            "/opt/shared/config.json"
        );
    }

    #[test]
    fn atomic_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("file.json");
        atomic_write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}");
    }
}
