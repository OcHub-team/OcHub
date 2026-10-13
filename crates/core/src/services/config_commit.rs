//! Recoverable file commits. Multiple renames are not a filesystem-wide atomic transaction.
//! Private journals retain exact bytes; public receipts expose paths and fingerprints only.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::AppError;

#[derive(Clone)]
pub struct PreparedFileWrite {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub file_mode: Option<u32>,
    pub dir_mode: Option<u32>,
}

#[derive(Clone, Default)]
pub struct PreparedConfigWrite {
    pub files: Vec<PreparedFileWrite>,
}

impl PreparedFileWrite {
    pub fn replace(path: PathBuf, after: Vec<u8>) -> Result<Self, AppError> {
        let before = read_optional(&path)?;
        Ok(Self {
            path,
            before,
            after: Some(after),
            file_mode: None,
            dir_mode: None,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConfigApplyState {
    Prepared,
    FilesWritten,
    Completed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFileChange {
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigApplyReceipt {
    pub id: String,
    pub app: Option<String>,
    pub provider_id: Option<String>,
    pub created_at: i64,
    pub state: ConfigApplyState,
    pub files: Vec<ConfigFileChange>,
    pub undo_available: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct JournalFile {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    before_mode: Option<u32>,
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
    parent_before_mode: Option<u32>,
    created_directories: Vec<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    schema: u32,
    id: String,
    created_at: i64,
    state: ConfigApplyState,
    files: Vec<JournalFile>,
    #[serde(default)]
    context: Value,
    undo_available: bool,
}

pub struct ConfigMutationGuard(File);

impl ConfigMutationGuard {
    pub fn acquire() -> Result<Self, AppError> {
        let path = crate::runtime::runtime_dir().join("config-apply.lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| AppError::io(&path, e))?;
        file.try_lock_exclusive().map_err(|e| AppError::IoContext {
            context: "Another configuration operation is in progress".into(),
            source: e,
        })?;
        Ok(Self(file))
    }
}

impl Drop for ConfigMutationGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub struct FileCommitTransaction {
    journal: Journal,
}

impl FileCommitTransaction {
    pub fn prepare(
        plan: &PreparedConfigWrite,
        context: Value,
        undo_available: bool,
    ) -> Result<Self, AppError> {
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        let mut total = 0usize;
        for file in &plan.files {
            let path = normalize_target(&file.path)?;
            if !seen.insert(path.clone()) {
                return Err(AppError::InvalidInput(
                    "configuration plan has a relative or duplicate target".into(),
                ));
            }
            validate_target(&path)?;
            if read_optional(&path)? != file.before {
                return Err(conflict(&path));
            }
            total +=
                file.before.as_ref().map_or(0, Vec::len) + file.after.as_ref().map_or(0, Vec::len);
            if total > 64 * 1024 * 1024 {
                return Err(AppError::InvalidInput(
                    "configuration plan exceeds 64 MiB".into(),
                ));
            }
            let parent = path.parent().ok_or_else(|| {
                AppError::InvalidInput("configuration target has no parent".into())
            })?;
            let mut created_directories = Vec::new();
            let mut cursor = parent;
            while !cursor.exists() {
                created_directories.push(cursor.to_path_buf());
                let Some(next) = cursor.parent() else {
                    break;
                };
                cursor = next;
            }
            files.push(JournalFile {
                path: path.clone(),
                before: file.before.clone(),
                after: file.after.clone(),
                before_mode: mode(&path),
                file_mode: file.file_mode,
                dir_mode: file.dir_mode,
                parent_before_mode: mode(parent),
                created_directories,
            });
        }
        let transaction = Self {
            journal: Journal {
                schema: 1,
                id: uuid::Uuid::new_v4().to_string(),
                created_at: chrono::Utc::now().timestamp_millis(),
                state: ConfigApplyState::Prepared,
                files,
                context,
                undo_available,
            },
        };
        transaction.persist()?;
        Ok(transaction)
    }

    pub fn load(id: &str) -> Result<Self, AppError> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(AppError::InvalidInput("receipt id must be a UUID".into()));
        }
        let path = journal_dir().join(format!("{id}.json"));
        let metadata = fs::symlink_metadata(&path).map_err(|e| AppError::io(&path, e))?;
        if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
            return Err(AppError::InvalidInput(
                "unsafe configuration receipt".into(),
            ));
        }
        let bytes = fs::read(&path).map_err(|e| AppError::io(&path, e))?;
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|e| AppError::json(&path, e))?;
        if journal.schema != 1 || journal.id != id {
            return Err(AppError::InvalidInput(
                "unsupported configuration receipt".into(),
            ));
        }
        Ok(Self { journal })
    }

    pub fn id(&self) -> &str {
        &self.journal.id
    }
    pub fn context(&self) -> &Value {
        &self.journal.context
    }
    pub fn state(&self) -> ConfigApplyState {
        self.journal.state
    }
    pub fn journal_undo_capable(&self) -> bool {
        self.journal.undo_available
    }
    pub fn receipt(&self) -> ConfigApplyReceipt {
        ConfigApplyReceipt {
            id: self.journal.id.clone(),
            app: self
                .journal
                .context
                .get("app")
                .and_then(Value::as_str)
                .map(str::to_owned),
            provider_id: self
                .journal
                .context
                .get("providerId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            created_at: self.journal.created_at,
            state: self.journal.state,
            undo_available: self.journal.undo_available
                && self.journal.state == ConfigApplyState::Completed,
            files: self
                .journal
                .files
                .iter()
                .map(|f| ConfigFileChange {
                    path: f.path.display().to_string(),
                    before_hash: f.before.as_deref().map(bytes_hash),
                    after_hash: f.after.as_deref().map(bytes_hash),
                })
                .collect(),
        }
    }

    pub fn apply(&mut self) -> Result<(), AppError> {
        for file in &self.journal.files {
            let result = validate_target(&file.path).and_then(|()| {
                if read_optional(&file.path)? != file.before {
                    Err(conflict(&file.path))
                } else {
                    Ok(())
                }
            });
            if let Err(error) = result {
                self.journal.state = ConfigApplyState::RolledBack;
                self.persist()?;
                return Err(error);
            }
        }
        for index in 0..self.journal.files.len() {
            let file = &self.journal.files[index];
            let result = (|| {
                validate_target(&file.path)?;
                if read_optional(&file.path)? != file.before {
                    return Err(conflict(&file.path));
                }
                write_contents(
                    &file.path,
                    file.after.as_deref(),
                    file.file_mode.or(file.before_mode),
                    file.dir_mode,
                )
            })();
            if let Err(error) = result {
                return match self.rollback() {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(AppError::Message(format!(
                        "{error}; recovery required for receipt {}: {rollback}",
                        self.id()
                    ))),
                };
            }
        }
        self.journal.state = ConfigApplyState::FilesWritten;
        if let Err(error) = self.persist() {
            let rollback = self.rollback();
            return Err(AppError::Message(format!(
                "failed to record file commit: {error}; rollback: {}",
                rollback
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "completed".into())
            )));
        }
        Ok(())
    }

    pub fn complete(&mut self) -> Result<(), AppError> {
        let previous = self.journal.state;
        self.journal.state = ConfigApplyState::Completed;
        if let Err(error) = self.persist() {
            self.journal.state = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn verify_after(&self) -> Result<(), AppError> {
        for file in &self.journal.files {
            validate_target(&file.path)?;
            if read_optional(&file.path)? != file.after {
                return Err(conflict(&file.path));
            }
        }
        Ok(())
    }

    pub fn reverse_plan(&self) -> Result<PreparedConfigWrite, AppError> {
        self.verify_after()?;
        Ok(PreparedConfigWrite {
            files: self
                .journal
                .files
                .iter()
                .map(|f| PreparedFileWrite {
                    path: f.path.clone(),
                    before: f.after.clone(),
                    after: f.before.clone(),
                    file_mode: f.before_mode,
                    dir_mode: f.parent_before_mode,
                })
                .collect(),
        })
    }

    /// Restore only bytes still equal to this operation's before/after image.
    /// An unrelated external write is never silently overwritten during recovery.
    pub fn rollback(&mut self) -> Result<(), AppError> {
        let mut errors = Vec::new();
        for file in self.journal.files.iter().rev() {
            let result = (|| {
                validate_target(&file.path)?;
                let now = read_optional(&file.path)?;
                if now != file.before && now != file.after {
                    return Err(conflict(&file.path));
                }
                if now != file.before {
                    write_contents(&file.path, file.before.as_deref(), file.before_mode, None)?;
                }
                if let Some(parent) = file.path.parent() {
                    set_mode(parent, file.parent_before_mode)?;
                }
                for dir in &file.created_directories {
                    match fs::remove_dir(dir) {
                        Ok(()) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::DirectoryNotEmpty
                            ) => {}
                        Err(e) => return Err(AppError::io(dir, e)),
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        self.journal.state = if errors.is_empty() {
            ConfigApplyState::RolledBack
        } else {
            ConfigApplyState::RecoveryRequired
        };
        if let Err(error) = self.persist() {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AppError::Message(errors.join("; ")))
        }
    }

    pub fn mark_rolled_back(&mut self) -> Result<(), AppError> {
        self.journal.state = ConfigApplyState::RolledBack;
        self.persist()
    }
    pub fn mark_recovery_required(&mut self) -> Result<(), AppError> {
        self.journal.state = ConfigApplyState::RecoveryRequired;
        self.persist()
    }

    fn persist(&self) -> Result<(), AppError> {
        let dir = journal_dir();
        fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
        set_mode(&dir, Some(0o700))?;
        let path = dir.join(format!("{}.json", self.id()));
        let bytes = serde_json::to_vec(&self.journal)
            .map_err(|source| AppError::JsonSerialize { source })?;
        crate::paths::atomic_write_with_mode(&path, &bytes, Some(0o600))
    }
}

pub fn execute_files(plan: &PreparedConfigWrite) -> Result<(), AppError> {
    let _guard = ConfigMutationGuard::acquire()?;
    if !pending_receipts()?.is_empty() {
        return Err(AppError::Message(
            "An interrupted configuration write requires recovery".into(),
        ));
    }
    let mut transaction = FileCommitTransaction::prepare(plan, Value::Null, false)?;
    transaction.apply()?;
    transaction.complete()
}

pub fn pending_receipts() -> Result<Vec<String>, AppError> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut pending = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| AppError::io(&dir, e))? {
        let path = entry.map_err(|e| AppError::io(&dir, e))?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let transaction = FileCommitTransaction::load(id)?;
        if matches!(
            transaction.state(),
            ConfigApplyState::Prepared
                | ConfigApplyState::FilesWritten
                | ConfigApplyState::RecoveryRequired
        ) {
            pending.push(id.to_owned());
        }
    }
    Ok(pending)
}

/// A lost response may be retried after the applied draft was removed. Reuse
/// the recorded result instead of creating another connection or file write.
pub(crate) fn completed_draft_transaction(
    app: &str,
    draft_id: &str,
    revision: u64,
) -> Result<Option<FileCommitTransaction>, AppError> {
    let dir = journal_dir();
    if !dir.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(&dir).map_err(|e| AppError::io(&dir, e))? {
        let path = entry.map_err(|e| AppError::io(&dir, e))?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let transaction = FileCommitTransaction::load(id)?;
        let context = transaction.context();
        if transaction.state() == ConfigApplyState::Completed
            && context.get("app").and_then(Value::as_str) == Some(app)
            && context.pointer("/draft/id").and_then(Value::as_str) == Some(draft_id)
            && context.pointer("/draft/revision").and_then(Value::as_u64) == Some(revision)
        {
            return Ok(Some(transaction));
        }
    }
    Ok(None)
}

fn journal_dir() -> PathBuf {
    crate::runtime::runtime_dir().join("config-commits")
}

fn normalize_target(path: &Path) -> Result<PathBuf, AppError> {
    use std::path::Component;
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(AppError::InvalidInput(
            "configuration targets must be absolute without parent traversal".into(),
        ));
    }
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(AppError::InvalidInput(format!(
            "configuration target is a symlink: {}",
            path.display()
        )));
    }
    let mut ancestor = path
        .parent()
        .ok_or_else(|| AppError::InvalidInput("configuration target has no parent".into()))?;
    let mut suffix = vec![
        path.file_name()
            .ok_or_else(|| AppError::InvalidInput("configuration target has no filename".into()))?
            .to_os_string(),
    ];
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .ok_or_else(|| AppError::InvalidInput("invalid configuration parent".into()))?
                .to_os_string(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| AppError::InvalidInput("invalid configuration parent".into()))?;
    }
    let mut normalized = fs::canonicalize(ancestor).map_err(|e| AppError::io(ancestor, e))?;
    for component in suffix.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

fn validate_target(path: &Path) -> Result<(), AppError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::InvalidInput(format!(
                    "configuration path was redirected by a symlink: {}",
                    ancestor.display()
                )));
            }
            Ok(metadata) if ancestor == path && !metadata.is_file() => {
                return Err(AppError::InvalidInput(format!(
                    "configuration target is not a regular file: {}",
                    path.display()
                )));
            }
            Ok(metadata) if ancestor != path && !metadata.is_dir() => {
                return Err(AppError::InvalidInput(format!(
                    "configuration parent is not a directory: {}",
                    ancestor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::io(ancestor, error)),
        }
    }
    Ok(())
}
pub fn bytes_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io(path, e)),
    }
}
fn conflict(path: &Path) -> AppError {
    AppError::InvalidInput(format!(
        "configuration changed since it was inspected: {}",
        path.display()
    ))
}
fn mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = mode {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|e| AppError::io(path, e))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
fn write_contents(
    path: &Path,
    bytes: Option<&[u8]>,
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> Result<(), AppError> {
    if let Some(bytes) = bytes {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
            set_mode(parent, dir_mode)?;
        }
        crate::paths::atomic_write_with_mode(path, bytes, file_mode)
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::io(path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_file_group_can_restore_original_bytes() {
        let home = crate::test_support::TestHome::new();
        let first = home.path().join("first.json");
        let second = home.path().join("second.json");
        fs::write(&first, b"{ invalid but valuable").unwrap();
        let plan = PreparedConfigWrite {
            files: vec![
                PreparedFileWrite::replace(first.clone(), b"next".to_vec()).unwrap(),
                PreparedFileWrite::replace(second.clone(), b"new".to_vec()).unwrap(),
            ],
        };
        let mut transaction = FileCommitTransaction::prepare(&plan, Value::Null, true).unwrap();
        let id = transaction.id().to_string();
        transaction.apply().unwrap();
        drop(transaction);
        assert_eq!(pending_receipts().unwrap(), vec![id.clone()]);
        let mut reopened = FileCommitTransaction::load(&id).unwrap();
        reopened.rollback().unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"{ invalid but valuable");
        assert!(!second.exists());
        assert!(pending_receipts().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn second_file_failure_rolls_back_first_file() {
        use std::os::unix::fs::PermissionsExt;
        let home = crate::test_support::TestHome::new();
        let first = home.path().join("first");
        let locked = home.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let second = locked.join("second");
        fs::write(&first, b"original first").unwrap();
        fs::write(&second, b"original second").unwrap();
        let plan = PreparedConfigWrite {
            files: vec![
                PreparedFileWrite::replace(first.clone(), b"new first".to_vec()).unwrap(),
                PreparedFileWrite::replace(second.clone(), b"new second".to_vec()).unwrap(),
            ],
        };
        let mut transaction = FileCommitTransaction::prepare(&plan, Value::Null, true).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let result = transaction.apply();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(first).unwrap(), b"original first");
        assert_eq!(fs::read(second).unwrap(), b"original second");
        assert_eq!(transaction.state(), ConfigApplyState::RolledBack);
    }
    #[cfg(unix)]
    #[test]
    fn parent_symlink_swap_after_preparation_is_rejected() {
        use std::os::unix::fs::symlink;
        let home = crate::test_support::TestHome::new();
        let root = home.path().join("root");
        let outside = home.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let plan = PreparedConfigWrite {
            files: vec![
                PreparedFileWrite::replace(root.join("new.json"), b"must not escape".to_vec())
                    .unwrap(),
            ],
        };
        let mut transaction = FileCommitTransaction::prepare(&plan, Value::Null, true).unwrap();
        fs::rename(&root, home.path().join("old-root")).unwrap();
        symlink(&outside, &root).unwrap();
        assert!(transaction.apply().is_err());
        assert!(!outside.join("new.json").exists());
        assert_eq!(transaction.state(), ConfigApplyState::RolledBack);
    }
    #[test]
    fn recovery_does_not_overwrite_an_unrelated_external_write() {
        let home = crate::test_support::TestHome::new();
        let path = home.path().join("file");
        fs::write(&path, b"old").unwrap();
        let plan = PreparedConfigWrite {
            files: vec![PreparedFileWrite::replace(path.clone(), b"ours".to_vec()).unwrap()],
        };
        let mut transaction = FileCommitTransaction::prepare(&plan, Value::Null, true).unwrap();
        transaction.apply().unwrap();
        fs::write(&path, b"someone else").unwrap();
        assert!(transaction.rollback().is_err());
        assert_eq!(transaction.state(), ConfigApplyState::RecoveryRequired);
        assert_eq!(fs::read(path).unwrap(), b"someone else");
    }
    #[test]
    fn receipt_persist_failure_does_not_advertise_undo() {
        let home = crate::test_support::TestHome::new();
        let path = home.path().join("file");
        let plan = PreparedConfigWrite {
            files: vec![PreparedFileWrite::replace(path, b"new".to_vec()).unwrap()],
        };
        let mut transaction = FileCommitTransaction::prepare(&plan, Value::Null, true).unwrap();
        transaction.apply().unwrap();
        let record = journal_dir().join(format!("{}.json", transaction.id()));
        fs::remove_file(&record).unwrap();
        fs::create_dir(&record).unwrap();
        assert!(transaction.complete().is_err());
        assert_eq!(transaction.state(), ConfigApplyState::FilesWritten);
        assert!(!transaction.receipt().undo_available);
    }
}
