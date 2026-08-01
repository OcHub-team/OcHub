//! Application update checks.
//!
//! Discovery reads the signed `latest.json` published as a release asset (see
//! [`manifest`]) rather than the GitHub API, for two reasons: the manifest
//! carries the signature and download URL the installer needs, and asset
//! downloads are not subject to the API's 60-requests-per-hour-per-IP
//! unauthenticated limit, which one NAT'd network can exhaust between them.
//!
//! The GitHub API remains as a fallback so that a release published before the
//! manifest existed still reports "a newer version exists" — check-only, since
//! there is nothing to verify a download against.

pub mod channel;
pub mod headless;
pub mod install;
pub mod manifest;
pub mod relaunch;

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::Duration;

use crate::{AppError, Result};

pub(crate) const DEFAULT_REPO: &str = "OcHub-team/OcHub";

/// Update discovery should fail fast; the app checks on a timer and a stalled
/// request would otherwise pin a connection for the shared client's 10 minutes.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Spacing between automatic checks.
///
/// Daily rather than hourly: releases are not frequent enough for a shorter
/// period to surface anything, and every check is a request users did not ask
/// for.
pub const AUTO_CHECK_INTERVAL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub has_update: bool,
    pub release_url: String,
    pub release_notes: Option<String>,
    pub published_at: Option<String>,
    /// How this copy was installed, e.g. `macos-app` or `linux-system-package`.
    pub install_channel: String,
    /// Whether the in-app "update now" path is available. False for
    /// package-manager and portable installs, for builds with no signing key
    /// compiled in, and for releases that ship no payload for this target — in
    /// all of which the UI offers the release page instead.
    pub can_self_install: bool,
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

/// The current version of this build, as a semver string.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether `latest` is strictly newer than the running build.
///
/// Unparseable versions compare as "not newer", so a malformed manifest can
/// never trigger an install.
pub fn is_newer_than_current(latest: &str) -> bool {
    compare_semver(current_version(), latest).is_some_and(|order| order == Ordering::Less)
}

/// Whether `latest` is strictly newer than an arbitrary installed version.
///
/// Headless nodes report their running version to the controlling desktop, so
/// the comparison cannot always use this process' package version.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    compare_semver(current, latest).is_some_and(|order| order == Ordering::Less)
}

/// Whether an automatic check should run now.
///
/// A `last_check_at` in the future — a clock that was wound back, or a settings
/// file synced from another machine — counts as due rather than locking checks
/// out until real time catches up.
pub fn auto_check_due(auto_update_check: bool, last_check_at: Option<i64>, now: i64) -> bool {
    if !auto_update_check {
        return false;
    }
    match last_check_at {
        None => true,
        Some(last) if last > now => true,
        Some(last) => now - last >= AUTO_CHECK_INTERVAL_SECONDS,
    }
}

/// Whether to raise an unprompted notification for a discovered update.
///
/// Only suppresses the exact version already announced; a later release still
/// notifies, so dismissing one cannot silently become "never update again".
pub fn should_notify(result: &UpdateCheckResult, skipped_version: Option<&str>) -> bool {
    if !result.has_update {
        return false;
    }
    match (result.latest_version.as_deref(), skipped_version) {
        (Some(latest), Some(skipped)) => normalize_version(latest) != normalize_version(skipped),
        _ => true,
    }
}

/// Apply a verified update and arrange for the new build to start.
///
/// Returns once the caller should quit. Quitting is deliberately left to the
/// caller rather than done here: the UI owns the only clean shutdown path
/// (GPUI's quit handlers persist window bounds), and an updater that calls
/// `process::exit` itself is exactly the shape that made the reference
/// cc-switch app leak a tray icon and skip its own cleanup.
pub fn apply_and_arm_restart(prepared: install::PreparedUpdate) -> Result<()> {
    let version = prepared.version.clone();
    match prepared.apply()? {
        install::InstallOutcome::Replaced { relaunch } => {
            relaunch::after_exit(&relaunch)?;
            log::info!("[Update] {version} installed; quit to complete the restart");
            Ok(())
        }
        // The NSIS installer waits for this process to exit, then replaces the
        // files and relaunches with `/R`. Nothing to arm.
        install::InstallOutcome::InstallerSpawned => {
            log::info!("[Update] installer for {version} will take over after exit");
            Ok(())
        }
    }
}

pub async fn check_for_updates(repo: Option<&str>) -> Result<UpdateCheckResult> {
    let repo = repo.unwrap_or(DEFAULT_REPO);
    match fetch_manifest(repo).await {
        Ok(Some(manifest)) => Ok(result_from_manifest(repo, &manifest)),
        // No manifest on the latest release: it predates in-app updating, so
        // report what the API knows and leave installation to the release page.
        Ok(None) => check_via_github_api(repo).await,
        Err(error) => {
            log::warn!("[Update] manifest fetch failed, falling back to GitHub API: {error}");
            check_via_github_api(repo).await
        }
    }
}

fn result_from_manifest(repo: &str, manifest: &manifest::UpdateManifest) -> UpdateCheckResult {
    let channel = channel::detect();
    let latest_version = normalize_version(&manifest.version);
    let has_update = latest_version.as_deref().is_some_and(is_newer_than_current);

    // Every condition must hold for the one-click path to be honest: a channel
    // that can be replaced, a key to verify against, and a payload built for
    // this target in this particular release.
    let can_self_install = has_update
        && channel.supports_self_install()
        && manifest::signing_configured()
        && manifest.entry_for_current_target().is_some();

    UpdateCheckResult {
        current_version: current_version().to_string(),
        latest_version,
        has_update,
        release_url: release_tag_url(repo, &manifest.version),
        release_notes: manifest.notes.clone(),
        published_at: manifest.pub_date.clone(),
        install_channel: channel.as_str().to_string(),
        can_self_install,
    }
}

async fn check_via_github_api(repo: &str) -> Result<UpdateCheckResult> {
    let channel = channel::detect();
    let current = current_version().to_string();
    let release = match fetch_latest_release(repo).await {
        Ok(release) => release,
        Err(AppError::HttpStatus { status: 404, .. }) => {
            return Ok(UpdateCheckResult {
                current_version: current,
                latest_version: None,
                has_update: false,
                release_url: latest_release_url(Some(repo)),
                release_notes: None,
                published_at: None,
                install_channel: channel.as_str().to_string(),
                can_self_install: false,
            });
        }
        Err(error) => return Err(error),
    };
    let latest_version = normalize_version(&release.tag_name);
    let has_update = latest_version.as_deref().is_some_and(is_newer_than_current);

    Ok(UpdateCheckResult {
        current_version: current,
        latest_version,
        has_update,
        release_url: release.html_url,
        release_notes: release.body,
        published_at: release.published_at,
        install_channel: channel.as_str().to_string(),
        // Without a manifest there is no signature, and an unverified download
        // is exactly what this feature must not do.
        can_self_install: false,
    })
}

/// Fetch and parse `latest.json`.
///
/// `Ok(None)` means the release ships no manifest, which is a normal state
/// during rollout rather than a failure.
pub async fn fetch_manifest(repo: &str) -> Result<Option<manifest::UpdateManifest>> {
    let url = manifest_url(repo);
    let response = crate::http_client::get()
        .get(&url)
        .header("accept", "application/json")
        .header("user-agent", user_agent())
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("获取更新清单失败: {error}")))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("读取更新清单失败: {error}")))?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }

    serde_json::from_str(&body)
        .map(Some)
        .map_err(|error| AppError::Message(format!("解析更新清单失败: {error}")))
}

fn manifest_url(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/latest.json")
}

fn user_agent() -> String {
    format!("OcHub/{}", current_version())
}

pub fn latest_release_url(repo: Option<&str>) -> String {
    format!(
        "https://github.com/{}/releases/latest",
        repo.unwrap_or(DEFAULT_REPO)
    )
}

fn release_tag_url(repo: &str, version: &str) -> String {
    match normalize_version(version) {
        Some(version) => format!("https://github.com/{repo}/releases/tag/v{version}"),
        None => latest_release_url(Some(repo)),
    }
}

async fn fetch_latest_release(repo: &str) -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = crate::http_client::get()
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", user_agent())
        .timeout(CHECK_TIMEOUT)
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

    const DAY: i64 = AUTO_CHECK_INTERVAL_SECONDS;

    #[test]
    fn auto_check_respects_the_interval() {
        assert!(auto_check_due(true, None, 1_000_000), "first run is due");
        assert!(!auto_check_due(true, Some(1_000_000), 1_000_000 + DAY - 1));
        assert!(auto_check_due(true, Some(1_000_000), 1_000_000 + DAY));
    }

    #[test]
    fn auto_check_is_skipped_when_disabled() {
        assert!(!auto_check_due(false, None, 1_000_000));
        assert!(!auto_check_due(false, Some(0), 1_000_000));
    }

    #[test]
    fn a_future_timestamp_does_not_wedge_checking() {
        // A settings file synced from a machine with a fast clock would
        // otherwise disable checks until real time caught up.
        assert!(auto_check_due(true, Some(2_000_000), 1_000_000));
    }

    fn result_with_latest(latest: &str) -> UpdateCheckResult {
        UpdateCheckResult {
            current_version: "0.1.0".to_string(),
            latest_version: Some(latest.to_string()),
            has_update: true,
            release_url: String::new(),
            release_notes: None,
            published_at: None,
            install_channel: "macos-app".to_string(),
            can_self_install: true,
        }
    }

    #[test]
    fn an_already_announced_version_is_not_notified_again() {
        let result = result_with_latest("0.2.0");
        assert!(!should_notify(&result, Some("0.2.0")));
        // Tag spelling must not defeat the match.
        assert!(!should_notify(&result, Some("v0.2.0")));
    }

    #[test]
    fn announcing_one_version_does_not_silence_the_next() {
        // The property that keeps dismissal from becoming "never update".
        assert!(should_notify(&result_with_latest("0.3.0"), Some("0.2.0")));
    }

    #[test]
    fn being_up_to_date_never_notifies() {
        let mut result = result_with_latest("0.1.0");
        result.has_update = false;
        assert!(!should_notify(&result, None));
    }
}
