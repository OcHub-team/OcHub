//! Connectivity probe for the app-wide proxy settings page.
//!
//! Builds a throwaway client from whatever the user has typed rather than the
//! shared pool in [`crate::http_client`], so "Test" can check a candidate
//! configuration before it is saved anywhere.

use std::time::Duration;

use crate::error::AppError;
use crate::settings::ProxySettings;

/// Already used by the update checker (`services::update`) to reach GitHub,
/// so a working proxy for this probe is a working proxy for update checks.
const PROBE_URL: &str = "https://api.github.com";
const PROBE_TIMEOUT_SECS: u64 = 8;

pub async fn check_connection(candidate: &ProxySettings) -> Result<(), AppError> {
    let Some(proxy_url) = candidate.url() else {
        return Err(AppError::localized(
            "proxy.probe.incomplete",
            "请先填写代理地址和端口",
            "Enter a proxy host and port first.",
        ));
    };
    let proxy = reqwest::Proxy::all(&proxy_url)
        .map_err(|error| AppError::Config(format!("代理配置无效: {error}")))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
        .map_err(|error| AppError::Config(format!("无法创建测试客户端: {error}")))?;
    client
        .get(PROBE_URL)
        .send()
        .await
        .map_err(|error| AppError::Config(format!("代理连接失败: {error}")))?;
    Ok(())
}
