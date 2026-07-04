//! Connectivity (stream) check result/config types used by the DB DAO layer.
//!
//! Ported from cc-switch `services/stream_check.rs` (only the serializable data
//! types — the async `StreamCheckService` that performs the probe is not part
//! of routedeck-core). Field names / serde renames match the reference 1:1.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// 健康状态枚举
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Operational,
    Degraded,
    Failed,
}

/// 连通性检查配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCheckConfig {
    /// 单次探测超时（秒）
    pub timeout_secs: u64,
    /// 超时类失败的最大重试次数
    pub max_retries: u32,
    /// 降级阈值（毫秒）
    pub degraded_threshold_ms: u64,
}

impl Default for StreamCheckConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 8,
            max_retries: 1,
            degraded_threshold_ms: 6000,
        }
    }
}

/// 连通性检查结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCheckResult {
    pub status: HealthStatus,
    pub success: bool,
    pub message: String,
    pub response_time_ms: Option<u64>,
    pub http_status: Option<u16>,
    /// 保留字段以兼容 `stream_check_logs` 表结构；连通性检查恒为空串。
    pub model_used: String,
    pub tested_at: i64,
    pub retry_count: u32,
    /// 细粒度错误分类；连通性检查不再细分，恒为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}
