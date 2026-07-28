use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::CliError;

const MAX_STRUCTURED_INPUT_BYTES: u64 = 16 * 1024 * 1024;

pub fn read_text_limited(path: &Path, max_bytes: u64) -> Result<String, CliError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(CliError::InvalidInput(format!(
            "input must be a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(CliError::InvalidInput(format!(
            "input exceeds {max_bytes} bytes: {}",
            path.display()
        )));
    }
    Ok(std::fs::read_to_string(path)?)
}

pub fn read_structured<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(CliError::InvalidInput(format!(
            "structured input must be a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_STRUCTURED_INPUT_BYTES {
        return Err(CliError::InvalidInput(format!(
            "structured input exceeds {MAX_STRUCTURED_INPUT_BYTES} bytes: {}",
            path.display()
        )));
    }
    let content = read_text_limited(path, MAX_STRUCTURED_INPUT_BYTES)?;
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => Ok(serde_json::from_str(&content)?),
        Some("yaml" | "yml") => Ok(serde_yaml::from_str(&content)?),
        _ => serde_json::from_str(&content)
            .or_else(|_| serde_yaml::from_str(&content))
            .map_err(|error| {
                CliError::InvalidInput(format!(
                    "cannot parse {} as JSON or YAML: {error}",
                    path.display()
                ))
            }),
    }
}

pub fn write_structured<T: Serialize>(path: &Path, value: &T) -> Result<(), CliError> {
    let content = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("yaml" | "yml") => serde_yaml::to_string(value)?,
        _ => serde_json::to_string_pretty(value)?,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() && std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(CliError::InvalidInput(format!(
            "refusing to replace a symbolic link: {}",
            path.display()
        )));
    }
    ochub_core::paths::atomic_write(path, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn parse_value(raw: &str, force_string: bool) -> Value {
    if force_string {
        Value::String(raw.to_string())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
    }
}
