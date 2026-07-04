//! Proxy-related configuration and state types used by the DB DAO layer.
//!
//! Ported from cc-switch `proxy/types.rs` and `proxy/circuit_breaker.rs`
//! (only the data structures the persistence layer needs — the runtime proxy
//! server, adapters, and circuit-breaker *behaviour* are not part of routedeck-core).
//! Field names and serde renames match the reference 1:1 so settings-table JSON
//! payloads stay byte-compatible.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// 代理服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 监听地址
    pub listen_address: String,
    /// 监听端口
    pub listen_port: u16,
    /// 最大重试次数
    pub max_retries: u8,
    /// 请求超时时间（秒）- 已废弃，保留兼容
    pub request_timeout: u64,
    /// 是否启用日志
    pub enable_logging: bool,
    /// 是否正在接管 Live 配置
    #[serde(default)]
    pub live_takeover_active: bool,
    /// 流式首字超时（秒）
    #[serde(default = "default_streaming_first_byte_timeout")]
    pub streaming_first_byte_timeout: u64,
    /// 流式静默超时（秒）
    #[serde(default = "default_streaming_idle_timeout")]
    pub streaming_idle_timeout: u64,
    /// 非流式总超时（秒）
    #[serde(default = "default_non_streaming_timeout")]
    pub non_streaming_timeout: u64,
}

fn default_streaming_first_byte_timeout() -> u64 {
    60
}

fn default_streaming_idle_timeout() -> u64 {
    120
}

fn default_non_streaming_timeout() -> u64 {
    600
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_address: "127.0.0.1".to_string(),
            listen_port: 15721,
            max_retries: 3,
            request_timeout: 600,
            enable_logging: true,
            live_takeover_active: false,
            streaming_first_byte_timeout: 60,
            streaming_idle_timeout: 120,
            non_streaming_timeout: 600,
        }
    }
}

/// 代理服务器状态
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyStatus {
    pub running: bool,
    pub address: String,
    pub port: u16,
    pub active_connections: usize,
    pub total_requests: u64,
    pub success_requests: u64,
    pub failed_requests: u64,
    pub success_rate: f32,
    pub uptime_seconds: u64,
    pub current_provider: Option<String>,
    pub current_provider_id: Option<String>,
    pub last_request_at: Option<String>,
    pub last_error: Option<String>,
    pub failover_count: u64,
    #[serde(default)]
    pub active_targets: Vec<ActiveTarget>,
}

/// 活跃的代理目标信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTarget {
    pub app_type: String,
    pub provider_name: String,
    pub provider_id: String,
}

/// 代理服务器信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServerInfo {
    pub address: String,
    pub port: u16,
    pub started_at: String,
}

/// 各应用的接管状态
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyTakeoverStatus {
    pub claude: bool,
    pub codex: bool,
    pub gemini: bool,
    pub opencode: bool,
    pub openclaw: bool,
}

/// Provider健康状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub provider_id: String,
    pub app_type: String,
    pub is_healthy: bool,
    pub consecutive_failures: u32,
    pub last_success_at: Option<String>,
    pub last_failure_at: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// Live 配置备份记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveBackup {
    pub app_type: String,
    pub original_config: String,
    pub backed_up_at: String,
}

/// 全局代理配置（统一字段，三行镜像）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalProxyConfig {
    pub proxy_enabled: bool,
    pub listen_address: String,
    pub listen_port: u16,
    pub enable_logging: bool,
}

/// 应用级代理配置（每个 app 独立）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProxyConfig {
    pub app_type: String,
    pub enabled: bool,
    pub auto_failover_enabled: bool,
    pub max_retries: u32,
    pub streaming_first_byte_timeout: u32,
    pub streaming_idle_timeout: u32,
    pub non_streaming_timeout: u32,
    pub circuit_failure_threshold: u32,
    pub circuit_success_threshold: u32,
    pub circuit_timeout_seconds: u32,
    pub circuit_error_rate_threshold: f64,
    pub circuit_min_requests: u32,
}

/// 整流器配置（存储在 settings 表中）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RectifierConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub request_thinking_signature: bool,
    #[serde(default = "default_true")]
    pub request_thinking_budget: bool,
    #[serde(default = "default_true")]
    pub request_media_fallback: bool,
    #[serde(default = "default_true")]
    pub request_media_heuristic: bool,
}

fn default_true() -> bool {
    true
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for RectifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            request_thinking_signature: true,
            request_thinking_budget: true,
            request_media_fallback: true,
            request_media_heuristic: true,
        }
    }
}

/// 请求优化器配置（存储在 settings 表中，key = "optimizer_config"）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub thinking_optimizer: bool,
    #[serde(default = "default_true")]
    pub cache_injection: bool,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: String,
}

fn default_cache_ttl() -> String {
    "1h".to_string()
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            thinking_optimizer: true,
            cache_injection: true,
            cache_ttl: "1h".to_string(),
        }
    }
}

/// Copilot 优化器配置（存储在 settings 表中，key = "copilot_optimizer_config"）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotOptimizerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub request_classification: bool,
    #[serde(default = "default_true")]
    pub tool_result_merging: bool,
    #[serde(default = "default_true")]
    pub compact_detection: bool,
    #[serde(default = "default_true")]
    pub deterministic_request_id: bool,
    #[serde(default = "default_true")]
    pub subagent_detection: bool,
    #[serde(default = "default_true")]
    pub warmup_downgrade: bool,
    #[serde(default = "default_warmup_model")]
    pub warmup_model: String,
    #[serde(default = "default_true")]
    pub strip_thinking: bool,
}

fn default_warmup_model() -> String {
    "gpt-5-mini".to_string()
}

impl Default for CopilotOptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            request_classification: true,
            tool_result_merging: true,
            compact_detection: true,
            deterministic_request_id: true,
            subagent_detection: true,
            warmup_downgrade: true,
            warmup_model: "gpt-5-mini".to_string(),
            strip_thinking: true,
        }
    }
}

/// 日志配置（存储在 settings 表的 log_config 字段中，JSON 格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: "info".to_string(),
        }
    }
}

impl LogConfig {
    /// 将配置转换为 log::LevelFilter
    pub fn to_level_filter(&self) -> log::LevelFilter {
        if !self.enabled {
            return log::LevelFilter::Off;
        }
        match self.level.to_lowercase().as_str() {
            "error" => log::LevelFilter::Error,
            "warn" => log::LevelFilter::Warn,
            "info" => log::LevelFilter::Info,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,
            _ => log::LevelFilter::Info,
        }
    }
}

/// 熔断器配置
///
/// Ported from cc-switch `proxy/circuit_breaker.rs`. The DAO layer persists the
/// config struct; the runtime breaker implementation lives in `crate::proxy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub timeout_seconds: u64,
    pub error_rate_threshold: f64,
    pub min_requests: u32,
}

impl From<&AppProxyConfig> for CircuitBreakerConfig {
    fn from(config: &AppProxyConfig) -> Self {
        Self {
            failure_threshold: config.circuit_failure_threshold,
            success_threshold: config.circuit_success_threshold,
            timeout_seconds: config.circuit_timeout_seconds as u64,
            error_rate_threshold: config.circuit_error_rate_threshold,
            min_requests: config.circuit_min_requests,
        }
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 4,
            success_threshold: 2,
            timeout_seconds: 60,
            error_rate_threshold: 0.6,
            min_requests: 10,
        }
    }
}
