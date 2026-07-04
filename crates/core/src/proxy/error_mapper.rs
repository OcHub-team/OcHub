//! Proxy error to status/message helpers.

use super::ProxyError;

pub fn map_proxy_error_to_status(error: &ProxyError) -> u16 {
    match error {
        ProxyError::AlreadyRunning => 409,
        ProxyError::NotRunning => 503,
        ProxyError::UpstreamError { status, .. } => *status,
        ProxyError::Timeout(_) | ProxyError::StreamIdleTimeout(_) => 504,
        ProxyError::ForwardFailed(_) => 502,
        ProxyError::NoAvailableProvider
        | ProxyError::AllProvidersCircuitOpen
        | ProxyError::NoProvidersConfigured
        | ProxyError::MaxRetriesExceeded
        | ProxyError::ProviderUnhealthy(_) => 503,
        ProxyError::ConfigError(_) | ProxyError::InvalidRequest(_) => 400,
        ProxyError::AuthError(_) => 401,
        ProxyError::DatabaseError(_) => 500,
        ProxyError::TransformError(_) => 422,
        _ => 500,
    }
}

pub fn get_error_message(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamError { status, body } => body
            .as_ref()
            .map(|body| format!("上游错误 ({status}): {body}"))
            .unwrap_or_else(|| format!("上游错误 ({status})")),
        ProxyError::Timeout(message) => format!("请求超时: {message}"),
        ProxyError::ForwardFailed(message) => format!("转发失败: {message}"),
        ProxyError::NoAvailableProvider => "无可用 Provider".to_string(),
        ProxyError::AllProvidersCircuitOpen => "所有供应商已熔断，无可用渠道".to_string(),
        ProxyError::NoProvidersConfigured => "未配置供应商".to_string(),
        ProxyError::MaxRetriesExceeded => "所有 Provider 都失败，重试耗尽".to_string(),
        ProxyError::ProviderUnhealthy(message) => format!("Provider 不健康: {message}"),
        ProxyError::DatabaseError(message) => format!("数据库错误: {message}"),
        ProxyError::TransformError(message) => format!("请求/响应转换错误: {message}"),
        _ => error.to_string(),
    }
}
