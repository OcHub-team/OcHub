//! Gateway HTTP server: inference endpoints for all three dialects, an
//! OpenAI-style model list, token counting, and a WebSocket transport for the
//! responses dialect.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::{Value, json};

use crate::gateway::pipeline::{self, GatewayState, PipelineOutcome, StreamFrame};
use crate::gateway::types::{Dialect, GatewayKey};

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
        match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(upgrade) => upgrade.on_upgrade(move |socket| ws_session(state, socket, key)),
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
    match pipeline::run(state, inlet, body, key).await {
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

/// Minimal WS protocol: each client text frame carries one responses-dialect
/// request JSON; the gateway streams back the `response.*` event payloads as
/// text frames. Frames for distinct requests are never interleaved.
async fn ws_session(state: GatewayState, mut socket: WebSocket, key: Option<GatewayKey>) {
    while let Some(Ok(frame)) = socket.recv().await {
        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => break,
            _ => continue, // ping/pong handled by axum
        };
        // Force stream mode over WS regardless of the request flag.
        let request = match serde_json::from_str::<Value>(&text) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("stream".into(), json!(true));
                }
                v
            }
            Err(e) => {
                let _ = socket
                    .send(Message::Text(
                        json!({
                            "type": "error",
                            "error": { "type": "invalid_request_error", "message": format!("invalid JSON: {e}") }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                continue;
            }
        };
        let outcome = pipeline::run(
            state.clone(),
            Dialect::Responses,
            Bytes::from(request.to_string()),
            key.clone(),
        )
        .await;
        match outcome {
            PipelineOutcome::Json { body, .. } => {
                // Errors (or non-stream results) surface as a single frame.
                if socket
                    .send(Message::Text(body.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            PipelineOutcome::Stream { mut rx } => {
                while let Some(frame) = rx.recv().await {
                    match frame {
                        StreamFrame::Event(ev) => {
                            if socket.send(Message::Text(ev.data.into())).await.is_err() {
                                return;
                            }
                        }
                        StreamFrame::Done => break,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Aux endpoints
// ---------------------------------------------------------------------------

/// Rough token estimate for `count_tokens`: total serialized text length / 4.
/// Dialect-exact counting is impossible across arbitrary upstreams; clients use
/// this only for context-window bookkeeping.
async fn handle_count_tokens(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = authorize(&state, &headers, Dialect::Messages).await {
        return resp;
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut chars = 0usize;
    collect_text_len(&parsed, &mut chars);
    let tokens = (chars / 4).max(1);
    (
        StatusCode::OK,
        axum::Json(json!({ "input_tokens": tokens })),
    )
        .into_response()
}

fn collect_text_len(v: &Value, acc: &mut usize) {
    match v {
        Value::String(s) => *acc += s.len(),
        Value::Array(a) => a.iter().for_each(|x| collect_text_len(x, acc)),
        Value::Object(o) => o.values().for_each(|x| collect_text_len(x, acc)),
        _ => {}
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
