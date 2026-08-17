use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::OfficialTool;
use crate::error::AppError;
use crate::paths::get_app_config_dir;

pub fn official_auth_dir() -> PathBuf {
    get_app_config_dir().join("official_auth")
}

fn tool_dir(tool: OfficialTool) -> PathBuf {
    official_auth_dir().join(tool.as_str())
}

fn catalog_path(tool: OfficialTool, provider_id: &str) -> Result<PathBuf, AppError> {
    Ok(tool_dir(tool).join(format!("{}.json", sanitize_provider_id(provider_id)?)))
}

fn sanitize_provider_id(provider_id: &str) -> Result<&str, AppError> {
    if provider_id.is_empty()
        || provider_id == "."
        || provider_id == ".."
        || !provider_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(AppError::localized(
            "official_auth.provider_id.invalid",
            "官方凭据卡 ID 无效",
            "invalid official credential card id",
        ));
    }
    Ok(provider_id)
}

pub fn read_catalog(tool: OfficialTool, provider_id: &str) -> Result<Option<Value>, AppError> {
    let path = catalog_path(tool, provider_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| AppError::io(&path, error))?;
    let value = serde_json::from_str(&text).map_err(|error| AppError::json(&path, error))?;
    Ok(Some(value))
}

pub fn write_catalog(tool: OfficialTool, provider_id: &str, blob: &Value) -> Result<(), AppError> {
    let path = catalog_path(tool, provider_id)?;
    write_secret_json(&path, blob)
}

pub fn delete_catalog(tool: OfficialTool, provider_id: &str) -> Result<(), AppError> {
    let path = catalog_path(tool, provider_id)?;
    if path.exists() {
        fs::remove_file(&path).map_err(|error| AppError::io(&path, error))?;
    }
    Ok(())
}

pub fn write_secret_json(path: &Path, value: &Value) -> Result<(), AppError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|error| AppError::JsonSerialize { source: error })?;
    write_secret_bytes(path, json.as_bytes())
}

pub fn write_secret_bytes(path: &Path, data: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::localized(
            "official_auth.path.invalid",
            "官方凭据路径无效",
            "invalid official credential path",
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    let tmp = parent.join(format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cred"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    {
        let mut file = fs::File::create(&tmp).map_err(|error| AppError::io(&tmp, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        file.write_all(data)
            .map_err(|error| AppError::io(&tmp, error))?;
        file.flush().map_err(|error| AppError::io(&tmp, error))?;
        let _ = file.sync_all();
    }

    #[cfg(windows)]
    if path.exists() {
        let _ = fs::remove_file(path);
    }

    fs::rename(&tmp, path).map_err(|error| AppError::IoContext {
        context: format!("原子替换失败: {} -> {}", tmp.display(), path.display()),
        source: error,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{env_lock, remove_var, set_var};

    #[test]
    #[cfg(unix)]
    fn secret_file_is_0600() {
        let _guard = env_lock();
        let home = tempfile::tempdir().unwrap();
        set_var("OCHUB_TEST_HOME", home.path());
        crate::settings::reload_settings().ok();
        let path = official_auth_dir().join("kimi").join("card.json");
        write_secret_json(&path, &serde_json::json!({"access_token": "x"})).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        remove_var("OCHUB_TEST_HOME");
        crate::settings::reload_settings().ok();
    }
}
