//! 官方订阅额度查询服务
//!
//! 读取 CLI 工具的已有 OAuth 凭据，查询官方订阅额度。
//! 第一层：仅读取凭据，不实现登录/刷新。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths as config;

// ── 数据类型 ──────────────────────────────────────────────

/// 凭据状态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Valid,
    Expired,
    NotFound,
    ParseError,
}

/// 单个限速窗口（如 5小时会话、7天周期）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaTier {
    /// 窗口标识：five_hour, seven_day, seven_day_opus, seven_day_sonnet 等
    pub name: String,
    /// 使用百分比 0–100
    pub utilization: f64,
    /// ISO 8601 重置时间
    pub resets_at: Option<String>,
    /// ZenMux: 已用额度（USD）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_value_usd: Option<f64>,
    /// ZenMux: 窗口上限（USD）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value_usd: Option<f64>,
}

/// 超额使用信息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

/// 订阅额度查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionQuota {
    pub tool: String,
    pub credential_status: CredentialStatus,
    pub credential_message: Option<String>,
    pub success: bool,
    pub tiers: Vec<QuotaTier>,
    pub extra_usage: Option<ExtraUsage>,
    pub error: Option<String>,
    pub queried_at: Option<i64>,
}

impl SubscriptionQuota {
    pub(crate) fn not_found(tool: &str) -> Self {
        Self {
            tool: tool.to_string(),
            credential_status: CredentialStatus::NotFound,
            credential_message: None,
            success: false,
            tiers: vec![],
            extra_usage: None,
            error: None,
            queried_at: None,
        }
    }

    pub(crate) fn error(tool: &str, status: CredentialStatus, message: String) -> Self {
        Self {
            tool: tool.to_string(),
            credential_status: status,
            credential_message: Some(message.clone()),
            success: false,
            tiers: vec![],
            extra_usage: None,
            error: Some(message),
            queried_at: Some(now_millis()),
        }
    }
}

// ── Claude 凭据读取 ──────────────────────────────────────

/// Claude OAuth 凭据文件中的嵌套结构
#[derive(Deserialize)]
struct ClaudeOAuthEntry {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
}

/// 读取 Claude OAuth 凭据
///
/// 按优先级尝试以下来源：
/// 1. macOS Keychain (service: "Claude Code-credentials")
/// 2. 凭据文件 ~/.claude/.credentials.json
///
/// JSON 格式（两种 key 都兼容）：
/// {"claudeAiOauth": {"accessToken": "...", "expiresAt": ...}}
/// {"claude.ai_oauth": {"accessToken": "...", "expiresAt": ...}}
fn read_claude_credentials() -> (Option<String>, CredentialStatus, Option<String>) {
    // 来源 1: macOS Keychain
    #[cfg(target_os = "macos")]
    {
        if let Some(result) = read_claude_credentials_from_keychain() {
            return result;
        }
    }

    // 来源 2: 凭据文件
    read_claude_credentials_from_file()
}

/// 从 macOS Keychain 读取 Claude 凭据
#[cfg(target_os = "macos")]
fn read_claude_credentials_from_keychain()
-> Option<(Option<String>, CredentialStatus, Option<String>)> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // Keychain 中无此条目，回退到文件
    }

    let json_str = String::from_utf8(output.stdout).ok()?;
    let json_str = json_str.trim();
    if json_str.is_empty() {
        return None;
    }

    Some(parse_claude_credentials_json(json_str))
}

/// 从文件读取 Claude 凭据
fn read_claude_credentials_from_file() -> (Option<String>, CredentialStatus, Option<String>) {
    let cred_path = config::get_claude_config_dir().join(".credentials.json");

    if !cred_path.exists() {
        return (None, CredentialStatus::NotFound, None);
    }

    let content = match std::fs::read_to_string(&cred_path) {
        Ok(c) => c,
        Err(e) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to read credentials file: {e}")),
            );
        }
    };

    parse_claude_credentials_json(&content)
}

/// 解析 Claude 凭据 JSON（Keychain 和文件共用）
fn parse_claude_credentials_json(
    content: &str,
) -> (Option<String>, CredentialStatus, Option<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse credentials JSON: {e}")),
            );
        }
    };

    // 兼容两种 key 名
    let entry_value = parsed
        .get("claudeAiOauth")
        .or_else(|| parsed.get("claude.ai_oauth"));

    let entry_value = match entry_value {
        Some(v) => v,
        None => {
            return (
                None,
                CredentialStatus::ParseError,
                Some("No OAuth entry found in credentials".to_string()),
            );
        }
    };

    let entry: ClaudeOAuthEntry = match serde_json::from_value(entry_value.clone()) {
        Ok(e) => e,
        Err(e) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse OAuth entry: {e}")),
            );
        }
    };

    let access_token = match entry.access_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                None,
                CredentialStatus::ParseError,
                Some("accessToken is empty or missing".to_string()),
            );
        }
    };

    // 检查 token 是否过期
    if let Some(expires_at) = entry.expires_at
        && is_token_expired(&expires_at)
    {
        return (
            Some(access_token),
            CredentialStatus::Expired,
            Some("OAuth token has expired".to_string()),
        );
    }

    (Some(access_token), CredentialStatus::Valid, None)
}

/// 判断 token 是否过期，兼容 Unix 时间戳（秒/毫秒）和 ISO 字符串
fn is_token_expired(expires_at: &serde_json::Value) -> bool {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match expires_at {
        serde_json::Value::Number(n) => {
            if let Some(ts) = n.as_u64() {
                // 区分秒和毫秒（毫秒级时间戳大于 1e12）
                let ts_secs = if ts > 1_000_000_000_000 {
                    ts / 1000
                } else {
                    ts
                };
                ts_secs < now_secs
            } else {
                false
            }
        }
        serde_json::Value::String(s) => {
            // 尝试解析 ISO 8601 格式
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                (dt.timestamp() as u64) < now_secs
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            {
                (dt.and_utc().timestamp() as u64) < now_secs
            } else {
                false // 无法解析时不视为过期
            }
        }
        _ => false,
    }
}

// ── Claude API 查询 ──────────────────────────────────────

/// Claude OAuth 用量 API 响应中的单个窗口
#[derive(Deserialize)]
struct ApiUsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// Claude OAuth 用量 API 响应中的超额用量
#[derive(Deserialize)]
struct ApiExtraUsage {
    is_enabled: Option<bool>,
    monthly_limit: Option<f64>,
    used_credits: Option<f64>,
    utilization: Option<f64>,
    currency: Option<String>,
}

/// 已知的 Claude 用量窗口名称。`QuotaTier::name` 会是其中之一。
pub const TIER_FIVE_HOUR: &str = "five_hour";
pub const TIER_SEVEN_DAY: &str = "seven_day";
pub const TIER_SEVEN_DAY_OPUS: &str = "seven_day_opus";
pub const TIER_SEVEN_DAY_SONNET: &str = "seven_day_sonnet";

/// Coding Plan（Kimi / MiniMax）的周窗口 tier 名。与 `coding_plan::query_*`
/// 写入、tray 渲染、commands::provider 扁平化三处共用同一标识。
pub const TIER_WEEKLY_LIMIT: &str = "weekly_limit";

/// Grok Build 新版积分系统的月窗口 tier 名。
pub const TIER_MONTHLY_LIMIT: &str = "monthly_limit";

const KNOWN_TIERS: &[&str] = &[
    TIER_FIVE_HOUR,
    TIER_SEVEN_DAY,
    TIER_SEVEN_DAY_OPUS,
    TIER_SEVEN_DAY_SONNET,
];

/// 查询 Claude 官方订阅额度
async fn query_claude_quota(access_token: &str) -> SubscriptionQuota {
    let client = crate::http_client::get();

    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return SubscriptionQuota::error(
                "claude",
                CredentialStatus::Valid,
                format!("Network error: {e}"),
            );
        }
    };

    let status = resp.status();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return SubscriptionQuota::error(
            "claude",
            CredentialStatus::Expired,
            format!("Authentication failed (HTTP {status}). Please re-login with Claude CLI."),
        );
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return SubscriptionQuota::error(
            "claude",
            CredentialStatus::Valid,
            format!("API error (HTTP {status}): {body}"),
        );
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return SubscriptionQuota::error(
                "claude",
                CredentialStatus::Valid,
                format!("Failed to parse API response: {e}"),
            );
        }
    };

    // 解析已知的 tier 窗口
    let mut tiers = Vec::new();
    for &tier_name in KNOWN_TIERS {
        if let Some(window) = body.get(tier_name)
            && let Ok(w) = serde_json::from_value::<ApiUsageWindow>(window.clone())
            && let Some(util) = w.utilization
        {
            tiers.push(QuotaTier {
                name: tier_name.to_string(),
                utilization: util,
                resets_at: w.resets_at,
                used_value_usd: None,
                max_value_usd: None,
            });
        }
    }

    // 也解析未知窗口（API 可能返回新的窗口类型）
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if key == "extra_usage" || KNOWN_TIERS.contains(&key.as_str()) {
                continue;
            }
            if let Ok(w) = serde_json::from_value::<ApiUsageWindow>(value.clone())
                && let Some(util) = w.utilization
            {
                tiers.push(QuotaTier {
                    name: key.clone(),
                    utilization: util,
                    resets_at: w.resets_at,
                    used_value_usd: None,
                    max_value_usd: None,
                });
            }
        }
    }

    // 解析超额使用
    let extra_usage = body.get("extra_usage").and_then(|v| {
        serde_json::from_value::<ApiExtraUsage>(v.clone())
            .ok()
            .map(|e| ExtraUsage {
                is_enabled: e.is_enabled.unwrap_or(false),
                monthly_limit: e.monthly_limit,
                used_credits: e.used_credits,
                utilization: e.utilization,
                currency: e.currency,
            })
    });

    SubscriptionQuota {
        tool: "claude".to_string(),
        credential_status: CredentialStatus::Valid,
        credential_message: None,
        success: true,
        tiers,
        extra_usage,
        error: None,
        queried_at: Some(now_millis()),
    }
}

// ── Codex 凭据读取 ──────────────────────────────────────

#[derive(Deserialize)]
struct CodexAuthJson {
    auth_mode: Option<String>,
    tokens: Option<CodexTokens>,
    last_refresh: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

/// (access_token, account_id, status, message)
type CodexCredentials = (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
);

/// 读取 Codex OAuth 凭据
///
/// 按优先级尝试以下来源：
/// 1. macOS Keychain (service: "Codex Auth")
/// 2. 凭据文件 ~/.codex/auth.json
///
/// 仅 auth_mode == "chatgpt" (OAuth) 时有效，API key 模式不支持用量查询。
fn read_codex_credentials() -> CodexCredentials {
    #[cfg(target_os = "macos")]
    {
        if let Some(result) = read_codex_credentials_from_keychain() {
            return result;
        }
    }

    read_codex_credentials_from_file()
}

/// 从 macOS Keychain 读取 Codex 凭据
#[cfg(target_os = "macos")]
fn read_codex_credentials_from_keychain() -> Option<CodexCredentials> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "Codex Auth", "-w"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json_str = String::from_utf8(output.stdout).ok()?;
    let json_str = json_str.trim();
    if json_str.is_empty() {
        return None;
    }

    Some(parse_codex_credentials_json(json_str))
}

/// 从文件读取 Codex 凭据
fn read_codex_credentials_from_file() -> CodexCredentials {
    let auth_path = crate::apps::codex::get_codex_auth_path();

    if !auth_path.exists() {
        return (None, None, CredentialStatus::NotFound, None);
    }

    let content = match std::fs::read_to_string(&auth_path) {
        Ok(c) => c,
        Err(e) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to read Codex auth file: {e}")),
            );
        }
    };

    parse_codex_credentials_json(&content)
}

/// 解析 Codex 凭据 JSON（Keychain 和文件共用）
fn parse_codex_credentials_json(content: &str) -> CodexCredentials {
    let auth: CodexAuthJson = match serde_json::from_str(content) {
        Ok(a) => a,
        Err(e) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Codex auth JSON: {e}")),
            );
        }
    };

    // 仅 OAuth 模式有用量数据
    if auth.auth_mode.as_deref() != Some("chatgpt") {
        return (
            None,
            None,
            CredentialStatus::NotFound,
            Some("Codex not using OAuth mode".to_string()),
        );
    }

    let tokens = match auth.tokens {
        Some(t) => t,
        None => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some("No tokens in Codex auth".to_string()),
            );
        }
    };

    let access_token = match tokens.access_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some("access_token is empty or missing".to_string()),
            );
        }
    };

    // 检查 token 是否可能过期（距上次刷新 > 8 天）
    if let Some(ref last_refresh) = auth.last_refresh
        && is_codex_token_stale(last_refresh)
    {
        return (
            Some(access_token),
            tokens.account_id,
            CredentialStatus::Expired,
            Some("Codex token may be stale (>8 days since last refresh)".to_string()),
        );
    }

    (
        Some(access_token),
        tokens.account_id,
        CredentialStatus::Valid,
        None,
    )
}

/// 判断 Codex token 是否可能过期（Codex CLI 在 >8 天时自动刷新）
fn is_codex_token_stale(last_refresh: &str) -> bool {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(last_refresh) {
        let age_secs = now_secs.saturating_sub(dt.timestamp() as u64);
        age_secs > 8 * 24 * 3600
    } else {
        false
    }
}

// ── Codex API 查询 ──────────────────────────────────────

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: Option<f64>,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexRateLimit {
    primary_window: Option<CodexRateLimitWindow>,
    secondary_window: Option<CodexRateLimitWindow>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<CodexRateLimit>,
}

/// 根据窗口秒数映射到 tier 名称（与 Claude 的命名兼容以复用前端 i18n）
fn window_seconds_to_tier_name(secs: i64) -> String {
    match secs {
        18000 => "five_hour".to_string(),
        604800 => "seven_day".to_string(),
        s => {
            let hours = s / 3600;
            if hours >= 24 {
                format!("{}_day", hours / 24)
            } else {
                format!("{}_hour", hours)
            }
        }
    }
}

/// Unix 时间戳（秒）转 ISO 8601 字符串
fn unix_ts_to_iso(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339())
}

/// 查询 Codex / ChatGPT 反代订阅额度
///
/// 参数化 `tool_label` 和 `expired_message` 让该函数可被两个调用点共用：
/// - `"codex"` + "Please re-login with Codex CLI."（CLI 凭据路径）
/// - `"codex_oauth"` + "Please re-login via OcHub."（OcHub 自管 OAuth 路径）
pub(crate) async fn query_codex_quota(
    access_token: &str,
    account_id: Option<&str>,
    tool_label: &str,
    expired_message: &str,
) -> SubscriptionQuota {
    let client = crate::http_client::get();

    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json");

    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }

    let resp = match req.timeout(std::time::Duration::from_secs(15)).send().await {
        Ok(r) => r,
        Err(e) => {
            return SubscriptionQuota::error(
                tool_label,
                CredentialStatus::Valid,
                format!("Network error: {e}"),
            );
        }
    };

    let status = resp.status();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return SubscriptionQuota::error(
            tool_label,
            CredentialStatus::Expired,
            format!("{expired_message} (HTTP {status})"),
        );
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return SubscriptionQuota::error(
            tool_label,
            CredentialStatus::Valid,
            format!("API error (HTTP {status}): {body}"),
        );
    }

    let body: CodexUsageResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return SubscriptionQuota::error(
                tool_label,
                CredentialStatus::Valid,
                format!("Failed to parse API response: {e}"),
            );
        }
    };

    let mut tiers = Vec::new();

    if let Some(rate_limit) = body.rate_limit {
        for window in [rate_limit.primary_window, rate_limit.secondary_window]
            .into_iter()
            .flatten()
        {
            if let Some(used) = window.used_percent {
                tiers.push(QuotaTier {
                    name: window
                        .limit_window_seconds
                        .map(window_seconds_to_tier_name)
                        .unwrap_or_else(|| "unknown".to_string()),
                    utilization: used,
                    resets_at: window.reset_at.and_then(unix_ts_to_iso),
                    used_value_usd: None,
                    max_value_usd: None,
                });
            }
        }
    }

    SubscriptionQuota {
        tool: tool_label.to_string(),
        credential_status: CredentialStatus::Valid,
        credential_message: None,
        success: true,
        tiers,
        extra_usage: None,
        error: None,
        queried_at: Some(now_millis()),
    }
}

// ── Kimi Code 凭据与额度 ──────────────────────────────────

#[derive(Deserialize)]
struct KimiOAuthCredentials {
    access_token: Option<String>,
    expires_at: Option<f64>,
}

fn read_kimi_credentials() -> (Option<String>, CredentialStatus, Option<String>) {
    let path = crate::apps::kimi_code::get_kimi_code_config_dir()
        .join("credentials")
        .join("kimi-code.json");
    if !path.exists() {
        return (None, CredentialStatus::NotFound, None);
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to read Kimi Code credentials: {error}")),
            );
        }
    };
    let credentials: KimiOAuthCredentials = match serde_json::from_str(&content) {
        Ok(credentials) => credentials,
        Err(error) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Kimi Code credentials: {error}")),
            );
        }
    };
    let Some(token) = credentials
        .access_token
        .filter(|token| !token.trim().is_empty())
    else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("Kimi Code access_token is empty or missing".to_string()),
        );
    };
    let expired = credentials
        .expires_at
        .is_some_and(|expires_at| expires_at <= now_seconds_f64());
    if expired {
        return (
            Some(token),
            CredentialStatus::Expired,
            Some("Kimi Code OAuth token has expired. Please run `kimi login`.".to_string()),
        );
    }
    (Some(token), CredentialStatus::Valid, None)
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn quota_utilization(limit: f64, remaining: f64) -> f64 {
    if limit <= 0.0 {
        return 0.0;
    }
    ((limit - remaining).max(0.0) / limit * 100.0).clamp(0.0, 100.0)
}

fn quota_reset_time(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.to_string());
    }
    let timestamp = value.as_i64()?;
    let millis = if timestamp < 1_000_000_000_000 {
        timestamp * 1_000
    } else {
        timestamp
    };
    chrono::DateTime::from_timestamp_millis(millis).map(|time| time.to_rfc3339())
}

fn kimi_quota_from_body(body: &Value) -> SubscriptionQuota {
    let mut tiers = Vec::new();
    if let Some(limits) = body.get("limits").and_then(Value::as_array) {
        for item in limits {
            let detail = item.get("detail").unwrap_or(item);
            let Some(limit) = detail.get("limit").and_then(json_f64) else {
                continue;
            };
            let remaining = detail.get("remaining").and_then(json_f64).unwrap_or(0.0);
            tiers.push(QuotaTier {
                name: TIER_FIVE_HOUR.to_string(),
                utilization: quota_utilization(limit, remaining),
                resets_at: detail.get("resetTime").and_then(quota_reset_time),
                used_value_usd: None,
                max_value_usd: None,
            });
        }
    }
    if let Some(usage) = body.get("usage")
        && let Some(limit) = usage.get("limit").and_then(json_f64)
    {
        let remaining = usage.get("remaining").and_then(json_f64).unwrap_or(0.0);
        tiers.push(QuotaTier {
            name: TIER_WEEKLY_LIMIT.to_string(),
            utilization: quota_utilization(limit, remaining),
            resets_at: usage.get("resetTime").and_then(quota_reset_time),
            used_value_usd: None,
            max_value_usd: None,
        });
    }

    SubscriptionQuota {
        tool: "kimi-code".to_string(),
        credential_status: CredentialStatus::Valid,
        credential_message: None,
        success: true,
        tiers,
        extra_usage: None,
        error: None,
        queried_at: Some(now_millis()),
    }
}

async fn query_kimi_quota(access_token: &str) -> SubscriptionQuota {
    let response = crate::http_client::get()
        .get("https://api.kimi.com/coding/v1/usages")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return SubscriptionQuota::error(
                "kimi-code",
                CredentialStatus::Valid,
                format!("Network error: {error}"),
            );
        }
    };
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return SubscriptionQuota::error(
            "kimi-code",
            CredentialStatus::Expired,
            format!("Authentication failed (HTTP {status}). Please run `kimi login`."),
        );
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return SubscriptionQuota::error(
            "kimi-code",
            CredentialStatus::Valid,
            format!("API error (HTTP {status}): {body}"),
        );
    }
    match response.json::<Value>().await {
        Ok(body) => kimi_quota_from_body(&body),
        Err(error) => SubscriptionQuota::error(
            "kimi-code",
            CredentialStatus::Valid,
            format!("Failed to parse API response: {error}"),
        ),
    }
}

// ── Grok Build 凭据与额度 ─────────────────────────────────

#[derive(Deserialize)]
struct GrokOAuthCredentials {
    key: Option<String>,
    user_id: Option<String>,
    auth_mode: Option<String>,
    expires_at: Option<String>,
    oidc_issuer: Option<String>,
}

type GrokCredentials = (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
);

fn read_grok_credentials() -> GrokCredentials {
    let path = crate::apps::grokbuild::get_grok_config_dir().join("auth.json");
    if !path.exists() {
        return (None, None, CredentialStatus::NotFound, None);
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to read Grok auth file: {error}")),
            );
        }
    };
    let store: serde_json::Map<String, Value> = match serde_json::from_str(&content) {
        Ok(store) => store,
        Err(error) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Failed to parse Grok auth JSON: {error}")),
            );
        }
    };

    let candidate = store
        .iter()
        .filter_map(|(scope, value)| {
            let credentials = serde_json::from_value::<GrokOAuthCredentials>(value.clone()).ok()?;
            let mode = credentials.auth_mode.as_deref()?.to_ascii_lowercase();
            if mode == "api_key" || mode == "apikey" {
                return None;
            }
            let first_party_scope = scope.starts_with("https://auth.x.ai::");
            let first_party_issuer = credentials
                .oidc_issuer
                .as_deref()
                .is_some_and(|issuer| issuer.trim_end_matches('/') == "https://auth.x.ai");
            (first_party_scope || first_party_issuer).then_some(credentials)
        })
        .next();
    let Some(credentials) = candidate else {
        return (
            None,
            None,
            CredentialStatus::NotFound,
            Some("Grok Build is not using an xAI OAuth login".to_string()),
        );
    };
    let Some(token) = credentials.key.filter(|token| !token.trim().is_empty()) else {
        return (
            None,
            None,
            CredentialStatus::ParseError,
            Some("Grok OAuth token is empty or missing".to_string()),
        );
    };
    let Some(user_id) = credentials
        .user_id
        .filter(|user_id| !user_id.trim().is_empty())
    else {
        return (
            Some(token),
            None,
            CredentialStatus::ParseError,
            Some("Grok OAuth user_id is empty or missing".to_string()),
        );
    };
    let expired = credentials
        .expires_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|expires_at| expires_at <= chrono::Utc::now());
    if expired {
        return (
            Some(token),
            Some(user_id),
            CredentialStatus::Expired,
            Some("Grok OAuth token has expired. Please run `grok login`.".to_string()),
        );
    }
    (Some(token), Some(user_id), CredentialStatus::Valid, None)
}

fn grok_quota_from_body(body: &Value) -> SubscriptionQuota {
    let Some(config) = body.get("config").and_then(Value::as_object) else {
        return SubscriptionQuota::error(
            "grokbuild",
            CredentialStatus::Valid,
            "Grok billing response did not include quota data".to_string(),
        );
    };
    let utilization = config
        .get("creditUsagePercent")
        .and_then(json_f64)
        .or_else(|| {
            let limit = config
                .get("monthlyLimit")
                .and_then(|value| value.get("val"))
                .and_then(json_f64)?;
            let used = config
                .get("used")
                .and_then(|value| value.get("val"))
                .and_then(json_f64)?;
            (limit > 0.0).then_some((used / limit * 100.0).clamp(0.0, 100.0))
        });
    let Some(utilization) = utilization else {
        return SubscriptionQuota::error(
            "grokbuild",
            CredentialStatus::Valid,
            "Grok billing response did not include a usage percentage".to_string(),
        );
    };
    let current_period = config.get("currentPeriod");
    let period_type = current_period
        .and_then(|period| period.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = if period_type.contains("WEEKLY") {
        TIER_WEEKLY_LIMIT
    } else if period_type.contains("MONTHLY") {
        TIER_MONTHLY_LIMIT
    } else {
        "billing_period"
    };
    let resets_at = current_period
        .and_then(|period| period.get("end"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            config
                .get("billingPeriodEnd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });

    SubscriptionQuota {
        tool: "grokbuild".to_string(),
        credential_status: CredentialStatus::Valid,
        credential_message: body
            .get("subscriptionTier")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        success: true,
        tiers: vec![QuotaTier {
            name: name.to_string(),
            utilization: utilization.clamp(0.0, 100.0),
            resets_at,
            used_value_usd: None,
            max_value_usd: None,
        }],
        extra_usage: None,
        error: None,
        queried_at: Some(now_millis()),
    }
}

async fn query_grok_quota(access_token: &str, user_id: &str) -> SubscriptionQuota {
    let base = std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL")
        .unwrap_or_else(|_| "https://cli-chat-proxy.grok.com/v1".to_string());
    let url = format!("{}/billing?format=credits", base.trim_end_matches('/'));
    let response = crate::http_client::get()
        .get(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-userid", user_id)
        .header("x-grok-client-version", env!("CARGO_PKG_VERSION"))
        .header("User-Agent", "grok-cli")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return SubscriptionQuota::error(
                "grokbuild",
                CredentialStatus::Valid,
                format!("Network error: {error}"),
            );
        }
    };
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return SubscriptionQuota::error(
            "grokbuild",
            CredentialStatus::Expired,
            format!("Authentication failed (HTTP {status}). Please run `grok login`."),
        );
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return SubscriptionQuota::error(
            "grokbuild",
            CredentialStatus::Valid,
            format!("API error (HTTP {status}): {body}"),
        );
    }
    match response.json::<Value>().await {
        Ok(body) => grok_quota_from_body(&body),
        Err(error) => SubscriptionQuota::error(
            "grokbuild",
            CredentialStatus::Valid,
            format!("Failed to parse API response: {error}"),
        ),
    }
}

// ── 入口函数 ──────────────────────────────────────────────

/// 查询指定 CLI 工具的官方订阅额度
pub async fn get_subscription_quota(tool: &str) -> Result<SubscriptionQuota, String> {
    match tool {
        "claude" => {
            let (token, status, message) = read_claude_credentials();

            match status {
                CredentialStatus::NotFound => Ok(SubscriptionQuota::not_found("claude")),
                CredentialStatus::ParseError => Ok(SubscriptionQuota::error(
                    "claude",
                    CredentialStatus::ParseError,
                    message.unwrap_or_else(|| "Failed to parse credentials".to_string()),
                )),
                CredentialStatus::Expired => {
                    // 即使过期也尝试调用 API（token 可能实际上仍有效）
                    if let Some(token) = token {
                        let result = query_claude_quota(&token).await;
                        if result.success {
                            return Ok(result);
                        }
                    }
                    Ok(SubscriptionQuota::error(
                        "claude",
                        CredentialStatus::Expired,
                        message.unwrap_or_else(|| "OAuth token has expired".to_string()),
                    ))
                }
                CredentialStatus::Valid => {
                    let token = token.expect("token must be Some when status is Valid");
                    Ok(query_claude_quota(&token).await)
                }
            }
        }
        "codex" => {
            let (token, account_id, status, message) = read_codex_credentials();

            match status {
                CredentialStatus::NotFound => Ok(SubscriptionQuota::not_found("codex")),
                CredentialStatus::ParseError => Ok(SubscriptionQuota::error(
                    "codex",
                    CredentialStatus::ParseError,
                    message.unwrap_or_else(|| "Failed to parse credentials".to_string()),
                )),
                CredentialStatus::Expired => {
                    // 即使可能过期也尝试调用 API
                    if let Some(token) = token {
                        let result = query_codex_quota(
                            &token,
                            account_id.as_deref(),
                            "codex",
                            "Authentication failed. Please re-login with Codex CLI.",
                        )
                        .await;
                        if result.success {
                            return Ok(result);
                        }
                    }
                    Ok(SubscriptionQuota::error(
                        "codex",
                        CredentialStatus::Expired,
                        message.unwrap_or_else(|| "Codex OAuth token may be stale".to_string()),
                    ))
                }
                CredentialStatus::Valid => {
                    let token = token.expect("token must be Some when status is Valid");
                    Ok(query_codex_quota(
                        &token,
                        account_id.as_deref(),
                        "codex",
                        "Authentication failed. Please re-login with Codex CLI.",
                    )
                    .await)
                }
            }
        }
        "kimi" | "kimi-code" => {
            let (token, status, message) = read_kimi_credentials();
            query_token_quota("kimi-code", token, status, message, |token| async move {
                query_kimi_quota(&token).await
            })
            .await
        }
        "grok" | "grokbuild" => {
            let (token, user_id, status, message) = read_grok_credentials();
            match status {
                CredentialStatus::NotFound => Ok(SubscriptionQuota::not_found("grokbuild")),
                CredentialStatus::ParseError => Ok(SubscriptionQuota::error(
                    "grokbuild",
                    CredentialStatus::ParseError,
                    message.unwrap_or_else(|| "Failed to parse Grok credentials".to_string()),
                )),
                CredentialStatus::Expired | CredentialStatus::Valid => {
                    let Some(token) = token else {
                        return Ok(SubscriptionQuota::error(
                            "grokbuild",
                            CredentialStatus::ParseError,
                            "Grok OAuth token is missing".to_string(),
                        ));
                    };
                    let Some(user_id) = user_id else {
                        return Ok(SubscriptionQuota::error(
                            "grokbuild",
                            CredentialStatus::ParseError,
                            "Grok OAuth user_id is missing".to_string(),
                        ));
                    };
                    let result = query_grok_quota(&token, &user_id).await;
                    if result.success || matches!(status, CredentialStatus::Valid) {
                        Ok(result)
                    } else {
                        Ok(SubscriptionQuota::error(
                            "grokbuild",
                            CredentialStatus::Expired,
                            message.unwrap_or_else(|| "Grok OAuth token has expired".to_string()),
                        ))
                    }
                }
            }
        }
        _ => Ok(SubscriptionQuota::not_found(tool)),
    }
}

/// Query Claude / Kimi official quota with an already-resolved access token
/// (live slot or a per-card catalog).
pub async fn get_subscription_quota_with_token(
    tool: &str,
    access_token: &str,
) -> Result<SubscriptionQuota, String> {
    match tool {
        "claude" => Ok(query_claude_quota(access_token).await),
        "kimi" | "kimi-code" => Ok(query_kimi_quota(access_token).await),
        _ => Ok(SubscriptionQuota::not_found(tool)),
    }
}

// ── 辅助函数 ──────────────────────────────────────────────

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn now_seconds_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

async fn query_token_quota<F, Fut>(
    tool: &str,
    token: Option<String>,
    status: CredentialStatus,
    message: Option<String>,
    query: F,
) -> Result<SubscriptionQuota, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = SubscriptionQuota>,
{
    match status {
        CredentialStatus::NotFound => Ok(SubscriptionQuota::not_found(tool)),
        CredentialStatus::ParseError => Ok(SubscriptionQuota::error(
            tool,
            CredentialStatus::ParseError,
            message.unwrap_or_else(|| "Failed to parse credentials".to_string()),
        )),
        CredentialStatus::Expired | CredentialStatus::Valid => {
            let Some(token) = token else {
                return Ok(SubscriptionQuota::error(
                    tool,
                    CredentialStatus::ParseError,
                    "OAuth token is missing".to_string(),
                ));
            };
            let result = query(token).await;
            if result.success || matches!(status, CredentialStatus::Valid) {
                Ok(result)
            } else {
                Ok(SubscriptionQuota::error(
                    tool,
                    CredentialStatus::Expired,
                    message.unwrap_or_else(|| "OAuth token has expired".to_string()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod official_quota_tests {
    use super::{
        TIER_MONTHLY_LIMIT, TIER_WEEKLY_LIMIT, grok_quota_from_body, kimi_quota_from_body,
    };
    use serde_json::json;

    #[test]
    fn kimi_usage_is_converted_to_used_percentages() {
        let quota = kimi_quota_from_body(&json!({
            "limits": [{
                "detail": {"limit": 1000, "remaining": 750, "resetTime": 2_000_000_000}
            }],
            "usage": {"limit": "4000", "remaining": "1000", "resetTime": "2026-08-09T00:00:00Z"}
        }));
        assert!(quota.success);
        assert_eq!(quota.tiers.len(), 2);
        assert_eq!(quota.tiers[0].utilization, 25.0);
        assert_eq!(quota.tiers[1].name, TIER_WEEKLY_LIMIT);
        assert_eq!(quota.tiers[1].utilization, 75.0);
    }

    #[test]
    fn grok_credit_usage_percent_and_period_are_parsed() {
        let quota = grok_quota_from_body(&json!({
            "config": {
                "creditUsagePercent": 37.5,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "end": "2026-09-01T00:00:00Z"
                }
            },
            "subscriptionTier": "SuperGrok"
        }));
        assert!(quota.success);
        assert_eq!(quota.tiers[0].name, TIER_MONTHLY_LIMIT);
        assert_eq!(quota.tiers[0].utilization, 37.5);
        assert_eq!(quota.credential_message.as_deref(), Some("SuperGrok"));
    }

    #[test]
    fn grok_legacy_monthly_limit_is_supported() {
        let quota = grok_quota_from_body(&json!({
            "config": {
                "monthlyLimit": {"val": 2000},
                "used": {"val": 500},
                "billingPeriodEnd": "2026-09-01T00:00:00Z"
            }
        }));
        assert!(quota.success);
        assert_eq!(quota.tiers[0].utilization, 25.0);
    }
}
