//! Gateway forwarding pipeline: inlet request → channel selection → dialect
//! conversion → upstream call → response conversion → usage logging.
//!
//! Transport-agnostic core: [`run`] returns either a JSON body or a stream of
//! [`StreamFrame`]s; the HTTP handler encodes frames as SSE and the WebSocket
//! handler sends them as text frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use ochub_convert::aggregate;
use ochub_convert::usage as conv_usage;
use ochub_convert::{
    MessagesRequestOptions, Output, ResponsesRequestOptions, SignatureCapture, SseParser,
    WireEvent, chat as conv_chat, chat_upstream as conv_chat_upstream, messages as conv_messages,
    responses as conv_responses,
};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use crate::db::Database;
use crate::gateway::router::candidates_for_model_ranked;
use crate::gateway::types::{
    ChannelHealth, Dialect, GatewayAppModelPolicy, GatewayChannel, GatewayConfig, GatewayKey,
    GatewayModelRule, GatewayReasoningConfig, GatewayReasoningMode, GatewayRoute,
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
    let cache_creation = usage.get("cache_creation");
    let nested = |key: &str| {
        cache_creation
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    let cache_creation_5m_tokens =
        nested("ephemeral_5m_input_tokens").max(g("claude_cache_creation_5_m_tokens"));
    let cache_creation_1h_tokens =
        nested("ephemeral_1h_input_tokens").max(g("claude_cache_creation_1_h_tokens"));
    let cache_creation_tokens = g("cache_creation_input_tokens")
        .max(cache_creation_5m_tokens.saturating_add(cache_creation_1h_tokens));
    TokenUsage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_creation_tokens,
        cache_creation_5m_tokens,
        cache_creation_1h_tokens,
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

pub(crate) struct PreparedWsCandidate {
    pub channel: GatewayChannel,
    pub frame: String,
    pub client_model: String,
    pub upstream_model: String,
}

pub(crate) struct WsPreparationError {
    pub status: u16,
    pub body: Value,
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
    if let Some(name) = &ev.event
        && !name.is_empty()
    {
        return Some(name.clone());
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
            if let Some(u) = parsed.get("usage")
                && !u.is_null()
            {
                *merged = Some(chat_usage_to_messages(u));
            }
        }
        Dialect::Responses => {
            if parsed.get("type").and_then(Value::as_str) == Some("response.completed")
                && let Some(u) = parsed.pointer("/response/usage")
            {
                *merged = Some(conv_usage::responses_usage_to_messages(u));
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
            if let Some(err) = v.get("error")
                && !err.is_null()
            {
                return Err(err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_string());
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

/// Headers the gateway owns, and so must not copy from the client.
///
/// Everything else a client sends is forwarded: `anthropic-beta`,
/// `user-agent`, `x-stainless-*`, and whatever tracing headers a tool adds are
/// all things the upstream may act on, and dropping them silently changes the
/// request the user thinks they are making.
///
/// The exclusions are the headers that describe *this* hop rather than the
/// request:
///
/// - **Credentials.** The gateway authenticates to the upstream with the
///   channel's own key. The client's key authenticates it to the gateway and
///   means nothing beyond it; forwarding would send two and let the upstream
///   choose.
/// - **Hop-by-hop.** Scoped to a single connection by RFC 9110 §7.6.1, so they
///   are meaningless — or actively wrong — to the next hop.
/// - **Body description.** The gateway rebuilds the body, and a converted
///   request rarely has the length the client announced.
/// - **`accept-encoding`.** This crate's reqwest has no decompression feature
///   enabled, so forwarding `gzip` would return bytes nothing here can read.
/// - **`sec-websocket-*`.** Generated per connection by the WebSocket client
///   request builder on the upstream hop.
const GATEWAY_OWNED_HEADERS: &[&str] = &[
    // Credentials.
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    // Hop-by-hop.
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "host",
    // Body description, rebuilt per hop.
    "content-length",
    "content-type",
    "accept-encoding",
    // Per-connection WebSocket handshake.
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-accept",
    "sec-websocket-protocol",
];

fn is_gateway_owned_header(name: &HeaderName) -> bool {
    GATEWAY_OWNED_HEADERS.contains(&name.as_str())
}

/// The client's headers, minus the ones this hop owns.
pub(crate) fn forwardable_client_headers(client_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in client_headers {
        if is_gateway_owned_header(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// Authenticate to the upstream as the channel, replacing whatever the client
/// used to authenticate to the gateway.
///
/// `anthropic-version` is only defaulted, not forced: a Messages client that
/// pinned a version knows which response shape it parses, and the gateway has
/// no reason to overrule it.
fn apply_channel_auth(headers: &mut HeaderMap, channel: &GatewayChannel) {
    match channel.dialect {
        Dialect::Messages => {
            if let Ok(value) = HeaderValue::from_str(&channel.api_key) {
                headers.insert("x-api-key", value);
            }
            if !headers.contains_key("anthropic-version") {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        _ => {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", channel.api_key)) {
                headers.insert(header::AUTHORIZATION, value);
            }
        }
    }
}

/// Channel-configured headers win over anything forwarded: they are the
/// operator's explicit statement about this upstream.
fn apply_extra_headers(headers: &mut HeaderMap, channel: &GatewayChannel) {
    for (name, value) in &channel.extra_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

fn upstream_request(
    state: &GatewayState,
    channel: &GatewayChannel,
    body: &Value,
    stream: bool,
    client_headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut headers = forwardable_client_headers(client_headers);

    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    apply_channel_auth(&mut headers, channel);
    apply_extra_headers(&mut headers, channel);

    state
        .http_client
        .post(channel.endpoint_url())
        .headers(headers)
        .body(body.to_string())
}

fn count_tokens_request(
    state: &GatewayState,
    channel: &GatewayChannel,
    body: &Value,
    client_headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let url = format!(
        "{}/count_tokens",
        channel.endpoint_url().trim_end_matches('/')
    );
    let mut headers = forwardable_client_headers(client_headers);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    apply_channel_auth(&mut headers, channel);
    apply_extra_headers(&mut headers, channel);

    state
        .http_client
        .post(url)
        .headers(headers)
        .body(body.to_string())
}

fn local_token_estimate(body: &Value) -> u64 {
    fn collect_text_len(value: &Value, chars: &mut usize) {
        match value {
            Value::String(text) => *chars += text.len(),
            Value::Array(values) => values
                .iter()
                .for_each(|value| collect_text_len(value, chars)),
            Value::Object(values) => values
                .values()
                .for_each(|value| collect_text_len(value, chars)),
            _ => {}
        }
    }

    let mut chars = 0usize;
    collect_text_len(body, &mut chars);
    (chars / 4).max(1) as u64
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

fn request_model_rule<'a>(
    policy: Option<&'a GatewayAppModelPolicy>,
    route: Option<&'a GatewayRoute>,
    model: &str,
) -> Option<&'a GatewayModelRule> {
    match policy {
        Some(policy) => policy.rule_for_model(model),
        None => route.and_then(|route| route.rule_for_model(model)),
    }
}

fn request_model_override<'a>(
    rule: Option<&'a GatewayModelRule>,
    policy: Option<&'a GatewayAppModelPolicy>,
    route: Option<&'a GatewayRoute>,
) -> Option<&'a str> {
    match rule {
        // A matched rule with no target is an explicit pass-through and must
        // suppress the unmatched fallback.
        Some(rule) => rule.upstream_model_override(),
        None => match policy {
            Some(policy) => policy.fallback_model.as_deref(),
            None => route.and_then(|route| route.default_model.as_deref()),
        },
    }
}

/// Count a Messages request with the same route and model policy as inference.
///
/// Exact counting is delegated to an eligible Messages upstream. The local
/// character estimate is used only when the effective route has no Messages
/// channel that can serve the mapped model.
pub async fn count_tokens(
    state: GatewayState,
    raw_body: bytes::Bytes,
    key: Option<GatewayKey>,
    client_headers: HeaderMap,
) -> PipelineOutcome {
    let body: Value = match serde_json::from_slice(&raw_body) {
        Ok(value) => value,
        Err(error) => {
            return PipelineOutcome::Json {
                status: 400,
                body: error_body(Dialect::Messages, &format!("invalid JSON body: {error}")),
            };
        }
    };
    let route = match route_for_key(&state.db, key.as_ref()) {
        Ok(route) => route,
        Err(message) => {
            return PipelineOutcome::Json {
                status: 503,
                body: error_body(Dialect::Messages, &message),
            };
        }
    };
    let model_policy = key.as_ref().and_then(|key| key.model_policy.as_ref());
    let mut client_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if client_model.is_empty() {
        let default_model = model_policy
            .and_then(|policy| {
                policy
                    .preferred_model
                    .as_deref()
                    .or(policy.fallback_model.as_deref())
            })
            .or_else(|| {
                if model_policy.is_none() {
                    route
                        .as_ref()
                        .and_then(|route| route.default_model.as_deref())
                } else {
                    None
                }
            });
        if let Some(default_model) = default_model {
            client_model = default_model.to_string();
        }
    }
    if client_model.is_empty() {
        return PipelineOutcome::Json {
            status: 400,
            body: error_body(Dialect::Messages, "missing model field"),
        };
    }

    let channels = match state.db.get_gateway_channels() {
        Ok(channels) => channels,
        Err(error) => {
            return PipelineOutcome::Json {
                status: 500,
                body: error_body(
                    Dialect::Messages,
                    &format!("channel lookup failed: {error}"),
                ),
            };
        }
    };
    let rule = request_model_rule(model_policy, route.as_ref(), &client_model).cloned();
    let route_model_override = request_model_override(rule.as_ref(), model_policy, route.as_ref());
    let routing_model = route_model_override.unwrap_or(&client_model);
    let messages_channels: Vec<GatewayChannel> = channels
        .into_iter()
        .filter(|channel| channel.dialect == Dialect::Messages)
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
        .filter(|_| {
            rule.as_ref()
                .and_then(|rule| rule.dialect)
                .is_none_or(|dialect| dialect == Dialect::Messages)
        })
        .collect();

    let health = state.health.read().await.clone();
    let unhealthy = |channel: &GatewayChannel| {
        matches!(health.get(&channel.id), Some(ChannelHealth::Unhealthy(_)))
    };
    let mut candidates =
        candidates_for_model_ranked(&messages_channels, routing_model, unhealthy, |_| 0, entropy);
    if candidates.is_empty() {
        candidates = candidates_for_model_ranked(
            &messages_channels,
            routing_model,
            |_| false,
            |_| 0,
            entropy,
        );
    }
    if candidates.is_empty() {
        return PipelineOutcome::Json {
            status: 200,
            body: json!({ "input_tokens": local_token_estimate(&body) }),
        };
    }

    let mut last_error = String::from("all Messages count_tokens channels failed");
    for channel in candidates {
        let upstream_model = route_model_override
            .map(str::to_string)
            .or_else(|| channel.model_override.clone())
            .unwrap_or_else(|| client_model.clone());
        let mut upstream_body = body.clone();
        apply_reasoning_policy(
            &mut upstream_body,
            Dialect::Messages,
            Dialect::Messages,
            route.as_ref().map(|route| &route.reasoning),
        );
        if let Some(object) = upstream_body.as_object_mut() {
            object.insert("model".into(), json!(upstream_model));
        }
        ochub_convert::signature::restore_thinking_blocks(&mut upstream_body, &*state.signatures);

        let response = match count_tokens_request(&state, &channel, &upstream_body, &client_headers)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("channel '{}' unreachable: {error}", channel.name);
                mark_health(
                    &state,
                    &channel.id,
                    ChannelHealth::Unhealthy(error.to_string()),
                )
                .await;
                continue;
            }
        };
        let status = response.status().as_u16();
        let raw = match response.bytes().await {
            Ok(raw) => raw,
            Err(error) => {
                last_error = format!(
                    "channel '{}' count_tokens body read failed: {error}",
                    channel.name
                );
                continue;
            }
        };
        let parsed = serde_json::from_slice::<Value>(&raw).ok();
        if status >= 400 {
            let snippet: String = String::from_utf8_lossy(&raw).chars().take(300).collect();
            last_error = format!("channel '{}' returned {status}: {snippet}", channel.name);
            if status >= 500 {
                mark_health(
                    &state,
                    &channel.id,
                    ChannelHealth::Unhealthy(format!("HTTP {status}")),
                )
                .await;
            }
            if failover_worthy(status)
                || model_unavailable_response(status, &String::from_utf8_lossy(&raw))
            {
                continue;
            }
            return PipelineOutcome::Json {
                status,
                body: parsed.unwrap_or_else(|| error_body(Dialect::Messages, &last_error)),
            };
        }
        let Some(parsed) = parsed else {
            last_error = format!(
                "channel '{}' returned invalid count_tokens JSON",
                channel.name
            );
            continue;
        };
        if parsed.get("input_tokens").and_then(Value::as_u64).is_none() {
            last_error = format!(
                "channel '{}' count_tokens response is missing input_tokens",
                channel.name
            );
            continue;
        }
        mark_health(&state, &channel.id, ChannelHealth::Healthy).await;
        return PipelineOutcome::Json {
            status: 200,
            body: parsed,
        };
    }

    PipelineOutcome::Json {
        status: 502,
        body: error_body(Dialect::Messages, &last_error),
    }
}

/// Resolve one downstream `response.create` frame to native Responses upstream
/// candidates. WebSocket transport is deliberately Responses-only: unlike the
/// HTTP pipeline, this path never converts to Messages/Chat or falls back to SSE.
pub(crate) fn responses_ws_available(
    state: &GatewayState,
    key: Option<&GatewayKey>,
) -> Result<bool, String> {
    let Some(route) = route_for_key(&state.db, key)? else {
        return Ok(false);
    };
    if !route.websocket_enabled {
        return Ok(false);
    }
    let channels = state
        .db
        .get_gateway_channels()
        .map_err(|error| format!("读取模型供应商接口失败: {error}"))?;
    Ok(channels.iter().any(|channel| {
        channel.enabled
            && channel.dialect == Dialect::Responses
            && route.allows_channel(&channel.id)
    }))
}

pub(crate) async fn prepare_responses_ws_turn(
    state: &GatewayState,
    frame: &str,
    key: Option<&GatewayKey>,
) -> Result<Vec<PreparedWsCandidate>, WsPreparationError> {
    let mut body: Value = serde_json::from_str(frame).map_err(|error| WsPreparationError {
        status: 400,
        body: error_body(Dialect::Responses, &format!("invalid JSON frame: {error}")),
    })?;
    let Some(object) = body.as_object_mut() else {
        return Err(WsPreparationError {
            status: 400,
            body: error_body(Dialect::Responses, "request frame must be a JSON object"),
        });
    };
    match object.get("type").and_then(Value::as_str) {
        Some("response.create") => {
            object.remove("type");
        }
        None => {}
        Some(event_type) => {
            return Err(WsPreparationError {
                status: 400,
                body: error_body(
                    Dialect::Responses,
                    &format!("unsupported WebSocket event type '{event_type}'"),
                ),
            });
        }
    }

    let route = route_for_key(&state.db, key).map_err(|message| WsPreparationError {
        status: 503,
        body: error_body(Dialect::Responses, &message),
    })?;
    if !route.as_ref().is_some_and(|route| route.websocket_enabled) {
        return Err(WsPreparationError {
            status: 426,
            body: error_body(
                Dialect::Responses,
                "Responses WebSocket is not enabled for this model provider",
            ),
        });
    }
    let model_policy = key.and_then(|key| key.model_policy.as_ref());
    let mut client_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if client_model.is_empty() {
        let default_model = model_policy
            .and_then(|policy| {
                policy
                    .preferred_model
                    .as_deref()
                    .or(policy.fallback_model.as_deref())
            })
            .or_else(|| {
                if model_policy.is_none() {
                    route
                        .as_ref()
                        .and_then(|route| route.default_model.as_deref())
                } else {
                    None
                }
            });
        if let Some(default_model) = default_model {
            client_model = default_model.to_string();
        }
    }
    if client_model.is_empty() {
        return Err(WsPreparationError {
            status: 400,
            body: error_body(Dialect::Responses, "missing model field"),
        });
    }

    let channels = state
        .db
        .get_gateway_channels()
        .map_err(|error| WsPreparationError {
            status: 500,
            body: error_body(
                Dialect::Responses,
                &format!("channel lookup failed: {error}"),
            ),
        })?;
    let rule = request_model_rule(model_policy, route.as_ref(), &client_model).cloned();
    let route_model_override = request_model_override(rule.as_ref(), model_policy, route.as_ref());
    let routing_model = route_model_override.unwrap_or(&client_model);
    let responses_channels: Vec<GatewayChannel> = channels
        .into_iter()
        .filter(|channel| channel.dialect == Dialect::Responses)
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
        .filter(|_| {
            rule.as_ref()
                .and_then(|rule| rule.dialect)
                .is_none_or(|dialect| dialect == Dialect::Responses)
        })
        .collect();

    let health = state.health.read().await.clone();
    let unhealthy = |channel: &GatewayChannel| {
        matches!(health.get(&channel.id), Some(ChannelHealth::Unhealthy(_)))
    };
    let mut candidates = candidates_for_model_ranked(
        &responses_channels,
        routing_model,
        unhealthy,
        |_| 0,
        entropy,
    );
    if candidates.is_empty() {
        candidates = candidates_for_model_ranked(
            &responses_channels,
            routing_model,
            |_| false,
            |_| 0,
            entropy,
        );
    }
    if candidates.is_empty() {
        return Err(WsPreparationError {
            status: 503,
            body: error_body(
                Dialect::Responses,
                "Responses WebSocket requires a matching Responses upstream",
            ),
        });
    }

    let mut prepared_candidates = Vec::with_capacity(candidates.len());
    for channel in candidates {
        let prepared = prepare_request(
            Dialect::Responses,
            &channel,
            &body,
            RequestConversionOptions {
                client_model: &client_model,
                route_model_override,
                reasoning: route.as_ref().map(|route| &route.reasoning),
                client_stream: true,
            },
            &state.signatures,
        )
        .map_err(|error| WsPreparationError {
            status: 400,
            body: error_body(
                Dialect::Responses,
                &format!("request conversion failed: {error}"),
            ),
        })?;
        let mut upstream_body = prepared.body;
        if let Some(object) = upstream_body.as_object_mut() {
            object.insert("type".into(), Value::String("response.create".into()));
        }
        prepared_candidates.push(PreparedWsCandidate {
            channel,
            frame: upstream_body.to_string(),
            client_model: client_model.clone(),
            upstream_model: prepared.upstream_model,
        });
    }
    Ok(prepared_candidates)
}

pub(crate) async fn mark_ws_channel_health(
    state: &GatewayState,
    channel_id: &str,
    health: ChannelHealth,
) {
    mark_health(state, channel_id, health).await;
}

pub(crate) fn record_responses_ws_usage(
    state: &GatewayState,
    candidate: &PreparedWsCandidate,
    key: Option<&GatewayKey>,
    completed_event: &Value,
    latency_ms: u64,
    first_token_ms: Option<u64>,
) {
    let Some(usage) = completed_event.pointer("/response/usage") else {
        return;
    };
    let messages_usage = conv_usage::responses_usage_to_messages(usage);
    log_usage(
        &state.db,
        &candidate.channel.id,
        key,
        &candidate.client_model,
        &candidate.upstream_model,
        token_usage_from_messages(&messages_usage, Some(candidate.upstream_model.clone())),
        latency_ms,
        first_token_ms,
        200,
        true,
    );
}

/// Run one inference request through the gateway.
pub async fn run(
    state: GatewayState,
    inlet: Dialect,
    raw_body: bytes::Bytes,
    key: Option<GatewayKey>,
    client_headers: HeaderMap,
) -> PipelineOutcome {
    let body: Value = match serde_json::from_slice(&raw_body) {
        Ok(v) => v,
        Err(e) => {
            return PipelineOutcome::Json {
                status: 400,
                body: error_body(inlet, &format!("invalid JSON body: {e}")),
            };
        }
    };
    let route = match route_for_key(&state.db, key.as_ref()) {
        Ok(route) => route,
        Err(message) => {
            return PipelineOutcome::Json {
                status: 503,
                body: error_body(inlet, &message),
            };
        }
    };
    let model_policy = key.as_ref().and_then(|key| key.model_policy.as_ref());
    let mut meta = request_meta(inlet, &body);
    if meta.model.is_empty() {
        let default_model = model_policy
            .and_then(|policy| {
                policy
                    .preferred_model
                    .as_deref()
                    .or(policy.fallback_model.as_deref())
            })
            .or_else(|| {
                if model_policy.is_none() {
                    route
                        .as_ref()
                        .and_then(|route| route.default_model.as_deref())
                } else {
                    None
                }
            });
        if let Some(default_model) = default_model {
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
            };
        }
    };
    let rule = request_model_rule(model_policy, route.as_ref(), &meta.model).cloned();
    let route_model_override = request_model_override(rule.as_ref(), model_policy, route.as_ref());
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

        let resp = match upstream_request(
            &state,
            &channel,
            &prepared.body,
            meta.stream,
            &client_headers,
        )
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
                };
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
                if let Some(content) = client_body.get("content").and_then(Value::as_array)
                    && let Some(capture) = ochub_convert::signature::capture_from_content(content)
                {
                    ochub_convert::signature::store_capture(&*state.signatures, &capture);
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
                };
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
        Ok(Some(_)) => Err("当前绑定的模型供应商已停用".to_string()),
        Ok(None) => Err("当前绑定的模型供应商不存在".to_string()),
        Err(err) => Err(format!("读取模型供应商绑定失败: {err}")),
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
    fn matched_passthrough_rule_suppresses_the_unmatched_fallback() {
        let policy = GatewayAppModelPolicy {
            fallback_model: Some("grok-4.5".into()),
            model_rules: vec![GatewayModelRule {
                model: "claude-opus-5".into(),
                upstream_model: String::new(),
                channel_id: None,
                dialect: None,
            }],
            ..Default::default()
        };

        let matched = request_model_rule(Some(&policy), None, "claude-opus-5");
        assert!(matched.is_some());
        assert_eq!(
            request_model_override(matched, Some(&policy), None),
            None,
            "a matched pass-through rule must not inherit the fallback"
        );
        let unmatched = request_model_rule(Some(&policy), None, "claude-sonnet-5");
        assert_eq!(
            request_model_override(unmatched, Some(&policy), None),
            Some("grok-4.5")
        );
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

    #[tokio::test(flavor = "multi_thread")]
    async fn count_tokens_prefers_messages_upstream_and_rewrites_model() {
        let received = Arc::new(std::sync::Mutex::new(None::<(Value, String, String)>));
        let received_for_handler = received.clone();
        let app = axum::Router::new().route(
            "/v1/messages/count_tokens",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let received = received_for_handler.clone();
                    async move {
                        *received.lock().unwrap() = Some((
                            body,
                            headers
                                .get("x-api-key")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                            headers
                                .get("x-count-test")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        ));
                        axum::Json(json!({ "input_tokens": 37 }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "messages".into(),
            endpoint_id: Some("mock".into()),
            name: "mock".into(),
            dialect: Dialect::Messages,
            base_url: format!("http://{addr}"),
            api_key: "upstream-key".into(),
            path_override: None,
            models: vec![],
            model_override: Some("upstream-model".into()),
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![("x-count-test".into(), "present".into())],
            imported_from: None,
        })
        .unwrap();
        let state = GatewayState {
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let request = json!({
            "model": "client-model",
            "messages": [{ "role": "user", "content": "hello" }]
        });

        let outcome = count_tokens(
            state,
            bytes::Bytes::from(request.to_string()),
            None,
            HeaderMap::new(),
        )
        .await;
        let PipelineOutcome::Json { status, body } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 200);
        assert_eq!(body["input_tokens"], 37);

        let (upstream_body, api_key, extra_header) = received.lock().unwrap().clone().unwrap();
        assert_eq!(upstream_body["model"], "upstream-model");
        assert!(upstream_body.get("stream").is_none());
        assert_eq!(api_key, "upstream-key");
        assert_eq!(extra_header, "present");
    }

    /// Spin up a Messages upstream that records every header it was sent.
    async fn messages_upstream_recording_headers(
        extra_headers: Vec<(String, String)>,
    ) -> (GatewayState, Arc<std::sync::Mutex<Option<HeaderMap>>>) {
        let received = Arc::new(std::sync::Mutex::new(None::<HeaderMap>));
        let received_for_handler = received.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, axum::Json(_body): axum::Json<Value>| {
                    let received = received_for_handler.clone();
                    async move {
                        *received.lock().unwrap() = Some(headers);
                        axum::Json(json!({
                            "id": "msg_1",
                            "type": "message",
                            "role": "assistant",
                            "model": "upstream-model",
                            "content": [{ "type": "text", "text": "ok" }],
                            "stop_reason": "end_turn",
                            "usage": { "input_tokens": 1, "output_tokens": 1 }
                        }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "messages".into(),
            endpoint_id: Some("mock".into()),
            name: "mock".into(),
            dialect: Dialect::Messages,
            base_url: format!("http://{addr}"),
            api_key: "upstream-key".into(),
            path_override: None,
            models: vec![],
            model_override: None,
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers,
            imported_from: None,
        })
        .unwrap();

        let state = GatewayState {
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        (state, received)
    }

    fn client_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    async fn run_messages_turn(state: GatewayState, headers: HeaderMap) {
        let body = json!({
            "model": "client-model",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let outcome = run(
            state,
            Dialect::Messages,
            bytes::Bytes::from(body.to_string()),
            None,
            headers,
        )
        .await;
        let PipelineOutcome::Json { status, .. } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 200);
    }

    /// The gateway is a proxy, not a rewriter: a header the client set is part
    /// of the request it is making, and the upstream may act on it.
    #[tokio::test]
    async fn client_headers_reach_the_upstream() {
        let (state, received) = messages_upstream_recording_headers(vec![]).await;
        run_messages_turn(
            state,
            client_headers(&[
                ("anthropic-beta", "context-1m-2025-08-07"),
                ("user-agent", "claude-cli/2.0.1"),
                ("x-stainless-lang", "js"),
                ("x-trace-id", "abc123"),
            ]),
        )
        .await;

        let sent = received.lock().unwrap().clone().unwrap();
        let value = |name: &str| {
            sent.get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(value("anthropic-beta"), "context-1m-2025-08-07");
        assert_eq!(value("user-agent"), "claude-cli/2.0.1");
        assert_eq!(value("x-stainless-lang"), "js");
        assert_eq!(value("x-trace-id"), "abc123");
    }

    /// The client's key authenticates it to the gateway and means nothing past
    /// it. Forwarding it would send two credentials and let the upstream pick.
    #[tokio::test]
    async fn the_clients_credential_is_replaced_rather_than_forwarded() {
        let (state, received) = messages_upstream_recording_headers(vec![]).await;
        run_messages_turn(
            state,
            client_headers(&[
                ("x-api-key", "gateway-key-the-client-holds"),
                ("authorization", "Bearer gateway-key-the-client-holds"),
            ]),
        )
        .await;

        let sent = received.lock().unwrap().clone().unwrap();
        assert_eq!(
            sent.get("x-api-key").unwrap().to_str().unwrap(),
            "upstream-key"
        );
        assert_eq!(sent.get_all("x-api-key").iter().count(), 1);
        assert!(sent.get("authorization").is_none());
    }

    /// Hop-scoped headers describe the client→gateway connection, and a body the
    /// gateway rebuilds. `accept-encoding` would ask for bytes this client has
    /// no feature enabled to decompress.
    #[tokio::test]
    async fn headers_describing_this_hop_do_not_travel_to_the_next_one() {
        let (state, received) = messages_upstream_recording_headers(vec![]).await;
        run_messages_turn(
            state,
            client_headers(&[
                ("host", "127.0.0.1:1"),
                ("content-length", "999999"),
                ("accept-encoding", "gzip, br"),
                ("connection", "keep-alive"),
            ]),
        )
        .await;

        let sent = received.lock().unwrap().clone().unwrap();
        assert!(sent.get("accept-encoding").is_none());
        assert_ne!(sent.get("host").unwrap().to_str().unwrap(), "127.0.0.1:1");
        assert_ne!(
            sent.get("content-length").map(|v| v.to_str().unwrap()),
            Some("999999")
        );
    }

    /// A client that pinned a version knows which response shape it parses.
    #[tokio::test]
    async fn a_client_pinned_anthropic_version_is_not_overruled() {
        let (state, received) = messages_upstream_recording_headers(vec![]).await;
        run_messages_turn(
            state,
            client_headers(&[("anthropic-version", "2026-01-01")]),
        )
        .await;

        let sent = received.lock().unwrap().clone().unwrap();
        assert_eq!(
            sent.get("anthropic-version").unwrap().to_str().unwrap(),
            "2026-01-01"
        );
    }

    #[tokio::test]
    async fn anthropic_version_is_defaulted_when_the_client_sends_none() {
        let (state, received) = messages_upstream_recording_headers(vec![]).await;
        run_messages_turn(state, HeaderMap::new()).await;

        let sent = received.lock().unwrap().clone().unwrap();
        assert_eq!(
            sent.get("anthropic-version").unwrap().to_str().unwrap(),
            "2023-06-01"
        );
    }

    /// A channel header is the operator's explicit statement about this
    /// upstream, so it outranks whatever the client happened to send.
    #[tokio::test]
    async fn a_channel_header_outranks_the_forwarded_one() {
        let (state, received) =
            messages_upstream_recording_headers(vec![("x-tenant".into(), "from-channel".into())])
                .await;
        run_messages_turn(state, client_headers(&[("x-tenant", "from-client")])).await;

        let sent = received.lock().unwrap().clone().unwrap();
        assert_eq!(
            sent.get("x-tenant").unwrap().to_str().unwrap(),
            "from-channel"
        );
        assert_eq!(sent.get_all("x-tenant").iter().count(), 1);
    }

    #[tokio::test]
    async fn count_tokens_falls_back_only_without_messages_candidate() {
        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "chat-only".into(),
            endpoint_id: None,
            name: "chat-only".into(),
            dialect: Dialect::Chat,
            base_url: "http://127.0.0.1:9".into(),
            api_key: "unused".into(),
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
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let request = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "abcdefgh" }]
        });

        let outcome = count_tokens(
            state,
            bytes::Bytes::from(request.to_string()),
            None,
            HeaderMap::new(),
        )
        .await;
        let PipelineOutcome::Json { status, body } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 200);
        assert_eq!(body["input_tokens"], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn count_tokens_does_not_hide_messages_upstream_failure_with_estimate() {
        let app = axum::Router::new().route(
            "/v1/messages/count_tokens",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": "temporary failure" }
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Arc::new(crate::db::Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "messages".into(),
            endpoint_id: None,
            name: "messages".into(),
            dialect: Dialect::Messages,
            base_url: format!("http://{addr}"),
            api_key: "key".into(),
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
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig::default())),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let request = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "abcdefgh" }]
        });

        let outcome = count_tokens(
            state,
            bytes::Bytes::from(request.to_string()),
            None,
            HeaderMap::new(),
        )
        .await;
        let PipelineOutcome::Json { status, body } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 502, "{body}");
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
            HeaderMap::new(),
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
        assert!(
            datas
                .iter()
                .any(|d| d.contains("\"finish_reason\":\"stop\""))
        );
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
    async fn per_app_policy_overrides_station_mapping_and_keeps_reasoning() {
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
                        "model": "grok-4.5",
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
            models: vec!["grok-*".into()],
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
                model: "claude-opus-5".into(),
                upstream_model: "wrong-global-model".into(),
                channel_id: Some("blocked".into()),
                dialect: Some(Dialect::Chat),
            }],
            reasoning: GatewayReasoningConfig {
                mode: GatewayReasoningMode::Auto,
                low_budget: 7_777,
                ..GatewayReasoningConfig::default()
            },
            websocket_enabled: false,
            enabled: true,
            created_at: 1,
        })
        .unwrap();
        let key = GatewayKey {
            id: "key-route".into(),
            name: "claude".into(),
            key: "rd-route".into(),
            route_id: Some("route-test".into()),
            model_policy: Some(crate::gateway::types::GatewayAppModelPolicy {
                models: vec!["grok-4.5".into()],
                preferred_model: None,
                fallback_model: None,
                model_rules: vec![crate::gateway::types::GatewayModelRule {
                    model: "claude-opus-5".into(),
                    upstream_model: "grok-4.5".into(),
                    channel_id: None,
                    dialect: Some(Dialect::Messages),
                }],
            }),
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
            "model": "claude-opus-5",
            "messages": [{ "role": "user", "content": "hi" }],
            "reasoning_effort": "low",
            "stream": false
        });
        let outcome = run(
            state,
            Dialect::Chat,
            bytes::Bytes::from(request.to_string()),
            Some(key),
            HeaderMap::new(),
        )
        .await;
        let PipelineOutcome::Json { status, body } = outcome else {
            panic!("expected JSON response");
        };
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["choices"][0]["message"]["content"], "routed");

        let upstream = received.lock().unwrap().clone().unwrap();
        assert_eq!(upstream["model"], "grok-4.5");
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
