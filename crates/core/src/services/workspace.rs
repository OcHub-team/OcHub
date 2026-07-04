//! OpenClaw workspace and daily-memory file helpers.
//!
//! Ported from cc-switch `commands/workspace.rs` without Tauri. The service
//! keeps the same filename allow-list and daily memory validation so the UI can
//! safely expose editable workspace files.

use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

use crate::apps::openclaw::get_openclaw_dir;
use crate::error::AppError;
use crate::paths::write_text_file;

const ALLOWED_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "IDENTITY.md",
    "TOOLS.md",
    "MEMORY.md",
    "HEARTBEAT.md",
    "BOOTSTRAP.md",
    "BOOT.md",
];

static DAILY_MEMORY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}\.md$").unwrap());

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemoryFileInfo {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemorySearchResult {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub snippet: String,
    pub match_count: usize,
}

pub struct WorkspaceService;

impl WorkspaceService {
    pub fn allowed_workspace_files() -> &'static [&'static str] {
        ALLOWED_FILES
    }

    pub fn workspace_dir() -> std::path::PathBuf {
        get_openclaw_dir().join("workspace")
    }

    pub fn memory_dir() -> std::path::PathBuf {
        Self::workspace_dir().join("memory")
    }

    pub fn directory_for_subdir(subdir: &str) -> std::path::PathBuf {
        match subdir {
            "memory" => Self::memory_dir(),
            _ => Self::workspace_dir(),
        }
    }

    pub fn ensure_directory_for_subdir(subdir: &str) -> Result<std::path::PathBuf, AppError> {
        let dir = Self::directory_for_subdir(subdir);
        std::fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
        Ok(dir)
    }

    pub fn validate_workspace_filename(filename: &str) -> Result<(), AppError> {
        if !ALLOWED_FILES.contains(&filename) {
            return Err(AppError::InvalidInput(format!(
                "Invalid workspace filename: {filename}. Allowed: {}",
                ALLOWED_FILES.join(", ")
            )));
        }
        Ok(())
    }

    pub fn validate_daily_memory_filename(filename: &str) -> Result<(), AppError> {
        if !DAILY_MEMORY_RE.is_match(filename) {
            return Err(AppError::InvalidInput(format!(
                "Invalid daily memory filename: {filename}. Expected: YYYY-MM-DD.md"
            )));
        }
        Ok(())
    }

    pub fn read_workspace_file(filename: &str) -> Result<Option<String>, AppError> {
        Self::validate_workspace_filename(filename)?;
        let path = Self::workspace_dir().join(filename);
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| AppError::IoContext {
                context: format!("Failed to read workspace file {filename}"),
                source: e,
            })
    }

    pub fn write_workspace_file(filename: &str, content: &str) -> Result<(), AppError> {
        Self::validate_workspace_filename(filename)?;
        let workspace_dir = Self::workspace_dir();
        std::fs::create_dir_all(&workspace_dir).map_err(|e| AppError::io(&workspace_dir, e))?;
        let path = workspace_dir.join(filename);
        write_text_file(&path, content).map_err(|e| {
            AppError::Message(format!("Failed to write workspace file {filename}: {e}"))
        })
    }

    pub fn list_daily_memory_files() -> Result<Vec<DailyMemoryFileInfo>, AppError> {
        let memory_dir = Self::memory_dir();
        if !memory_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        let entries = std::fs::read_dir(&memory_dir).map_err(|e| AppError::IoContext {
            context: "Failed to read memory directory".to_string(),
            source: e,
        })?;

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }

            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }

            let date = name.trim_end_matches(".md").to_string();
            let size_bytes = meta.len();
            let modified_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let preview = std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();

            files.push(DailyMemoryFileInfo {
                filename: name,
                date,
                size_bytes,
                modified_at,
                preview,
            });
        }

        files.sort_by(|a, b| b.filename.cmp(&a.filename));
        Ok(files)
    }

    pub fn read_daily_memory_file(filename: &str) -> Result<Option<String>, AppError> {
        Self::validate_daily_memory_filename(filename)?;
        let path = Self::memory_dir().join(filename);
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| AppError::IoContext {
                context: format!("Failed to read daily memory file {filename}"),
                source: e,
            })
    }

    pub fn write_daily_memory_file(filename: &str, content: &str) -> Result<(), AppError> {
        Self::validate_daily_memory_filename(filename)?;
        let memory_dir = Self::memory_dir();
        std::fs::create_dir_all(&memory_dir).map_err(|e| AppError::io(&memory_dir, e))?;
        let path = memory_dir.join(filename);
        write_text_file(&path, content).map_err(|e| {
            AppError::Message(format!("Failed to write daily memory file {filename}: {e}"))
        })
    }

    pub fn delete_daily_memory_file(filename: &str) -> Result<(), AppError> {
        Self::validate_daily_memory_filename(filename)?;
        let path = Self::memory_dir().join(filename);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| AppError::IoContext {
                context: format!("Failed to delete daily memory file {filename}"),
                source: e,
            })?;
        }
        Ok(())
    }

    pub fn search_daily_memory_files(
        query: &str,
    ) -> Result<Vec<DailyMemorySearchResult>, AppError> {
        let memory_dir = Self::memory_dir();
        if !memory_dir.exists() || query.is_empty() {
            return Ok(Vec::new());
        }

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        let entries = std::fs::read_dir(&memory_dir).map_err(|e| AppError::IoContext {
            context: "Failed to read memory directory".to_string(),
            source: e,
        })?;

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(meta) if meta.is_file() => meta,
                _ => continue,
            };

            let date = name.trim_end_matches(".md").to_string();
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let content_lower = content.to_lowercase();
            let content_matches: Vec<usize> = content_lower
                .match_indices(&query_lower)
                .map(|(i, _)| i)
                .collect();
            let date_matches = date.to_lowercase().contains(&query_lower);

            if content_matches.is_empty() && !date_matches {
                continue;
            }

            let snippet = if let Some(&first_pos) = content_matches.first() {
                let start = if first_pos > 50 {
                    floor_char_boundary(&content, first_pos - 50)
                } else {
                    0
                };
                let end = ceil_char_boundary(&content, (first_pos + 70).min(content.len()));
                let mut snippet = String::new();
                if start > 0 {
                    snippet.push_str("...");
                }
                snippet.push_str(&content[start..end]);
                if end < content.len() {
                    snippet.push_str("...");
                }
                snippet
            } else {
                let end = ceil_char_boundary(&content, 120.min(content.len()));
                let mut snippet = content[..end].to_string();
                if end < content.len() {
                    snippet.push_str("...");
                }
                snippet
            };

            let size_bytes = meta.len();
            let modified_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            results.push(DailyMemorySearchResult {
                filename: name,
                date,
                size_bytes,
                modified_at,
                snippet,
                match_count: content_matches.len(),
            });
        }

        results.sort_by(|a, b| b.filename.cmp(&a.filename));
        Ok(results)
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
