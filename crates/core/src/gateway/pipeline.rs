//! Gateway forwarding pipeline: inlet request → channel selection → dialect
//! conversion → upstream call → response conversion → usage logging.
//!
//! Transport-agnostic core: [`run`] returns either a JSON body or a stream of
//! [`StreamFrame`]s; the HTTP handler encodes frames as SSE and the WebSocket
//! handler sends them as text frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use ochub_convert::aggregate;
use ochub_convert::usage as conv_usage;
use ochub_convert::{
    chat as conv_chat, chat_upstream as conv_chat_upstream, messages as conv_messages,
    responses as conv_responses, MessagesRequestOptions, Output, ResponsesRequestOptions,
    SignatureCapture, SseParser, WireEvent,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::db::Database;
use crate::gateway::router::candidates_for_model_ranked;
use crate::gateway::types::{
    ChannelHealth, Dialect, GatewayChannel, GatewayConfig, GatewayKey, GatewayReasoningConfig,
    GatewayReasoningMode, GatewayRoute,
};
use crate::usage_tracking::logger::UsageLogger;
use crate::usage_tracking::parser::TokenUsage;

/// Shared state for the gateway server + pipeline.
#[derive(Clone)]
pub struct GatewayState {
    pub db: Arc<Database>,
    pub http_client: reqwest::Client,
    pub config: Arc<RwLock<GatewayConfig>>,
    pub health: Arc<RwLock<HashMap<String, ChannelHealth>>>,
    /// Thinking-signature round-trip store shared across requests.
    pub signatures: Arc<ochub_convert::MemorySignatureStore>,
}

/// One frame of a streaming reply.
#[derive(Debug)]
pub enum StreamFrame {
    Event(WireEvent),
    /// End of stream. Chat SSE encoders emit `data: [DONE]` on this.
    Done,
}

/// Pipeline outcome, transport-neutral.
pub enum PipelineOutcome {
    Json { status: u16, body: Value },
    Stream { rx: mpsc::Receiver<StreamFrame> },
}

/// Can requests of `inlet` dialect be served by a channel of `channel` dialect?
/// All nine pairs convert (chat-upstream reverse conversion is lossy: thinking
/// signatures and cache markers have no chat representation). Kept as the single
/// source of truth so UI/apply guards lift automatically with the matrix.
pub fn conversion_supported(_inlet: Dialect, _channel: Dialect) -> bool {
    true
}

/// Codex remote compaction v2 is a Responses-only protocol extension. Its
/// `compaction_trigger` input has no faithful Messages/Chat representation, so
/// it must never enter a lossy dialect conversion or fail over to one.
fn is_remote_compaction_request(inlet: Dialect, body: &Value) -> bool {
    inlet == Dialect::Responses
        && body
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("compaction_trigger")
                })
            })
}

fn channel_supports_request(inlet: Dialect, channel: Dialect, remote_compaction: bool) -> bool {
    conversion_supported(inlet, channel) && (!remote_compaction || channel == Dialect::Responses)
}

fn error_body(inlet: Dialect, message: &str) -> Value {
    match inlet {
        Dialect::Messages => json!({
            "type": "error",
            "error": { "type": "api_error", "message": message }
        }),
        Dialect::Chat | Dialect::Responses => json!({
            "error": { "type": "api_error", "message": message }
        }),
    }
}

/// messages-shape usage value → `TokenUsage` row fields.
fn token_usage_from_messages(usage: &Value, model: Option<String>) -> TokenUsage {
    let g = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    TokenUsage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_creation_tokens: g("cache_creation_input_tokens"),
        model,
        message_id: None,
    }
}

/// chat-shape usage value → messages-shape (exclusive input accounting).
fn chat_usage_to_messages(usage: &Value) -> Value {
    let g = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let prompt = g("prompt_tokens");
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation = usage
        .pointer("/prompt_tokens_details/cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens": prompt.saturating_sub(cached + cache_creation),
        "cache_read_input_tokens": cached,
        "cache_creation_input_tokens": cache_creation,
        "output_tokens": g("completion_tokens"),
    })
}

// ---------------------------------------------------------------------------
// Request conversion
// ---------------------------------------------------------------------------

struct PreparedRequest {
    body: Value,
    /// Upstream model actually requested (after channel model_override).
    upstream_model: String,
}

struct RequestConversionOptions<'a> {
    client_model: &'a str,
    route_model_override: Option<&'a str>,
    reasoning: Option<&'a GatewayReasoningConfig>,
    client_stream: bool,
}

fn prepare_request(
    inlet: Dialect,
    channel: &GatewayChannel,
    body: &Value,
    options: RequestConversionOptions<'_>,
    signatures: &ochub_convert::MemorySignatureStore,
) -> Result<PreparedRequest, String> {
    let RequestConversionOptions {
        client_model,
        route_model_override,
        reasoning,
        client_stream,
    } = options;
    let upstream_model = route_model_override
        .map(str::to_string)
        .or_else(|| channel.model_override.clone())
        .unwrap_or_else(|| client_model.to_string());
    let mut source = body.clone();
    apply_reasoning_policy(&mut source, inlet, channel.dialect, reasoning);
    let messages_options = MessagesRequestOptions {
        default_thinking_budget: reasoning
            .map(|config| config.medium_budget as i64)
            .unwrap_or_else(|| MessagesRequestOptions::default().default_thinking_budget),
        ..Default::default()
    };

    let mut converted = match (inlet, channel.dialect) {
        // Same dialect: passthrough.
        (a, b) if a == b => source.clone(),
        (Dialect::Chat, Dialect::Messages) => {
            conv_chat::request_to_messages(&source, &messages_options).map_err(|e| e.to_string())?
        }
        (Dialect::Responses, Dialect::Messages) => {
            conv_responses::request_to_messages(&source, &messages_options)
                .map_err(|e| e.to_string())?
        }
        (Dialect::Messages, Dialect::Responses) => {
            let opts = ResponsesRequestOptions {
                reasoning_effort: reasoning_effort_for_messages(&source, reasoning),
                force_stream: client_stream,
                ..Default::default()
            };
            conv_messages::request_to_responses(&source, &opts).map_err(|e| e.to_string())?
        }
        // chat → responses pivots through the messages dialect.
        (Dialect::Chat, Dialect::Responses) => {
            let mid = conv_chat::request_to_messages(&source, &messages_options)
                .map_err(|e| e.to_string())?;
            let opts = ResponsesRequestOptions {
                reasoning_effort: reasoning_effort_for_messages(&mid, reasoning),
                force_stream: client_stream,
                ..Default::default()
            };
            conv_messages::request_to_responses(&mid, &opts).map_err(|e| e.to_string())?
        }
        (Dialect::Messages, Dialect::Chat) => {
            let opts = conv_chat_upstream::ChatRequestOptions {
                reasoning_effort: reasoning_effort_for_messages(&source, reasoning),
                force_stream: client_stream,
            };
            conv_chat_upstream::request_to_chat(&source, &opts).map_err(|e| e.to_string())?
        }
        // responses → chat pivots through the messages dialect.
        (Dialect::Responses, Dialect::Chat) => {
            let mid = conv_responses::request_to_messages(&source, &messages_options)
                .map_err(|e| e.to_string())?;
            let opts = conv_chat_upstream::ChatRequestOptions {
                reasoning_effort: reasoning_effort_for_messages(&mid, reasoning),
                force_stream: client_stream,
            };
            conv_chat_upstream::request_to_chat(&mid, &opts).map_err(|e| e.to_string())?
        }
        // All (inlet, channel) pairs are handled above.
        _ => unreachable!("conversion matrix is total"),
    };

    if let Some(obj) = converted.as_object_mut() {
        obj.insert("model".into(), json!(upstream_model));
        obj.insert("stream".into(), json!(client_stream));
    }
    // Replay stored thinking blocks for messages-dialect upstreams.
    if channel.dialect == Dialect::Messages {
        ochub_convert::signature::restore_thinking_blocks(&mut converted, signatures);
    }
    Ok(PreparedRequest {
        body: converted,
        upstream_model,
    })
}

fn apply_reasoning_policy(
    body: &mut Value,
    inlet: Dialect,
    channel: Dialect,
    reasoning: Option<&GatewayReasoningConfig>,
) {
    let Some(config) = reasoning else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match config.mode {
        GatewayReasoningMode::Passthrough => {}
        GatewayReasoningMode::Disabled => {
            obj.remove("thinking");
            obj.remove("reasoning");
            obj.remove("reasoning_effort");
        }
        GatewayReasoningMode::Auto if inlet != channel && inlet != Dialect::Messages => {
            let effort = obj
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .or_else(|| {
                    obj.get("reasoning")
                        .and_then(|value| value.get("effort"))
                        .and_then(Value::as_str)
                });
            if let Some(effort) = effort {
                match config.budget_for_effort(effort) {
                    Some(budget) => {
                        obj.insert(
                            "thinking".into(),
                            json!({ "type": "enabled", "budget_tokens": budget }),
                        );
                    }
                    None => {
                        obj.remove("thinking");
                    }
                }
            }
        }
        GatewayReasoningMode::Auto => {}
    }
}

fn reasoning_effort_for_messages(
    body: &Value,
    reasoning: Option<&GatewayReasoningConfig>,
) -> Option<String> {
    let config = reasoning?;
    match config.mode {
        GatewayReasoningMode::Disabled => None,
        GatewayReasoningMode::Passthrough => None,
        GatewayReasoningMode::Auto => body
            .pointer("/thinking/budget_tokens")
            .and_then(Value::as_u64)
            .map(|budget| {
                config
                    .effort_for_budget(budget.min(u32::MAX as u64) as u32)
                    .to_string()
            }),
    }
}

// ---------------------------------------------------------------------------
// Stream conversion
// ---------------------------------------------------------------------------

/// Per-request stream converter (channel dialect events → inlet dialect events).
enum StreamConverter {
    Passthrough {
        inlet: Dialect,
        merged_usage: Option<Value>,
    },
    MessagesToChat(conv_chat::MessagesToChatStream),
    MessagesToResponses(conv_responses::MessagesToResponsesStream),
    ResponsesToMessages(conv_messages::ResponsesToMessagesStream),
    /// responses upstream → chat client, chained through messages events.
    ResponsesToChat(
        conv_messages::ResponsesToMessagesStream,
        conv_chat::MessagesToChatStream,
    ),
    /// chat upstream → messages client.
    ChatToMessages(conv_chat_upstream::ChatToMessagesStream),
    /// chat upstream → responses client, chained through messages events.
    ChatToResponses(
        conv_chat_upstream::ChatToMessagesStream,
        conv_responses::MessagesToResponsesStream,
    ),
}

/// What a converter produced for one upstream event.
#[derive(Default)]
struct Converted {
    frames: Vec<StreamFrame>,
    usage: Option<Value>,
    capture: Option<SignatureCapture>,
    errored: bool,
}

impl StreamConverter {
    fn new(
        inlet: Dialect,
        channel: Dialect,
        display_model: &str,
        include_usage: bool,
    ) -> Option<Self> {
        match (channel, inlet) {
            (a, b) if a == b => Some(StreamConverter::Passthrough {
                inlet,
                merged_usage: None,
            }),
            (Dialect::Messages, Dialect::Chat) => Some(StreamConverter::MessagesToChat(
                conv_chat::MessagesToChatStream::new(display_model, include_usage),
            )),
            (Dialect::Messages, Dialect::Responses) => Some(StreamConverter::MessagesToResponses(
                conv_responses::MessagesToResponsesStream::new(display_model),
            )),
            (Dialect::Responses, Dialect::Messages) => Some(StreamConverter::ResponsesToMessages(
                conv_messages::ResponsesToMessagesStream::new(display_model),
            )),
            (Dialect::Responses, Dialect::Chat) => Some(StreamConverter::ResponsesToChat(
                conv_messages::ResponsesToMessagesStream::new(display_model),
                conv_chat::MessagesToChatStream::new(display_model, include_usage),
            )),
            (Dialect::Chat, Dialect::Messages) => Some(StreamConverter::ChatToMessages(
                conv_chat_upstream::ChatToMessagesStream::new(display_model),
            )),
            (Dialect::Chat, Dialect::Responses) => Some(StreamConverter::ChatToResponses(
                conv_chat_upstream::ChatToMessagesStream::new(display_model),
                conv_responses::MessagesToResponsesStream::new(display_model),
            )),
            _ => None,
        }
    }

    fn push(&mut self, ev: &WireEvent) -> Converted {
        let mut out = Converted::default();
        match self {
            StreamConverter::Passthrough {
                inlet,
                merged_usage,
            } => {
                passthrough_usage_tap(*inlet, ev, merged_usage);
                let done = match inlet {
                    Dialect::Chat => ev.data.trim() == "[DONE]",
                    Dialect::Messages => event_name(ev) == Some("message_stop".into()),
                    Dialect::Responses => {
                        matches!(
                            event_name(ev).as_deref(),
                            Some("response.completed") | Some("response.failed")
                        )
                    }
                };
                // Forward everything verbatim except the chat [DONE] marker,
                // which the transport encoder re-emits from `Done`.
                if !(matches!(inlet, Dialect::Chat) && done) {
                    out.frames.push(StreamFrame::Event(ev.clone()));
                }
                if done {
                    out.usage = merged_usage.clone();
                    out.frames.push(StreamFrame::Done);
                }
            }
            StreamConverter::MessagesToChat(c) => collect_outputs(c.push(ev), &mut out),
            StreamConverter::MessagesToResponses(c) => collect_outputs(c.push(ev), &mut out),
            StreamConverter::ResponsesToMessages(c) => collect_outputs(c.push(ev), &mut out),
            StreamConverter::ResponsesToChat(outer, inner) => {
                for o in outer.push(ev) {
                    match o {
                        Output::Event(mid) => collect_outputs(inner.push(&mid), &mut out),
                        Output::Usage(u) => out.usage = Some(u),
                        Output::Error(e) => {
                            out.errored = true;
                            let _ = e;
                        }
                        // Done/Capture propagate from the inner converter.
                        _ => {}
                    }
                }
            }
            StreamConverter::ChatToMessages(c) => collect_outputs(c.push(ev), &mut out),
            StreamConverter::ChatToResponses(outer, inner) => {
                for o in outer.push(ev) {
                    match o {
                        Output::Event(mid) => collect_outputs(inner.push(&mid), &mut out),
                        Output::Usage(u) => out.usage = Some(u),
                        Output::Error(e) => {
                            out.errored = true;
                            let _ = e;
                        }
                        // Done/Capture propagate from the inner converter.
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

fn collect_outputs(outputs: Vec<Output>, out: &mut Converted) {
    for o in outputs {
        match o {
            Output::Event(e) => out.frames.push(StreamFrame::Event(e)),
            Output::Usage(u) => out.usage = Some(u),
            Output::Capture(c) => out.capture = Some(c),
            Output::Error(_) => out.errored = true,
            Output::Done => out.frames.push(StreamFrame::Done),
        }
    }
}

fn event_name(ev: &WireEvent) -> Option<String> {
    if let Some(name) = &ev.event {
        if !name.is_empty() {
            return Some(name.clone());
        }
    }
    serde_json::from_str::<Value>(&ev.data)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
}

/// Track usage while passing a same-dialect stream through unchanged.
fn passthrough_usage_tap(inlet: Dialect, ev: &WireEvent, merged: &mut Option<Value>) {
    let Ok(parsed) = serde_json::from_str::<Value>(&ev.data) else {
        return;
    };
    match inlet {
        Dialect::Messages => match parsed.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = parsed.pointer("/message/usage") {
                    *merged = Some(u.clone());
                }
            }
            Some("message_delta") => {
                if let Some(u) = parsed.get("usage") {
                    conv_usage::merge_messages_usage(merged, u);
                }
            }
            _ => {}
        },
        Dialect::Chat => {
            if let Some(u) = parsed.get("usage") {
                if !u.is_null() {
                    *merged = Some(chat_usage_to_messages(u));
                }
            }
        }
        Dialect::Responses => {
            if parsed.get("type").and_then(Value::as_str) == Some("response.completed") {
                if let Some(u) = parsed.pointer("/response/usage") {
                    *merged = Some(conv_usage::responses_usage_to_messages(u));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Non-stream conversion
// ---------------------------------------------------------------------------

/// Convert a complete upstream body to the inlet dialect.
/// Returns (client body, messages-shape usage).
fn convert_nonstream(
    inlet: Dialect,
    channel: Dialect,
    raw: &[u8],
    display_model: &str,
) -> Result<(Value, Option<Value>), String> {
    match channel {
        Dialect::Messages => {
            let msg = aggregate::parse_message_body(raw)
                .ok_or_else(|| "failed to parse upstream messages body".to_string())?;
            if msg.get("type").and_then(Value::as_str) == Some("error") {
                return Err(msg
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_string());
            }
            let usage = msg.get("usage").cloned();
            let body = match inlet {
                Dialect::Messages => msg,
                Dialect::Chat => conv_chat::response_from_message(&msg, display_model),
                Dialect::Responses => conv_responses::response_from_message(&msg, display_model),
            };
            Ok((body, usage))
        }
        Dialect::Responses => {
            let resp = aggregate::parse_response_body(raw)
                .ok_or_else(|| "failed to parse upstream response body".to_string())?;
            let msg = conv_messages::response_from_response(&resp, display_model);
            let usage = msg.get("usage").cloned();
            let body = match inlet {
                Dialect::Messages => msg,
                Dialect::Chat => conv_chat::response_from_message(&msg, display_model),
                Dialect::Responses => {
                    // Same dialect: echo the upstream response with the display model.
                    let mut r = resp;
                    if let Some(obj) = r.as_object_mut() {
                        obj.insert("model".into(), json!(display_model));
                    }
                    r
                }
            };
            Ok((body, usage))
        }
        Dialect::Chat => {
            let v: Value = serde_json::from_slice(raw)
                .map_err(|_| "failed to parse upstream chat body".to_string())?;
            if let Some(err) = v.get("error") {
                if !err.is_null() {
                    return Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("upstream error")
                        .to_string());
                }
            }
            let usage = v.get("usage").map(chat_usage_to_messages);
            let body = match inlet {
                Dialect::Chat => v,
                Dialect::Messages => {
                    conv_chat_upstream::response_from_completion(&v, display_model)
                }
                Dialect::Responses => {
                    let msg = conv_chat_upstream::response_from_completion(&v, display_model);
                    conv_responses::response_from_message(&msg, display_model)
                }
            };
            Ok((body, usage))
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline entry
// ---------------------------------------------------------------------------

struct RequestMeta {
    model: String,
    stream: bool,
    include_usage: bool,
}

fn request_meta(inlet: Dialect, body: &Value) -> RequestMeta {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = match inlet {
        Dialect::Chat => body
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => true,
    };
    RequestMeta {
        model,
        stream,
        include_usage,
    }
}

fn upstream_request(
    state: &GatewayState,
    channel: &GatewayChannel,
    body: &Value,
    stream: bool,
) -> reqwest::RequestBuilder {
    let mut req = state
        .http_client
        .post(channel.endpoint_url())
        .header("content-type", "application/json");
    req = match channel.dialect {
        Dialect::Messages => req
            .header("x-api-key", &channel.api_key)
            .header("anthropic-version", "2023-06-01"),
        _ => req.header("authorization", format!("Bearer {}", channel.api_key)),
    };
    if stream {
        req = req.header("accept", "text/event-stream");
    }
    for (name, value) in &channel.extra_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    req.body(body.to_string())
}

/// Should this upstream failure trigger failover to the next candidate?
fn failover_worthy(status: u16) -> bool {
    status == 401 || status == 403 || status == 408 || status == 429 || status >= 500
}

/// Model availability is often scoped to one of a relay's API dialects. A
/// recognized "this model is not served here" response should therefore try
/// the station's next interface, while ordinary validation errors still return
/// immediately to the client.
fn model_unavailable_response(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 404 | 422) {
        return false;
    }
    let message = body.to_ascii_lowercase();
    let names_model = message.contains("model") || message.contains("模型");
    names_model
        && [
            "not found",
            "not supported",
            "unsupported",
            "does not exist",
            "not available",
            "unknown model",
            "invalid model",
            "模型不存在",
            "不支持",
            "无效模型",
        ]
        .iter()
        .any(|needle| message.contains(needle))
}

/// Prefer the client's native wire format, then the richer Messages ↔
/// Responses bridge, and keep conversions involving Chat as the last automatic
/// choice because Chat cannot preserve every advanced reasoning capability.
fn dialect_conversion_rank(inlet: Dialect, upstream: Dialect) -> u8 {
    if inlet == upstream {
        0
    } else if !matches!(inlet, Dialect::Chat) && !matches!(upstream, Dialect::Chat) {
        1
    } else {
        2
    }
}

#[allow(clippy::too_many_arguments)]
fn log_usage(
    db: &Database,
    channel_id: &str,
    key: Option<&GatewayKey>,
    client_model: &str,
    upstream_model: &str,
    usage: TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    status: u16,
    is_streaming: bool,
) {
    if !usage.has_billable_tokens() {
        return;
    }
    let logger = UsageLogger::new(db);
    let pricing_model = usage
        .model
        .clone()
        .unwrap_or_else(|| upstream_model.to_string());
    if let Err(e) = logger.log_with_calculation(
        uuid::Uuid::new_v4().to_string(),
        channel_id.to_string(),
        "gateway".to_string(),
        client_model.to_string(),
        upstream_model.to_string(),
        pricing_model,
        usage,
        rust_decimal::Decimal::ONE,
        latency_ms,
        first_token_ms,
        status,
        key.map(|k| k.name.clone()),
        Some("gateway".to_string()),
        is_streaming,
    ) {
        log::warn!("[gateway] usage log failed: {e}");
    }
}

/// Run one inference request through the gateway.
pub async fn run(
    state: GatewayState,
    inlet: Dialect,
    raw_body: bytes::Bytes,
    key: Option<GatewayKey>,
) -> PipelineOutcome {
    let body: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(e) => {
            return PipelineOutcome::Json {
                status: 400,
                body: error_body(inlet, &format!("invalid JSON body: {e}")),
            }
        }
    };
    let route = match route_for_key(&state.db, key.as_ref()) {
        Ok(route) => route,
        Err(message) => {
            return PipelineOutcome::Json {
                status: 503,
                body: error_body(inlet, &message),
            }
        }
    };
    let mut meta = request_meta(inlet, &body);
    if meta.model.is_empty() {
        if let Some(default_model) = route
            .as_ref()
            .and_then(|route| route.default_model.as_deref())
        {
            meta.model = default_model.to_string();
        }
    }
    if meta.model.is_empty() {
        return PipelineOutcome::Json {
            status: 400,
            body: error_body(inlet, "missing model field"),
        };
    }

    let channels = match state.db.get_gateway_channels() {
        Ok(c) => c,
        Err(e) => {
            return PipelineOutcome::Json {
                status: 500,
                body: error_body(inlet, &format!("channel lookup failed: {e}")),
            }
        }
    };
    let rule = route
        .as_ref()
        .and_then(|route| route.rule_for_model(&meta.model))
        .cloned();
    let route_model_override = match rule.as_ref() {
        Some(rule) => rule.upstream_model_override(),
        None => route
            .as_ref()
            .and_then(|route| route.default_model.as_deref()),
    };
    let remote_compaction = is_remote_compaction_request(inlet, &body);
    let convertible: Vec<GatewayChannel> = channels
        .into_iter()
        .filter(|channel| channel_supports_request(inlet, channel.dialect, remote_compaction))
        .filter(|channel| {
            route
                .as_ref()
                .is_none_or(|route| route.allows_channel(&channel.id))
        })
        .filter(|channel| {
            rule.as_ref()
                .and_then(|rule| rule.channel_id.as_deref())
                .is_none_or(|channel_id| channel.id == channel_id)
        })
        .filter(|channel| {
            rule.as_ref()
                .and_then(|rule| rule.dialect)
                .is_none_or(|dialect| channel.dialect == dialect)
        })
        .collect();

    let health = state.health.read().await.clone();
    let unhealthy =
        |c: &GatewayChannel| matches!(health.get(&c.id), Some(ChannelHealth::Unhealthy(_)));
    // Upstream channel model filters describe the name that will actually be
    // sent upstream, not the client-facing alias.
    let routing_model = route_model_override.unwrap_or(&meta.model);
    let rank = |channel: &GatewayChannel| dialect_conversion_rank(inlet, channel.dialect);
    let mut candidates =
        candidates_for_model_ranked(&convertible, routing_model, unhealthy, rank, entropy);
    if candidates.is_empty() {
        // All matching channels may be marked unhealthy — retry without the
        // health filter rather than failing outright.
        candidates =
            candidates_for_model_ranked(&convertible, routing_model, |_| false, rank, entropy);
    }
    if candidates.is_empty() {
        let message = if remote_compaction {
            "remote compaction requires an OpenAI Responses upstream".to_string()
        } else {
            format!("no gateway channel serves model '{}'", meta.model)
        };
        return PipelineOutcome::Json {
            status: 503,
            body: error_body(inlet, &message),
        };
    }

    let started = Instant::now();
    let mut last_error = String::from("all channels failed");

    for channel in candidates {
        let prepared = match prepare_request(
            inlet,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: &meta.model,
                route_model_override,
                reasoning: route.as_ref().map(|route| &route.reasoning),
                client_stream: meta.stream,
            },
            &state.signatures,
        ) {
            Ok(p) => p,
            Err(e) => {
                last_error = format!("request conversion failed: {e}");
                continue;
            }
        };

        let resp = match upstream_request(&state, &channel, &prepared.body, meta.stream)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("channel '{}' unreachable: {e}", channel.name);
                mark_health(&state, &channel.id, ChannelHealth::Unhealthy(e.to_string())).await;
                continue;
            }
        };

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            last_error = format!("channel '{}' returned {status}: {snippet}", channel.name);
            if status >= 500 {
                mark_health(
                    &state,
                    &channel.id,
                    ChannelHealth::Unhealthy(format!("HTTP {status}")),
                )
                .await;
            }
            if failover_worthy(status) || model_unavailable_response(status, &text) {
                continue;
            }
            return PipelineOutcome::Json {
                status,
                body: error_body(inlet, &last_error),
            };
        }

        mark_health(&state, &channel.id, ChannelHealth::Healthy).await;

        if meta.stream {
            return stream_response(state, inlet, channel, prepared, meta, key, started, resp);
        }

        // Non-stream: drain and convert.
        let raw = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return PipelineOutcome::Json {
                    status: 502,
                    body: error_body(inlet, &format!("upstream body read failed: {e}")),
                }
            }
        };
        match convert_nonstream(inlet, channel.dialect, &raw, &meta.model) {
            Ok((client_body, usage)) => {
                if let Some(u) = &usage {
                    log_usage(
                        &state.db,
                        &channel.id,
                        key.as_ref(),
                        &meta.model,
                        &prepared.upstream_model,
                        token_usage_from_messages(u, Some(prepared.upstream_model.clone())),
                        started.elapsed().as_millis() as u64,
                        None,
                        200,
                        false,
                    );
                }
                // Store thinking signatures for messages-dialect follow-ups.
                if let Some(content) = client_body.get("content").and_then(Value::as_array) {
                    if let Some(capture) = ochub_convert::signature::capture_from_content(content) {
                        ochub_convert::signature::store_capture(&*state.signatures, &capture);
                    }
                }
                return PipelineOutcome::Json {
                    status: 200,
                    body: client_body,
                };
            }
            Err(e) => {
                return PipelineOutcome::Json {
                    status: 502,
                    body: error_body(inlet, &e),
                }
            }
        }
    }

    PipelineOutcome::Json {
        status: 502,
        body: error_body(inlet, &last_error),
    }
}

fn route_for_key(db: &Database, key: Option<&GatewayKey>) -> Result<Option<GatewayRoute>, String> {
    let Some(route_id) = key.and_then(|key| key.route_id.as_deref()) else {
        return Ok(None);
    };
    match db.get_gateway_route_by_id(route_id) {
        Ok(Some(route)) if route.enabled => Ok(Some(route)),
        Ok(Some(_)) => Err("当前绑定的转发站已停用".to_string()),
        Ok(None) => Err("当前绑定的转发站不存在".to_string()),
        Err(err) => Err(format!("读取转发站绑定失败: {err}")),
    }
}

fn entropy() -> u64 {
    // uuid v4 is backed by the OS RNG; take 8 bytes as the routing ticket.
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

async fn mark_health(state: &GatewayState, channel_id: &str, health: ChannelHealth) {
    state
        .health
        .write()
        .await
        .insert(channel_id.to_string(), health);
}

/// Spawn the upstream-reading task and hand the caller a frame receiver.
#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: GatewayState,
    inlet: Dialect,
    channel: GatewayChannel,
    prepared: PreparedRequest,
    meta: RequestMeta,
    key: Option<GatewayKey>,
    started: Instant,
    resp: reqwest::Response,
) -> PipelineOutcome {
    let (tx, rx) = mpsc::channel::<StreamFrame>(64);
    tokio::spawn(async move {
        let mut converter =
            match StreamConverter::new(inlet, channel.dialect, &meta.model, meta.include_usage) {
                Some(c) => c,
                None => return,
            };
        let mut parser = SseParser::new();
        let mut last_usage: Option<Value> = None;
        let mut first_token_ms: Option<u64> = None;
        let mut capture: Option<SignatureCapture> = None;
        let mut done_sent = false;

        let mut body = resp.bytes_stream();
        use futures::StreamExt;
        'outer: loop {
            let chunk = body.next().await;
            let (events, ended) = match chunk {
                Some(Ok(bytes)) => (parser.feed(&bytes), false),
                Some(Err(e)) => {
                    log::warn!("[gateway] upstream stream error: {e}");
                    (parser.finish(), true)
                }
                None => (parser.finish(), true),
            };
            for ev in &events {
                if first_token_ms.is_none() {
                    first_token_ms = Some(started.elapsed().as_millis() as u64);
                }
                let converted = converter.push(ev);
                if let Some(u) = converted.usage {
                    last_usage = Some(u);
                }
                if let Some(c) = converted.capture {
                    capture = Some(c);
                }
                for frame in converted.frames {
                    let is_done = matches!(frame, StreamFrame::Done);
                    if tx.send(frame).await.is_err() {
                        break 'outer; // client hung up
                    }
                    if is_done {
                        done_sent = true;
                        break 'outer;
                    }
                }
            }
            if ended {
                break;
            }
        }

        if !done_sent {
            let _ = tx.send(StreamFrame::Done).await;
        }
        if let Some(c) = &capture {
            ochub_convert::signature::store_capture(&*state.signatures, c);
        }
        if let Some(u) = &last_usage {
            log_usage(
                &state.db,
                &channel.id,
                key.as_ref(),
                &meta.model,
                &prepared.upstream_model,
                token_usage_from_messages(u, Some(prepared.upstream_model.clone())),
                started.elapsed().as_millis() as u64,
                first_token_ms,
                200,
                true,
            );
        }
    });
    PipelineOutcome::Stream { rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_matrix() {
        use Dialect::*;
        for inlet in [Messages, Chat, Responses] {
            for channel in [Messages, Chat, Responses] {
                assert!(conversion_supported(inlet, channel));
            }
        }
    }

    #[test]
    fn remote_compaction_is_restricted_to_responses_channels() {
        let compact = json!({
            "model": "gpt-5.6",
            "input": [
                { "role": "user", "content": "long context" },
                { "type": "compaction_trigger" }
            ]
        });
        assert!(is_remote_compaction_request(Dialect::Responses, &compact));
        assert!(channel_supports_request(
            Dialect::Responses,
            Dialect::Responses,
            true
        ));
        assert!(!channel_supports_request(
            Dialect::Responses,
            Dialect::Messages,
            true
        ));
        assert!(!channel_supports_request(
            Dialect::Responses,
            Dialect::Chat,
            true
        ));

        let ordinary = json!({
            "model": "gpt-5.6",
            "input": [{ "role": "user", "content": "hello" }]
        });
        assert!(!is_remote_compaction_request(Dialect::Responses, &ordinary));
        assert!(channel_supports_request(
            Dialect::Responses,
            Dialect::Messages,
            false
        ));
    }

    #[test]
    fn native_and_rich_conversion_paths_rank_before_chat() {
        assert_eq!(
            dialect_conversion_rank(Dialect::Messages, Dialect::Messages),
            0
        );
        assert_eq!(
            dialect_conversion_rank(Dialect::Messages, Dialect::Responses),
            1
        );
        assert_eq!(dialect_conversion_rank(Dialect::Messages, Dialect::Chat), 2);
    }

    #[test]
    fn only_model_availability_validation_errors_trigger_failover() {
        assert!(model_unavailable_response(
            400,
            r#"{"error":{"message":"model gpt-x is not supported"}}"#
        ));
        assert!(model_unavailable_response(
            404,
            r#"{"message":"模型不存在"}"#
        ));
        assert!(!model_unavailable_response(
            400,
            r#"{"error":{"message":"messages is required"}}"#
        ));
        assert!(!model_unavailable_response(
            401,
            r#"{"message":"unknown model"}"#
        ));
    }

    #[test]
    fn chat_usage_maps_to_exclusive_input() {
        let u = json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": { "cached_tokens": 30, "cache_creation_input_tokens": 20 }
        });
        let m = chat_usage_to_messages(&u);
        assert_eq!(m["input_tokens"], 50);
        assert_eq!(m["cache_read_input_tokens"], 30);
        assert_eq!(m["cache_creation_input_tokens"], 20);
        assert_eq!(m["output_tokens"], 7);
    }

    #[test]
    fn prepare_request_same_dialect_overrides_model_and_stream() {
        let store = ochub_convert::MemorySignatureStore::default();
        let channel = GatewayChannel {
            id: "c".into(),
            endpoint_id: None,
            name: "c".into(),
            dialect: Dialect::Messages,
            base_url: "https://x".into(),
            api_key: String::new(),
            path_override: None,
            models: vec![],
            model_override: Some("upstream-m".into()),
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        };
        let body = json!({ "model": "client-m", "max_tokens": 5, "messages": [], "stream": false });
        let p = prepare_request(
            Dialect::Messages,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: "client-m",
                route_model_override: None,
                reasoning: None,
                client_stream: true,
            },
            &store,
        )
        .unwrap();
        assert_eq!(p.body["model"], "upstream-m");
        assert_eq!(p.body["stream"], true);
        assert_eq!(p.upstream_model, "upstream-m");
    }

    #[test]
    fn prepare_request_chat_to_responses_pivots() {
        let store = ochub_convert::MemorySignatureStore::default();
        let channel = GatewayChannel {
            id: "c".into(),
            endpoint_id: None,
            name: "c".into(),
            dialect: Dialect::Responses,
            base_url: "https://x".into(),
            api_key: String::new(),
            path_override: None,
            models: vec![],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        };
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"}
            ],
            "stream": true
        });
        let p = prepare_request(
            Dialect::Chat,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: "m",
                route_model_override: None,
                reasoning: None,
                client_stream: true,
            },
            &store,
        )
        .unwrap();
        assert_eq!(p.body["model"], "m");
        assert_eq!(p.body["instructions"], "sys");
        assert_eq!(p.body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn prepare_request_messages_to_chat_converts_and_overrides() {
        let store = ochub_convert::MemorySignatureStore::default();
        let channel = GatewayChannel {
            id: "c".into(),
            endpoint_id: None,
            name: "c".into(),
            dialect: Dialect::Chat,
            base_url: "https://x".into(),
            api_key: String::new(),
            path_override: None,
            models: vec![],
            model_override: Some("upstream-m".into()),
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        };
        let body = json!({
            "model": "client-m",
            "max_tokens": 100,
            "system": [{"type": "text", "text": "sys"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "stream": true
        });
        let p = prepare_request(
            Dialect::Messages,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: "client-m",
                route_model_override: None,
                reasoning: None,
                client_stream: true,
            },
            &store,
        )
        .unwrap();
        assert_eq!(p.body["model"], "upstream-m");
        assert_eq!(p.body["messages"][0]["role"], "system");
        assert_eq!(p.body["messages"][0]["content"], "sys");
        assert_eq!(p.body["messages"][1]["content"], "hi");
        assert_eq!(p.body["stream"], true);
        assert_eq!(p.body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn prepare_request_responses_to_chat_pivots() {
        let store = ochub_convert::MemorySignatureStore::default();
        let channel = GatewayChannel {
            id: "c".into(),
            endpoint_id: None,
            name: "c".into(),
            dialect: Dialect::Chat,
            base_url: "https://x".into(),
            api_key: String::new(),
            path_override: None,
            models: vec![],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        };
        let body = json!({
            "model": "m",
            "instructions": "sys",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "hi"}]}],
            "stream": true
        });
        let p = prepare_request(
            Dialect::Responses,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: "m",
                route_model_override: None,
                reasoning: None,
                client_stream: true,
            },
            &store,
        )
        .unwrap();
        assert_eq!(p.body["model"], "m");
        assert_eq!(p.body["messages"][0]["role"], "system");
        assert_eq!(p.body["messages"][1]["role"], "user");
        assert_eq!(p.body["messages"][1]["content"], "hi");
    }

    #[test]
    fn chat_upstream_stream_reaches_messages_client() {
        let mut conv =
            StreamConverter::new(Dialect::Messages, Dialect::Chat, "display-x", true).unwrap();
        let chunks = [
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk",
                   "choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},
                               "finish_reason":null}]})
            .to_string(),
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk",
                   "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            .to_string(),
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[],
                   "usage":{"prompt_tokens":8,"completion_tokens":3}})
            .to_string(),
            "[DONE]".to_string(),
        ];
        let mut names: Vec<String> = Vec::new();
        let mut usage = None;
        let mut done = false;
        for data in &chunks {
            let out = conv.push(&WireEvent::data_only(data.clone()));
            for frame in out.frames {
                match frame {
                    StreamFrame::Event(e) => names.push(e.event.unwrap_or_default()),
                    StreamFrame::Done => done = true,
                }
            }
            if let Some(u) = out.usage {
                usage = Some(u);
            }
        }
        assert!(done);
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(usage.unwrap()["output_tokens"], 3);
    }

    #[test]
    fn chat_upstream_stream_chains_to_responses_client() {
        let mut conv =
            StreamConverter::new(Dialect::Responses, Dialect::Chat, "display-x", true).unwrap();
        let chunks = [
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk",
                   "choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},
                               "finish_reason":null}]})
            .to_string(),
            json!({"id":"chatcmpl-1","object":"chat.completion.chunk",
                   "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            .to_string(),
            "[DONE]".to_string(),
        ];
        let mut names: Vec<String> = Vec::new();
        let mut done = false;
        for data in &chunks {
            let out = conv.push(&WireEvent::data_only(data.clone()));
            for frame in out.frames {
                match frame {
                    StreamFrame::Event(e) => names.push(e.event.unwrap_or_default()),
                    StreamFrame::Done => done = true,
                }
            }
        }
        assert!(done);
        assert!(names.first().is_some_and(|n| n == "response.created"));
        assert!(names.iter().any(|n| n == "response.output_text.delta"));
        assert!(names.last().is_some_and(|n| n == "response.completed"));
    }

    #[test]
    fn passthrough_tap_merges_messages_usage() {
        let mut conv = StreamConverter::Passthrough {
            inlet: Dialect::Messages,
            merged_usage: None,
        };
        let start = WireEvent::new(
            "message_start",
            json!({"type":"message_start","message":{"usage":{"input_tokens":9,"output_tokens":1}}})
                .to_string(),
        );
        let delta = WireEvent::new(
            "message_delta",
            json!({"type":"message_delta","usage":{"output_tokens":5}}).to_string(),
        );
        let stop = WireEvent::new("message_stop", json!({"type":"message_stop"}).to_string());
        conv.push(&start);
        conv.push(&delta);
        let out = conv.push(&stop);
        let usage = out.usage.unwrap();
        assert_eq!(usage["input_tokens"], 9);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(out.frames.len(), 2); // message_stop event + Done
        assert!(matches!(out.frames[1], StreamFrame::Done));
    }

    /// End-to-end: chat client → gateway → mock messages upstream (SSE),
    /// verifying converted chunks, the Done frame, and the usage log row.
    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_chat_client_over_messages_upstream() {
        const UPSTREAM_SSE: &str = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":2}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        // Mock upstream speaking the messages dialect over SSE.
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|| async {
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from(UPSTREAM_SSE))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "ch1".into(),
            endpoint_id: Some("mock".into()),
            name: "mock".into(),
            dialect: Dialect::Messages,
            base_url: format!("http://{addr}"),
            api_key: "k".into(),
            path_override: None,
            models: vec![],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        })
        .unwrap();

        let state = GatewayState {
            db: db.clone(),
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };

        let body = json!({
            "model": "test-model",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let outcome = run(
            state,
            Dialect::Chat,
            bytes::Bytes::from(body.to_string()),
            None,
        )
        .await;

        let PipelineOutcome::Stream { mut rx } = outcome else {
            panic!("expected stream outcome");
        };
        let mut datas: Vec<String> = Vec::new();
        let mut got_done = false;
        while let Some(frame) = rx.recv().await {
            match frame {
                StreamFrame::Event(ev) => {
                    assert_eq!(ev.event, None); // chat chunks are data-only
                    datas.push(ev.data);
                }
                StreamFrame::Done => {
                    got_done = true;
                    break;
                }
            }
        }
        assert!(got_done);
        assert!(datas.iter().any(|d| d.contains("\"content\":\"Hello\"")));
        assert!(datas
            .iter()
            .any(|d| d.contains("\"finish_reason\":\"stop\"")));
        // Terminal usage chunk: prompt = 10 + 2 cached.
        let usage_chunk = datas.last().unwrap();
        assert!(
            usage_chunk.contains("\"prompt_tokens\":12"),
            "{usage_chunk}"
        );
        assert!(usage_chunk.contains("\"completion_tokens\":6"));
        // All chunks echo the client's model name.
        assert!(datas.iter().all(|d| d.contains("\"model\":\"test-model\"")));

        // Usage row recorded (the logging task races the Done frame slightly).
        let mut logged = 0i64;
        for _ in 0..50 {
            logged = {
                let conn = db.conn.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM usage_logs WHERE app_type = 'gateway'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            };
            if logged > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(logged, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn route_isolates_upstream_and_maps_model_and_reasoning() {
        let received = Arc::new(std::sync::Mutex::new(None::<Value>));
        let received_for_handler = received.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let received = received_for_handler.clone();
                async move {
                    *received.lock().unwrap() = Some(body);
                    axum::Json(json!({
                        "id": "msg_route",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-6",
                        "content": [{ "type": "text", "text": "routed" }],
                        "stop_reason": "end_turn",
                        "usage": { "input_tokens": 2, "output_tokens": 1 }
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "allowed".into(),
            endpoint_id: Some("allowed-endpoint".into()),
            name: "allowed".into(),
            dialect: Dialect::Messages,
            base_url: format!("http://{addr}"),
            api_key: "k".into(),
            path_override: None,
            // This deliberately matches only the mapped upstream model, not
            // the client-facing alias.
            models: vec!["claude-sonnet-*".into()],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        })
        .unwrap();
        db.upsert_gateway_channel(&GatewayChannel {
            id: "blocked".into(),
            endpoint_id: Some("blocked-endpoint".into()),
            name: "blocked".into(),
            dialect: Dialect::Chat,
            base_url: "http://127.0.0.1:9".into(),
            api_key: "k".into(),
            path_override: None,
            models: vec![],
            model_override: None,
            priority: -10,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        })
        .unwrap();
        db.upsert_gateway_route(&GatewayRoute {
            id: "route-test".into(),
            name: "test".into(),
            website_url: None,
            app_type: Some("claude".into()),
            channel_ids: vec!["allowed".into(), "blocked".into()],
            default_model: None,
            model_rules: vec![crate::gateway::types::GatewayModelRule {
                model: "sonnet".into(),
                upstream_model: "claude-sonnet-4-6".into(),
                channel_id: None,
                dialect: Some(Dialect::Messages),
            }],
            reasoning: GatewayReasoningConfig {
                mode: GatewayReasoningMode::Auto,
                low_budget: 7_777,
                ..GatewayReasoningConfig::default()
            },
            enabled: true,
            created_at: 1,
        })
        .unwrap();
        let key = GatewayKey {
            id: "key-route".into(),
            name: "claude".into(),
            key: "rd-route".into(),
            route_id: Some("route-test".into()),
            enabled: true,
            created_at: 1,
        };
        let state = GatewayState {
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let request = json!({
            "model": "sonnet",
            "messages": [{ "role": "user", "content": "hi" }],
            "reasoning_effort": "low",
            "stream": false
        });
        let outcome = run(
            state,
            Dialect::Chat,
            bytes::Bytes::from(request.to_string()),
            Some(key),
        )
        .await;
        let PipelineOutcome::Json { status, body } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["model"], "sonnet");
        assert_eq!(body["choices"][0]["message"]["content"], "routed");

        let upstream = received.lock().unwrap().clone().unwrap();
        assert_eq!(upstream["model"], "claude-sonnet-4-6");
        assert_eq!(upstream["thinking"]["budget_tokens"], 7_777);
    }

    #[test]
    fn chained_responses_to_chat_converts() {
        let mut conv = StreamConverter::new(Dialect::Chat, Dialect::Responses, "disp", true)
            .expect("supported");
        let mut frames = Vec::new();
        let events = [
            (
                "response.created",
                json!({"type":"response.created","response":{"id":"r1"}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1","role":"assistant","content":[]}}),
            ),
            (
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","delta":"Hi"}),
            ),
            (
                "response.output_text.done",
                json!({"type":"response.output_text.done","text":"Hi"}),
            ),
            (
                "response.completed",
                json!({"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":3,"output_tokens":2}}}),
            ),
        ];
        let mut usage = None;
        for (name, data) in events {
            let out = conv.push(&WireEvent::new(name, data.to_string()));
            if let Some(u) = out.usage {
                usage = Some(u);
            }
            frames.extend(out.frames);
        }
        // Chat chunks came out with a Done at the end.
        assert!(matches!(frames.last().unwrap(), StreamFrame::Done));
        let chunk_datas: Vec<String> = frames
            .iter()
            .filter_map(|f| match f {
                StreamFrame::Event(e) => Some(e.data.clone()),
                _ => None,
            })
            .collect();
        assert!(chunk_datas.iter().any(|d| d.contains("\"content\":\"Hi\"")));
        let usage = usage.unwrap();
        assert_eq!(usage["input_tokens"], 3);
    }
}
