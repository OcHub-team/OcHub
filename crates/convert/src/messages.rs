//! Pair: **messages client ⇄ responses upstream**.
//!
//! - [`request_to_responses`] — messages request body → responses request body.
//! - [`response_from_response`] — complete `response` object → messages message.
//! - [`ResponsesToMessagesStream`] — `response.*` events → messages SSE event
//!   sequence (push-based; the upstream may arrive over SSE or WebSocket).

use serde_json::{Map, Value, json};

use crate::usage::responses_usage_to_messages;
use crate::util::short_id;
use crate::{ConvertError, Output, WireEvent};

/// Options for building a responses-dialect request.
#[derive(Debug, Clone)]
pub struct ResponsesRequestOptions {
    /// Replace the request model (route-mapped upstream model). `None` keeps the
    /// client's model string.
    pub model_override: Option<String>,
    /// Explicit reasoning effort. `None` derives it from the request's
    /// `thinking.budget_tokens` (≤4096 → low, ≤10000 → medium, else high), and
    /// omits reasoning entirely when thinking is off.
    pub reasoning_effort: Option<String>,
    /// Request encrypted reasoning content in the response (`include` field), so
    /// thinking blocks can round-trip with a signature.
    pub include_encrypted_reasoning: bool,
    /// Put system text into the `instructions` field (standard). When `false`
    /// it is prepended as a first user message instead — useful when the
    /// upstream overwrites `instructions` with its own prompt.
    pub system_as_instructions: bool,
    /// Value for the `store` field.
    pub store: bool,
    /// Force the upstream call into streaming mode regardless of the client's
    /// `stream` flag (aggregate afterwards via
    /// [`crate::aggregate::parse_response_body`]).
    pub force_stream: bool,
}

impl Default for ResponsesRequestOptions {
    fn default() -> Self {
        Self {
            model_override: None,
            reasoning_effort: None,
            include_encrypted_reasoning: true,
            system_as_instructions: true,
            store: false,
            force_stream: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Request: messages → responses
// ---------------------------------------------------------------------------

fn text_part(role_is_assistant: bool, text: &str) -> Value {
    let part_type = if role_is_assistant {
        "output_text"
    } else {
        "input_text"
    };
    json!({ "type": part_type, "text": text })
}

/// Flatten the messages `system` value (string | array of text blocks) to text.
fn system_to_text(system: Option<&Value>) -> Option<String> {
    let parts: Vec<String> = match system {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => vec![],
    };
    let joined = parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Flatten a `tool_result` content value (string | array of blocks) to text.
fn tool_result_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// messages tool `{name, description, input_schema}` → responses function tool.
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())?;
            let mut tool = Map::new();
            tool.insert("type".into(), json!("function"));
            tool.insert("name".into(), json!(name));
            if let Some(d) = t.get("description").and_then(Value::as_str) {
                tool.insert("description".into(), json!(d));
            }
            let schema = t
                .get("input_schema")
                .cloned()
                .filter(|s| s.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            tool.insert("parameters".into(), schema);
            Some(Value::Object(tool))
        })
        .collect()
}

/// messages `tool_choice` → responses `tool_choice`.
fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => tc
            .get("name")
            .and_then(Value::as_str)
            .map(|n| json!({ "type": "function", "name": n })),
        _ => None,
    }
}

/// Derive a reasoning effort from a thinking budget.
fn budget_to_effort(budget: i64) -> &'static str {
    if budget <= 4096 {
        "low"
    } else if budget <= 10000 {
        "medium"
    } else {
        "high"
    }
}

/// Convert a messages request body to a responses request body.
pub fn request_to_responses(
    body: &Value,
    opts: &ResponsesRequestOptions,
) -> Result<Value, ConvertError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ConvertError::InvalidRequest("request body must be a JSON object".into()))?;
    let model = opts
        .model_override
        .clone()
        .or_else(|| obj.get("model").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| ConvertError::InvalidRequest("missing model field".into()))?;

    let system_text = system_to_text(obj.get("system"));
    let mut input: Vec<Value> = Vec::new();
    if !opts.system_as_instructions
        && let Some(text) = &system_text
    {
        input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        }));
    }

    if let Some(messages) = obj.get("messages").and_then(Value::as_array) {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let is_assistant = role == "assistant";
            match message.get("content") {
                Some(Value::String(text)) => {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [text_part(is_assistant, text)],
                    }));
                }
                Some(Value::Array(blocks)) => {
                    // Text-ish parts accumulate into one message item; tool_use /
                    // tool_result flush it and become standalone items so the
                    // original ordering is preserved.
                    let mut parts: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("tool_use") => {
                                flush_parts(&mut input, role, &mut parts);
                                let args = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| json!({}))
                                    .to_string();
                                input.push(json!({
                                    "type": "function_call",
                                    "call_id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                    "arguments": args,
                                }));
                            }
                            Some("tool_result") => {
                                flush_parts(&mut input, role, &mut parts);
                                input.push(json!({
                                    "type": "function_call_output",
                                    "call_id": block
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    "output": tool_result_to_text(block.get("content")),
                                }));
                            }
                            Some("image") => {
                                if let Some(source) = block.get("source") {
                                    let media = source
                                        .get("media_type")
                                        .and_then(Value::as_str)
                                        .unwrap_or("image/png");
                                    let data =
                                        source.get("data").and_then(Value::as_str).unwrap_or("");
                                    parts.push(json!({
                                        "type": "input_image",
                                        "image_url": format!("data:{media};base64,{data}"),
                                    }));
                                }
                            }
                            Some("thinking") => {
                                // No responses-side input representation for a
                                // replayed thinking block (the upstream signs its
                                // own reasoning); carry the text so context isn't
                                // lost.
                                if let Some(t) = block.get("thinking").and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    parts.push(text_part(is_assistant, t));
                                }
                            }
                            _ => {
                                if let Some(t) = block.get("text").and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    parts.push(text_part(is_assistant, t));
                                }
                            }
                        }
                    }
                    flush_parts(&mut input, role, &mut parts);
                }
                _ => {}
            }
        }
    }

    let tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| convert_tools(t))
        .unwrap_or_default();
    let tool_choice = obj
        .get("tool_choice")
        .and_then(convert_tool_choice)
        .unwrap_or_else(|| json!("auto"));

    // Reasoning: explicit option wins; otherwise derive from the thinking budget.
    let effort: Option<String> = match &opts.reasoning_effort {
        Some(e) => Some(e.clone()),
        None => {
            let thinking = obj.get("thinking");
            let enabled =
                thinking.and_then(|t| t.get("type")).and_then(Value::as_str) == Some("enabled");
            if enabled {
                let budget = thinking
                    .and_then(|t| t.get("budget_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(10000);
                Some(budget_to_effort(budget).to_string())
            } else {
                None
            }
        }
    };

    let stream = if opts.force_stream {
        true
    } else {
        obj.get("stream").and_then(Value::as_bool).unwrap_or(false)
    };

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    if opts.system_as_instructions
        && let Some(text) = &system_text
    {
        out.insert("instructions".into(), json!(text));
    }
    out.insert("input".into(), Value::Array(input));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        out.insert("tool_choice".into(), tool_choice);
    }
    out.insert("parallel_tool_calls".into(), json!(false));
    if let Some(e) = effort {
        out.insert(
            "reasoning".into(),
            json!({ "effort": e, "summary": "auto" }),
        );
    }
    if let Some(mt) = obj.get("max_tokens").and_then(Value::as_i64) {
        out.insert("max_output_tokens".into(), json!(mt));
    }
    out.insert("store".into(), json!(opts.store));
    out.insert("stream".into(), json!(stream));
    if opts.include_encrypted_reasoning {
        out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    Ok(Value::Object(out))
}

fn flush_parts(input: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": role,
        "content": std::mem::take(parts),
    }));
}

// ---------------------------------------------------------------------------
// Response (non-stream): response object → messages message
// ---------------------------------------------------------------------------

fn parse_tool_input(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| {
        if input.is_empty() {
            json!({})
        } else {
            json!({ "_raw": input })
        }
    })
}

fn output_stop_reason(output: &[Value]) -> &'static str {
    let used_tool = output.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call") | Some("custom_tool_call")
        )
    });
    if used_tool { "tool_use" } else { "end_turn" }
}

fn output_to_content(output: &[Value]) -> Vec<Value> {
    let mut content = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let thinking = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                if !thinking.is_empty() {
                    content.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    }));
                }
            }
            Some("message") => {
                if item.get("role").and_then(Value::as_str) == Some("assistant")
                    && let Some(parts) = item.get("content").and_then(Value::as_array)
                {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(t) = part.get("text").and_then(Value::as_str)
                        {
                            content.push(json!({ "type": "text", "text": t }));
                        }
                    }
                }
            }
            Some("function_call") => {
                content.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                    "input": parse_tool_input(
                        item.get("arguments").and_then(Value::as_str).unwrap_or("")
                    ),
                }));
            }
            Some("custom_tool_call") => {
                content.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                    "input": parse_tool_input(
                        item.get("input").and_then(Value::as_str).unwrap_or("")
                    ),
                }));
            }
            _ => {}
        }
    }
    content
}

/// Build a complete messages message from a complete `response` object.
pub fn response_from_response(response: &Value, display_model: &str) -> Value {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let message_id = output
        .iter()
        .find_map(|item| {
            if item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("assistant")
            {
                item.get("id").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            response
                .get("id")
                .and_then(Value::as_str)
                .map(|id| id.replacen("resp_", "msg_", 1))
                .unwrap_or_else(|| format!("msg_{}", short_id()))
        });

    let usage = response
        .get("usage")
        .map(responses_usage_to_messages)
        .unwrap_or_else(|| responses_usage_to_messages(&json!({})));

    json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": display_model,
        "content": output_to_content(&output),
        "stop_reason": output_stop_reason(&output),
        "stop_sequence": Value::Null,
        "usage": usage,
    })
}

// ---------------------------------------------------------------------------
// Stream: response.* events → messages SSE events
// ---------------------------------------------------------------------------

/// Push-based converter from `response.*` stream events to the messages event
/// sequence (`message_start` … `message_stop`).
pub struct ResponsesToMessagesStream {
    display_model: String,
    /// Next messages content-block index; blocks in the responses dialect are
    /// strictly sequential, so a single counter suffices.
    content_index: u64,
    used_tool: bool,
}

impl ResponsesToMessagesStream {
    pub fn new(display_model: impl Into<String>) -> Self {
        Self {
            display_model: display_model.into(),
            content_index: 0,
            used_tool: false,
        }
    }

    fn event(name: &str, data: Value) -> Output {
        Output::Event(WireEvent::new(name, data.to_string()))
    }

    fn block_stop(&mut self) -> Output {
        let ev = Self::event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": self.content_index }),
        );
        self.content_index += 1;
        ev
    }

    /// Feed one parsed wire event.
    pub fn push(&mut self, ev: &WireEvent) -> Vec<Output> {
        self.push_event(ev.event.as_deref(), &ev.data)
    }

    /// Feed one upstream event by (optional) name + data payload.
    pub fn push_event(&mut self, name: Option<&str>, data: &str) -> Vec<Output> {
        let parsed: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let name = name
            .filter(|n| !n.is_empty())
            .or_else(|| parsed.get("type").and_then(Value::as_str))
            .unwrap_or("");
        let mut out: Vec<Output> = Vec::new();

        match name {
            "response.created" => {
                out.push(Self::event(
                    "message_start",
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": format!("msg_{}", short_id()),
                            "type": "message",
                            "role": "assistant",
                            "model": self.display_model,
                            "content": [],
                            "stop_reason": Value::Null,
                            "stop_sequence": Value::Null,
                            "usage": {
                                "input_tokens": 0,
                                "cache_creation_input_tokens": 0,
                                "cache_read_input_tokens": 0,
                                "output_tokens": 0,
                                "service_tier": "standard"
                            }
                        }
                    }),
                ));
            }
            "response.in_progress" => {
                out.push(Self::event("ping", json!({ "type": "ping" })));
            }
            "response.output_item.added" => {
                let item = parsed.get("item");
                match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                    Some("reasoning") => {
                        out.push(Self::event(
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": self.content_index,
                                "content_block": {
                                    "type": "thinking",
                                    "thinking": "",
                                    "signature": ""
                                }
                            }),
                        ));
                    }
                    Some("message") => {
                        out.push(Self::event(
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": self.content_index,
                                "content_block": { "type": "text", "text": "" }
                            }),
                        ));
                    }
                    Some("function_call") | Some("custom_tool_call") => {
                        self.used_tool = true;
                        out.push(Self::event(
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": self.content_index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": item
                                        .and_then(|i| i.get("call_id"))
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    "name": item
                                        .and_then(|i| i.get("name"))
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    "input": {}
                                }
                            }),
                        ));
                    }
                    _ => {}
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = parsed.get("delta").and_then(Value::as_str) {
                    out.push(Self::event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.content_index,
                            "delta": { "type": "thinking_delta", "thinking": delta }
                        }),
                    ));
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = parsed.get("delta").and_then(Value::as_str) {
                    out.push(Self::event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.content_index,
                            "delta": { "type": "text_delta", "text": delta }
                        }),
                    ));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = parsed.get("delta").and_then(Value::as_str) {
                    out.push(Self::event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": self.content_index,
                            "delta": { "type": "input_json_delta", "partial_json": delta }
                        }),
                    ));
                }
            }
            // Text and tool blocks close on their own `*.done` events; the
            // reasoning block closes on `response.output_item.done` (which also
            // carries the encrypted reasoning used as the thinking signature).
            "response.output_text.done" | "response.function_call_arguments.done" => {
                out.push(self.block_stop());
            }
            "response.output_item.done" => {
                let item = parsed.get("item");
                if item.and_then(|i| i.get("type")).and_then(Value::as_str) == Some("reasoning") {
                    let signature = item
                        .and_then(|i| i.get("encrypted_content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !signature.is_empty() {
                        out.push(Self::event(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": self.content_index,
                                "delta": { "type": "signature_delta", "signature": signature }
                            }),
                        ));
                    }
                    out.push(self.block_stop());
                }
            }
            "response.completed" => {
                let response = parsed.get("response");
                let stop_reason = response
                    .and_then(|r| r.get("output"))
                    .and_then(Value::as_array)
                    .map(|o| output_stop_reason(o))
                    .filter(|_| !self.used_tool)
                    .unwrap_or(if self.used_tool {
                        "tool_use"
                    } else {
                        "end_turn"
                    });
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .map(responses_usage_to_messages);
                if let Some(u) = &usage {
                    out.push(Output::Usage(u.clone()));
                }
                out.push(Self::event(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                        "usage": usage.unwrap_or_else(|| responses_usage_to_messages(&json!({}))),
                    }),
                ));
                out.push(Self::event(
                    "message_stop",
                    json!({ "type": "message_stop" }),
                ));
                out.push(Output::Done);
            }
            "error" | "response.failed" => {
                let error_body = parsed
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| parsed.get("error"))
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "api_error", "message": "upstream error" }));
                out.push(Self::event(
                    "error",
                    json!({ "type": "error", "error": error_body }),
                ));
                out.push(Output::Error(parsed));
            }
            _ => {}
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ResponsesRequestOptions {
        ResponsesRequestOptions::default()
    }

    #[test]
    fn request_maps_tool_round_trip_in_order() {
        let body = json!({
            "model": "m1",
            "max_tokens": 256,
            "system": [{"type": "text", "text": "be nice"}],
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "c1", "name": "get_weather", "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "sunny"}
                ]}
            ],
            "tools": [{"name": "get_weather", "description": "d", "input_schema": {}}]
        });
        let out = request_to_responses(&body, &opts()).unwrap();
        assert_eq!(out["model"], "m1");
        assert_eq!(out["instructions"], "be nice");
        let input = out["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[2]["arguments"], "{\"city\":\"SF\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "sunny");
        // Empty tool schema normalized.
        assert_eq!(out["tools"][0]["parameters"]["type"], "object");
        assert_eq!(out["max_output_tokens"], 256);
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert_eq!(out["include"][0], "reasoning.encrypted_content");
        // No thinking → no reasoning object.
        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn request_derives_effort_from_thinking_budget() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "enabled", "budget_tokens": 16000}
        });
        let out = request_to_responses(&body, &opts()).unwrap();
        assert_eq!(out["reasoning"]["effort"], "high");
    }

    #[test]
    fn request_maps_images_and_system_placement() {
        let body = json!({
            "model": "m",
            "system": "sys",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abcd"}}
            ]}]
        });
        let mut o = opts();
        o.system_as_instructions = false;
        let out = request_to_responses(&body, &o).unwrap();
        assert!(out.get("instructions").is_none());
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][0]["text"], "sys");
        assert_eq!(input[1]["content"][1]["type"], "input_image");
        assert_eq!(
            input[1]["content"][1]["image_url"],
            "data:image/png;base64,abcd"
        );
    }

    #[test]
    fn nonstream_response_builds_message() {
        let resp = json!({
            "id": "resp_9",
            "object": "response",
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "SIG",
                 "summary": [{"type": "summary_text", "text": "think"}]},
                {"type": "message", "id": "msg_a", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Hi"}]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"}
            ],
            "usage": {
                "input_tokens": 60,
                "input_tokens_details": {"cached_tokens": 20},
                "output_tokens": 5
            }
        });
        let msg = response_from_response(&resp, "display-x");
        assert_eq!(msg["id"], "msg_a");
        assert_eq!(msg["model"], "display-x");
        assert_eq!(msg["stop_reason"], "tool_use");
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][0]["signature"], "SIG");
        assert_eq!(msg["content"][1]["text"], "Hi");
        assert_eq!(msg["content"][2]["type"], "tool_use");
        assert_eq!(msg["content"][2]["input"]["a"], 1);
        assert_eq!(msg["usage"]["input_tokens"], 40);
        assert_eq!(msg["usage"]["cache_read_input_tokens"], 20);
    }

    #[test]
    fn nonstream_response_handles_unparseable_arguments() {
        let resp = json!({
            "id": "resp_1", "output": [
                {"type": "function_call", "call_id": "c", "name": "f", "arguments": "not json"}
            ]
        });
        let msg = response_from_response(&resp, "m");
        assert_eq!(msg["content"][0]["input"]["_raw"], "not json");
    }

    #[test]
    fn stream_converts_full_turn() {
        let events: Vec<(&str, Value)> = vec![
            (
                "response.created",
                json!({"type":"response.created","response":{"id":"resp_1","model":"up"}}),
            ),
            (
                "response.in_progress",
                json!({"type":"response.in_progress"}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({"type":"response.reasoning_summary_text.delta","delta":"think"}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"SIG","summary":[{"type":"summary_text","text":"think"}]}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_a","role":"assistant","content":[]}}),
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
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_a","role":"assistant","content":[{"type":"output_text","text":"Hi"}]}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"c1","name":"f","arguments":""}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","delta":"{\"a\":1}"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","arguments":"{\"a\":1}"}),
            ),
            (
                "response.completed",
                json!({"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":6}}}),
            ),
        ];
        let mut conv = ResponsesToMessagesStream::new("display-x");
        let mut wire: Vec<(String, Value)> = Vec::new();
        let mut usage = None;
        let mut done = false;
        for (name, data) in events {
            for out in conv.push_event(Some(name), &data.to_string()) {
                match out {
                    Output::Event(e) => wire.push((
                        e.event.clone().unwrap(),
                        serde_json::from_str(&e.data).unwrap(),
                    )),
                    Output::Usage(u) => usage = Some(u),
                    Output::Done => done = true,
                    Output::Error(_) => panic!("unexpected error"),
                    _ => {}
                }
            }
        }
        assert!(done);
        let names: Vec<&str> = wire.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "ping",
                "content_block_start", // thinking (index 0)
                "content_block_delta", // thinking_delta
                "content_block_delta", // signature_delta
                "content_block_stop",  // index 0
                "content_block_start", // text (index 1)
                "content_block_delta", // text_delta
                "content_block_stop",  // index 1
                "content_block_start", // tool_use (index 2)
                "content_block_delta", // input_json_delta
                "content_block_stop",  // index 2
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(wire[0].1["message"]["model"], "display-x");
        assert_eq!(wire[2].1["content_block"]["type"], "thinking");
        assert_eq!(wire[4].1["delta"]["signature"], "SIG");
        assert_eq!(wire[6].1["index"], 1);
        assert_eq!(wire[9].1["content_block"]["id"], "c1");
        assert_eq!(wire[12].1["delta"]["stop_reason"], "tool_use");
        // usage: input 10 total - 4 cached = 6 exclusive
        assert_eq!(wire[12].1["usage"]["input_tokens"], 6);
        let usage = usage.unwrap();
        assert_eq!(usage["cache_read_input_tokens"], 4);
        assert_eq!(usage["output_tokens"], 6);
    }

    #[test]
    fn stream_maps_upstream_error() {
        let mut conv = ResponsesToMessagesStream::new("m");
        let outs = conv.push_event(
            Some("response.failed"),
            &json!({"type":"response.failed","response":{"error":{"code":"x","message":"boom"}}})
                .to_string(),
        );
        match &outs[0] {
            Output::Event(e) => {
                assert_eq!(e.event.as_deref(), Some("error"));
                let v: Value = serde_json::from_str(&e.data).unwrap();
                assert_eq!(v["error"]["message"], "boom");
            }
            other => panic!("expected event, got {other:?}"),
        }
        assert!(matches!(outs[1], Output::Error(_)));
    }
}
