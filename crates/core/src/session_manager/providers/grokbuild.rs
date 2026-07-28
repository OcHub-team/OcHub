use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};

use super::utils::{TITLE_MAX_CHARS, extract_text, parse_timestamp_to_ms, truncate_summary};

#[derive(Debug, Deserialize)]
struct GrokSessionInfo {
    id: String,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrokSessionSummary {
    info: GrokSessionInfo,
    #[serde(default)]
    session_summary: Option<String>,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    created_at: Option<Value>,
    #[serde(default)]
    updated_at: Option<Value>,
    #[serde(default)]
    last_active_at: Option<Value>,
}

pub fn session_roots() -> Vec<PathBuf> {
    let config_dir = crate::apps::grokbuild::get_grok_config_dir();
    vec![
        config_dir.join("sessions"),
        config_dir.join("archived_sessions"),
    ]
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let mut summaries = Vec::new();
    for root in session_roots() {
        collect_summary_files(&root, &mut summaries);
    }
    summaries
        .into_iter()
        .filter_map(|path| parse_summary(&path))
        .collect()
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let session_dir = path
        .parent()
        .ok_or_else(|| format!("Invalid Grok Build session path: {}", path.display()))?;
    let chat_path = session_dir.join("chat_history.jsonl");
    let file = File::open(&chat_path)
        .map_err(|error| format!("Failed to open Grok Build chat history: {error}"))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let role = match kind {
            "system" | "user" | "assistant" | "tool" => kind,
            _ => continue,
        };
        let content = value.get("content").map(extract_text).unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let ts = value
            .get("timestamp")
            .or_else(|| value.get("ts"))
            .and_then(parse_timestamp_to_ms);
        messages.push(SessionMessage {
            role: role.to_string(),
            content,
            ts,
        });
    }
    Ok(messages)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    if !path.starts_with(root) {
        return Err(format!(
            "Grok Build session source is outside the session root: {}",
            path.display()
        ));
    }
    if path.file_name().and_then(|name| name.to_str()) != Some("summary.json") {
        return Err(format!(
            "Unexpected Grok Build session source: {}",
            path.display()
        ));
    }
    let summary = read_summary(path)?;
    if summary.info.id != session_id {
        return Err(format!(
            "Grok Build session ID mismatch: expected {session_id}, found {}",
            summary.info.id
        ));
    }
    let session_dir = path
        .parent()
        .ok_or_else(|| format!("Invalid Grok Build session path: {}", path.display()))?;
    if session_dir == root || !session_dir.starts_with(root) {
        return Err(format!(
            "Refusing to delete Grok Build session directory outside its root: {}",
            session_dir.display()
        ));
    }
    if session_dir.file_name().and_then(|name| name.to_str()) != Some(session_id) {
        return Err(format!(
            "Grok Build session directory does not match session ID: {}",
            session_dir.display()
        ));
    }
    std::fs::remove_dir_all(session_dir).map_err(|error| {
        format!(
            "Failed to delete Grok Build session directory {}: {error}",
            session_dir.display()
        )
    })?;
    Ok(true)
}

fn collect_summary_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_summary_files(&path, files);
        } else if path.file_name().and_then(|name| name.to_str()) == Some("summary.json") {
            files.push(path);
        }
    }
}

fn read_summary(path: &Path) -> Result<GrokSessionSummary, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("Failed to read Grok Build session summary: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("Failed to parse Grok Build session summary: {error}"))
}

fn parse_summary(path: &Path) -> Option<SessionMeta> {
    let summary = read_summary(path).ok()?;
    let session_id = summary.info.id;
    let title = summary
        .generated_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            summary
                .session_summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| truncate_summary(value, TITLE_MAX_CHARS));
    let session_summary = summary
        .session_summary
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| truncate_summary(value, 160));
    let created_at = summary.created_at.as_ref().and_then(parse_timestamp_to_ms);
    let last_active_at = summary
        .last_active_at
        .as_ref()
        .or(summary.updated_at.as_ref())
        .and_then(parse_timestamp_to_ms);

    Some(SessionMeta {
        provider_id: "grokbuild".into(),
        session_id: session_id.clone(),
        title,
        summary: session_summary,
        project_dir: summary.info.cwd,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command: Some(format!("grok --resume {session_id}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_native_chat_history() {
        let temp = tempdir().unwrap();
        let summary_path = temp.path().join("summary.json");
        std::fs::write(&summary_path, "{}").unwrap();
        std::fs::write(
            temp.path().join("chat_history.jsonl"),
            concat!(
                "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}\n",
                "{\"type\":\"reasoning\",\"content\":\"private\"}\n",
                "{\"type\":\"assistant\",\"content\":\"Hi\"}\n"
            ),
        )
        .unwrap();
        let messages = load_messages(&summary_path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
    }
}
