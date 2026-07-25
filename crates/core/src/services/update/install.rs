//! Downloading, verifying, and applying an update.
//!
//! The order of operations here is the security-relevant part:
//!
//! 1. Re-fetch the manifest, so a stale check cannot drive an install.
//! 2. Refuse anything that is not strictly newer than the running build. The
//!    manifest itself is served over TLS but is not signed — only the payload
//!    is — so someone able to rewrite it could otherwise point an install at an
//!    older *genuinely signed* release and roll users back onto a known bug.
//! 3. Download to memory, then verify the signature against the key compiled
//!    into this binary. Nothing reaches the filesystem before it verifies.
//! 4. Only then hand the bytes to a platform installer.
//!
//! Steps 1-3 live here; step 4 is in the per-platform submodules, which differ
//! in a way that matters to callers. See [`InstallOutcome`].

use std::path::PathBuf;

use super::{channel, manifest};
use crate::{AppError, Result};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// What the caller must do after a successful install.
///
/// The distinction is not cosmetic. On Windows the installer runs as a separate
/// process that replaces files while this one is still alive, so the app must
/// release everything it holds *before* the installer starts and then exit
/// promptly. On macOS and Linux the swap completes synchronously here, so the
/// app can shut down cleanly afterwards and only then relaunch. The reference
/// cc-switch app documents the same split at
/// `src-tauri/src/commands/settings.rs:193`, having hit the failure mode where
/// the pre-install cleanup ran on a platform that did not need it and left the
/// gateway stopped after an install that then failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Files are already replaced. Shut down cleanly, then launch `relaunch`.
    Replaced { relaunch: RelaunchTarget },
    /// An external installer is running and will replace files once this
    /// process exits. Exit now; it relaunches the app itself.
    InstallerSpawned,
}

/// A downloaded, signature-verified update that has not been applied yet.
///
/// Preparing and applying are separate so that every way an update can fail
/// harmlessly — network, signature, wrong platform, downgrade — happens while
/// the app is still fully running. Only once bytes are proven good does the
/// caller shut anything down. Applying consumes the value, so the same payload
/// cannot be installed twice.
pub struct PreparedUpdate {
    pub version: String,
    payload: Vec<u8>,
}

impl PreparedUpdate {
    /// Whether the app must release its resources before [`Self::apply`].
    ///
    /// True on Windows only, where apply spawns an external installer that
    /// starts replacing files while this process is still alive. Everywhere
    /// else apply finishes the swap itself and the caller shuts down after.
    pub fn requires_shutdown_before_apply(&self) -> bool {
        cfg!(target_os = "windows")
    }

    /// Replace the installed application. See [`InstallOutcome`].
    pub fn apply(self) -> Result<InstallOutcome> {
        log::info!("[Update] applying update {}", self.version);
        apply_payload(&self.payload)
    }
}

/// How to start the updated build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaunchTarget {
    /// `open -a <bundle>`, which lets LaunchServices start the new bundle
    /// rather than this process exec'ing a binary that has just been moved.
    MacOsBundle(PathBuf),
    /// Execute this path directly.
    Executable(PathBuf),
}

/// Progress of the payload download, in bytes.
pub type ProgressFn = Box<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// Download and verify the newest release without applying it.
///
/// Returns `Ok(None)` when the running build is already current. Any error
/// leaves the installation untouched — nothing has been written yet.
pub async fn prepare(
    repo: Option<&str>,
    progress: Option<ProgressFn>,
) -> Result<Option<PreparedUpdate>> {
    let repo = repo.unwrap_or(super::DEFAULT_REPO);
    let channel = channel::detect();
    if !channel.supports_self_install() {
        return Err(AppError::Message(format!(
            "当前安装方式（{}）不支持应用内更新，请从发布页下载新版本",
            channel.as_str()
        )));
    }
    if !manifest::signing_configured() {
        return Err(AppError::Message(
            "此版本未内置更新签名公钥，无法验证更新包；请从发布页手动下载".to_string(),
        ));
    }

    let Some(manifest) = super::fetch_manifest(repo).await? else {
        return Err(AppError::Message(
            "最新发布未提供更新清单，无法应用内更新；请从发布页手动下载".to_string(),
        ));
    };

    if !super::is_newer_than_current(&manifest.version) {
        log::info!(
            "[Update] refusing install: manifest {} is not newer than {}",
            manifest.version,
            super::current_version()
        );
        return Ok(None);
    }

    let Some(entry) = manifest.entry_for_current_target() else {
        return Err(AppError::Message(format!(
            "最新版本没有为当前平台（{}）提供更新包，请从发布页下载",
            manifest::current_target_key()
        )));
    };

    validate_download_url(&entry.url)?;
    log::info!(
        "[Update] downloading {} for {}",
        manifest.version,
        manifest::current_target_key()
    );
    let payload = download(&entry.url, progress).await?;

    // The gate the whole feature rests on. Bytes that fail here are dropped
    // without ever being written to disk.
    manifest::verify_payload(&payload, &entry.signature)?;
    log::info!(
        "[Update] signature verified for {} ({} bytes)",
        manifest.version,
        payload.len()
    );

    Ok(Some(PreparedUpdate {
        version: manifest.version.clone(),
        payload,
    }))
}

/// Reject a payload URL that does not come from GitHub releases over TLS.
///
/// Defence in depth rather than the primary control: the signature check
/// already stops an attacker-supplied binary from being installed. This limits
/// what a rewritten manifest can make the app talk to at all.
fn validate_download_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|error| AppError::Message(format!("更新包地址无效: {error}")))?;
    if parsed.scheme() != "https" {
        return Err(AppError::Message("更新包地址必须使用 HTTPS".to_string()));
    }
    let host = parsed.host_str().unwrap_or_default();
    let allowed = host == "github.com"
        || host == "objects.githubusercontent.com"
        || host.ends_with(".githubusercontent.com");
    if !allowed {
        return Err(AppError::Message(format!(
            "更新包地址的主机不在允许列表内: {host}"
        )));
    }
    Ok(())
}

async fn download(url: &str, progress: Option<ProgressFn>) -> Result<Vec<u8>> {
    use futures::StreamExt as _;

    let response = crate::http_client::get()
        .get(url)
        .header("user-agent", format!("OcHub/{}", super::current_version()))
        .send()
        .await
        .map_err(|error| AppError::Message(format!("下载更新包失败: {error}")))?;

    let status = response.status();
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }

    let total = response.content_length();
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::Message(format!("下载更新包中断: {error}")))?;
        bytes.extend_from_slice(&chunk);
        if let Some(report) = progress.as_ref() {
            report(bytes.len() as u64, total);
        }
    }
    Ok(bytes)
}

/// Hand verified bytes to the platform installer.
#[allow(unused_variables)]
fn apply_payload(payload: &[u8]) -> Result<InstallOutcome> {
    #[cfg(target_os = "macos")]
    return macos::apply(payload);
    #[cfg(target_os = "windows")]
    return windows::apply(payload);
    #[cfg(target_os = "linux")]
    return linux::apply(payload);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    return Err(AppError::Message("当前平台不支持应用内更新".to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_release_urls_are_accepted() {
        validate_download_url("https://github.com/OcHub-team/OcHub/releases/download/v0.2.0/a.dmg")
            .unwrap();
        // GitHub redirects release downloads to object storage.
        validate_download_url("https://objects.githubusercontent.com/foo").unwrap();
    }

    #[test]
    fn plaintext_urls_are_rejected() {
        let error = validate_download_url("http://github.com/a/b").unwrap_err();
        assert!(error.to_string().contains("HTTPS"), "{error}");
    }

    #[test]
    fn a_foreign_host_is_rejected() {
        // What a rewritten manifest would try first.
        let error = validate_download_url("https://evil.example/payload.dmg").unwrap_err();
        assert!(error.to_string().contains("允许列表"), "{error}");
    }

    #[test]
    fn a_host_merely_containing_github_is_rejected() {
        let error = validate_download_url("https://github.com.evil.example/x").unwrap_err();
        assert!(error.to_string().contains("允许列表"), "{error}");
    }

    #[test]
    fn a_malformed_url_is_rejected() {
        assert!(validate_download_url("not a url").is_err());
    }
}
