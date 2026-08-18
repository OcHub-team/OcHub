use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use super::store::write_secret_json;
use crate::error::AppError;
use crate::paths::{get_claude_config_dir, get_home_dir};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub fn live_path() -> PathBuf {
    get_claude_config_dir().join(".credentials.json")
}

pub fn read_live() -> Result<Option<Value>, AppError> {
    if use_keychain()
        && let Some(blob) = read_keychain()?
    {
        return Ok(Some(blob));
    }
    read_file()
}

pub fn write_live(blob: &Value) -> Result<(), AppError> {
    if use_keychain() {
        write_keychain(blob)?;
    }
    write_secret_json(&live_path(), blob)
}

pub fn clear_live() -> Result<(), AppError> {
    if use_keychain() {
        let mut command = Command::new("security");
        command.args(["delete-generic-password", "-s", KEYCHAIN_SERVICE]);
        if let Some(account) = existing_keychain_account() {
            command.args(["-a", &account]);
        }
        let _ = command.output();
    }
    let path = live_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|error| AppError::io(&path, error))?;
    }
    Ok(())
}

fn read_file() -> Result<Option<Value>, AppError> {
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
    let Some(env) = settings.get("env").and_then(Value::as_object) else {
        return true;
    };
    !["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        .iter()
        .any(|key| {
            env.get(*key)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
        })
}

fn use_keychain() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    // Tests must never touch the real login keychain.
    std::env::var_os("OCHUB_TEST_HOME").is_none()
}

fn read_keychain() -> Result<Option<Value>, AppError> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .map_err(|error| {
            AppError::localized(
                "official_auth.claude.keychain.read",
                format!("读取 Claude Keychain 失败: {error}"),
                format!("failed to read Claude Keychain: {error}"),
            )
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let json = json.trim();
    if json.is_empty() {
        return Ok(None);
    }
    let value = serde_json::from_str(json).map_err(|error| {
        AppError::localized(
            "official_auth.claude.keychain.parse",
            format!("解析 Claude Keychain 失败: {error}"),
            format!("failed to parse Claude Keychain: {error}"),
        )
    })?;
    Ok(Some(value))
}

fn write_keychain(blob: &Value) -> Result<(), AppError> {
    let json =
        serde_json::to_string(blob).map_err(|error| AppError::JsonSerialize { source: error })?;
    let account = existing_keychain_account().unwrap_or_else(default_keychain_account);
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &account,
            "-w",
            &json,
            "-U",
        ])
        .output()
        .map_err(|error| {
            AppError::localized(
                "official_auth.claude.keychain.write",
                format!("写入 Claude Keychain 失败: {error}"),
                format!("failed to write Claude Keychain: {error}"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::localized(
            "official_auth.claude.keychain.write_failed",
            format!("写入 Claude Keychain 失败: {stderr}"),
            format!("failed to write Claude Keychain: {stderr}"),
        ));
    }
    Ok(())
}

fn existing_keychain_account() -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_keychain_account(&String::from_utf8_lossy(&output.stdout))
}

fn parse_keychain_account(dump: &str) -> Option<String> {
    for line in dump.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"acct\"")
            && let Some(start) = rest.rfind("=\"")
        {
            let value = &rest[start + 2..];
            return Some(value.trim_end_matches('"').to_string()).filter(|s| !s.is_empty());
        }
    }
    None
}

fn default_keychain_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| {
            get_home_dir()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ochub")
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::parse_keychain_account;

    #[test]
    fn parses_security_acct_line() {
        let dump = r#"
keychain: "/Users/me/Library/Keychains/login.keychain-db"
class: "genp"
attributes:
    "acct"<blob>="sleepstars"
    "svce"<blob>="Claude Code-credentials"
"#;
        assert_eq!(parse_keychain_account(dump).as_deref(), Some("sleepstars"));
    }
}
