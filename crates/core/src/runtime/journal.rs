use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::{ApplicationError, ApplicationResult, redact_json};

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Planned,
    Prepared,
    Completed,
    Failed,
    RecoveryRequired,
    RolledBack,
}

impl OperationState {
    pub fn blocks_mutations(self) -> bool {
        matches!(self, Self::Prepared | Self::RecoveryRequired)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub schema_version: u32,
    pub id: String,
    pub operation: String,
    pub actor: String,
    pub pid: u32,
    pub state: OperationState,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub input_summary: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_backup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

pub struct OperationHandle {
    record: OperationRecord,
}

impl OperationHandle {
    /// Record a reviewed-but-not-yet-applied remote plan.
    ///
    /// Planned records are intentionally non-blocking: only the later
    /// transition to `Prepared` means a mutation may have started.
    pub fn plan(
        id: impl Into<String>,
        operation: impl Into<String>,
        actor: impl Into<String>,
        input_summary: Value,
    ) -> ApplicationResult<OperationRecord> {
        let id = id.into();
        validate_id(&id)?;
        ensure_operations_dir()?;
        let path = record_path(&id);
        if path.exists() {
            return Err(ApplicationError::InvalidInput(format!(
                "operation {id} already exists"
            )));
        }
        let now = chrono::Utc::now().to_rfc3339();
        let record = OperationRecord {
            schema_version: JOURNAL_SCHEMA_VERSION,
            id,
            operation: operation.into(),
            actor: actor.into(),
            pid: std::process::id(),
            state: OperationState::Planned,
            started_at: now.clone(),
            updated_at: now,
            input_summary: redact_json(&input_summary),
            result_summary: None,
            error: None,
            database_backup: None,
            resolution: None,
        };
        write_record(&record)?;
        Ok(record)
    }

    /// Atomically mark a previously journaled plan as prepared for mutation.
    pub fn prepare(id: &str) -> ApplicationResult<Self> {
        let mut record = inspect_operation(id)?;
        if record.state != OperationState::Planned {
            return Err(ApplicationError::InvalidInput(format!(
                "operation {id} cannot be prepared from state {:?}",
                record.state
            )));
        }
        record.state = OperationState::Prepared;
        record.pid = std::process::id();
        record.updated_at = chrono::Utc::now().to_rfc3339();
        write_record(&record)?;
        Ok(Self { record })
    }

    pub fn begin(
        operation: impl Into<String>,
        actor: impl Into<String>,
        input_summary: Value,
    ) -> ApplicationResult<Self> {
        ensure_operations_dir()?;
        let now = chrono::Utc::now().to_rfc3339();
        let record = OperationRecord {
            schema_version: JOURNAL_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            actor: actor.into(),
            pid: std::process::id(),
            state: OperationState::Prepared,
            started_at: now.clone(),
            updated_at: now,
            input_summary: redact_json(&input_summary),
            result_summary: None,
            error: None,
            database_backup: None,
            resolution: None,
        };
        write_record(&record)?;
        Ok(Self { record })
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn set_database_backup(&mut self, backup: impl Into<String>) -> ApplicationResult<()> {
        self.record.database_backup = Some(backup.into());
        self.touch_and_write()
    }

    pub fn complete(mut self, result: Value) -> ApplicationResult<()> {
        self.record.state = OperationState::Completed;
        self.record.result_summary = Some(redact_json(&result));
        self.record.error = None;
        self.touch_and_write()
    }

    pub fn fail(mut self, error: impl Into<String>) -> ApplicationResult<()> {
        self.record.state = OperationState::Failed;
        self.record.error = Some(redact_error(&error.into()));
        self.touch_and_write()
    }

    fn touch_and_write(&mut self) -> ApplicationResult<()> {
        self.record.updated_at = chrono::Utc::now().to_rfc3339();
        write_record(&self.record)
    }
}

pub fn list_operations() -> ApplicationResult<Vec<OperationRecord>> {
    ensure_operations_dir()?;
    let mut records = Vec::new();
    for entry in fs::read_dir(super::operations_dir())
        .map_err(|error| crate::AppError::io(super::operations_dir(), error))?
    {
        let entry = entry.map_err(|error| {
            ApplicationError::OperationFailed(format!("cannot read operation entry: {error}"))
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        records.push(read_record_path(&path)?);
    }
    records.sort_by(|left, right| right.started_at.cmp(&left.started_at));
    Ok(records)
}

pub fn inspect_operation(id: &str) -> ApplicationResult<OperationRecord> {
    validate_id(id)?;
    let path = record_path(id);
    if !path.exists() {
        return Err(ApplicationError::NotFound {
            kind: "operation",
            id: id.to_string(),
        });
    }
    read_record_path(&path)
}

/// Add redacted audit fields before a planned remote operation is prepared.
pub fn annotate_operation(id: &str, additional: Value) -> ApplicationResult<OperationRecord> {
    let mut record = inspect_operation(id)?;
    if record.state != OperationState::Planned {
        return Err(ApplicationError::InvalidInput(format!(
            "operation {id} cannot be annotated from state {:?}",
            record.state
        )));
    }
    let additional = redact_json(&additional);
    match (&mut record.input_summary, additional) {
        (Value::Object(existing), Value::Object(additional)) => existing.extend(additional),
        (_, additional) => record.input_summary = additional,
    }
    record.updated_at = chrono::Utc::now().to_rfc3339();
    write_record(&record)?;
    Ok(record)
}

pub fn blocking_operations() -> ApplicationResult<Vec<OperationRecord>> {
    Ok(list_operations()?
        .into_iter()
        .filter(|record| record.state.blocks_mutations())
        .collect())
}

/// Explicitly accept the current state after inspecting an interrupted
/// operation. This is deliberately recorded as a recovery resolution rather
/// than deleting history.
pub fn recover_operation(id: &str) -> ApplicationResult<OperationRecord> {
    let mut record = inspect_operation(id)?;
    if !record.state.blocks_mutations() {
        return Err(ApplicationError::InvalidInput(format!(
            "operation {id} does not require recovery"
        )));
    }
    record.state = OperationState::Completed;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    record.resolution = Some("accepted-current-state".to_string());
    record.error = None;
    write_record(&record)?;
    Ok(record)
}

pub fn rollback_operation(
    id: &str,
    database: &crate::Database,
) -> ApplicationResult<OperationRecord> {
    let mut record = inspect_operation(id)?;
    if !record.state.blocks_mutations() && record.state != OperationState::Failed {
        return Err(ApplicationError::InvalidInput(format!(
            "operation {id} cannot be rolled back from state {:?}",
            record.state
        )));
    }
    let backup =
        record
            .database_backup
            .clone()
            .ok_or_else(|| ApplicationError::CapabilityUnsupported {
                app: "operation".to_string(),
                capability: "rollback-without-recorded-backup",
            })?;
    let safety_backup = database.restore_from_backup(&backup)?;
    record.state = OperationState::RolledBack;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    record.resolution = Some(format!("database-restored; safety-backup={safety_backup}"));
    record.error = None;
    write_record(&record)?;
    Ok(record)
}

fn ensure_operations_dir() -> ApplicationResult<PathBuf> {
    super::ensure_runtime_dir()?;
    let path = super::operations_dir();
    fs::create_dir_all(&path).map_err(|error| crate::AppError::io(&path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|error| crate::AppError::io(&path, error))?;
    }
    Ok(path)
}

fn record_path(id: &str) -> PathBuf {
    super::operations_dir().join(format!("{id}.json"))
}

fn validate_id(id: &str) -> ApplicationResult<()> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(ApplicationError::InvalidInput(
            "operation id must be a UUID".to_string(),
        ));
    }
    Ok(())
}

fn read_record_path(path: &Path) -> ApplicationResult<OperationRecord> {
    let metadata = fs::metadata(path).map_err(|error| crate::AppError::io(path, error))?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err(ApplicationError::InvalidInput(format!(
            "unsafe operation journal record: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| crate::AppError::io(path, error))?;
    let record = serde_json::from_slice::<OperationRecord>(&bytes)
        .map_err(|source| crate::AppError::json(path, source))?;
    if record.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(ApplicationError::ProtocolIncompatible(format!(
            "operation journal schema {}, supported {}",
            record.schema_version, JOURNAL_SCHEMA_VERSION
        )));
    }
    Ok(record)
}

fn write_record(record: &OperationRecord) -> ApplicationResult<()> {
    ensure_operations_dir()?;
    let path = record_path(&record.id);
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|source| crate::AppError::JsonSerialize { source })?;
    crate::paths::atomic_write(&path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| crate::AppError::io(&path, error))?;
    }
    Ok(())
}

fn redact_error(message: &str) -> String {
    if message.chars().count() <= 4_096 {
        message.to_string()
    } else {
        format!("{}…", message.chars().take(4_096).collect::<String>())
    }
}
