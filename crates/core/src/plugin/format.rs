//! Shared file-format (de)serialization for the manifest engine.
//!
//! Env files use stable, sorted `KEY=VALUE` output and JSON uses
//! `serde_json::to_string_pretty`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::provider_config::Language;

use super::manifest::FileFormat;

/// The preview [`Language`] for a file format.
pub fn language(format: FileFormat) -> Language {
    match format {
        FileFormat::Env => Language::Env,
        FileFormat::Json => Language::Json,
        FileFormat::Toml => Language::Toml,
        FileFormat::Yaml => Language::Yaml,
    }
}

/// Serialize a store subtree to the file's on-disk text.
pub fn serialize(format: FileFormat, store: &Value) -> Result<String, String> {
    match format {
        FileFormat::Env => Ok(serialize_env(store)),
        FileFormat::Json => serde_json::to_string_pretty(store).map_err(|e| e.to_string()),
        FileFormat::Toml => {
            toml::to_string_pretty(store).map_err(|e| format!("TOML 序列化失败: {e}"))
        }
        FileFormat::Yaml => {
            serde_norway::to_string(store).map_err(|e| format!("YAML 序列化失败: {e}"))
        }
    }
}

/// Parse a file's on-disk text back into a store subtree.
pub fn parse(format: FileFormat, content: &str, file_id: &str) -> Result<Value, String> {
    match format {
        FileFormat::Env => {
            let obj: Map<String, Value> = parse_env_file(content)
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            Ok(Value::Object(obj))
        }
        FileFormat::Json => {
            serde_json::from_str(content).map_err(|e| format!("文件 {file_id} JSON 解析失败: {e}"))
        }
        FileFormat::Toml => {
            toml::from_str(content).map_err(|e| format!("文件 {file_id} TOML 解析失败: {e}"))
        }
        FileFormat::Yaml => serde_norway::from_str(content)
            .map_err(|e| format!("文件 {file_id} YAML 解析失败: {e}")),
    }
}

/// Byte-identical to the native Gemini `.env` writer: string values only, keys
/// sorted, `KEY=VALUE` lines joined by `\n`, no trailing newline.
fn serialize_env(store: &Value) -> String {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(obj) = store.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_string());
            }
        }
    }
    map.into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_env_file(content: &str) -> BTreeMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() || !key.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}
