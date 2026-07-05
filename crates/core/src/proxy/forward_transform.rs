//! Cross-format transform forwarding path.
//!
//! When the selected provider declares an `api_format` other than `anthropic`
//! (i.e. `openai_chat`, `openai_responses`, or `gemini_native`), the local proxy
//! must convert the incoming Claude-Messages request into the upstream wire
//! format, forward it, and convert the upstream response back into Claude format
//! (re-encoding the SSE stream for streaming responses).
//!
//! This is the tier-1/2/3 transform path ported from cc-switch's
//! `proxy::forwarder` + `proxy::handlers` + `proxy::response_processor`,
//! restructured around the simpler standalone [`super::forward::forward`] entry
//! point. The path includes request transform + response/SSE re-encode, managed
//! account auth injection, Copilot dynamic endpoint/header shaping, Gemini
//! shadow replay, prompt-cache session keys, request filtering/rectification,
//! and transform-path usage accounting so the proxy performs real format
//! bridging instead of only passthrough.

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
use crate::proxy::providers::{
    apply_copilot_model_normalization, claude_api_format_needs_transform, get_adapter,
    get_claude_api_format, normalize_anthropic_messages_for_provider, streaming, streaming_gemini,
    streaming_responses, transform, transform_claude_request_for_api_format, transform_gemini,
    transform_responses, ProviderAdapter,
};

use super::server::ProxyState;

/// Does this `(app, provider)` pair require a cross-format transform?
///
/// Only the Claude app currently drives `api_format` transforms (a Claude
/// client speaking to an OpenAI/Gemini upstream). Codex and Gemini apps keep
/// the existing passthrough/native path.
pub fn requires_transform(app_type: &AppType, provider: &Provider) -> bool {
    if !matches!(app_type, AppType::Claude | AppType::ClaudeDesktop) {
        return false;
    }
    if provider.is_github_copilot() || provider.is_codex_oauth() {
        return true;
    }
    let api_format = effective_claude_api_format(provider);
    claude_api_format_needs_transform(api_format)
}

/// Forward an incoming Claude-Messages request through the cross-format
/// transform path. The caller has already selected `provider` and recorded a
/// circuit-breaker permit; this function only does the transform + send +
/// re-encode and returns the final client Response.
pub async fn forward_transformed(
    state: &ProxyState,
    app_type: AppType,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    provider: &Provider,
    body: bytes::Bytes,
) -> Result<Response, ProxyError> {
    let start = std::time::Instant::now();
    let api_format = effective_claude_api_format(provider).to_string();

    // 1. Parse the client (Claude Messages) body.
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|e| ProxyError::InvalidRequest(format!("invalid JSON request body: {e}")))?;

    // Apply Claude role-alias model mapping (haiku/sonnet/opus/fable/default env
    // overrides) before the format transform so the upstream sees the mapped id.
    let (mut client_body, original_model, mapped_model) =
        super::model_mapper::apply_model_mapping(parsed, provider);
    let request_model = original_model
        .clone()
        .or_else(|| {
            client_body
                .get("model")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let session_format = if matches!(app_type, AppType::Claude | AppType::ClaudeDesktop) {
        "claude"
    } else {
        app_type.as_str()
    };
    let session = super::session::extract_session_id(headers, &client_body, session_format);
    log::debug!(
        "[Forward/Transform] session={} source={:?} client_provided={}",
        session.session_id,
        session.source,
        session.client_provided
    );

    let copilot_headers = prepare_client_body_for_upstream(
        state,
        headers,
        provider,
        &api_format,
        &session.session_id,
        &mut client_body,
    );

    let outbound_model = client_body
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or(mapped_model)
        .unwrap_or_else(|| request_model.clone());

    let is_stream = client_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tool_schema_hints = transform_gemini::extract_anthropic_tool_schema_hints(&client_body);
    let tool_schema_hints = (!tool_schema_hints.is_empty()).then_some(tool_schema_hints);

    // 3. Resolve the upstream URL.
    let adapter = get_adapter(&app_type);
    let mut base_url = adapter.extract_base_url(provider)?;
    let is_full_url = provider
        .meta
        .as_ref()
        .and_then(|m| m.is_full_url)
        .unwrap_or(false);
    if provider.is_github_copilot() && !is_full_url {
        let dynamic_endpoint = super::managed_auth::copilot_api_endpoint(state, provider).await;
        if dynamic_endpoint != base_url {
            log::debug!(
                "[Copilot] using dynamic API endpoint: {dynamic_endpoint} (was {base_url})"
            );
            base_url = dynamic_endpoint;
        }
    }

    let endpoint = endpoint_with_query(path, query);
    let url = build_transform_url(
        &adapter,
        &base_url,
        &endpoint,
        &api_format,
        is_full_url,
        &client_body,
    );

    let rectifier_config = state.db.get_rectifier_config().unwrap_or_default();
    let mut retry_state = RectifierRetryState::default();

    loop {
        // 2. Transform the request body to the upstream format.
        let transformed_body = transform_claude_request_for_api_format(
            client_body.clone(),
            provider,
            &api_format,
            Some(&session.session_id),
            Some(state.gemini_shadow.as_ref()),
        )
        .map_err(|e| ProxyError::TransformError(e.to_string()))?;
        let transformed_body = prepare_upstream_request_body(transformed_body);

        // 4. Build + send the upstream request with adapter auth headers.
        let upstream = send_upstream(
            state,
            method,
            &url,
            headers,
            &*adapter,
            provider,
            &transformed_body,
            copilot_headers.as_ref(),
            &session.session_id,
            session.client_provided,
        )
        .await?;

        let status = upstream.status();
        let upstream_headers = upstream.headers().clone();
        let upstream_is_sse = upstream_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);

        // 5. Re-encode the response back into Claude format.
        if is_stream && upstream_is_sse {
            return Ok(stream_back_transformed(
                state.clone(),
                status,
                &api_format,
                upstream,
                app_type,
                provider.clone(),
                request_model,
                outbound_model,
                session.session_id,
                start,
                tool_schema_hints,
            ));
        }

        match non_stream_back_transformed(
            state,
            status,
            &api_format,
            upstream,
            &app_type,
            provider,
            &request_model,
            &outbound_model,
            &session.session_id,
            start,
            tool_schema_hints.as_ref(),
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(error) => {
                if apply_rectifier_retry(
                    &error,
                    provider,
                    &rectifier_config,
                    &mut retry_state,
                    &mut client_body,
                ) {
                    continue;
                }
                return Err(error);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct CopilotRequestHeaders {
    initiator: &'static str,
    request_classification: bool,
    request_id: Option<String>,
    interaction_id: Option<String>,
    is_subagent: bool,
}

#[derive(Debug, Default)]
struct RectifierRetryState {
    media: bool,
    thinking_budget: bool,
    thinking_signature: bool,
}

async fn send_upstream(
    state: &ProxyState,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    adapter: &dyn ProviderAdapter,
    provider: &Provider,
    body: &Value,
    copilot_headers: Option<&CopilotRequestHeaders>,
    session_id: &str,
    session_client_provided: bool,
) -> Result<reqwest::Response, ProxyError> {
    let mut req = state.http_client.request(method.clone(), url);

    // Copy a minimal, safe header set from the client; auth + hop-by-hop are
    // re-injected/stripped below.
    for (name, value) in headers.iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_OR_AUTH.contains(&lname.as_str()) {
            continue;
        }
        if should_strip_managed_header(provider, &lname) {
            continue;
        }
        // The body is rewritten, so the original content-length / type are wrong.
        if lname == "content-type" || lname == "content-length" || lname == "accept-encoding" {
            continue;
        }
        req = req.header(name.clone(), value.clone());
    }
    req = req.header("content-type", "application/json");
    // Avoid compressed responses we would otherwise have to decode before
    // re-encoding; requesting identity keeps the SSE re-encoder simple and
    // matches the rebuilt-body response path.
    req = req.header("accept-encoding", "identity");

    // Inject auth headers via the adapter.
    if let Some(auth) = adapter.extract_auth(provider) {
        let resolved = super::managed_auth::resolve_auth_info(state, provider, auth).await?;
        match adapter.get_auth_headers(&resolved.auth) {
            Ok(pairs) => {
                for (name, value) in pairs {
                    req = req.header(name, value);
                }
            }
            Err(e) => return Err(e),
        }
        if let Some(account_id) = resolved.codex_account_id.as_deref() {
            req = req.header("chatgpt-account-id", account_id);
        }
        if resolved.is_codex_oauth && session_client_provided {
            for (name, value) in super::managed_auth::codex_oauth_session_headers(session_id) {
                req = req.header(name, value);
            }
        }
    }

    if let Some(headers) = copilot_headers {
        if headers.request_classification {
            req = req.header("x-initiator", headers.initiator);
        }
        if headers.is_subagent {
            req = req.header("x-interaction-type", "conversation-subagent");
        }
        if let Some(request_id) = headers.request_id.as_deref() {
            req = req
                .header("x-request-id", request_id)
                .header("x-agent-task-id", request_id);
        }
        if let Some(interaction_id) = headers.interaction_id.as_deref() {
            req = req.header("x-interaction-id", interaction_id);
        }
    }

    let payload = serde_json::to_vec(body)
        .map_err(|e| ProxyError::TransformError(format!("serialize upstream body: {e}")))?;
    req = if matches!(method, &Method::GET | &Method::HEAD) {
        req.body(Vec::new())
    } else {
        req.body(payload)
    };

    req.send()
        .await
        .map_err(|e| ProxyError::ForwardFailed(e.to_string()))
}

const HOP_BY_HOP_OR_AUTH: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
];

const COPILOT_MANAGED_HEADERS: &[&str] = &[
    "user-agent",
    "editor-version",
    "editor-plugin-version",
    "copilot-integration-id",
    "x-github-api-version",
    "openai-intent",
    "x-initiator",
    "x-interaction-type",
    "x-interaction-id",
    "x-vscode-user-agent-library-version",
    "x-request-id",
    "x-agent-task-id",
];

const CODEX_OAUTH_MANAGED_HEADERS: &[&str] = &[
    "originator",
    "chatgpt-account-id",
    "session_id",
    "x-client-request-id",
    "x-codex-window-id",
];

fn effective_claude_api_format(provider: &Provider) -> &'static str {
    if provider.is_github_copilot() {
        "openai_chat"
    } else {
        get_claude_api_format(provider)
    }
}

fn should_strip_managed_header(provider: &Provider, lname: &str) -> bool {
    (provider.is_github_copilot() && COPILOT_MANAGED_HEADERS.contains(&lname))
        || (provider.is_codex_oauth() && CODEX_OAUTH_MANAGED_HEADERS.contains(&lname))
}

fn prepare_client_body_for_upstream(
    state: &ProxyState,
    headers: &HeaderMap,
    provider: &Provider,
    api_format: &str,
    session_id: &str,
    body: &mut Value,
) -> Option<CopilotRequestHeaders> {
    replace_body(body, super::thinking_rectifier::normalize_thinking_type);

    let copilot_headers = if provider.is_github_copilot() {
        replace_body(body, apply_copilot_model_normalization);
        prepare_copilot_request_body(state, headers, session_id, body)
    } else {
        replace_body(
            body,
            super::model_mapper::strip_one_m_suffix_for_upstream_from_body,
        );
        None
    };

    normalize_anthropic_messages_for_provider(body, provider, api_format);
    apply_media_prevention(state, body, provider);
    apply_bedrock_optimizer(state, body, provider);

    copilot_headers
}

fn replace_body(body: &mut Value, f: impl FnOnce(Value) -> Value) {
    let current = std::mem::replace(body, Value::Null);
    *body = f(current);
}

fn prepare_copilot_request_body(
    state: &ProxyState,
    headers: &HeaderMap,
    session_id: &str,
    body: &mut Value,
) -> Option<CopilotRequestHeaders> {
    let config = state.db.get_copilot_optimizer_config().unwrap_or_default();
    if !config.enabled {
        return None;
    }

    let has_anthropic_beta = headers.contains_key("anthropic-beta");
    let classification = super::copilot_optimizer::classify_request(
        body,
        has_anthropic_beta,
        config.compact_detection,
        config.subagent_detection,
    );
    log::debug!(
        "[Copilot] optimizer classification: initiator={}, warmup={}, compact={}, subagent={}",
        classification.initiator,
        classification.is_warmup,
        classification.is_compact,
        classification.is_subagent
    );

    replace_body(body, super::copilot_optimizer::sanitize_orphan_tool_results);
    if config.tool_result_merging {
        replace_body(body, super::copilot_optimizer::merge_tool_results);
    }
    if config.strip_thinking {
        replace_body(body, super::copilot_optimizer::strip_thinking_blocks);
    }
    if config.warmup_downgrade && classification.is_warmup {
        body["model"] = Value::String(config.warmup_model.clone());
    }

    let request_id = config
        .deterministic_request_id
        .then(|| super::copilot_optimizer::deterministic_request_id(body, session_id));
    let interaction_id = super::copilot_optimizer::deterministic_interaction_id(session_id);

    Some(CopilotRequestHeaders {
        initiator: classification.initiator,
        request_classification: config.request_classification,
        request_id,
        interaction_id,
        is_subagent: classification.is_subagent,
    })
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
        log::info!(
            "[Forward/Transform] media fallback preflight replaced {replaced} image block(s)"
        );
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

fn apply_rectifier_retry(
    error: &ProxyError,
    provider: &Provider,
    config: &crate::db::proxy_types::RectifierConfig,
    retry_state: &mut RectifierRetryState,
    client_body: &mut Value,
) -> bool {
    if config.enabled
        && config.request_media_fallback
        && !retry_state.media
        && super::media_sanitizer::is_unsupported_image_error(error)
    {
        let replaced = super::media_sanitizer::replace_image_blocks_with_marker(client_body);
        if replaced > 0 {
            retry_state.media = true;
            log::info!(
                "[Forward/Transform] media rectifier retry for provider {} replaced {replaced} image block(s)",
                provider.name
            );
            return true;
        }
    }

    let message = extract_error_message(error);
    if !retry_state.thinking_budget
        && super::thinking_budget_rectifier::should_rectify_thinking_budget(
            message.as_deref(),
            config,
        )
    {
        let result = super::thinking_budget_rectifier::rectify_thinking_budget(client_body);
        if result.applied {
            retry_state.thinking_budget = true;
            log::info!(
                "[Forward/Transform] thinking budget rectifier retry for provider {}, before={:?}, after={:?}",
                provider.name,
                result.before,
                result.after
            );
            return true;
        }
    }

    if !retry_state.thinking_signature
        && super::thinking_rectifier::should_rectify_thinking_signature(message.as_deref(), config)
    {
        let result = super::thinking_rectifier::rectify_anthropic_request(client_body);
        if result.applied {
            retry_state.thinking_signature = true;
            log::info!(
                "[Forward/Transform] thinking signature rectifier retry for provider {} removed thinking={}, redacted={}, signatures={}",
                provider.name,
                result.removed_thinking_blocks,
                result.removed_redacted_thinking_blocks,
                result.removed_signature_fields
            );
            return true;
        }
    }

    false
}

fn extract_error_message(error: &ProxyError) -> Option<String> {
    match error {
        ProxyError::UpstreamError { body, .. } => body.clone(),
        _ => Some(error.to_string()),
    }
}

/// Re-encode a streaming upstream SSE response into a Claude SSE stream.
#[allow(clippy::too_many_arguments)]
fn stream_back_transformed(
    state: ProxyState,
    status: StatusCode,
    api_format: &str,
    upstream: reqwest::Response,
    app_type: AppType,
    provider: Provider,
    request_model: String,
    outbound_model: String,
    session_id: String,
    start: std::time::Instant,
    tool_schema_hints: Option<transform_gemini::AnthropicToolSchemaHints>,
) -> Response {
    let byte_stream = upstream.bytes_stream();

    let body = match api_format {
        "openai_responses" => {
            let s = streaming_responses::create_anthropic_sse_stream_from_responses(byte_stream);
            Body::from_stream(stream_with_usage_logging(
                state,
                s,
                app_type,
                provider,
                request_model,
                outbound_model,
                session_id,
                start,
                status.as_u16(),
            ))
        }
        "gemini_native" => {
            let s = streaming_gemini::create_anthropic_sse_stream_from_gemini(
                byte_stream,
                Some(state.gemini_shadow.clone()),
                Some(provider.id.clone()),
                Some(session_id.clone()),
                tool_schema_hints,
            );
            Body::from_stream(stream_with_usage_logging(
                state,
                s,
                app_type,
                provider,
                request_model,
                outbound_model,
                session_id,
                start,
                status.as_u16(),
            ))
        }
        // openai_chat (and any other transform format) -> OpenAI Chat SSE.
        _ => {
            let s = streaming::create_anthropic_sse_stream(byte_stream);
            Body::from_stream(stream_with_usage_logging(
                state,
                s,
                app_type,
                provider,
                request_model,
                outbound_model,
                session_id,
                start,
                status.as_u16(),
            ))
        }
    };

    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        h.insert("content-type", "text/event-stream".parse().unwrap());
        h.insert("cache-control", "no-cache".parse().unwrap());
    }
    builder.body(body).unwrap_or_else(|_| {
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "build stream failed")
    })
}

#[allow(clippy::too_many_arguments)]
fn stream_with_usage_logging<S>(
    state: ProxyState,
    stream: S,
    app_type: AppType,
    provider: Provider,
    request_model: String,
    outbound_model: String,
    session_id: String,
    start: std::time::Instant,
    status_code: u16,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
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
                                if data.trim() == "[DONE]" || !claude_stream_usage_event_filter(data) {
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
            log_stream_usage_from_claude_events(
                &state,
                &app_type,
                &provider,
                &request_model,
                &outbound_model,
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

/// Re-encode a non-streaming upstream JSON response into Claude format.
#[allow(clippy::too_many_arguments)]
async fn non_stream_back_transformed(
    state: &ProxyState,
    status: StatusCode,
    api_format: &str,
    upstream: reqwest::Response,
    app_type: &AppType,
    provider: &Provider,
    request_model: &str,
    outbound_model: &str,
    session_id: &str,
    start: std::time::Instant,
    tool_schema_hints: Option<&transform_gemini::AnthropicToolSchemaHints>,
) -> Result<Response, ProxyError> {
    let headers = upstream.headers().clone();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| ProxyError::ForwardFailed(format!("read upstream body: {e}")))?;

    // On upstream error, pass the body through untouched so the client sees the
    // real provider error.
    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        return Err(ProxyError::UpstreamError {
            status: status.as_u16(),
            body: Some(body_str),
        });
    }

    // 用量统计 / 格式转换仅解析解压后的副本。accept-encoding: identity 已在上游请求
    // 强制，正常拿到明文；仍保留解压兜底（与 codex 的 stream_back_codex_chat_converted
    // _with_usage 对称），以防上游忽略 identity 而压缩响应体，否则 JSON 解析与 SSE
    // 嗅探都会失败、转换被静默跳过。重建的明文响应只带 content-type，天然剥离了
    // content-encoding/content-length。
    let parse_bytes = super::forward::decompressed_for_parse(&headers, &bytes);

    let upstream_json: Value = match serde_json::from_slice(&parse_bytes) {
        Ok(value) => value,
        // 兜底嗅探（#2234）：部分网关对 stream:false 强制返回 SSE 体，却把
        // Content-Type 标成 application/json，upstream_is_sse 检查失效。此时按 SSE
        // 聚合成单个 JSON 再走既有非流转换器，客户端仍收到 Anthropic JSON。
        // gemini_native 暂无聚合器，落诊断错误。
        Err(error) if api_format != "gemini_native" => {
            let body_str = String::from_utf8_lossy(&parse_bytes);
            if super::forward::body_looks_like_sse(&body_str) {
                log::warn!(
                    "[Forward/Transform] 上游对非流请求返回未标记的 SSE 体 (api_format={api_format})，按 SSE 聚合兜底"
                );
                let aggregated = if api_format == "openai_responses" {
                    super::forward::responses_sse_to_response_value(&body_str)
                } else {
                    super::forward::chat_sse_to_response_value(&body_str)
                };
                aggregated.map_err(|agg| {
                    ProxyError::TransformError(format!("SSE aggregate fallback failed: {agg}"))
                })?
            } else {
                return Err(ProxyError::TransformError(format!(
                    "parse upstream JSON: {error}"
                )));
            }
        }
        Err(error) => {
            return Err(ProxyError::TransformError(format!(
                "parse upstream JSON: {error}"
            )))
        }
    };

    let anthropic = match api_format {
        "openai_responses" => transform_responses::responses_to_anthropic(upstream_json),
        "gemini_native" => transform_gemini::gemini_to_anthropic_with_shadow_and_hints(
            upstream_json,
            Some(state.gemini_shadow.as_ref()),
            Some(&provider.id),
            Some(session_id),
            tool_schema_hints,
        ),
        _ => transform::openai_to_anthropic(upstream_json),
    }
    .map_err(|e| ProxyError::TransformError(e.to_string()))?;

    // Usage accounting: parse the re-encoded Claude response and log to
    // request_logs with cost computed from model_pricing.
    log_usage_from_claude_response(
        state,
        app_type,
        provider,
        request_model,
        outbound_model,
        Some(session_id.to_string()),
        &anthropic,
        start.elapsed().as_millis() as u64,
        status.as_u16(),
    )
    .await;

    let out = serde_json::to_vec(&anthropic)
        .map_err(|e| ProxyError::TransformError(format!("serialize anthropic body: {e}")))?;

    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        h.insert("content-type", "application/json".parse().unwrap());
    }
    Ok(builder.body(Body::from(out)).unwrap_or_else(|_| {
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "build response failed")
    }))
}

/// Build the upstream URL for a transformed Claude request.
///
/// Mirrors cc-switch `forwarder::rewrite_claude_transform_endpoint` +
/// `gemini_url::resolve_gemini_native_url` + adapter `build_url`.
fn build_transform_url(
    adapter: &Box<dyn ProviderAdapter>,
    base_url: &str,
    endpoint: &str,
    api_format: &str,
    is_full_url: bool,
    client_body: &Value,
) -> String {
    let rewritten = rewrite_claude_transform_endpoint(endpoint, api_format, client_body);

    if api_format == "gemini_native" {
        super::gemini_url::resolve_gemini_native_url(base_url, &rewritten, is_full_url)
    } else if is_full_url {
        // Full URL: base already points at the exact endpoint; keep query only.
        let (_, query) = split_endpoint_and_query(&rewritten);
        match query {
            Some(q) if !q.is_empty() => format!("{base_url}?{q}"),
            _ => base_url.to_string(),
        }
    } else {
        adapter.build_url(base_url, &rewritten)
    }
}

fn rewrite_claude_transform_endpoint(endpoint: &str, api_format: &str, body: &Value) -> String {
    let (path, query) = split_endpoint_and_query(endpoint);
    let passthrough_query = if is_claude_messages_path(path) {
        strip_beta_query(query)
    } else {
        query.map(ToString::to_string)
    };

    if !is_claude_messages_path(path) {
        return endpoint.to_string();
    }

    if api_format == "gemini_native" {
        let model = transform_gemini::extract_gemini_model(body).unwrap_or("unknown");
        let model = super::gemini_url::normalize_gemini_model_id(model);
        let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let target_path = if is_stream {
            format!("/v1beta/models/{model}:streamGenerateContent")
        } else {
            format!("/v1beta/models/{model}:generateContent")
        };
        let rewritten_query = merge_query_params(
            passthrough_query.as_deref(),
            if is_stream { Some("alt=sse") } else { None },
        );
        return match rewritten_query.as_deref() {
            Some(q) if !q.is_empty() => format!("{target_path}?{q}"),
            _ => target_path,
        };
    }

    let target_path = if api_format == "openai_responses" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };

    match passthrough_query.as_deref() {
        Some(q) if !q.is_empty() => format!("{target_path}?{q}"),
        _ => target_path.to_string(),
    }
}

fn split_endpoint_and_query(endpoint: &str) -> (&str, Option<&str>) {
    endpoint
        .split_once('?')
        .map_or((endpoint, None), |(path, query)| (path, Some(query)))
}

fn strip_beta_query(query: Option<&str>) -> Option<String> {
    let filtered = query.map(|query| {
        query
            .split('&')
            .filter(|pair| !pair.is_empty() && !pair.starts_with("beta="))
            .collect::<Vec<_>>()
            .join("&")
    });
    match filtered.as_deref() {
        Some("") | None => None,
        Some(_) => filtered,
    }
}

fn is_claude_messages_path(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/claude/v1/messages")
}

fn merge_query_params(base_query: Option<&str>, extra_param: Option<&str>) -> Option<String> {
    let mut params: Vec<String> = base_query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter(|pair| !pair.is_empty())
        .filter(|pair| !pair.starts_with("alt="))
        .map(ToString::to_string)
        .collect();
    if let Some(extra_param) = extra_param {
        params.push(extra_param.to_string());
    }
    if params.is_empty() {
        None
    } else {
        Some(params.join("&"))
    }
}

fn endpoint_with_query(path: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_string(),
    }
}

/// Parse usage from a re-encoded Claude response and log it with cost.
///
/// Mirrors cc-switch `response_processor::log_usage` for the non-streaming path:
/// resolve the provider/global pricing config, pick the pricing model per the
/// `pricing_model_source`, and write a row to `proxy_request_logs`.
async fn log_usage_from_claude_response(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    request_model: &str,
    outbound_model: &str,
    session_id: Option<String>,
    anthropic_response: &Value,
    latency_ms: u64,
    status_code: u16,
) {
    use crate::proxy::usage::parser::TokenUsage;

    let Some(usage) =
        TokenUsage::from_claude_response(anthropic_response).filter(|u| u.has_billable_tokens())
    else {
        return;
    };

    // The response normally echoes the upstream model; fall back to the outbound
    // model and then the original request model when a compatible provider
    // returns an empty/synthetic value.
    let response_model = anthropic_response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| {
            if outbound_model.is_empty() {
                request_model
            } else {
                outbound_model
            }
        })
        .to_string();

    log_usage(
        state,
        app_type,
        provider,
        &response_model,
        request_model,
        outbound_model,
        usage,
        latency_ms,
        None,
        status_code,
        session_id,
        false,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn log_stream_usage_from_claude_events(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    request_model: &str,
    outbound_model: &str,
    session_id: &str,
    events: Vec<Value>,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    status_code: u16,
) {
    use crate::proxy::usage::parser::TokenUsage;

    let Some(usage) =
        TokenUsage::from_claude_stream_events(&events).filter(|u| u.has_billable_tokens())
    else {
        log::debug!("[Forward/Transform] 流式响应 usage 全 0 或缺失，跳过消费记录");
        return;
    };

    let response_model = usage
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| {
            if outbound_model.is_empty() {
                request_model.to_string()
            } else {
                outbound_model.to_string()
            }
        });

    log_usage(
        state,
        app_type,
        provider,
        &response_model,
        request_model,
        outbound_model,
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
async fn log_usage(
    state: &ProxyState,
    app_type: &AppType,
    provider: &Provider,
    response_model: &str,
    request_model: &str,
    outbound_model: &str,
    usage: crate::proxy::usage::parser::TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    status_code: u16,
    session_id: Option<String>,
    is_streaming: bool,
) {
    use crate::proxy::usage::logger::UsageLogger;

    let logger = UsageLogger::new(&state.db);
    let (multiplier, pricing_model_source) = logger
        .resolve_pricing_config(&provider.id, app_type.as_str())
        .await;
    let pricing_model = if pricing_model_source == PRICING_SOURCE_REQUEST {
        outbound_model
    } else {
        response_model
    };

    let request_id = usage.dedup_request_id();
    let provider_type = provider.meta.as_ref().and_then(|m| m.provider_type.clone());

    if let Err(e) = logger.log_with_calculation(
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
        log::warn!("[USG-001] transform usage log failed: {e}");
    }
}

fn claude_stream_usage_event_filter(data: &str) -> bool {
    data.contains("\"message_start\"") || data.contains("\"message_delta\"")
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "error": { "type": "proxy_error", "message": message }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from(message.to_string())))
}
