//! Transparent passthrough forwarder.
//!
//! Copies the incoming request (method, headers, body) to the selected
//! provider's real upstream, replacing the takeover placeholder token with the
//! provider's real credential, and streams the response back to the client.
//!
//! This is the tier-1 "must-have" forwarding path: a same-format passthrough
//! that is correct for switching between providers speaking the same wire API
//! (the common case). It also dispatches to the cross-format transform tier when
//! a Claude provider declares `meta.api_format = openai_chat/openai_responses/
//! gemini_native`.

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::app_type::AppType;
use crate::db::PRICING_SOURCE_REQUEST;
use crate::model::Provider;
use crate::proxy::content_encoding;
use crate::proxy::error::ProxyError;
use crate::proxy::usage::parser::TokenUsage;
use crate::proxy::PROXY_TOKEN_PLACEHOLDER;

use super::server::ProxyState;

/// Hop-by-hop / proxy-managed headers that must not be forwarded verbatim.
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    // Auth headers are re-injected from the provider credential below.
    "authorization",
    "x-api-key",
    "x-goog-api-key",
];

const STRIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
];

/// How the upstream credential should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStrategy {
    /// `Authorization: Bearer <token>`
    Bearer,
    /// `x-api-key: <token>`
    ApiKey,
}

/// Resolved upstream target for a provider.
struct UpstreamTarget {
    base_url: String,
    token: Option<String>,
    strategy: AuthStrategy,
}

struct PreparedPassthroughRequest {
    path: String,
    query: Option<String>,
    body: Bytes,
    codex_chat_conversion: Option<CodexChatConversion>,
}

struct CodexChatConversion {
    tool_context: crate::proxy::providers::transform_codex_chat::CodexToolContext,
}

/// Forward an incoming request for `app_type` to the selected provider upstream.
///
/// On any routing/forwarding error returns a plain error response (no format
/// transform) so the client sees a usable status code rather than a hang.
pub async fn forward(
    state: ProxyState,
    app_type: AppType,
    method: Method,
    path: &str,
    query: Option<&str>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let request_start = std::time::Instant::now();
    record_request_started(&state, &app_type).await;

    // reqwest 的自动解压已禁用（透传 accept-encoding），故压缩的客户端请求体
    // （如 Codex Desktop 登录态发的 zstd/gzip）需在解析/转发前手动解压，否则
    // 后续所有 serde_json::from_slice（passthrough 与 transform 两路）都会失败。
    let (headers, body) = match decode_request_body(headers, body) {
        Ok(pair) => pair,
        Err(encoding) => {
            let message = format!("Unsupported request content-encoding: {encoding}");
            record_request_failed(&state, &message).await;
            return error_response(StatusCode::BAD_REQUEST, &message);
        }
    };

    let parsed_body = serde_json::from_slice::<Value>(&body).ok();
    let request_model = request_model_for_app(parsed_body.as_ref());
    let session_format = session_format_for_app(&app_type);
    let session_id = super::session::extract_session_id(
        &headers,
        parsed_body.as_ref().unwrap_or(&Value::Null),
        session_format,
    )
    .session_id;

    // 1. Select target providers. With auto failover off this is just the
    // current provider; with auto failover on it is the queue in priority order.
    let providers = match state
        .provider_router
        .select_providers(app_type.as_str())
        .await
    {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[Forward] no provider for {}: {e}", app_type.as_str());
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("no available provider: {e}"),
            );
        }
    };

    if providers.is_empty() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "no available provider");
    }

    let current_provider_id_at_start =
        crate::settings::get_effective_current_provider(&state.db, &app_type)
            .ok()
            .flatten();
    let total_providers = providers.len();
    let mut last_error = "all providers failed".to_string();
    let mut last_status = StatusCode::BAD_GATEWAY;

    for (index, provider) in providers.into_iter().enumerate() {
        let is_last = index + 1 == total_providers;
        set_attempt_provider(&state, &app_type, &provider).await;

        let permit = state
            .provider_router
            .allow_provider_request(&provider.id, app_type.as_str())
            .await;
        if !permit.allowed {
            last_error = format!(
                "provider {} temporarily unavailable (circuit open)",
                provider.name
            );
            last_status = StatusCode::SERVICE_UNAVAILABLE;
            record_request_failed(&state, &last_error).await;
            if is_last {
                return error_response(last_status, &last_error);
            }
            continue;
        }

        if super::forward_transform::requires_transform(&app_type, &provider) {
            let transformed = super::forward_transform::forward_transformed(
                &state,
                app_type,
                &method,
                path,
                query,
                &headers,
                &provider,
                body.clone(),
            )
            .await;

            match transformed {
                Ok(resp) => {
                    let success = resp.status().is_success();
                    let status = resp.status();
                    let _ = state
                        .provider_router
                        .record_result(
                            &provider.id,
                            app_type.as_str(),
                            permit.used_half_open_permit,
                            success,
                            if success {
                                None
                            } else {
                                Some(format!("upstream status {status}"))
                            },
                        )
                        .await;

                    if success {
                        record_request_succeeded(
                            &state,
                            &app_type,
                            &provider,
                            should_hot_switch(&current_provider_id_at_start, &provider),
                        )
                        .await;
                        trigger_failover_switch_if_needed(
                            &state,
                            app_type,
                            &provider,
                            &current_provider_id_at_start,
                        );
                    } else {
                        let message = format!("upstream status {status}");
                        record_request_failed(&state, &message).await;
                    }

                    return resp;
                }
                Err(e) => {
                    let retryable = is_retryable_proxy_error(&e);
                    let message = e.to_string();
                    let _ = state
                        .provider_router
                        .record_result(
                            &provider.id,
                            app_type.as_str(),
                            permit.used_half_open_permit,
                            false,
                            Some(message.clone()),
                        )
                        .await;
                    log::warn!(
                        "[Forward] transform forward failed for {}: {message}",
                        provider.name
                    );
                    last_status = proxy_error_status(&e);
                    last_error = message;
                    record_request_failed(&state, &last_error).await;
                    if retryable && !is_last {
                        continue;
                    }
                    use axum::response::IntoResponse;
                    return e.into_response();
                }
            }
        }

        let target = match resolve_upstream(&app_type, &provider) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "[Forward] resolve upstream failed for {}: {e}",
                    provider.name
                );
                last_error = e;
                last_status = StatusCode::BAD_GATEWAY;
                record_request_failed(&state, &last_error).await;
                if is_last {
                    return error_response(last_status, &last_error);
                }
                continue;
            }
        };

        let prepared = match prepare_passthrough_request(
            &state,
            &app_type,
            &provider,
            path,
            query,
            body.clone(),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                last_error = error;
                last_status = StatusCode::BAD_REQUEST;
                record_request_failed(&state, &last_error).await;
                if is_last {
                    return error_response(last_status, &last_error);
                }
                continue;
            }
        };
        let url = if prepared.codex_chat_conversion.is_some()
            && target
                .base_url
                .trim_end_matches('/')
                .to_ascii_lowercase()
                .ends_with("/chat/completions")
        {
            append_query_to_full_url(&target.base_url, prepared.query.as_deref())
        } else {
            build_url(&target.base_url, &prepared.path, prepared.query.as_deref())
        };
        let codex_chat_conversion = prepared.codex_chat_conversion;
        // Streaming/SSE passthrough must force upstream accept-encoding: identity so
        // the usage-logging tee sees plaintext SSE — a compressed event stream defeats
        // the substring matching and token accounting is silently skipped. Mirrors
        // cc-switch forwarder::force_identity_encoding (request_is_streaming branch).
        let prepared_endpoint = endpoint_with_query(&prepared.path, prepared.query.as_deref());
        let force_identity_encoding = codex_chat_conversion.is_some()
            || is_streaming_request(&prepared_endpoint, &prepared.body, &headers);
        let result = send_upstream(
            &state,
            &method,
            &url,
            &headers,
            &target,
            prepared.body,
            force_identity_encoding,
        )
        .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                let success = status.is_success();
                let _ = state
                    .provider_router
                    .record_result(
                        &provider.id,
                        app_type.as_str(),
                        permit.used_half_open_permit,
                        success,
                        if success {
                            None
                        } else {
                            Some(format!("upstream status {status}"))
                        },
                    )
                    .await;

                if success {
                    record_request_succeeded(
                        &state,
                        &app_type,
                        &provider,
                        should_hot_switch(&current_provider_id_at_start, &provider),
                    )
                    .await;
                    trigger_failover_switch_if_needed(
                        &state,
                        app_type,
                        &provider,
                        &current_provider_id_at_start,
                    );
                    if let Some(conversion) = codex_chat_conversion {
                        return stream_back_codex_chat_converted_with_usage(
                            state.clone(),
                            resp,
                            provider,
                            request_model.clone(),
                            session_id.clone(),
                            request_start,
                            conversion,
                        )
                        .await;
                    }
                    return stream_back_with_usage(
                        state.clone(),
                        resp,
                        app_type,
                        provider,
                        request_model.clone(),
                        session_id.clone(),
                        request_start,
                    )
                    .await;
                }

                let message = format!("upstream status {status}");
                record_request_failed(&state, &message).await;
                if is_retryable_status(status) && !is_last {
                    last_error = message;
                    last_status = status;
                    continue;
                }
                if codex_chat_conversion.is_some() {
                    return stream_back_codex_chat_error(resp).await;
                }
                return stream_back(resp);
            }
            Err(e) => {
                let _ = state
                    .provider_router
                    .record_result(
                        &provider.id,
                        app_type.as_str(),
                        permit.used_half_open_permit,
                        false,
                        Some(e.to_string()),
                    )
                    .await;
                log::warn!(
                    "[Forward] upstream request failed for {}: {e}",
                    provider.name
                );
                last_error = e;
                last_status = StatusCode::BAD_GATEWAY;
                record_request_failed(&state, &last_error).await;
                if !is_last {
                    continue;
                }
            }
        }
    }

    error_response(last_status, &last_error)
}

async fn prepare_passthrough_request(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    path: &str,
    query: Option<&str>,
    body: Bytes,
) -> Result<PreparedPassthroughRequest, String> {
    let mut effective_path = normalize_passthrough_path(app_type, path);
    let effective_query = query.map(ToString::to_string);
    let endpoint = endpoint_with_query(&effective_path, effective_query.as_deref());
    let codex_responses_to_chat = matches!(app_type, AppType::Codex)
        && crate::proxy::providers::should_convert_codex_responses_to_chat(provider, &endpoint);
    if codex_responses_to_chat {
        effective_path = "/chat/completions".to_string();
    }

    let (body, codex_chat_conversion) =
        prepare_passthrough_body(state, app_type, provider, body, codex_responses_to_chat).await?;

    Ok(PreparedPassthroughRequest {
        path: effective_path,
        query: effective_query,
        body,
        codex_chat_conversion,
    })
}

async fn prepare_passthrough_body(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    body: Bytes,
    codex_responses_to_chat: bool,
) -> Result<(Bytes, Option<CodexChatConversion>), String> {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return if codex_responses_to_chat {
            Err("Codex Responses -> Chat conversion requires a JSON request body".to_string())
        } else {
            Ok((body, None))
        };
    };
    let mut codex_chat_conversion = None;

    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => {
            let (mapped, _original_model, _mapped_model) =
                super::model_mapper::apply_model_mapping(value, provider);
            value = if provider.is_github_copilot() {
                crate::proxy::providers::apply_copilot_model_normalization(mapped)
            } else {
                super::model_mapper::strip_one_m_suffix_for_upstream_from_body(mapped)
            };
            let api_format = if provider.is_github_copilot() {
                "openai_chat"
            } else {
                crate::proxy::providers::get_claude_api_format(provider)
            };
            crate::proxy::providers::normalize_anthropic_messages_for_provider(
                &mut value, provider, api_format,
            );
            apply_media_prevention(state, &mut value, provider);
            apply_bedrock_optimizer(state, &mut value, provider);
        }
        AppType::Codex => {
            if codex_responses_to_chat {
                let restored = state.codex_chat_history.enrich_request(&mut value).await;
                if restored > 0 {
                    log::debug!(
                        "[Codex] restored/enriched {restored} cached function call item(s) for Chat upstream"
                    );
                }
                let tool_context =
                    crate::proxy::providers::transform_codex_chat::build_codex_tool_context_from_request(
                        &value,
                    );
                crate::proxy::providers::apply_codex_chat_upstream_model(provider, &mut value);
                let reasoning_config =
                    crate::proxy::providers::resolve_codex_chat_reasoning_config(provider, &value);
                value = crate::proxy::providers::transform_codex_chat::responses_to_chat_completions_with_reasoning(
                    value,
                    reasoning_config.as_ref(),
                )
                .map_err(|error| error.to_string())?;
                codex_chat_conversion = Some(CodexChatConversion { tool_context });
            }
            apply_media_prevention(state, &mut value, provider);
        }
        _ => {}
    }

    let value = prepare_upstream_request_body(value);
    serde_json::to_vec(&value)
        .map(|bytes| (Bytes::from(bytes), codex_chat_conversion))
        .map_err(|error| format!("serialize upstream request body: {error}"))
}

fn normalize_passthrough_path(app_type: &AppType, path: &str) -> String {
    let mut path = match app_type {
        AppType::Codex => path
            .strip_prefix("/codex")
            .filter(|rest| rest.starts_with('/'))
            .unwrap_or(path)
            .to_string(),
        AppType::Claude | AppType::ClaudeDesktop => path
            .strip_prefix("/claude")
            .filter(|rest| rest.starts_with('/'))
            .unwrap_or(path)
            .to_string(),
        _ => path.to_string(),
    };

    while path.contains("/v1/v1") {
        path = path.replace("/v1/v1", "/v1");
    }
    path
}

fn endpoint_with_query(path: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.to_string(),
    }
}

/// Whether the request is streaming / SSE-bound. Mirrors cc-switch
/// `forwarder::is_streaming_request`: a streaming request on the passthrough path
/// must force upstream `accept-encoding: identity` so the usage-logging tee sees
/// plaintext — a compressed event stream defeats the substring matching used to
/// extract usage rows, and token accounting is silently dropped.
fn is_streaming_request(endpoint: &str, body: &[u8], headers: &HeaderMap) -> bool {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if value
            .get("stream")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return true;
        }
    }

    if endpoint.contains("streamGenerateContent") || endpoint.contains("alt=sse") {
        return true;
    }

    headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .map(|accept| accept.contains("text/event-stream"))
        .unwrap_or(false)
}

fn apply_media_prevention(state: &ProxyState, body: &mut Value, provider: &Provider) {
    let config = state.db.get_rectifier_config().unwrap_or_default();
    if !(config.enabled && config.request_media_fallback) {
        return;
    }

    let replaced = super::media_sanitizer::replace_images_for_text_only_model(
        body,
        provider,
        config.request_media_heuristic,
    );
    if replaced > 0 {
        log::info!("[Forward] media fallback preflight replaced {replaced} image block(s)");
    }
}

fn apply_bedrock_optimizer(state: &ProxyState, body: &mut Value, provider: &Provider) {
    let config = state.db.get_optimizer_config().unwrap_or_default();
    if !(config.enabled && is_bedrock_provider(provider)) {
        return;
    }
    super::thinking_optimizer::optimize(body, &config);
    super::cache_injector::inject(body, &config);
}

fn is_bedrock_provider(provider: &Provider) -> bool {
    provider
        .settings_config
        .get("env")
        .and_then(|env| env.get("CLAUDE_CODE_USE_BEDROCK"))
        .and_then(Value::as_str)
        .map(|value| value == "1")
        .unwrap_or(false)
}

fn prepare_upstream_request_body(request_body: Value) -> Value {
    super::json_canonical::canonicalize_value(
        super::body_filter::filter_private_params_with_whitelist(request_body, &[]),
    )
}

async fn record_request_started(state: &ProxyState, app_type: &AppType) {
    let mut status = state.status.write().await;
    status.total_requests += 1;
    status.last_request_at = Some(chrono::Utc::now().to_rfc3339());
    refresh_success_rate(&mut status);

    let app = app_type.as_str();
    if !status
        .active_targets
        .iter()
        .any(|target| target.app_type == app)
    {
        status.active_targets.push(crate::proxy::ActiveTarget {
            app_type: app.to_string(),
            provider_name: String::new(),
            provider_id: String::new(),
        });
    }
}

async fn set_attempt_provider(state: &ProxyState, app_type: &AppType, provider: &Provider) {
    let mut status = state.status.write().await;
    status.current_provider = Some(provider.name.clone());
    status.current_provider_id = Some(provider.id.clone());
    set_active_target(&mut status, app_type, provider);
}

async fn record_request_succeeded(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    failover: bool,
) {
    let mut status = state.status.write().await;
    status.success_requests += 1;
    status.last_error = None;
    status.current_provider = Some(provider.name.clone());
    status.current_provider_id = Some(provider.id.clone());
    set_active_target(&mut status, app_type, provider);
    if failover {
        status.failover_count += 1;
    }
    refresh_success_rate(&mut status);
}

fn set_active_target(
    status: &mut crate::proxy::ProxyStatus,
    app_type: &AppType,
    provider: &Provider,
) {
    let app = app_type.as_str();
    if let Some(target) = status
        .active_targets
        .iter_mut()
        .find(|target| target.app_type == app)
    {
        target.provider_name = provider.name.clone();
        target.provider_id = provider.id.clone();
    } else {
        status.active_targets.push(crate::proxy::ActiveTarget {
            app_type: app.to_string(),
            provider_name: provider.name.clone(),
            provider_id: provider.id.clone(),
        });
    }
}

async fn record_request_failed(state: &ProxyState, error: &str) {
    let mut status = state.status.write().await;
    status.failed_requests += 1;
    status.last_error = Some(error.to_string());
    refresh_success_rate(&mut status);
}

fn refresh_success_rate(status: &mut crate::proxy::ProxyStatus) {
    if status.total_requests > 0 {
        status.success_rate =
            (status.success_requests as f32 / status.total_requests as f32) * 100.0;
    }
}

fn should_hot_switch(current_provider_id_at_start: &Option<String>, provider: &Provider) -> bool {
    current_provider_id_at_start.as_deref() != Some(provider.id.as_str())
}

fn trigger_failover_switch_if_needed(
    state: &ProxyState,
    app_type: AppType,
    provider: &Provider,
    current_provider_id_at_start: &Option<String>,
) {
    if !should_hot_switch(current_provider_id_at_start, provider) {
        return;
    }

    let manager = state.failover_manager.clone();
    let app = app_type.as_str().to_string();
    let provider_id = provider.id.clone();
    let provider_name = provider.name.clone();
    tokio::spawn(async move {
        if let Err(e) = manager.try_switch(&app, &provider_id, &provider_name).await {
            log::warn!("[Failover] hot-switch update failed: {e}");
        }
    });
}

fn is_retryable_status(status: StatusCode) -> bool {
    !matches!(
        status.as_u16(),
        400 | 405 | 406 | 413 | 414 | 415 | 422 | 501
    )
}

fn is_retryable_proxy_error(error: &ProxyError) -> bool {
    match error {
        ProxyError::Timeout(_)
        | ProxyError::ForwardFailed(_)
        | ProxyError::ProviderUnhealthy(_)
        | ProxyError::ConfigError(_)
        | ProxyError::TransformError(_)
        | ProxyError::AuthError(_)
        | ProxyError::StreamIdleTimeout(_) => true,
        ProxyError::UpstreamError { status, .. } => {
            StatusCode::from_u16(*status).map_or(true, is_retryable_status)
        }
        ProxyError::NoAvailableProvider => false,
        _ => false,
    }
}

fn proxy_error_status(error: &ProxyError) -> StatusCode {
    match error {
        ProxyError::UpstreamError { status, .. } => {
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        ProxyError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        ProxyError::NoAvailableProvider | ProxyError::AllProvidersCircuitOpen => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn session_format_for_app(app_type: &AppType) -> &'static str {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => "claude",
        AppType::Codex => "codex",
        other => other.as_str(),
    }
}

fn request_model_for_app(body: Option<&Value>) -> String {
    body.and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .or_else(|| {
            body.and_then(|value| value.get("response"))
                .and_then(|response| response.get("model"))
                .and_then(Value::as_str)
        })
        .unwrap_or("unknown")
        .to_string()
}

fn passthrough_stream_usage_event_filter(app_type: &AppType, data: &str) -> bool {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => {
            data.contains("\"message_start\"") || data.contains("\"message_delta\"")
        }
        AppType::Codex => data.contains("\"response.completed\"") || data.contains("\"usage\""),
        _ => data.contains("\"usage\""),
    }
}

#[allow(clippy::too_many_arguments)]
async fn log_passthrough_usage_from_stream_events(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    request_model: &str,
    session_id: &str,
    events: Vec<Value>,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    status_code: u16,
) {
    let usage = match app_type {
        AppType::Claude | AppType::ClaudeDesktop => TokenUsage::from_claude_stream_events(&events),
        AppType::Codex => TokenUsage::from_codex_stream_events_auto(&events),
        _ => None,
    };

    let Some(usage) = usage.filter(|usage| usage.has_billable_tokens()) else {
        return;
    };
    let model = usage
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| request_model.to_string());

    log_passthrough_usage(
        state,
        app_type,
        provider,
        &model,
        request_model,
        usage,
        latency_ms,
        first_token_ms,
        status_code,
        Some(session_id.to_string()),
        true,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn log_passthrough_usage_from_response(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    request_model: &str,
    session_id: &str,
    response: &Value,
    latency_ms: u64,
    status_code: u16,
    is_streaming: bool,
) {
    let usage = match app_type {
        AppType::Claude | AppType::ClaudeDesktop => TokenUsage::from_claude_response(response),
        AppType::Codex => TokenUsage::from_codex_response_auto(response),
        _ => None,
    };

    let Some(usage) = usage.filter(|usage| usage.has_billable_tokens()) else {
        return;
    };
    let model = usage
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .or_else(|| {
            response
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| request_model.to_string());

    log_passthrough_usage(
        state,
        app_type,
        provider,
        &model,
        request_model,
        usage,
        latency_ms,
        None,
        status_code,
        Some(session_id.to_string()),
        is_streaming,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn log_passthrough_usage(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    response_model: &str,
    request_model: &str,
    usage: TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    status_code: u16,
    session_id: Option<String>,
    is_streaming: bool,
) {
    let logger = crate::proxy::usage::logger::UsageLogger::new(&state.db);
    let (multiplier, pricing_model_source) = logger
        .resolve_pricing_config(&provider.id, app_type.as_str())
        .await;
    let pricing_model = if pricing_model_source == PRICING_SOURCE_REQUEST {
        request_model
    } else {
        response_model
    };
    let provider_type = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.clone());
    let request_id = usage.dedup_request_id();

    if let Err(error) = logger.log_with_calculation(
        request_id,
        provider.id.clone(),
        app_type.as_str().to_string(),
        response_model.to_string(),
        request_model.to_string(),
        pricing_model.to_string(),
        usage,
        multiplier,
        latency_ms,
        first_token_ms,
        status_code,
        session_id,
        provider_type,
        is_streaming,
    ) {
        log::warn!("[USG-001] passthrough usage log failed: {error}");
    }
}

async fn send_upstream(
    state: &ProxyState,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    target: &UpstreamTarget,
    body: bytes::Bytes,
    force_identity_encoding: bool,
) -> Result<reqwest::Response, String> {
    let mut req = state.http_client.request(method.clone(), url);

    // Copy through client headers except hop-by-hop and auth headers.
    for (name, value) in headers.iter() {
        let name_str = name.as_str().to_ascii_lowercase();
        if STRIP_REQUEST_HEADERS.contains(&name_str.as_str()) {
            continue;
        }
        // On the Codex Responses→Chat conversion path we must decode + re-parse
        // the upstream response, so drop the client's accept-encoding and force
        // identity below rather than letting a compressed body reach the parser.
        if force_identity_encoding && name_str == "accept-encoding" {
            continue;
        }
        // Drop placeholder-bearing values defensively.
        if value
            .to_str()
            .map(|v| v == PROXY_TOKEN_PLACEHOLDER)
            .unwrap_or(false)
        {
            continue;
        }
        req = req.header(name.clone(), value.clone());
    }
    if force_identity_encoding {
        req = req.header("accept-encoding", "identity");
    }

    // Inject the real credential.
    if let Some(token) = target.token.as_deref() {
        match target.strategy {
            AuthStrategy::Bearer => {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            AuthStrategy::ApiKey => {
                req = req.header("x-api-key", token);
            }
        }
    }

    req = if matches!(method, &Method::GET | &Method::HEAD) {
        req.body(Bytes::new())
    } else {
        req.body(body)
    };

    req.send().await.map_err(|e| e.to_string())
}

/// Build the streaming response back to the client.
fn stream_back(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);

    if let Some(headers) = builder.headers_mut() {
        for (name, value) in upstream.headers().iter() {
            let lname = name.as_str().to_ascii_lowercase();
            if STRIP_RESPONSE_HEADERS.contains(&lname.as_str()) {
                continue;
            }
            headers.insert(name.clone(), value.clone());
        }
    }

    // Stream the body chunk-by-chunk (works for both SSE and non-stream).
    let stream = upstream.bytes_stream();
    let body = Body::from_stream(stream);

    builder.body(body).unwrap_or_else(|_| {
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "build response failed")
    })
}

async fn stream_back_with_usage(
    state: ProxyState,
    upstream: reqwest::Response,
    app_type: AppType,
    provider: Provider,
    request_model: String,
    session_id: String,
    start: std::time::Instant,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let is_sse = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|content_type| content_type.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        let stream = upstream.bytes_stream();
        let body = Body::from_stream(passthrough_stream_with_usage_logging(
            state,
            stream,
            app_type,
            provider,
            request_model,
            session_id,
            status.as_u16(),
            start,
        ));
        return build_response_from_headers(status, &headers, body);
    }

    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("read upstream response failed: {error}"),
            );
        }
    };

    // 用量统计仅解析解压后的副本；透传给客户端的仍是保头的原始字节。压缩的
    // 非 SSE 响应若不先解压，serde_json 解析必然失败、token 计费被静默跳过。
    let parse_bytes = decompressed_for_parse(&headers, &bytes);
    if let Ok(json) = serde_json::from_slice::<Value>(&parse_bytes) {
        log_passthrough_usage_from_response(
            &state,
            &app_type,
            &provider,
            &request_model,
            &session_id,
            &json,
            start.elapsed().as_millis() as u64,
            status.as_u16(),
            false,
        )
        .await;
    }
    drop(parse_bytes);

    build_response_from_headers(status, &headers, Body::from(bytes))
}

async fn stream_back_codex_chat_converted_with_usage(
    state: ProxyState,
    upstream: reqwest::Response,
    provider: Provider,
    request_model: String,
    session_id: String,
    start: std::time::Instant,
    conversion: CodexChatConversion,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let is_sse = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|content_type| content_type.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        let converted =
            crate::proxy::providers::streaming_codex_chat::create_responses_sse_stream_from_chat_with_context(
                upstream.bytes_stream(),
                conversion.tool_context,
            );
        let converted = crate::proxy::providers::codex_chat_history::record_responses_sse_stream(
            converted,
            state.codex_chat_history.clone(),
        );
        let body = Body::from_stream(passthrough_stream_with_usage_logging(
            state,
            converted,
            AppType::Codex,
            provider,
            request_model,
            session_id,
            status.as_u16(),
            start,
        ));
        return build_response_from_headers(status, &codex_responses_headers(&headers, true), body);
    }

    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("read upstream response failed: {error}"),
            );
        }
    };
    // accept-encoding: identity 已在上游请求强制，正常拿到明文；仍保留解压兜底，
    // 以防上游忽略 identity 而压缩响应体，否则 JSON/SSE 解析会失败。
    let parse_bytes = decompressed_for_parse(&headers, &bytes);
    let converted = if status.is_success() {
        let parsed = match serde_json::from_slice::<Value>(&parse_bytes) {
            Ok(json) => Ok(json),
            // 与 Claude 侧对称的兜底嗅探（#2234）：上游对 stream:false 返回未标记
            // Content-Type 的 SSE 体时按 Chat SSE 聚合成单个 JSON 再走既有转换器。
            Err(error) => {
                let body_str = String::from_utf8_lossy(&parse_bytes);
                if body_looks_like_sse(&body_str) {
                    log::warn!(
                        "[Codex] 上游对非流请求返回未标记的 SSE 体，按 Chat SSE 聚合兜底"
                    );
                    chat_sse_to_response_value(&body_str)
                } else {
                    Err(ProxyError::TransformError(format!(
                        "parse chat completion response: {error}"
                    )))
                }
            }
        };
        parsed.and_then(|json| {
            crate::proxy::providers::transform_codex_chat::chat_completion_to_response_with_context(
                json,
                &conversion.tool_context,
            )
        })
    } else {
        let upstream_json = serde_json::from_slice::<Value>(&parse_bytes).ok();
        Ok(
            crate::proxy::providers::transform_codex_chat::chat_error_to_response_error(
                upstream_json.as_ref(),
            ),
        )
    };
    drop(parse_bytes);

    let converted = match converted {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("convert chat response failed: {error}"),
            );
        }
    };
    if status.is_success() {
        state.codex_chat_history.record_response(&converted).await;
    }

    log_passthrough_usage_from_response(
        &state,
        &AppType::Codex,
        &provider,
        &request_model,
        &session_id,
        &converted,
        start.elapsed().as_millis() as u64,
        status.as_u16(),
        false,
    )
    .await;

    match serde_json::to_vec(&converted) {
        Ok(bytes) => build_response_from_headers(
            status,
            &codex_responses_headers(&headers, false),
            Body::from(bytes),
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("serialize converted response failed: {error}"),
        ),
    }
}

async fn stream_back_codex_chat_error(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("read upstream response failed: {error}"),
            );
        }
    };
    // 与成功路径对称：identity 已被强制，但上游可能无视并压缩错误体，解压兜底
    // 后再解析，否则错误细节会退化成通用错误。
    let parse_bytes = decompressed_for_parse(&headers, &bytes);
    let json = serde_json::from_slice::<Value>(&parse_bytes).ok();
    let converted =
        crate::proxy::providers::transform_codex_chat::chat_error_to_response_error(json.as_ref());
    match serde_json::to_vec(&converted) {
        Ok(bytes) => build_response_from_headers(
            status,
            &codex_responses_headers(&headers, false),
            Body::from(bytes),
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("serialize converted error failed: {error}"),
        ),
    }
}

fn codex_responses_headers(headers: &HeaderMap, is_sse: bool) -> HeaderMap {
    let mut next = headers.clone();
    next.remove("content-length");
    // 该路径的 body 一律重建为明文（转换后的 JSON / 重编码 SSE）；上游若无视
    // identity 返回压缩体，残留的 content-encoding 会让客户端把明文当压缩字节解。
    next.remove("content-encoding");
    next.insert(
        "content-type",
        if is_sse {
            "text/event-stream"
        } else {
            "application/json"
        }
        .parse()
        .unwrap(),
    );
    next
}

fn build_response_from_headers(status: StatusCode, headers: &HeaderMap, body: Body) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(out_headers) = builder.headers_mut() {
        for (name, value) in headers.iter() {
            let lname = name.as_str().to_ascii_lowercase();
            if STRIP_RESPONSE_HEADERS.contains(&lname.as_str()) {
                continue;
            }
            out_headers.insert(name.clone(), value.clone());
        }
    }
    builder.body(body).unwrap_or_else(|_| {
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "build response failed")
    })
}

#[allow(clippy::too_many_arguments)]
fn passthrough_stream_with_usage_logging<S, E>(
    state: ProxyState,
    stream: S,
    app_type: AppType,
    provider: Provider,
    request_model: String,
    session_id: String,
    status_code: u16,
    start: std::time::Instant,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    async_stream::stream! {
        let logging_enabled = state
            .config
            .try_read()
            .map(|config| config.enable_logging)
            .unwrap_or(true);
        let mut buffer = String::new();
        let mut utf8_remainder = Vec::new();
        let mut events: Vec<Value> = Vec::new();
        let mut first_token_ms: Option<u64> = None;

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if logging_enabled {
                        super::sse::append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                        while let Some(block) = super::sse::take_sse_block(&mut buffer) {
                            if block.trim().is_empty() {
                                continue;
                            }
                            for line in block.lines() {
                                let Some(data) = super::sse::strip_sse_field(line, "data") else {
                                    continue;
                                };
                                if data.trim() == "[DONE]" || !passthrough_stream_usage_event_filter(&app_type, data) {
                                    continue;
                                }
                                let Ok(event) = serde_json::from_str::<Value>(data) else {
                                    continue;
                                };
                                if first_token_ms.is_none() {
                                    first_token_ms = Some(start.elapsed().as_millis() as u64);
                                }
                                events.push(event);
                            }
                        }
                    }
                    yield Ok(bytes);
                }
                Err(error) => {
                    yield Err(error);
                    break;
                }
            }
        }

        if logging_enabled {
            log_passthrough_usage_from_stream_events(
                &state,
                &app_type,
                &provider,
                &request_model,
                &session_id,
                events,
                start.elapsed().as_millis() as u64,
                first_token_ms,
                status_code,
            )
            .await;
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "type": "proxy_error",
            "message": message,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(message.to_string())))
}

/// Decompress a compressed client request body before it is parsed / forwarded.
///
/// reqwest 的自动解压已禁用（为了透传 accept-encoding），需要手动解压请求侧的
/// 压缩体。解压成功后剥掉已失真的实体头（content-encoding / content-length /
/// transfer-encoding）——转发层会基于明文 body 重新生成正确的头。返回
/// `Err(encoding)` 表示编码不受支持、调用方应直接拒绝（400）；无 content-encoding
/// 时原样返回。镜像 cc-switch handlers.rs `decode_codex_request_body`。
fn decode_request_body(mut headers: HeaderMap, body: Bytes) -> Result<(HeaderMap, Bytes), String> {
    let Some(encoding) = content_encoding::get_content_encoding(&headers) else {
        return Ok((headers, body));
    };
    if !content_encoding::is_supported_content_encoding(&encoding) {
        return Err(encoding);
    }
    match content_encoding::decompress_body(&encoding, &body) {
        Ok(Some(decompressed)) => {
            headers.remove("content-encoding");
            headers.remove("content-length");
            headers.remove("transfer-encoding");
            log::debug!("[Forward] 解压请求体: content-encoding={encoding}");
            Ok((headers, Bytes::from(decompressed)))
        }
        // is_supported_content_encoding 已确保受支持，正常不会返回 None；
        // 防御性兜底：宁可拒绝，也不能把压缩字节当 JSON 透传下去。
        Ok(None) => Err(encoding),
        Err(error) => {
            log::warn!("[Forward] 请求体解压失败 (content-encoding={encoding}): {error}");
            Err(encoding)
        }
    }
}

/// 为解析（用量统计 / 格式转换）而解压响应体，**绝不改动**透传给客户端的原始
/// 字节。无 content-encoding、编码不受支持或解压失败时借用原始字节，让调用方
/// 的 JSON 解析按既有逻辑（成功或降级）继续。
pub(super) fn decompressed_for_parse<'a>(
    headers: &HeaderMap,
    bytes: &'a [u8],
) -> std::borrow::Cow<'a, [u8]> {
    let Some(encoding) = content_encoding::get_content_encoding(headers) else {
        return std::borrow::Cow::Borrowed(bytes);
    };
    match content_encoding::decompress_body(&encoding, bytes) {
        Ok(Some(decompressed)) => std::borrow::Cow::Owned(decompressed),
        _ => std::borrow::Cow::Borrowed(bytes),
    }
}

// ============================================================================
// 未标记 SSE 兜底聚合（#2234）
//
// 部分网关对 `stream:false` 请求强制返回 SSE 体，却把 Content-Type 标成
// application/json（或不标），使 header 层的 is_sse 检查失效、直接 JSON 解析失败。
// 这里在 JSON 解析失败后按 SSE 聚合成单个 JSON，再喂给既有非流转换器，客户端
// 仍收到合法 JSON、非流语义不变。镜像 cc-switch handlers.rs 的同名函数。
// ============================================================================

/// 判断响应体是否"看起来像" SSE 文本（#2234 兜底嗅探）。
///
/// 仅在 JSON 解析已失败后调用：合法 JSON 不可能以这些前缀开头，误判面为零。
/// 覆盖 SSE 规范的全部四种字段行；包含 ":" 是因为 OpenRouter 等会在流前发
/// `: PROCESSING` 注释行。
pub(super) fn body_looks_like_sse(body: &str) -> bool {
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    ["data:", "event:", "id:", "retry:", ":"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// 从 SSE chunk 的 error 字段提取可报告的错误消息。占位形状（空对象、空消息、
/// false、空字符串等，常见于 OpenAI 兼容网关每 chunk 附带的 error 字段）返回
/// None——不应据此判定整条流失败（否则会把成功流误杀成 422）。
fn error_event_message(error: &Value) -> Option<String> {
    if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
        return (!msg.is_empty()).then(|| msg.to_string());
    }
    if let Some(s) = error.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    None
}

/// 解析单个 SSE 块的 event 名与 data 负载（多行 data 按规范以 \n 连接）。
/// 行首允许前导空白后再匹配字段名——与 body_looks_like_sse 的 trim 宽容度对齐。
/// 返回 None 表示无 data 行。
fn sse_block_parts(block: &str) -> Option<(String, String)> {
    let mut event_name = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.trim_start();
        if let Some(evt) = super::sse::strip_sse_field(line, "event") {
            event_name = evt.trim().to_string();
        } else if let Some(d) = super::sse::strip_sse_field(line, "data") {
            data_lines.push(d);
        }
    }
    (!data_lines.is_empty()).then(|| (event_name, data_lines.join("\n")))
}

/// envelope 字段是否"有意义"：过滤 null、空串与数值 0（含浮点 0.0——Azure
/// content-filter 前置块的占位值），避免占位值抢先冻结 id/model/created。
fn envelope_value_meaningful(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}

/// 合并单条 tool_calls 增量到按 index 聚合的 BTreeMap：OpenAI 流式把 id/name 放
/// 首个增量、arguments 分片下发，按 delta.index 定位目标；缺 index 时退到所在数组
/// 中的位置（message 形态的完整 tool_calls 常不带 index，按 0 会互相覆盖）。
fn merge_tool_call_delta(
    tool_calls: &mut std::collections::BTreeMap<usize, Value>,
    delta: &Value,
    fallback_index: usize,
) {
    let index = delta
        .get("index")
        .and_then(|i| i.as_u64())
        .map(|i| i as usize)
        .unwrap_or(fallback_index);
    let target = tool_calls.entry(index).or_insert_with(|| {
        serde_json::json!({
            "id": "",
            "type": "function",
            "function": {"name": "", "arguments": ""}
        })
    });
    if let Some(v) = delta
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        target["id"] = serde_json::json!(v);
    }
    if let Some(func) = delta.get("function") {
        if let Some(name) = func
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            target["function"]["name"] = serde_json::json!(name);
        }
        // arguments：string 直接拼接；object/array 序列化后拼接——非流 message
        // 快照常把 arguments 作对象回传（OpenAI 兼容偏差），只认 string 会丢参数。
        match func.get("arguments") {
            Some(Value::String(args)) => {
                if let Some(existing) = target["function"]["arguments"].as_str() {
                    target["function"]["arguments"] = serde_json::json!(format!("{existing}{args}"));
                }
            }
            Some(v @ (Value::Object(_) | Value::Array(_))) => {
                let serialized = serde_json::to_string(v).unwrap_or_default();
                if let Some(existing) = target["function"]["arguments"].as_str() {
                    target["function"]["arguments"] =
                        serde_json::json!(format!("{existing}{serialized}"));
                }
            }
            _ => {}
        }
    }
}

/// 把 Responses 流式 SSE 聚合为单个 response JSON（#2234 兜底）。
pub(super) fn responses_sse_to_response_value(body: &str) -> Result<Value, ProxyError> {
    let mut buffer = body.trim_start_matches('\u{feff}').to_string();
    let mut completed_response: Option<Value> = None;
    let mut output_items = Vec::new();

    // strict=false 用于残余尾块：截断的半截 JSON 忽略而非报错，避免破坏
    // 已聚合好的完整响应。
    let mut process_block = |block: &str, strict: bool| -> Result<(), ProxyError> {
        // 已拿到 completed 后残余尾块整体跳过，避免残余里的 response.failed
        // 把成功响应翻成失败。
        if !strict && completed_response.is_some() {
            return Ok(());
        }
        let mut event_name = "";
        let mut data_lines: Vec<&str> = Vec::new();

        for line in block.lines() {
            let line = line.trim_start();
            if let Some(evt) = super::sse::strip_sse_field(line, "event") {
                event_name = evt.trim();
            } else if let Some(d) = super::sse::strip_sse_field(line, "data") {
                data_lines.push(d);
            }
        }

        if data_lines.is_empty() {
            return Ok(());
        }

        let data_str = data_lines.join("\n");
        if data_str.trim() == "[DONE]" {
            return Ok(());
        }

        let data: Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(_) if !strict => return Ok(()),
            Err(e) => {
                return Err(ProxyError::TransformError(format!(
                    "Failed to parse upstream SSE event: {e}"
                )))
            }
        };

        match event_name {
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                completed_response = Some(data.get("response").cloned().unwrap_or(data));
            }
            "response.failed" => {
                let message = data
                    .pointer("/response/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("response.failed event received");
                return Err(ProxyError::TransformError(message.to_string()));
            }
            _ => {}
        }
        Ok(())
    };

    while let Some(block) = super::sse::take_sse_block(&mut buffer) {
        process_block(&block, true)?;
    }
    // 最后一个事件后可能没有空行分隔（错标 SSE 兜底/非规范上游常见）：
    // 残余 buffer 当最后一块处理，否则尾部的 response.completed 会被丢掉。
    process_block(&buffer, false)?;

    let mut response = completed_response.ok_or_else(|| {
        ProxyError::TransformError("No response.completed event in upstream SSE".to_string())
    })?;

    if !output_items.is_empty() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("output".to_string(), Value::Array(output_items));
        } else {
            return Err(ProxyError::TransformError(
                "response.completed payload is not an object".to_string(),
            ));
        }
    }

    Ok(response)
}

/// 把 Chat Completions 流式 SSE 聚合为单个 chat.completion JSON（#2234 兜底）。
///
/// 增量合并语义与 providers/streaming.rs 对齐：tool_calls 按 delta.index 定位，
/// id/name 出现即覆盖、arguments 字符串拼接；reasoning 各形态经公共提取器并入
/// 同一累加器；finish_reason 首个非 null 即锁定。
pub(super) fn chat_sse_to_response_value(body: &str) -> Result<Value, ProxyError> {
    // 剥 BOM：嗅探器接受 BOM 开头，但 strip_sse_field 按行首精确匹配，
    // 不剥会让首个 data 行静默丢失。
    let mut buffer = body.trim_start_matches('\u{feff}').to_string();

    let mut id = Value::Null;
    let mut created = Value::Null;
    let mut model = Value::Null;
    let mut content = String::new();
    let mut reasoning_content = String::new();
    // tool_calls 以 BTreeMap 按 index 聚合：上游可控的 index（u64）不会 densify
    // 数组——旧的 `while len() <= index { push }` 写法遇到超大 index 会 OOM。
    let mut tool_calls: std::collections::BTreeMap<usize, Value> =
        std::collections::BTreeMap::new();
    let mut finish_reason = Value::Null;
    let mut usage = Value::Null;
    let mut saw_choice = false;
    let mut saw_done = false;

    let mut process_event =
        |event_name: &str, data_str: &str, strict: bool| -> Result<(), ProxyError> {
            let trimmed = data_str.trim();
            if trimmed == "[DONE]" {
                saw_done = true;
                return Ok(());
            }
            if trimmed.is_empty() {
                return Ok(());
            }
            let chunk: Value = match serde_json::from_str(data_str) {
                Ok(v) => v,
                Err(_) if !strict => return Ok(()),
                Err(e) => {
                    return Err(ProxyError::TransformError(format!(
                        "Failed to parse upstream SSE chunk: {e}"
                    )))
                }
            };

            // `event: error` 事件：错误由事件名标记，data 体未必有 error 键。
            if event_name.eq_ignore_ascii_case("error") {
                let message = chunk
                    .get("error")
                    .and_then(error_event_message)
                    .or_else(|| error_event_message(&chunk))
                    .unwrap_or_else(|| "upstream error event in SSE stream".to_string());
                return Err(ProxyError::TransformError(message));
            }
            // 网关把错误作为普通 data chunk 下发：仅在 error 含可报告消息时判失败。
            if let Some(message) = chunk
                .get("error")
                .filter(|e| !e.is_null())
                .and_then(error_event_message)
            {
                return Err(ProxyError::TransformError(message));
            }

            // 首个"有意义"的值锁定 envelope（过滤 Azure content-filter 的占位值）。
            for (slot, key) in [
                (&mut id, "id"),
                (&mut created, "created"),
                (&mut model, "model"),
            ] {
                if slot.is_null() {
                    if let Some(v) = chunk.get(key).filter(|v| envelope_value_meaningful(v)) {
                        *slot = v.clone();
                    }
                }
            }
            // OpenAI 语义：usage 只在最终 chunk 非 null。
            if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                usage = u.clone();
            }

            // 代理上下文只存在单选择（n=1），仅聚合 index==0 的 choice。
            let Some(choice) = chunk
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|ch| ch.get("index").and_then(|i| i.as_u64()).unwrap_or(0) == 0)
                })
            else {
                return Ok(());
            };

            saw_choice = true;

            // finish_reason 首个非 null 即锁定（first-wins）。
            if finish_reason.is_null() {
                if let Some(fr) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                    finish_reason = fr.clone();
                }
            }
            // payload 选择：正常增量走 delta；假流式中转会把完整 chat.completion
            // 包成单事件（message 而非 delta）。delta 为空对象且存在 message 时改用
            // message 快照（覆盖此前累计的增量），否则内容被静默丢弃。
            let delta_nonempty = choice
                .get("delta")
                .and_then(|d| d.as_object())
                .is_some_and(|o| !o.is_empty());
            let (payload, is_full_message) = if delta_nonempty {
                (choice.get("delta").unwrap(), false)
            } else if let Some(message) = choice.get("message") {
                (message, true)
            } else if let Some(delta) = choice.get("delta") {
                (delta, false)
            } else {
                return Ok(());
            };
            if is_full_message {
                content.clear();
                reasoning_content.clear();
                tool_calls.clear();
            }
            match payload.get("content") {
                Some(Value::String(text)) => content.push_str(text),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            content.push_str(text);
                        } else if let Some(refusal) = part.get("refusal").and_then(|r| r.as_str()) {
                            content.push_str(refusal);
                        }
                    }
                }
                _ => {}
            }
            // refusal：OpenAI 官方拒绝形态（delta.refusal / message.refusal 字符串）。
            if let Some(refusal) = payload.get("refusal").and_then(|r| r.as_str()) {
                content.push_str(refusal);
            }
            // reasoning 字段穷举提取复用 codex_chat_common，避免第三份手写实现漏档。
            if let Some(text) =
                crate::proxy::providers::codex_chat_common::extract_reasoning_field_text(payload)
            {
                reasoning_content.push_str(&text);
            }
            if let Some(deltas) = payload.get("tool_calls").and_then(|t| t.as_array()) {
                for (pos, tc) in deltas.iter().enumerate() {
                    merge_tool_call_delta(&mut tool_calls, tc, pos);
                }
            } else if let Some(fc) = payload.get("function_call").filter(|v| !v.is_null()) {
                // legacy function_call（弃用但仍有中转回传）→ 当单个 tool_call。
                let synthetic = serde_json::json!({
                    "index": 0,
                    "id": fc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    "type": "function",
                    "function": fc,
                });
                merge_tool_call_delta(&mut tool_calls, &synthetic, 0);
            }
            Ok(())
        };

    while let Some(block) = super::sse::take_sse_block(&mut buffer) {
        if let Some((event, data)) = sse_block_parts(&block) {
            process_event(&event, &data, true)?;
        }
    }
    // 最后一个事件后可能没有空行分隔：残余 buffer 当最后一块处理（strict=false）。
    if let Some((event, data)) = sse_block_parts(&buffer) {
        process_event(&event, &data, false)?;
    }

    if !saw_choice {
        return Err(ProxyError::TransformError(
            "No chat completion choices in upstream SSE".to_string(),
        ));
    }
    // 完成性守卫：缺少 finish_reason 与 [DONE] 两个完成证据时按截断处理。
    if finish_reason.is_null() && !saw_done {
        return Err(ProxyError::TransformError(
            "Upstream SSE stream appears truncated (no finish_reason or [DONE] marker)".to_string(),
        ));
    }

    // tool_calls 终结化：全空壳丢弃；缺 id/name 的按原始 index 回填合成值。
    let tool_calls: Vec<Value> = tool_calls
        .into_iter()
        .filter(|(_, tc)| {
            tc["id"].as_str().is_some_and(|s| !s.is_empty())
                || tc["function"]["name"].as_str().is_some_and(|s| !s.is_empty())
                || tc["function"]["arguments"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        })
        .map(|(index, mut tc)| {
            if tc["id"].as_str().is_none_or(str::is_empty) {
                tc["id"] = serde_json::json!(format!("tool_call_{index}"));
            }
            if tc["function"]["name"].as_str().is_none_or(str::is_empty) {
                tc["function"]["name"] = serde_json::json!("unknown_tool");
            }
            tc
        })
        .collect();

    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), serde_json::json!("assistant"));
    message.insert("content".to_string(), serde_json::json!(content));
    if !reasoning_content.is_empty() {
        message.insert(
            "reasoning_content".to_string(),
            serde_json::json!(reasoning_content),
        );
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    // 上游未回传有效 id 时合成 UUID，避免下游 dedup 退化为常量键全局碰撞。
    let id = if envelope_value_meaningful(&id) {
        id
    } else {
        serde_json::json!(uuid::Uuid::new_v4().to_string())
    };

    let mut response = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
    });
    if !usage.is_null() {
        response["usage"] = usage;
    }
    Ok(response)
}

fn build_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    match query {
        Some(q) if !q.is_empty() => format!("{base}{path}?{q}"),
        _ => format!("{base}{path}"),
    }
}

fn append_query_to_full_url(base_url: &str, query: Option<&str>) -> String {
    match query.filter(|query| !query.is_empty()) {
        Some(query) if base_url.contains('?') => format!("{base_url}&{query}"),
        Some(query) => format!("{base_url}?{query}"),
        None => base_url.to_string(),
    }
}

/// Resolve the upstream base URL + credential for `provider` under `app_type`.
fn resolve_upstream(app_type: &AppType, provider: &Provider) -> Result<UpstreamTarget, String> {
    match app_type {
        AppType::Claude => resolve_claude(provider),
        AppType::Codex => resolve_codex(provider),
        other => Err(format!(
            "{} does not support proxy forwarding",
            other.as_str()
        )),
    }
}

fn settings_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn env_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get("env")
        .and_then(|env| env.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_placeholder(token: &str) -> bool {
    token == PROXY_TOKEN_PLACEHOLDER
}

fn resolve_claude(provider: &Provider) -> Result<UpstreamTarget, String> {
    let cfg = &provider.settings_config;

    let base_url = env_str(cfg, "ANTHROPIC_BASE_URL")
        .or_else(|| settings_str(cfg, "base_url"))
        .or_else(|| settings_str(cfg, "baseURL"))
        .or_else(|| settings_str(cfg, "apiEndpoint"))
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| "Claude provider missing base_url".to_string())?;

    // ANTHROPIC_AUTH_TOKEN -> Bearer; ANTHROPIC_API_KEY -> x-api-key.
    let (token, strategy) =
        match env_str(cfg, "ANTHROPIC_AUTH_TOKEN").filter(|t| !is_placeholder(t)) {
            Some(t) => (Some(t.to_string()), AuthStrategy::Bearer),
            None => match env_str(cfg, "ANTHROPIC_API_KEY").filter(|t| !is_placeholder(t)) {
                Some(t) => (Some(t.to_string()), AuthStrategy::ApiKey),
                None => (
                    settings_str(cfg, "api_key")
                        .filter(|t| !is_placeholder(t))
                        .map(str::to_string),
                    AuthStrategy::ApiKey,
                ),
            },
        };

    Ok(UpstreamTarget {
        base_url,
        token,
        strategy,
    })
}

fn resolve_codex(provider: &Provider) -> Result<UpstreamTarget, String> {
    let cfg = &provider.settings_config;

    let mut base_url = settings_str(cfg, "base_url")
        .or_else(|| settings_str(cfg, "baseURL"))
        .map(str::to_string);

    if base_url.is_none() {
        if let Some(config) = cfg.get("config") {
            if let Some(url) = config.get("base_url").and_then(Value::as_str) {
                base_url = Some(url.to_string());
            } else if let Some(config_str) = config.as_str() {
                base_url = parse_toml_base_url(config_str);
            }
        }
    }

    let base_url = base_url
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| "Codex provider missing base_url".to_string())?;

    // OPENAI_API_KEY (auth.json) -> Authorization: Bearer.
    let token = cfg
        .get("auth")
        .and_then(|auth| auth.get("OPENAI_API_KEY"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty() && !is_placeholder(t))
        .or_else(|| settings_str(cfg, "api_key").filter(|t| !is_placeholder(t)))
        .map(str::to_string);

    Ok(UpstreamTarget {
        base_url,
        token,
        strategy: AuthStrategy::Bearer,
    })
}

fn parse_toml_base_url(config_str: &str) -> Option<String> {
    if let Some(start) = config_str.find("base_url = \"") {
        let rest = &config_str[start + 12..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    if let Some(start) = config_str.find("base_url = '") {
        let rest = &config_str[start + 12..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn gzip(payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn decode_request_body_decompresses_gzip_and_strips_entity_headers() {
        // 回归 (#gzip request body through transform path)：客户端发 gzip 压缩的
        // JSON 请求体，解压后转发/转换层的 serde_json::from_slice 才能成功；同时
        // 已失真的实体头必须被剥掉，否则转发的明文 body 会带上错误的 content-encoding。
        let payload = br#"{"model":"claude-3","stream":false}"#;
        let compressed = gzip(payload);

        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert(
            "content-length",
            HeaderValue::from_str(&compressed.len().to_string()).unwrap(),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let (out_headers, out_body) =
            decode_request_body(headers, Bytes::from(compressed)).expect("supported encoding");

        assert_eq!(out_body.as_ref(), payload);
        assert!(out_headers.get("content-encoding").is_none());
        assert!(out_headers.get("content-length").is_none());
        // 非实体头保持不变
        assert_eq!(
            out_headers.get("content-type").unwrap(),
            "application/json"
        );
        // 解压后可被 JSON 解析
        let parsed: Value = serde_json::from_slice(&out_body).unwrap();
        assert_eq!(parsed["model"], "claude-3");
    }

    #[test]
    fn decode_request_body_passes_through_uncompressed() {
        let payload = Bytes::from_static(br#"{"ok":true}"#);
        let headers = HeaderMap::new();
        let (out_headers, out_body) =
            decode_request_body(headers, payload.clone()).expect("no encoding");
        assert_eq!(out_body, payload);
        assert!(out_headers.get("content-encoding").is_none());
    }

    #[test]
    fn decode_request_body_rejects_unsupported_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("snappy"));
        let err = decode_request_body(headers, Bytes::from_static(b"\x00\x01"))
            .expect_err("unsupported must reject");
        assert_eq!(err, "snappy");
    }

    #[test]
    fn body_looks_like_sse_detects_event_stream_prefixes() {
        assert!(body_looks_like_sse("data: {\"x\":1}\n\n"));
        assert!(body_looks_like_sse("event: message\ndata: {}\n\n"));
        assert!(body_looks_like_sse(": PROCESSING\n\n"));
        assert!(body_looks_like_sse("\u{feff}data: {}\n\n"));
        assert!(!body_looks_like_sse("{\"object\":\"chat.completion\"}"));
        assert!(!body_looks_like_sse("<html>blocked</html>"));
    }

    #[test]
    fn chat_sse_fallback_aggregates_unlabeled_stream() {
        // 回归 (#2234)：非流请求收到未标记 Content-Type 的 Chat SSE 体，
        // 按 SSE 聚合成单个 chat.completion JSON。
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        assert!(body_looks_like_sse(body));
        let value = chat_sse_to_response_value(body).expect("aggregates");
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["id"], "chatcmpl-1");
        assert_eq!(value["choices"][0]["message"]["content"], "Hello");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["completion_tokens"], 2);
    }

    #[test]
    fn chat_sse_fallback_rejects_truncated_stream() {
        // 无 finish_reason 且无 [DONE]：按截断处理，避免半截内容伪装成成功。
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        assert!(chat_sse_to_response_value(body).is_err());
    }

    #[test]
    fn responses_sse_fallback_aggregates_completed_event() {
        let body = concat!(
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"status\":\"completed\"}}\n\n",
        );
        let value = responses_sse_to_response_value(body).expect("aggregates");
        assert_eq!(value["id"], "resp_1");
        assert_eq!(value["output"][0]["type"], "message");
    }

    #[test]
    fn decompressed_for_parse_gzip_yields_parseable_json() {
        // 回归 (Finding 7)：上游忽略 accept-encoding: identity 而 gzip 压缩 stream:false
        // 响应体时，non_stream_back_transformed 的 serde_json 解析必须走解压兜底才能成功；
        // 否则用量统计 / 格式转换被静默跳过。
        let payload = br#"{"object":"chat.completion","usage":{"prompt_tokens":5}}"#;
        let compressed = gzip(payload);

        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));

        // 原始压缩字节解析必然失败（这正是 finding 描述的漏计费根因）。
        assert!(serde_json::from_slice::<Value>(&compressed).is_err());

        let parse_bytes = decompressed_for_parse(&headers, &compressed);
        let parsed: Value = serde_json::from_slice(&parse_bytes).expect("decompressed parses");
        assert_eq!(parsed["object"], "chat.completion");
        assert_eq!(parsed["usage"]["prompt_tokens"], 5);
    }

    #[test]
    fn decompressed_for_parse_gzip_sse_body_passes_sniff() {
        // 兜底嗅探同样需要先解压：gzip 压缩的未标记 SSE 体解压后 body_looks_like_sse
        // 才能命中，进而走 SSE 聚合。压缩态直接嗅探会漏判。
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let compressed = gzip(sse);

        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));

        assert!(!body_looks_like_sse(&String::from_utf8_lossy(&compressed)));

        let parse_bytes = decompressed_for_parse(&headers, &compressed);
        assert!(body_looks_like_sse(&String::from_utf8_lossy(&parse_bytes)));
    }

    #[test]
    fn decompressed_for_parse_plaintext_borrows_untouched() {
        let payload = br#"{"ok":true}"#;
        let headers = HeaderMap::new();
        let parse_bytes = decompressed_for_parse(&headers, payload);
        assert_eq!(parse_bytes.as_ref(), payload);
    }

    #[test]
    fn is_streaming_request_detects_stream_body_flag() {
        let headers = HeaderMap::new();
        assert!(is_streaming_request(
            "/v1/messages",
            br#"{"model":"claude-3","stream":true}"#,
            &headers,
        ));
        assert!(!is_streaming_request(
            "/v1/messages",
            br#"{"model":"claude-3","stream":false}"#,
            &headers,
        ));
        // 无 stream 字段、非 SSE 端点、无 accept 头：非流。
        assert!(!is_streaming_request(
            "/v1/messages",
            br#"{"model":"claude-3"}"#,
            &headers,
        ));
    }

    #[test]
    fn is_streaming_request_detects_sse_endpoint_and_accept_header() {
        let headers = HeaderMap::new();
        assert!(is_streaming_request(
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            b"{}",
            &headers,
        ));

        let mut accept = HeaderMap::new();
        accept.insert("accept", HeaderValue::from_static("text/event-stream"));
        assert!(is_streaming_request("/v1/messages", b"{}", &accept));
    }
}
