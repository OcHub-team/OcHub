//! Gateway HTTP server: inference endpoints for all three dialects, an
//! OpenAI-style model list, token counting, and a WebSocket transport for the
//! responses dialect.

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message as UpstreamMessage, protocol::CloseFrame};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::gateway::pipeline::{self, GatewayState, PipelineOutcome, StreamFrame};
use crate::gateway::types::{ChannelHealth, Dialect, GatewayChannel, GatewayKey};

pub fn build_router(state: GatewayState) -> Router {
    Router::new()
        .route("/health", get(|| async { (StatusCode::OK, "OK") }))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/chat/completions", post(handle_chat))
        .route("/v1/responses", any(handle_responses))
        .route("/v1/models", get(handle_models))
        .route("/models", get(handle_models))
        .layer(axum::extract::DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Extract the presented key secret from Authorization / x-api-key headers.
fn presented_secret(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(token) = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
    {
        return Some(token.trim().to_string());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// Resolve the caller's key. `Err` means required-but-invalid.
async fn authorize(
    state: &GatewayState,
    headers: &HeaderMap,
    inlet: Dialect,
) -> Result<Option<GatewayKey>, Response> {
    let require = state.config.read().await.require_key;
    let secret = presented_secret(headers);
    match secret {
        Some(s) => match state.db.find_gateway_key(&s) {
            Ok(Some(k)) => Ok(Some(k)),
            Ok(None) if require => Err(unauthorized(inlet)),
            Ok(None) => Ok(None),
            Err(e) => {
                log::warn!("[gateway] key lookup failed: {e}");
                Err(unauthorized(inlet))
            }
        },
        None if require => Err(unauthorized(inlet)),
        None => Ok(None),
    }
}

fn unauthorized(inlet: Dialect) -> Response {
    let body = match inlet {
        Dialect::Messages => json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid or missing gateway API key" }
        }),
        _ => json!({
            "error": { "type": "authentication_error", "message": "invalid or missing gateway API key" }
        }),
    };
    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Inference handlers
// ---------------------------------------------------------------------------

async fn handle_messages(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_http(state, Dialect::Messages, headers, body).await
}

async fn handle_chat(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_http(state, Dialect::Chat, headers, body).await
}

/// `/v1/responses` serves both plain POST (SSE / JSON) and WebSocket upgrades.
async fn handle_responses(
    State(state): State<GatewayState>,
    req: axum::extract::Request,
) -> Response {
    use axum::extract::FromRequestParts;
    let (mut parts, body) = req.into_parts();
    let is_ws = parts
        .headers
        .get("upgrade")
        .map(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
        .unwrap_or(false);
    if is_ws {
        let key = match authorize(&state, &parts.headers, Dialect::Responses).await {
            Ok(k) => k,
            Err(resp) => return resp,
        };
        match pipeline::responses_ws_available(&state, key.as_ref()) {
            Ok(true) => {}
            Ok(false) => return websocket_unavailable(),
            Err(error) => {
                log::warn!("[gateway] WebSocket availability check failed: {error}");
                return websocket_unavailable();
            }
        }
        let downstream_headers = parts.headers.clone();
        match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(upgrade) => {
                upgrade.on_upgrade(move |socket| ws_session(state, socket, key, downstream_headers))
            }
            Err(e) => e.into_response(),
        }
    } else {
        let headers = parts.headers.clone();
        let bytes = match axum::body::to_bytes(body, 200 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to read request body: {e}"),
                )
                    .into_response();
            }
        };
        run_http(state, Dialect::Responses, headers, bytes).await
    }
}

fn websocket_unavailable() -> Response {
    (
        StatusCode::UPGRADE_REQUIRED,
        axum::Json(json!({
            "error": {
                "type": "websocket_unavailable",
                "message": "Responses WebSocket is unavailable; retry this request over HTTP"
            }
        })),
    )
        .into_response()
}

async fn run_http(
    state: GatewayState,
    inlet: Dialect,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = match authorize(&state, &headers, inlet).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    match pipeline::run(state, inlet, body, key, headers).await {
        PipelineOutcome::Json { status, body } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(body),
        )
            .into_response(),
        PipelineOutcome::Stream { rx } => sse_response(rx, inlet),
    }
}

/// Encode pipeline frames as an SSE response body.
fn sse_response(rx: tokio::sync::mpsc::Receiver<StreamFrame>, inlet: Dialect) -> Response {
    let stream = futures::stream::unfold(Some(rx), move |rx| async move {
        let mut rx = rx?;
        match rx.recv().await {
            Some(StreamFrame::Event(ev)) => Some((
                Ok::<_, std::convert::Infallible>(ev.to_sse().into_bytes()),
                Some(rx),
            )),
            Some(StreamFrame::Done) => {
                let tail: &[u8] = match inlet {
                    Dialect::Chat => b"data: [DONE]\n\n",
                    _ => b"",
                };
                // Emit the terminal marker, then end on the next poll.
                Some((Ok(tail.to_vec()), None))
            }
            None => None,
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// WebSocket transport (responses dialect)
// ---------------------------------------------------------------------------

type UpstreamWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WsUpstreamSession {
    channel_id: String,
    socket: UpstreamWs,
}

enum WsPumpOutcome {
    Finished,
    RetryBeforeCommit(String),
    CommittedFailure(String),
    DownstreamClosed,
}

const WS_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const WS_EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Native Responses WebSocket proxy. Both hops remain WebSocket for the whole
/// session; Messages/Chat conversion and HTTP/SSE fallback are forbidden here.
async fn ws_session(
    state: GatewayState,
    mut downstream: WebSocket,
    key: Option<GatewayKey>,
    downstream_headers: HeaderMap,
) {
    let mut session: Option<WsUpstreamSession> = None;
    while let Some(frame) = read_ws_request(&mut downstream, &mut session).await {
        let candidates =
            match pipeline::prepare_responses_ws_turn(&state, &frame, key.as_ref()).await {
                Ok(candidates) => candidates,
                Err(error) => {
                    if send_ws_error(&mut downstream, error.status, &error.body)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
            };

        let mut last_error = String::from("all Responses WebSocket channels failed");
        let mut finished = false;

        if let Some(mut existing) = session.take() {
            let mut keep_existing = false;
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| candidate.channel.id == existing.channel_id)
            {
                let started = Instant::now();
                match existing
                    .socket
                    .send(UpstreamMessage::Text(candidate.frame.clone().into()))
                    .await
                {
                    Ok(()) => match pump_ws_events(
                        &state,
                        &mut downstream,
                        &mut existing.socket,
                        candidate,
                        key.as_ref(),
                        started,
                    )
                    .await
                    {
                        WsPumpOutcome::Finished => {
                            keep_existing = true;
                            finished = true;
                        }
                        WsPumpOutcome::RetryBeforeCommit(error) => {
                            last_error = error;
                        }
                        WsPumpOutcome::CommittedFailure(error) => {
                            let _ = send_ws_error_message(&mut downstream, 502, &error).await;
                            finished = true;
                        }
                        WsPumpOutcome::DownstreamClosed => return,
                    },
                    Err(error) => {
                        last_error = format!("upstream WebSocket send failed: {error}");
                    }
                }
            }
            if keep_existing {
                session = Some(existing);
            } else {
                let _ = existing.socket.close(None).await;
            }
        }
        if finished {
            continue;
        }

        for candidate in &candidates {
            let mut upstream =
                match connect_ws_upstream(&candidate.channel, &downstream_headers).await {
                    Ok(upstream) => upstream,
                    Err(error) => {
                        last_error = format!(
                            "channel '{}' WebSocket connect failed: {error}",
                            candidate.channel.name
                        );
                        pipeline::mark_ws_channel_health(
                            &state,
                            &candidate.channel.id,
                            ChannelHealth::Unhealthy(error),
                        )
                        .await;
                        continue;
                    }
                };
            if let Err(error) = upstream
                .send(UpstreamMessage::Text(candidate.frame.clone().into()))
                .await
            {
                last_error = format!(
                    "channel '{}' WebSocket send failed: {error}",
                    candidate.channel.name
                );
                pipeline::mark_ws_channel_health(
                    &state,
                    &candidate.channel.id,
                    ChannelHealth::Unhealthy(error.to_string()),
                )
                .await;
                continue;
            }

            let started = Instant::now();
            match pump_ws_events(
                &state,
                &mut downstream,
                &mut upstream,
                candidate,
                key.as_ref(),
                started,
            )
            .await
            {
                WsPumpOutcome::Finished => {
                    session = Some(WsUpstreamSession {
                        channel_id: candidate.channel.id.clone(),
                        socket: upstream,
                    });
                    finished = true;
                    break;
                }
                WsPumpOutcome::RetryBeforeCommit(error) => {
                    last_error = error;
                }
                WsPumpOutcome::CommittedFailure(error) => {
                    let _ = send_ws_error_message(&mut downstream, 502, &error).await;
                    finished = true;
                    break;
                }
                WsPumpOutcome::DownstreamClosed => return,
            }
        }

        if !finished
            && send_ws_error_message(&mut downstream, 502, &last_error)
                .await
                .is_err()
        {
            break;
        }
    }

    if let Some(mut session) = session {
        let _ = session.socket.close(None).await;
    }
    let _ = downstream.close().await;
}

async fn read_ws_request(
    downstream: &mut WebSocket,
    session: &mut Option<WsUpstreamSession>,
) -> Option<String> {
    let mut keepalive = tokio::time::interval(WS_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    loop {
        if let Some(upstream) = session.as_mut() {
            let mut upstream_dead = false;
            tokio::select! {
                message = downstream.recv() => match message {
                    Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                    Some(Ok(Message::Binary(bytes))) => {
                        return String::from_utf8(bytes.to_vec()).ok();
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if downstream.send(Message::Pong(payload)).await.is_err() {
                            return None;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                },
                message = upstream.socket.next() => match message {
                    Some(Ok(UpstreamMessage::Ping(payload))) => {
                        if upstream.socket.send(UpstreamMessage::Pong(payload)).await.is_err() {
                            upstream_dead = true;
                        }
                    }
                    Some(Ok(UpstreamMessage::Close(_))) | None | Some(Err(_)) => {
                        upstream_dead = true;
                    }
                    Some(Ok(_)) => {}
                },
                _ = keepalive.tick() => {
                    if downstream.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return None;
                    }
                    if upstream.socket.send(UpstreamMessage::Ping(Vec::new().into())).await.is_err() {
                        upstream_dead = true;
                    }
                }
            }
            if upstream_dead && let Some(mut dead) = session.take() {
                let _ = dead.socket.close(None).await;
            }
        } else {
            tokio::select! {
                message = downstream.recv() => match message {
                    Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                    Some(Ok(Message::Binary(bytes))) => {
                        return String::from_utf8(bytes.to_vec()).ok();
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if downstream.send(Message::Pong(payload)).await.is_err() {
                            return None;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                },
                _ = keepalive.tick() => {
                    if downstream.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return None;
                    }
                }
            }
        }
    }
}

async fn connect_ws_upstream(
    channel: &GatewayChannel,
    downstream_headers: &HeaderMap,
) -> Result<UpstreamWs, String> {
    let mut url = url::Url::parse(&channel.endpoint_url()).map_err(|error| error.to_string())?;
    let ws_scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        scheme => return Err(format!("unsupported upstream WebSocket scheme '{scheme}'")),
    };
    url.set_scheme(ws_scheme)
        .map_err(|_| "failed to set upstream WebSocket scheme".to_string())?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|error| error.to_string())?;
    // Same rule as the HTTP hop: forward what the client sent, minus the
    // headers this hop owns. The previous allowlist here named eight Codex
    // headers, which meant any other beta flag or tracing header the client
    // added was dropped without a trace.
    //
    // Iterate borrowed: the owned iterator reports a repeated key as `None`,
    // which would silently drop the second value of a multi-value header.
    let forwarded = pipeline::forwardable_client_headers(downstream_headers);
    for (name, value) in forwarded.iter() {
        request.headers_mut().append(name.clone(), value.clone());
    }
    request.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", channel.api_key))
            .map_err(|error| error.to_string())?,
    );
    for (name, value) in &channel.extra_headers {
        let name = axum::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| error.to_string())?;
        let value = axum::http::HeaderValue::from_str(value).map_err(|error| error.to_string())?;
        request.headers_mut().insert(name, value);
    }

    let (socket, _) = connect_async(request)
        .await
        .map_err(|error| error.to_string())?;
    Ok(socket)
}

async fn pump_ws_events(
    state: &GatewayState,
    downstream: &mut WebSocket,
    upstream: &mut UpstreamWs,
    candidate: &pipeline::PreparedWsCandidate,
    key: Option<&GatewayKey>,
    started: Instant,
) -> WsPumpOutcome {
    let mut committed = false;
    let mut first_token_ms = None;
    let mut keepalive = tokio::time::interval(WS_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    loop {
        let idle_timeout = tokio::time::sleep(WS_EVENT_IDLE_TIMEOUT);
        tokio::pin!(idle_timeout);
        let message = loop {
            tokio::select! {
                result = upstream.next() => {
                    break match result {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => {
                            let message = format!("upstream WebSocket error: {error}");
                            return if committed {
                                WsPumpOutcome::CommittedFailure(message)
                            } else {
                                WsPumpOutcome::RetryBeforeCommit(message)
                            };
                        }
                        None => {
                            let message =
                                "upstream WebSocket closed before a terminal response".to_string();
                            return if committed {
                                WsPumpOutcome::CommittedFailure(message)
                            } else {
                                WsPumpOutcome::RetryBeforeCommit(message)
                            };
                        }
                    };
                }
                _ = &mut idle_timeout => {
                    let message = "upstream WebSocket idle timeout".to_string();
                    return if committed {
                        WsPumpOutcome::CommittedFailure(message)
                    } else {
                        WsPumpOutcome::RetryBeforeCommit(message)
                    };
                }
                _ = keepalive.tick() => {
                    if downstream.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return WsPumpOutcome::DownstreamClosed;
                    }
                    if upstream.send(UpstreamMessage::Ping(Vec::new().into())).await.is_err() {
                        let message = "failed to ping upstream WebSocket".to_string();
                        return if committed {
                            WsPumpOutcome::CommittedFailure(message)
                        } else {
                            WsPumpOutcome::RetryBeforeCommit(message)
                        };
                    }
                }
            }
        };

        match message {
            UpstreamMessage::Text(text) => {
                let parsed = serde_json::from_str::<Value>(&text).ok();
                let event_type = parsed
                    .as_ref()
                    .and_then(|value| value.get("type"))
                    .and_then(Value::as_str);
                if !committed && matches!(event_type, Some("response.failed") | Some("error")) {
                    return WsPumpOutcome::RetryBeforeCommit(format!(
                        "channel '{}' returned {text}",
                        candidate.channel.name
                    ));
                }
                if !committed {
                    committed = true;
                    first_token_ms = Some(started.elapsed().as_millis() as u64);
                }
                if downstream
                    .send(Message::Text(text.to_string().into()))
                    .await
                    .is_err()
                {
                    return WsPumpOutcome::DownstreamClosed;
                }
                match event_type {
                    Some("response.completed") => {
                        pipeline::mark_ws_channel_health(
                            state,
                            &candidate.channel.id,
                            ChannelHealth::Healthy,
                        )
                        .await;
                        if let Some(completed) = parsed.as_ref() {
                            pipeline::record_responses_ws_usage(
                                state,
                                candidate,
                                key,
                                completed,
                                started.elapsed().as_millis() as u64,
                                first_token_ms,
                            );
                        }
                        return WsPumpOutcome::Finished;
                    }
                    Some("response.failed") | Some("error") => {
                        pipeline::mark_ws_channel_health(
                            state,
                            &candidate.channel.id,
                            ChannelHealth::Healthy,
                        )
                        .await;
                        return WsPumpOutcome::Finished;
                    }
                    _ => {}
                }
            }
            UpstreamMessage::Binary(bytes) => {
                committed = true;
                if downstream
                    .send(Message::Binary(bytes.to_vec().into()))
                    .await
                    .is_err()
                {
                    return WsPumpOutcome::DownstreamClosed;
                }
            }
            UpstreamMessage::Ping(payload) => {
                if upstream.send(UpstreamMessage::Pong(payload)).await.is_err() {
                    let message = "failed to answer upstream WebSocket ping".to_string();
                    return if committed {
                        WsPumpOutcome::CommittedFailure(message)
                    } else {
                        WsPumpOutcome::RetryBeforeCommit(message)
                    };
                }
            }
            UpstreamMessage::Pong(_) | UpstreamMessage::Frame(_) => {}
            UpstreamMessage::Close(frame) => {
                let message = close_message(frame.as_ref());
                return if committed {
                    WsPumpOutcome::CommittedFailure(message)
                } else {
                    WsPumpOutcome::RetryBeforeCommit(message)
                };
            }
        }
    }
}

fn close_message(frame: Option<&CloseFrame>) -> String {
    frame.map_or_else(
        || "upstream WebSocket closed before a terminal response".to_string(),
        |frame| {
            format!(
                "upstream WebSocket closed before a terminal response: {} {}",
                frame.code, frame.reason
            )
        },
    )
}

async fn send_ws_error(socket: &mut WebSocket, status: u16, body: &Value) -> Result<(), ()> {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("WebSocket request failed");
    send_ws_error_message(socket, status, message).await
}

async fn send_ws_error_message(
    socket: &mut WebSocket,
    status: u16,
    message: &str,
) -> Result<(), ()> {
    socket
        .send(Message::Text(
            json!({
                "type": "error",
                "status": status,
                "error": { "type": "server_error", "message": message }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Aux endpoints
// ---------------------------------------------------------------------------

/// Route token counting to a compatible Messages upstream, with a local rough
/// estimate only when the effective route has no matching Messages channel.
async fn handle_count_tokens(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = match authorize(&state, &headers, Dialect::Messages).await {
        Ok(key) => key,
        Err(resp) => return resp,
    };
    match pipeline::count_tokens(state, body, key, headers).await {
        PipelineOutcome::Json { status, body } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(body),
        )
            .into_response(),
        PipelineOutcome::Stream { .. } => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// OpenAI-style model list: exact model names + overrides from all enabled
/// channels (wildcard patterns are skipped — they have no enumerable form).
async fn handle_models(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    let key = match authorize(&state, &headers, Dialect::Chat).await {
        Ok(key) => key,
        Err(resp) => return resp,
    };
    let route = match key.as_ref().and_then(|key| key.route_id.as_deref()) {
        Some(route_id) => match state.db.get_gateway_route_by_id(route_id) {
            Ok(Some(route)) if route.enabled => Some(route),
            Ok(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({
                        "error": { "message": "client route is unavailable" }
                    })),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "error": { "message": "failed to load client route" }
                    })),
                )
                    .into_response();
            }
        },
        None => None,
    };
    let mut models: Vec<String> = Vec::new();
    let model_policy = key.as_ref().and_then(|key| key.model_policy.as_ref());
    let effective_rules = match model_policy {
        Some(policy) => policy.model_rules.as_slice(),
        None => route
            .as_ref()
            .map(|route| route.model_rules.as_slice())
            .unwrap_or_default(),
    };
    let mapped_targets: std::collections::HashSet<String> = effective_rules
        .iter()
        .filter(|rule| !rule.model.contains('*'))
        .filter_map(|rule| rule.upstream_model_override())
        .map(str::to_string)
        .collect();
    if let Some(policy) = model_policy {
        models = policy.client_models();
    } else if let Some(route) = &route {
        for rule in &route.model_rules {
            let model = rule.model.trim();
            if !model.is_empty()
                && !model.contains('*')
                && !models.iter().any(|existing| existing == model)
            {
                models.push(model.to_string());
            }
        }
        if let Some(default_model) = &route.default_model
            && !models.contains(default_model)
        {
            models.push(default_model.clone());
        }
    }
    // A per-app policy is an explicit catalog selection. Only legacy keys
    // inherit every model advertised by the station.
    if model_policy.is_none()
        && let Ok(channels) = state.db.get_gateway_channels()
    {
        for c in channels.iter().filter(|channel| {
            channel.enabled
                && route
                    .as_ref()
                    .is_none_or(|route| route.allows_channel(&channel.id))
        }) {
            for m in &c.models {
                let model = m.trim();
                if !model.is_empty()
                    && !model.contains('*')
                    && !mapped_targets.contains(model)
                    && !models.iter().any(|existing| existing == model)
                {
                    models.push(model.to_string());
                }
            }
        }
    }
    models.sort();
    let data: Vec<Value> = models
        .into_iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "gateway" }))
        .collect();
    (
        StatusCode::OK,
        axum::Json(json!({ "object": "list", "data": data })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::{SinkExt, StreamExt};
    use tokio::sync::RwLock;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    use super::*;
    use crate::db::Database;
    use crate::gateway::types::{GatewayConfig, GatewayKey, GatewayReasoningConfig, GatewayRoute};

    #[tokio::test(flavor = "multi_thread")]
    async fn responses_websocket_returns_426_when_not_enabled() {
        let db = Arc::new(Database::memory().unwrap());
        let state = GatewayState {
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig {
                require_key: false,
                ..GatewayConfig::default()
            })),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });

        let error = connect_async(format!("ws://{addr}/v1/responses"))
            .await
            .unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected an HTTP upgrade rejection");
        };
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn responses_websocket_stays_websocket_upstream_and_reuses_connection() {
        let handshakes = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let upstream_auth = Arc::new(Mutex::new(String::new()));
        let upstream_app = {
            let handshakes = handshakes.clone();
            let received = received.clone();
            let upstream_auth = upstream_auth.clone();
            Router::new().route(
                "/v1/responses",
                any(move |ws: WebSocketUpgrade, headers: HeaderMap| {
                    let handshakes = handshakes.clone();
                    let received = received.clone();
                    let upstream_auth = upstream_auth.clone();
                    async move {
                        handshakes.fetch_add(1, Ordering::SeqCst);
                        *upstream_auth.lock().unwrap() = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        ws.on_upgrade(move |mut socket| async move {
                            let mut turn = 0u64;
                            while let Some(Ok(message)) = socket.recv().await {
                                let Message::Text(text) = message else {
                                    if matches!(message, Message::Close(_)) {
                                        break;
                                    }
                                    continue;
                                };
                                let request: Value = serde_json::from_str(&text).unwrap();
                                received.lock().unwrap().push(request);
                                turn += 1;
                                socket
                                    .send(Message::Text(
                                        json!({
                                            "type": "response.created",
                                            "response": {
                                                "id": format!("resp-{turn}"),
                                                "model": "upstream-model"
                                            }
                                        })
                                        .to_string()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                                socket
                                    .send(Message::Text(
                                        json!({
                                            "type": "response.completed",
                                            "response": {
                                                "id": format!("resp-{turn}"),
                                                "model": "upstream-model",
                                                "output": [],
                                                "usage": {
                                                    "input_tokens": 4,
                                                    "output_tokens": 2
                                                }
                                            }
                                        })
                                        .to_string()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                        })
                    }
                }),
            )
        };
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream_app).await.unwrap() });

        let db = Arc::new(Database::memory().unwrap());
        db.upsert_gateway_channel(&GatewayChannel {
            id: "responses".into(),
            endpoint_id: Some("mock".into()),
            name: "mock".into(),
            dialect: Dialect::Responses,
            base_url: format!("http://{upstream_addr}"),
            api_key: "upstream-key".into(),
            path_override: None,
            models: vec![],
            model_override: Some("upstream-model".into()),
            priority: 0,
            weight: 1,
            enabled: true,
            extra_headers: vec![],
            imported_from: None,
        })
        .unwrap();
        db.upsert_gateway_route(&GatewayRoute {
            id: "route-ws".into(),
            name: "WS route".into(),
            website_url: None,
            app_type: Some("codex".into()),
            channel_ids: vec!["responses".into()],
            default_model: None,
            model_rules: vec![],
            reasoning: GatewayReasoningConfig::default(),
            websocket_enabled: true,
            enabled: true,
            created_at: 1,
        })
        .unwrap();
        db.upsert_gateway_key(&GatewayKey {
            id: "key-ws".into(),
            name: "WS key".into(),
            key: "rd-ws-test".into(),
            route_id: Some("route-ws".into()),
            model_policy: None,
            created_at: 1,
            enabled: true,
        })
        .unwrap();
        let state = GatewayState {
            db,
            http_client: reqwest::Client::new(),
            config: Arc::new(RwLock::new(GatewayConfig {
                require_key: true,
                ..GatewayConfig::default()
            })),
            health: Arc::new(RwLock::new(HashMap::new())),
            signatures: Arc::new(ochub_convert::MemorySignatureStore::default()),
        };
        let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(gateway_listener, build_router(state))
                .await
                .unwrap()
        });

        let mut request = format!("ws://{gateway_addr}/v1/responses")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer rd-ws-test".parse().unwrap());
        let (mut client, _) = connect_async(request).await.unwrap();
        for turn in 1..=2 {
            client
                .send(ClientMessage::Text(
                    json!({
                        "type": "response.create",
                        "model": "client-model",
                        "input": [{ "role": "user", "content": format!("turn {turn}") }]
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            loop {
                let message = client.next().await.unwrap().unwrap();
                let ClientMessage::Text(text) = message else {
                    continue;
                };
                let event: Value = serde_json::from_str(&text).unwrap();
                if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                    break;
                }
            }
        }
        client.close(None).await.unwrap();

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert_eq!(*upstream_auth.lock().unwrap(), "Bearer upstream-key");
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request["type"], "response.create");
            assert_eq!(request["model"], "upstream-model");
            assert_eq!(request["stream"], true);
        }
    }
}
