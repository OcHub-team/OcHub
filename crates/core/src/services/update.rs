//! Application update checks.
//!
//! The reference Tauri app used `tauri_plugin_updater` for installation. This
//! GPUI/Axum port keeps update discovery transport-agnostic here, while
//! platform-specific installation remains outside core.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::{AppError, Result};

const DEFAULT_REPO: &str = "sleepstars/RouteDeck";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub has_update: bool,
    pub release_url: String,
    pub release_notes: Option<String>,
    pub published_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

pub async fn check_for_updates(repo: Option<&str>) -> Result<UpdateCheckResult> {
    let repo = repo.unwrap_or(DEFAULT_REPO);
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let release = match fetch_latest_release(repo).await {
        Ok(release) => release,
        Err(AppError::HttpStatus { status: 404, .. }) => {
            return Ok(UpdateCheckResult {
                current_version,
                latest_version: None,
                has_update: false,
                release_url: latest_release_url(Some(repo)),
                release_notes: None,
                published_at: None,
            });
        }
        Err(error) => return Err(error),
    };
    let latest_version = normalize_version(&release.tag_name);
    let has_update = latest_version
        .as_deref()
        .and_then(|latest| compare_semver(&current_version, latest))
        .is_some_and(|ordering| ordering == Ordering::Less);

    Ok(UpdateCheckResult {
        current_version,
        latest_version,
        has_update,
        release_url: release.html_url,
        release_notes: release.body,
        published_at: release.published_at,
    })
}

pub fn latest_release_url(repo: Option<&str>) -> String {
    format!(
        "https://github.com/{}/releases/latest",
        repo.unwrap_or(DEFAULT_REPO)
    )
}

async fn fetch_latest_release(repo: &str) -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = reqwest::Client::new()
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header(
            "user-agent",
            format!("RouteDeck/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| AppError::Message(format!("检查更新失败: {error}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("读取更新响应失败: {error}")))?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }

    let release: GitHubRelease = serde_json::from_str(&body)
        .map_err(|error| AppError::Message(format!("解析更新响应失败: {error}")))?;
    if release.draft {
        return Err(AppError::Message(
            "最新 release 仍是草稿，无法更新".to_string(),
        ));
    }
    if release.prerelease {
        log::debug!(
            "[Update] latest GitHub release is prerelease: {}",
            release.tag_name
        );
    }
    Ok(release)
}

fn normalize_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_semver(value: &str) -> Option<([u64; 3], Vec<String>)> {
    let version = normalize_version(value)?;
    let (core, pre) = version
        .split_once('-')
        .map_or((version.as_str(), ""), |(core, pre)| (core, pre));
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    let pre = if pre.is_empty() {
        Vec::new()
    } else {
        pre.split('.').map(ToString::to_string).collect()
    };
    Some(([major, minor, patch], pre))
}

fn compare_semver(a: &str, b: &str) -> Option<Ordering> {
    let (a_core, a_pre) = parse_semver(a)?;
    let (b_core, b_pre) = parse_semver(b)?;
    match a_core.cmp(&b_core) {
        Ordering::Equal => Some(compare_prerelease(&a_pre, &b_pre)),
        other => Some(other),
    }
}

fn compare_prerelease(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }

    for (left, right) in a.iter().zip(b.iter()) {
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(l), Ok(r)) => l.cmp(&r),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_handles_v_prefix_and_prerelease() {
        assert_eq!(compare_semver("0.1.0", "v0.2.0"), Some(Ordering::Less));
        assert_eq!(
            compare_semver("1.0.0-beta.1", "1.0.0"),
            Some(Ordering::Less)
        );
        assert_eq!(compare_semver("1.2.3", "1.2.3"), Some(Ordering::Equal));
    }

    #[test]
    fn update_result_detects_newer_latest() {
        let latest = normalize_version("v2.0.0").unwrap();
        assert_eq!(
            compare_semver("1.9.9", &latest),
            Some(std::cmp::Ordering::Less)
        );
    }
}
