//! Runtime ownership, mutation serialization, and transport-neutral IPC frames.

pub mod journal;

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::{ApplicationError, ApplicationResult};

pub const PROTOCOL_VERSION: u32 = 1;

static LIGHTWEIGHT_MODE: AtomicBool = AtomicBool::new(false);

pub fn lightweight_mode() -> bool {
    LIGHTWEIGHT_MODE.load(Ordering::Acquire)
}

pub fn set_lightweight_mode(enabled: bool) {
    LIGHTWEIGHT_MODE.store(enabled, Ordering::Release);
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    Gui,
    Daemon,
    Foreground,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OwnerRecord {
    pub protocol_version: u32,
    pub pid: u32,
    pub kind: OwnerKind,
    pub started_at: String,
    pub data_dir: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcRequest {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: Value,
    pub exit_code: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcResponse {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

pub fn default_root_dir() -> PathBuf {
    crate::paths::get_home_dir().join(".ochub")
}

pub fn runtime_dir() -> PathBuf {
    default_root_dir().join("runtime")
}

pub fn owner_lock_path() -> PathBuf {
    runtime_dir().join("owner.lock")
}

pub fn mutation_lock_path() -> PathBuf {
    runtime_dir().join("mutation.lock")
}

pub fn owner_record_path() -> PathBuf {
    runtime_dir().join("owner.json")
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    runtime_dir().join("ochub.sock")
}

#[cfg(windows)]
pub fn socket_path() -> PathBuf {
    runtime_dir().join("ochub.pipe")
}

pub fn operations_dir() -> PathBuf {
    runtime_dir().join("operations")
}

pub fn ensure_runtime_dir() -> ApplicationResult<PathBuf> {
    let path = runtime_dir();
    fs::create_dir_all(&path)
        .map_err(|error| ApplicationError::Core(crate::AppError::io(path.as_path(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|error| ApplicationError::Core(crate::AppError::io(path.as_path(), error)))?;
    }
    Ok(path)
}

fn open_lock(path: &Path) -> ApplicationResult<File> {
    if let Some(parent) = path.parent() {
        ensure_runtime_dir()?;
        fs::create_dir_all(parent)
            .map_err(|error| ApplicationError::Core(crate::AppError::io(parent, error)))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| ApplicationError::Core(crate::AppError::io(path, error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ApplicationError::Core(crate::AppError::io(path, error)))?;
    }
    Ok(file)
}

pub struct OwnerGuard {
    file: File,
    record: OwnerRecord,
}

impl OwnerGuard {
    pub fn acquire(kind: OwnerKind, data_dir: &Path, endpoint: String) -> ApplicationResult<Self> {
        let path = owner_lock_path();
        let mut file = open_lock(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == ErrorKind::WouldBlock {
                ApplicationError::OwnerConflict(
                    read_owner_record()
                        .ok()
                        .flatten()
                        .map(|record| format!("{} (pid {})", record.endpoint, record.pid))
                        .unwrap_or_else(|| "owner lock is held".to_string()),
                )
            } else {
                ApplicationError::Core(crate::AppError::io(&path, error))
            }
        })?;
        let record = OwnerRecord {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            kind,
            started_at: chrono::Utc::now().to_rfc3339(),
            data_dir: data_dir.to_string_lossy().into_owned(),
            endpoint,
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|source| crate::AppError::JsonSerialize { source })?;
        file.set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(&bytes))
            .and_then(|_| file.sync_data())
            .map_err(|error| ApplicationError::Core(crate::AppError::io(&path, error)))?;
        crate::paths::write_json_file(&owner_record_path(), &record)?;
        Ok(Self { file, record })
    }

    pub fn record(&self) -> &OwnerRecord {
        &self.record
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if read_owner_record()
            .ok()
            .flatten()
            .is_some_and(|record| record.pid == self.record.pid)
        {
            let _ = fs::remove_file(owner_record_path());
        }
        let _ = self.file.unlock();
    }
}

pub struct MutationGuard {
    file: File,
}

impl MutationGuard {
    pub fn acquire() -> ApplicationResult<Self> {
        let path = mutation_lock_path();
        let file = open_lock(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == ErrorKind::WouldBlock {
                ApplicationError::OwnerConflict("another mutation is in progress".to_string())
            } else {
                ApplicationError::Core(crate::AppError::io(&path, error))
            }
        })?;
        Ok(Self { file })
    }
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn read_owner_record() -> ApplicationResult<Option<OwnerRecord>> {
    let path = owner_record_path();
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .map_err(|error| ApplicationError::Core(crate::AppError::io(&path, error)))?;
    let record = serde_json::from_slice(&bytes)
        .map_err(|source| ApplicationError::Core(crate::AppError::json(&path, source)))?;
    Ok(Some(record))
}

pub fn active_owner() -> ApplicationResult<Option<OwnerRecord>> {
    let path = owner_lock_path();
    let file = open_lock(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(None)
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => read_owner_record(),
        Err(error) => Err(ApplicationError::Core(crate::AppError::io(&path, error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_frames_use_versioned_camel_case_contract() {
        let frame = IpcRequest {
            frame_type: "request".to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: "request-1".to_string(),
            operation: "status".to_string(),
            params: Value::Null,
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["protocolVersion"], 1);
        assert_eq!(value["requestId"], "request-1");
    }
}
