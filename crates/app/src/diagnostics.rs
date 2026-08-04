//! Persistent, privacy-aware application diagnostics and support-bundle export.
//!
//! Runtime logs stay local under `<data_dir>/logs`. The user can explicitly
//! export a redacted ZIP from the Tools page; no diagnostics are uploaded.

use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::Serialize;
use zip::DateTime;
use zip::write::SimpleFileOptions;

const LOG_DIR_NAME: &str = "logs";
const CURRENT_LOG_NAME: &str = "ochub.log";
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
const ROTATED_LOG_COUNT: usize = 4;
const MAX_EXPORTED_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const EXPORT_SCHEMA_VERSION: u32 = 1;

static LOGGER: OnceLock<DiagnosticLogger> = OnceLock::new();
static REDACTION_RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();

struct DiagnosticLogger {
    level: log::LevelFilter,
    session_id: String,
    state: Mutex<Option<LogFileState>>,
}

struct LogFileState {
    directory: PathBuf,
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedLogEntry<'a> {
    timestamp: &'a str,
    level: &'a str,
    target: &'a str,
    pid: u32,
    session_id: &'a str,
    thread: &'a str,
    thread_id: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportManifest {
    schema_version: u32,
    generated_at: String,
    app_version: &'static str,
    os: &'static str,
    architecture: &'static str,
    session_id: String,
    included_files: Vec<String>,
    exclusions: Vec<&'static str>,
}

impl LogFileState {
    fn open(data_dir: &Path) -> std::io::Result<Self> {
        let directory = data_dir.join(LOG_DIR_NAME);
        fs::create_dir_all(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::other(
                "diagnostic log directory is not a regular directory",
            ));
        }
        secure_directory(&directory);

        let path = directory.join(CURRENT_LOG_NAME);
        let bytes_written = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let mut state = Self {
            directory,
            path,
            file: None,
            bytes_written,
        };
        if state.bytes_written >= MAX_LOG_BYTES {
            state.rotate()?;
        } else {
            state.file = Some(open_private_append(&state.path)?);
        }
        Ok(state)
    }

    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.bytes_written > 0
            && self.bytes_written.saturating_add(line.len() as u64) > MAX_LOG_BYTES
        {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("diagnostic log is not open"))?;
        file.write_all(line)?;
        self.bytes_written = self.bytes_written.saturating_add(line.len() as u64);
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.flush();
        self.file.take();

        let oldest = rotated_log_path(&self.directory, ROTATED_LOG_COUNT);
        if oldest.exists() {
            let _ = fs::remove_file(&oldest);
        }
        for index in (1..ROTATED_LOG_COUNT).rev() {
            let source = rotated_log_path(&self.directory, index);
            if source.exists() {
                let destination = rotated_log_path(&self.directory, index + 1);
                let _ = fs::rename(source, destination);
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, rotated_log_path(&self.directory, 1))?;
        }
        self.file = Some(open_private_append(&self.path)?);
        self.bytes_written = 0;
        Ok(())
    }
}

impl log::Log for DiagnosticLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let current_thread = std::thread::current();
        let thread = current_thread.name().unwrap_or("unnamed");
        let thread_id = format!("{:?}", current_thread.id());
        let raw_message = record.args().to_string();
        let mut message = redact_text(&raw_message);
        if message.chars().count() > 16_384 {
            message = format!(
                "{}…[truncated]",
                message.chars().take(16_384).collect::<String>()
            );
        }
        message = message.replace(['\r', '\n'], "\\n");

        eprintln!("[{}] {}: {}", record.level(), record.target(), message);

        let entry = PersistedLogEntry {
            timestamp: &timestamp,
            level: record.level().as_str(),
            target: record.target(),
            pid: std::process::id(),
            session_id: &self.session_id,
            thread,
            thread_id: &thread_id,
            message: &message,
        };
        let Ok(mut line) = serde_json::to_vec(&entry) else {
            return;
        };
        line.push(b'\n');

        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state.as_mut()
            && let Err(error) = state.write(&line)
        {
            eprintln!("[WARN] ochub::diagnostics: failed to persist log: {error}");
        }
    }

    fn flush(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state.as_mut() {
            state.flush();
        }
    }
}

/// Install the process-wide logger. It always keeps stderr output available;
/// failure to create the private log directory only disables file persistence.
pub fn install_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| value.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);
    let data_dir = ochub_core::paths::get_app_config_dir();
    harden_existing_private_file(&data_dir.join("crash.log"));
    let (state, persistence_error) = match LogFileState::open(&data_dir) {
        Ok(state) => (Some(state), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let logger = DiagnosticLogger {
        level,
        session_id: uuid::Uuid::new_v4().to_string(),
        state: Mutex::new(state),
    };
    if LOGGER.set(logger).is_err() {
        eprintln!("[WARN] ochub::diagnostics: logger was already initialized");
        return;
    }
    let Some(logger) = LOGGER.get() else {
        return;
    };
    if log::set_logger(logger).is_err() {
        eprintln!("[WARN] ochub::diagnostics: failed to install application logger");
        return;
    }
    log::set_max_level(level);
    log::info!(
        target: "ochub::diagnostics",
        "diagnostic session started; session_id={}",
        logger.session_id
    );
    if let Some(error) = persistence_error {
        log::warn!(target: "ochub::diagnostics", "file logging is unavailable: {error}");
    }
}

pub fn current_session_id() -> String {
    LOGGER
        .get()
        .map(|logger| logger.session_id.clone())
        .unwrap_or_else(|| "uninitialized".to_string())
}

/// Export a local, redacted diagnostics ZIP. Configuration, databases, request
/// bodies, and credentials are deliberately outside the source set.
pub fn export_bundle(target: &Path) -> Result<(), String> {
    log::logger().flush();
    export_bundle_from(
        target,
        &ochub_core::paths::get_app_config_dir(),
        current_session_id(),
    )
}

fn export_bundle_from(target: &Path, data_dir: &Path, session_id: String) -> Result<(), String> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "diagnostics export path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create export directory: {error}"))?;

    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(DateTime::default());
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let mut included_files = Vec::new();

    let log_dir = data_dir.join(LOG_DIR_NAME);
    for (source, archive_name) in diagnostic_log_sources(&log_dir) {
        add_redacted_file(
            &mut writer,
            options,
            &source,
            &archive_name,
            &mut included_files,
        )?;
    }
    add_redacted_file(
        &mut writer,
        options,
        &data_dir.join("crash.log"),
        "diagnostics/crash.log",
        &mut included_files,
    )?;

    let readme = "OcHub diagnostics bundle\n\n\
        This archive was created only after the user chose an export location.\n\
        Log and crash-report text is redacted on a best-effort basis.\n\
        It does not contain the OcHub database, provider configuration, API keys,\n\
        request bodies, response bodies, or synchronization credentials.\n";
    add_zip_text(&mut writer, options, "README.txt", readme.as_bytes())?;
    included_files.push("README.txt".to_string());

    let manifest = ExportManifest {
        schema_version: EXPORT_SCHEMA_VERSION,
        generated_at: chrono::Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        session_id,
        included_files: included_files.clone(),
        exclusions: vec![
            "ochub database",
            "provider and application configuration",
            "credentials and synchronization secrets",
            "request and response bodies",
        ],
    };
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("serialize diagnostics manifest: {error}"))?;
    add_zip_text(&mut writer, options, "diagnostics/manifest.json", &manifest)?;

    let cursor = writer
        .finish()
        .map_err(|error| format!("finish diagnostics ZIP: {error}"))?;
    prepare_private_target(target).map_err(|error| format!("prepare export target: {error}"))?;
    ochub_core::paths::atomic_write(target, &cursor.into_inner())
        .map_err(|error| error.to_string())?;
    harden_existing_private_file(target);
    Ok(())
}

fn diagnostic_log_sources(log_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::with_capacity(ROTATED_LOG_COUNT + 1);
    for index in (1..=ROTATED_LOG_COUNT).rev() {
        sources.push((
            rotated_log_path(log_dir, index),
            format!("diagnostics/logs/{CURRENT_LOG_NAME}.{index}"),
        ));
    }
    sources.push((
        log_dir.join(CURRENT_LOG_NAME),
        format!("diagnostics/logs/{CURRENT_LOG_NAME}"),
    ));
    sources
}

fn add_redacted_file(
    writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>,
    options: SimpleFileOptions,
    source: &Path,
    archive_name: &str,
    included_files: &mut Vec<String>,
) -> Result<(), String> {
    let Ok(metadata) = fs::symlink_metadata(source) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(());
    }
    let raw = read_tail(source, MAX_EXPORTED_ENTRY_BYTES)
        .map_err(|error| format!("read {}: {error}", source.display()))?;
    let text = String::from_utf8_lossy(&raw);
    let redacted = redact_text(&text);
    add_zip_text(writer, options, archive_name, redacted.as_bytes())?;
    included_files.push(archive_name.to_string());
    Ok(())
}

fn add_zip_text(
    writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>,
    options: SimpleFileOptions,
    archive_name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    writer
        .start_file(archive_name, options)
        .map_err(|error| format!("start ZIP entry {archive_name}: {error}"))?;
    writer
        .write_all(bytes)
        .map_err(|error| format!("write ZIP entry {archive_name}: {error}"))
}

fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut bytes = Vec::with_capacity(len.min(max_bytes) as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn rotated_log_path(directory: &Path, index: usize) -> PathBuf {
    directory.join(format!("{CURRENT_LOG_NAME}.{index}"))
}

fn redaction_rules() -> &'static [(Regex, &'static str)] {
    REDACTION_RULES.get_or_init(|| {
        vec![
            (
                Regex::new(r"(?i)(https?://)[^/\s:@]+:[^@\s/]+@").expect("valid URL rule"),
                "$1[REDACTED]@",
            ),
            (
                Regex::new(
                    r#"(?i)(authorization\s*[:=]\s*(?:bearer|basic)?\s*|bearer\s+)[^\s,;\"']+"#,
                )
                .expect("valid authorization rule"),
                "$1[REDACTED]",
            ),
            (
                Regex::new(
                    r#"(?i)([\"']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|client[_-]?secret|password|passwd|secret|cookie|user[_-]?code)[\"']?\s*[:=]\s*[\"'])[^\"']*([\"'])"#,
                )
                .expect("valid quoted secret rule"),
                "$1[REDACTED]$2",
            ),
            (
                Regex::new(
                    r#"(?i)([\"']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|client[_-]?secret|password|passwd|secret|cookie|user[_-]?code)[\"']?\s*[:=]\s*)[^\s,;}&\"']+"#,
                )
                .expect("valid unquoted secret rule"),
                "$1[REDACTED]",
            ),
            (
                Regex::new(r#"([?&][A-Za-z0-9_.~-]+)=([^&#\s\"']+)"#)
                    .expect("valid query rule"),
                "$1=[REDACTED]",
            ),
        ]
    })
}

pub(crate) fn redact_text(input: &str) -> String {
    let mut redacted = input.to_string();
    for (rule, replacement) in redaction_rules() {
        redacted = rule.replace_all(&redacted, *replacement).into_owned();
    }
    if let Some(home) = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| home.to_string_lossy().into_owned())
    {
        redacted = redacted.replace(&home, "~");
    }
    redacted
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(std::io::Error::other(
            "refusing to open a symbolic link as a diagnostic log",
        ));
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    harden_existing_private_file(path);
    Ok(file)
}

fn prepare_private_target(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::other(
                "diagnostics export target is not a regular file",
            ));
        }
        Ok(_) => harden_existing_private_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let _ = options.open(path)?;
            harden_existing_private_file(path);
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn secure_directory(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
}

pub(crate) fn harden_existing_private_file(path: &Path) {
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_and_url_queries() {
        let raw = concat!(
            "Authorization: Bearer sk-live-secret ",
            "api_key=abc123 ",
            r#""refresh_token":"refresh-me" "#,
            "https://alice:password@example.com/v1?token=abc&model=gpt"
        );
        let redacted = redact_text(raw);
        for secret in ["sk-live-secret", "abc123", "refresh-me", "password", "gpt"] {
            assert!(!redacted.contains(secret), "secret remained: {secret}");
        }
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn export_contains_only_redacted_diagnostic_sources() {
        let temp = tempfile::tempdir().expect("temporary diagnostics directory");
        let data_dir = temp.path().join("data");
        let log_dir = data_dir.join(LOG_DIR_NAME);
        fs::create_dir_all(&log_dir).expect("create log directory");
        fs::write(
            log_dir.join(CURRENT_LOG_NAME),
            "request failed api_key=do-not-export\n",
        )
        .expect("write log");
        fs::write(
            data_dir.join("crash.log"),
            "panic at https://example.com/fail?token=query-value-123\n",
        )
        .expect("write crash log");
        fs::write(data_dir.join("config.json"), "api_key=raw-config-secret")
            .expect("write excluded config");
        fs::write(data_dir.join("ochub.db"), "database-secret").expect("write excluded database");

        let target = temp.path().join("OcHub-diagnostics.zip");
        export_bundle_from(&target, &data_dir, "test-session".to_string())
            .expect("export diagnostics");

        let file = File::open(&target).expect("open export");
        let mut archive = zip::ZipArchive::new(file).expect("read export");
        let mut names = Vec::new();
        let mut combined = String::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).expect("read entry");
            names.push(entry.name().to_string());
            entry
                .read_to_string(&mut combined)
                .expect("read text entry");
        }
        assert!(names.contains(&"diagnostics/logs/ochub.log".to_string()));
        assert!(names.contains(&"diagnostics/crash.log".to_string()));
        assert!(names.contains(&"diagnostics/manifest.json".to_string()));
        assert!(!names.iter().any(|name| name.contains("config")));
        assert!(!names.iter().any(|name| name.contains("ochub.db")));
        for secret in [
            "do-not-export",
            "query-value-123",
            "raw-config-secret",
            "database-secret",
        ] {
            assert!(!combined.contains(secret), "secret remained: {secret}");
        }
    }
}
