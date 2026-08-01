//! Signed update discovery for the single-binary headless node runtime.
//!
//! Desktop application payloads and headless payloads deliberately use
//! separate manifests. A `.app.tar.gz`, AppImage, or NSIS installer cannot
//! replace `ochcli`; the headless manifest instead names one raw executable
//! for every supported target. Both direct downloads on the node and
//! controller-relayed downloads end at [`verify_payload`].

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{DEFAULT_REPO, is_newer_version};
use crate::{AppError, Result};

pub const MANIFEST_NAME: &str = "headless.json";
pub const MAX_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessPlatformEntry {
    pub url: String,
    pub signature: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessUpdateManifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub pub_date: Option<String>,
    #[serde(default = "protocol_min")]
    pub protocol_min: u32,
    #[serde(default = "protocol_max")]
    pub protocol_max: u32,
    pub targets: BTreeMap<String, HeadlessPlatformEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessUpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub target: String,
    pub release_url: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub payload_size: Option<u64>,
    pub signed: bool,
    pub direct_download: bool,
    pub direct_error: Option<String>,
}

const fn protocol_min() -> u32 {
    1
}

const fn protocol_max() -> u32 {
    2
}

pub fn manifest_url(repo: Option<&str>) -> String {
    format!(
        "https://github.com/{}/releases/latest/download/{MANIFEST_NAME}",
        repo.unwrap_or(DEFAULT_REPO)
    )
}

pub fn release_url(repo: Option<&str>, version: &str) -> String {
    format!(
        "https://github.com/{}/releases/tag/v{}",
        repo.unwrap_or(DEFAULT_REPO),
        version.trim().trim_start_matches('v')
    )
}

pub fn target_key(os: &str, arch: &str) -> Option<String> {
    let os = match os.trim().to_ascii_lowercase().as_str() {
        "macos" | "darwin" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "x64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

pub fn current_target_key() -> Option<String> {
    target_key(std::env::consts::OS, std::env::consts::ARCH)
}

impl HeadlessUpdateManifest {
    pub fn entry_for(&self, os: &str, arch: &str) -> Option<(&str, &HeadlessPlatformEntry)> {
        let key = target_key(os, arch)?;
        self.targets
            .get_key_value(&key)
            .map(|(key, entry)| (key.as_str(), entry))
    }

    pub fn entry_for_current_target(&self) -> Option<(&str, &HeadlessPlatformEntry)> {
        self.entry_for(std::env::consts::OS, std::env::consts::ARCH)
    }
}

pub async fn fetch_manifest(repo: Option<&str>) -> Result<HeadlessUpdateManifest> {
    let url = manifest_url(repo);
    let response = crate::http_client::get()
        .get(&url)
        .header("accept", "application/json")
        .header("user-agent", user_agent())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("获取节点更新清单失败: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("读取节点更新清单失败: {error}")))?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_str(&body)
        .map_err(|error| AppError::Message(format!("解析节点更新清单失败: {error}")))
}

pub async fn check(
    repo: Option<&str>,
    current_version: &str,
    os: &str,
    arch: &str,
    probe_direct: bool,
) -> Result<HeadlessUpdateCheck> {
    let manifest = fetch_manifest(repo).await?;
    let target = target_key(os, arch)
        .ok_or_else(|| AppError::Message(format!("不支持的节点平台: {os}-{arch}")))?;
    let entry = manifest.targets.get(&target);
    let (direct_download, direct_error) = if probe_direct {
        match entry {
            Some(entry) => match probe_download(&entry.url).await {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            },
            None => (false, Some(format!("最新版本没有 {target} 节点程序"))),
        }
    } else {
        (false, None)
    };
    Ok(HeadlessUpdateCheck {
        current_version: current_version.to_string(),
        latest_version: manifest.version.clone(),
        has_update: is_newer_version(current_version, &manifest.version),
        target,
        release_url: release_url(repo, &manifest.version),
        notes: manifest.notes,
        published_at: manifest.pub_date,
        payload_size: entry.map(|entry| entry.size),
        signed: entry.is_some_and(|entry| !entry.signature.trim().is_empty()),
        direct_download,
        direct_error,
    })
}

pub async fn probe_download(url: &str) -> Result<()> {
    validate_download_url(url)?;
    let response = crate::http_client::get()
        .get(url)
        .header("range", "bytes=0-0")
        .header("user-agent", user_agent())
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("节点无法连接更新地址: {error}")))?;
    validate_download_url(response.url().as_str())?;
    let status = response.status();
    if status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT {
        Ok(())
    } else {
        Err(AppError::HttpStatus {
            status: status.as_u16(),
            body: String::new(),
        })
    }
}

pub async fn download(entry: &HeadlessPlatformEntry) -> Result<Vec<u8>> {
    validate_entry(entry)?;
    let response = crate::http_client::get()
        .get(&entry.url)
        .header("user-agent", user_agent())
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("下载节点更新失败: {error}")))?;
    validate_download_url(response.url().as_str())?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PAYLOAD_BYTES || length != entry.size)
    {
        return Err(AppError::Message(
            "节点更新包长度与签名清单不一致".to_string(),
        ));
    }
    let mut payload = Vec::with_capacity(entry.size.min(MAX_PAYLOAD_BYTES) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| AppError::Message(format!("下载节点更新中断: {error}")))?;
        if payload.len().saturating_add(chunk.len()) > MAX_PAYLOAD_BYTES as usize {
            return Err(AppError::Message("节点更新包超过大小限制".to_string()));
        }
        payload.extend_from_slice(&chunk);
    }
    verify_payload(&payload, entry)?;
    Ok(payload)
}

pub fn verify_payload(payload: &[u8], entry: &HeadlessPlatformEntry) -> Result<()> {
    validate_entry(entry)?;
    verify_size_and_hash(payload, entry)?;
    super::manifest::verify_payload(payload, &entry.signature)
}

fn verify_size_and_hash(payload: &[u8], entry: &HeadlessPlatformEntry) -> Result<()> {
    if payload.len() as u64 != entry.size {
        return Err(AppError::Message(format!(
            "节点更新包大小不匹配：期望 {}，实际 {}",
            entry.size,
            payload.len()
        )));
    }
    let digest = sha256_hex(payload);
    if !digest.eq_ignore_ascii_case(entry.sha256.trim()) {
        return Err(AppError::Message(
            "节点更新包 SHA-256 与发布清单不一致".to_string(),
        ));
    }
    Ok(())
}

fn validate_entry(entry: &HeadlessPlatformEntry) -> Result<()> {
    validate_download_url(&entry.url)?;
    if entry.size == 0 || entry.size > MAX_PAYLOAD_BYTES {
        return Err(AppError::Message(
            "节点更新清单中的文件大小无效".to_string(),
        ));
    }
    let hash = entry.sha256.trim();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::Message(
            "节点更新清单中的 SHA-256 无效".to_string(),
        ));
    }
    if entry.signature.trim().is_empty() {
        return Err(AppError::Message("节点更新包没有发布签名".to_string()));
    }
    Ok(())
}

fn validate_download_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|error| AppError::Message(format!("节点更新地址无效: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(AppError::Message("节点更新地址必须使用 HTTPS".to_string()));
    }
    let host = parsed.host_str().unwrap_or_default();
    if host != "github.com"
        && host != "objects.githubusercontent.com"
        && !host.ends_with(".githubusercontent.com")
    {
        return Err(AppError::Message(format!(
            "节点更新地址不在允许列表内: {host}"
        )));
    }
    Ok(())
}

fn user_agent() -> String {
    format!("OcHub-Headless/{}", env!("CARGO_PKG_VERSION"))
}

fn sha256_hex(payload: &[u8]) -> String {
    Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_for(payload: &[u8]) -> HeadlessPlatformEntry {
        HeadlessPlatformEntry {
            url: "https://github.com/OcHub-team/OcHub/releases/download/v1.0.0/ochcli".to_string(),
            signature: "placeholder".to_string(),
            sha256: sha256_hex(payload),
            size: payload.len() as u64,
        }
    }

    #[test]
    fn target_aliases_normalize_to_release_keys() {
        assert_eq!(
            target_key("darwin", "arm64").as_deref(),
            Some("macos-aarch64")
        );
        assert_eq!(
            target_key("linux", "amd64").as_deref(),
            Some("linux-x86_64")
        );
        assert_eq!(
            target_key("windows", "x64").as_deref(),
            Some("windows-x86_64")
        );
        assert!(target_key("freebsd", "x86_64").is_none());
    }

    #[test]
    fn manifest_selects_the_requested_remote_target() {
        let entry = entry_for(b"binary");
        let manifest = HeadlessUpdateManifest {
            version: "1.2.3".to_string(),
            notes: None,
            pub_date: None,
            protocol_min: 1,
            protocol_max: 2,
            targets: BTreeMap::from([("linux-x86_64".to_string(), entry.clone())]),
        };
        assert_eq!(
            manifest.entry_for("linux", "amd64"),
            Some(("linux-x86_64", &entry))
        );
        assert!(manifest.entry_for("macos", "aarch64").is_none());
    }

    #[test]
    fn parses_the_manifest_shape_generated_by_release_ci() {
        let raw = r#"{
          "version": "1.2.3",
          "notes": "Headless node v1.2.3",
          "pubDate": "2026-08-01T00:00:00Z",
          "protocolMin": 1,
          "protocolMax": 2,
          "targets": {
            "macos-aarch64": {
              "signature": "c2ln",
              "url": "https://github.com/OcHub-team/OcHub/releases/download/v1.2.3/OcHub_1.2.3_macos_aarch64_ochcli",
              "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "size": 42
            },
            "macos-x86_64": {
              "signature": "c2ln",
              "url": "https://github.com/OcHub-team/OcHub/releases/download/v1.2.3/OcHub_1.2.3_macos_x86_64_ochcli",
              "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "size": 43
            },
            "linux-x86_64": {
              "signature": "c2ln",
              "url": "https://github.com/OcHub-team/OcHub/releases/download/v1.2.3/OcHub_1.2.3_linux_x86_64_ochcli",
              "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
              "size": 44
            }
          }
        }"#;
        let manifest: HeadlessUpdateManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.protocol_min, 1);
        assert_eq!(manifest.protocol_max, 2);
        assert_eq!(manifest.targets.len(), 3);
        assert!(manifest.targets.contains_key("linux-x86_64"));
    }

    #[test]
    fn payload_size_and_hash_are_checked_before_signature() {
        let payload = b"signed release bytes";
        let entry = entry_for(payload);
        assert!(verify_size_and_hash(payload, &entry).is_ok());
        assert!(verify_size_and_hash(b"tampered", &entry).is_err());
        let mut wrong_size = entry;
        wrong_size.size += 1;
        assert!(verify_size_and_hash(payload, &wrong_size).is_err());
    }

    #[test]
    fn rejects_non_release_download_hosts() {
        let mut entry = entry_for(b"payload");
        entry.url = "https://example.com/ochcli".to_string();
        assert!(validate_entry(&entry).is_err());
    }
}
