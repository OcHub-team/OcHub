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
    /// `x-goog-api-key: <token>` (Gemini)
    GoogApiKey,
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
    let parsed_body = serde_json::from_slice::<Value>(&body).ok();
    let request_model = request_model_for_app(&app_type, path, parsed_body.as_ref());
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
        let result = send_upstream(&state, &method, &url, &headers, &target, prepared.body).await;

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
        AppType::Gemini => path
            .strip_prefix("/gemini")
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
        AppType::Gemini => "gemini",
        other => other.as_str(),
    }
}

fn request_model_for_app(app_type: &AppType, path: &str, body: Option<&Value>) -> String {
    if matches!(app_type, AppType::Gemini) {
        return extract_gemini_model_from_path(path).unwrap_or_else(|| {
            body.and_then(|value| value.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        });
    }

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

fn extract_gemini_model_from_path(path: &str) -> Option<String> {
    let marker = "/models/";
    let start = path.find(marker)? + marker.len();
    let rest = &path[start..];
    let end = rest
        .find(':')
        .or_else(|| rest.find('/'))
        .unwrap_or(rest.len());
    let model = &rest[..end];
    (!model.is_empty()).then(|| model.to_string())
}

fn passthrough_stream_usage_event_filter(app_type: &AppType, data: &str) -> bool {
    match app_type {
        AppType::Claude | AppType::ClaudeDesktop => {
            data.contains("\"message_start\"") || data.contains("\"message_delta\"")
        }
        AppType::Codex => data.contains("\"response.completed\"") || data.contains("\"usage\""),
        AppType::Gemini => data.contains("\"usageMetadata\""),
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
        AppType::Gemini => TokenUsage::from_gemini_stream_chunks(&events),
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
        AppType::Gemini => TokenUsage::from_gemini_response(response),
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
) -> Result<reqwest::Response, String> {
    let mut req = state.http_client.request(method.clone(), url);

    // Copy through client headers except hop-by-hop and auth headers.
    for (name, value) in headers.iter() {
        let name_str = name.as_str().to_ascii_lowercase();
        if STRIP_REQUEST_HEADERS.contains(&name_str.as_str()) {
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

    // Inject the real credential.
    if let Some(token) = target.token.as_deref() {
        match target.strategy {
            AuthStrategy::Bearer => {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            AuthStrategy::ApiKey => {
                req = req.header("x-api-key", token);
            }
            AuthStrategy::GoogApiKey => {
                req = req.header("x-goog-api-key", token);
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

    if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
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
    let upstream_json = serde_json::from_slice::<Value>(&bytes).ok();
    let converted = if status.is_success() {
        match upstream_json {
            Some(json) => {
                crate::proxy::providers::transform_codex_chat::chat_completion_to_response_with_context(
                    json,
                    &conversion.tool_context,
                )
            }
            None => Err(ProxyError::TransformError(
                "parse chat completion response".to_string(),
            )),
        }
    } else {
        Ok(
            crate::proxy::providers::transform_codex_chat::chat_error_to_response_error(
                upstream_json.as_ref(),
            ),
        )
    };

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
    let json = serde_json::from_slice::<Value>(&bytes).ok();
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
        AppType::Gemini => resolve_gemini(provider),
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

fn resolve_gemini(provider: &Provider) -> Result<UpstreamTarget, String> {
    let cfg = &provider.settings_config;

    let base_url = env_str(cfg, "GOOGLE_GEMINI_BASE_URL")
        .or_else(|| settings_str(cfg, "base_url"))
        .or_else(|| settings_str(cfg, "baseURL"))
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| "Gemini provider missing base_url".to_string())?;

    let token = env_str(cfg, "GEMINI_API_KEY")
        .or_else(|| env_str(cfg, "GOOGLE_API_KEY"))
        .filter(|t| !is_placeholder(t))
        .or_else(|| settings_str(cfg, "api_key").filter(|t| !is_placeholder(t)))
        .map(str::to_string);

    Ok(UpstreamTarget {
        base_url,
        token,
        strategy: AuthStrategy::GoogApiKey,
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
