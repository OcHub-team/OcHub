//! Pair: **messages client ⇄ chat upstream**.
//!
//! - [`request_to_chat`] — messages request body → chat request body.
//! - [`response_from_completion`] — complete `chat.completion` → messages message.
//! - [`ChatToMessagesStream`] — `chat.completion.chunk` events → messages SSE
//!   event sequence (push-based; feed it from SSE bytes or WebSocket frames).
//!
//! The chat dialect cannot express signed thinking or cache markers, so replayed
//! `thinking` blocks are dropped on the request side and streamed reasoning comes
//! back as unsigned thinking blocks (never replayable upstream).

use serde_json::{json, Map, Value};

use crate::usage::chat_usage_to_messages;
use crate::util::short_id;
use crate::{ConvertError, Output, WireEvent};

/// Options for building a chat-dialect request.
#[derive(Debug, Clone, Default)]
pub struct ChatRequestOptions {
    /// Emit `reasoning_effort` for upstreams that honor it. `None` omits the
    /// field entirely (safest for strict OpenAI-compatible relays).
    pub reasoning_effort: Option<String>,
    /// Force the upstream call into streaming mode regardless of the client's
    /// `stream` flag.
    pub force_stream: bool,
}

/// Map a chat `finish_reason` to a messages `stop_reason`.
pub fn finish_to_stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

// ---------------------------------------------------------------------------
// Request: messages → chat
// ---------------------------------------------------------------------------

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

/// messages tool `{name, description, input_schema}` → chat function tool.
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())?;
            let mut function = Map::new();
            function.insert("name".into(), json!(name));
            if let Some(d) = t.get("description").and_then(Value::as_str) {
                function.insert("description".into(), json!(d));
            }
            let schema = t
                .get("input_schema")
                .cloned()
                .filter(|s| s.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            function.insert("parameters".into(), schema);
            Some(json!({ "type": "function", "function": function }))
        })
        .collect()
}

/// messages `tool_choice` → chat `tool_choice`.
fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => tc
            .get("name")
            .and_then(Value::as_str)
            .map(|n| json!({ "type": "function", "function": { "name": n } })),
        _ => None,
    }
}

/// Push accumulated text/image parts as one chat message. A text-only run
/// collapses to plain string content (maximally compatible); parts with images
/// keep the array form.
fn flush_parts(messages: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    let taken = std::mem::take(parts);
    let all_text = taken
        .iter()
        .all(|p| p.get("type").and_then(Value::as_str) == Some("text"));
    let content: Value = if all_text {
        taken
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
            .into()
    } else {
        Value::Array(taken)
    };
    messages.push(json!({ "role": role, "content": content }));
}

/// Convert a messages request body to a chat request body.
pub fn request_to_chat(body: &Value, opts: &ChatRequestOptions) -> Result<Value, ConvertError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ConvertError::InvalidRequest("request body must be a JSON object".into()))?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ConvertError::InvalidRequest("missing model field".into()))?;

    let mut messages: Vec<Value> = Vec::new();
    if let Some(text) = system_to_text(obj.get("system")) {
        messages.push(json!({ "role": "system", "content": text }));
    }

    if let Some(source) = obj.get("messages").and_then(Value::as_array) {
        for message in source {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            match message.get("content") {
                Some(Value::String(text)) => {
                    if !text.is_empty() {
                        messages.push(json!({ "role": role, "content": text }));
                    }
                }
                Some(Value::Array(blocks)) if role == "assistant" => {
                    let mut text = String::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    text.push_str(t);
                                }
                            }
                            Some("tool_use") => {
                                let args = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| json!({}))
                                    .to_string();
                                tool_calls.push(json!({
                                    "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": block
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                        "arguments": args,
                                    }
                                }));
                            }
                            // The chat dialect has no replayable representation
                            // for thinking blocks; drop them (the upstream signs
                            // nothing, so no context round-trip exists to keep).
                            Some("thinking") | Some("redacted_thinking") => {}
                            _ => {}
                        }
                    }
                    if text.is_empty() && tool_calls.is_empty() {
                        continue;
                    }
                    let mut m = json!({ "role": "assistant", "content": text });
                    if !tool_calls.is_empty() {
                        if text.is_empty() {
                            m["content"] = Value::Null;
                        }
                        m["tool_calls"] = json!(tool_calls);
                    }
                    messages.push(m);
                }
                Some(Value::Array(blocks)) => {
                    // User turn: text/image parts accumulate; tool_result blocks
                    // flush them and become standalone `tool` messages so the
                    // original ordering is preserved.
                    let mut parts: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                flush_parts(&mut messages, role, &mut parts);
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": block
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    "content": tool_result_to_text(block.get("content")),
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
                                        "type": "image_url",
                                        "image_url": {
                                            "url": format!("data:{media};base64,{data}")
                                        }
                                    }));
                                }
                            }
                            _ => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        parts.push(json!({ "type": "text", "text": t }));
                                    }
                                }
                            }
                        }
                    }
                    flush_parts(&mut messages, role, &mut parts);
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
    let tool_choice = obj.get("tool_choice").and_then(convert_tool_choice);

    let stream = if opts.force_stream {
        true
    } else {
        obj.get("stream").and_then(Value::as_bool).unwrap_or(false)
    };

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    if let Some(mt) = obj.get("max_tokens").and_then(Value::as_i64) {
        out.insert("max_tokens".into(), json!(mt));
    }
    if let Some(t) = obj.get("temperature").and_then(Value::as_f64) {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(stops) = obj.get("stop_sequences").and_then(Value::as_array) {
        if !stops.is_empty() {
            out.insert("stop".into(), json!(stops));
        }
    }
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = tool_choice {
            out.insert("tool_choice".into(), tc);
        }
    }
    if let Some(effort) = &opts.reasoning_effort {
        out.insert("reasoning_effort".into(), json!(effort));
    }
    out.insert("stream".into(), json!(stream));
    if stream {
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// Response (non-stream): chat.completion → messages message
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

/// Build a complete messages message from a complete `chat.completion` body.
pub fn response_from_completion(completion: &Value, display_model: &str) -> Value {
    let id = completion
        .get("id")
        .and_then(Value::as_str)
        .map(|id| format!("msg_{}", id.trim_start_matches("chatcmpl-")))
        .unwrap_or_else(|| format!("msg_{}", short_id()));
    let message = completion.pointer("/choices/0/message");

    let mut content: Vec<Value> = Vec::new();
    if let Some(reasoning) = message
        .and_then(|m| m.get("reasoning_content").or_else(|| m.get("reasoning")))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        // Unsigned thinking: display-only, dropped again on any replay upstream.
        content.push(json!({
            "type": "thinking",
            "thinking": reasoning,
            "signature": "",
        }));
    }
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    let mut used_tool = false;
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for tc in tool_calls {
            used_tool = true;
            content.push(json!({
                "type": "tool_use",
                "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": tc
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                "input": parse_tool_input(
                    tc.pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            }));
        }
    }

    let finish = completion
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str);
    let stop_reason = if finish.is_none() && used_tool {
        "tool_use"
    } else {
        finish_to_stop_reason(finish)
    };
    let usage = completion
        .get("usage")
        .map(chat_usage_to_messages)
        .unwrap_or_else(|| chat_usage_to_messages(&json!({})));

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": display_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage,
    })
}

// ---------------------------------------------------------------------------
// Stream: chat.completion.chunk → messages SSE events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text,
    Thinking,
    /// The chat-side tool_call index currently streaming.
    Tool(u64),
}

/// Push-based converter from chat chunks to the messages event sequence
/// (`message_start` … `message_stop`).
pub struct ChatToMessagesStream {
    display_model: String,
    started: bool,
    content_index: u64,
    open_block: Option<OpenBlock>,
    stop_reason: Option<&'static str>,
    used_tool: bool,
    /// Merged usage in messages shape (chat upstreams report it on the final
    /// chunk when `stream_options.include_usage` is set).
    usage: Option<Value>,
}

impl ChatToMessagesStream {
    pub fn new(display_model: impl Into<String>) -> Self {
        Self {
            display_model: display_model.into(),
            started: false,
            content_index: 0,
            open_block: None,
            stop_reason: None,
            used_tool: false,
            usage: None,
        }
    }

    fn event(name: &str, data: Value) -> Output {
        Output::Event(WireEvent::new(name, data.to_string()))
    }

    fn ensure_started(&mut self, out: &mut Vec<Output>) {
        if self.started {
            return;
        }
        self.started = true;
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

    fn close_block(&mut self, out: &mut Vec<Output>) {
        if self.open_block.take().is_some() {
            out.push(Self::event(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": self.content_index }),
            ));
            self.content_index += 1;
        }
    }

    fn ensure_block(&mut self, block: OpenBlock, start: Value, out: &mut Vec<Output>) {
        if self.open_block == Some(block) {
            return;
        }
        self.close_block(out);
        self.open_block = Some(block);
        out.push(Self::event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": self.content_index,
                "content_block": start,
            }),
        ));
    }

    fn delta(&self, delta: Value) -> Output {
        Self::event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": self.content_index,
                "delta": delta,
            }),
        )
    }

    fn finish(&mut self, out: &mut Vec<Output>) {
        self.ensure_started(out);
        self.close_block(out);
        let stop_reason = self.stop_reason.unwrap_or(if self.used_tool {
            "tool_use"
        } else {
            "end_turn"
        });
        let usage = self
            .usage
            .clone()
            .unwrap_or_else(|| chat_usage_to_messages(&json!({})));
        out.push(Self::event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                "usage": usage,
            }),
        ));
        out.push(Self::event(
            "message_stop",
            json!({ "type": "message_stop" }),
        ));
        out.push(Output::Done);
    }

    /// Feed one parsed wire event.
    pub fn push(&mut self, ev: &WireEvent) -> Vec<Output> {
        self.push_event(ev.event.as_deref(), &ev.data)
    }

    /// Feed one upstream event by (optional) name + data payload. Chat SSE
    /// carries no event names; the terminal marker is the literal `[DONE]`.
    pub fn push_event(&mut self, _name: Option<&str>, data: &str) -> Vec<Output> {
        let mut out: Vec<Output> = Vec::new();
        if data.trim() == "[DONE]" {
            self.finish(&mut out);
            return out;
        }
        let parsed: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return out,
        };
        if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
            out.push(Self::event(
                "error",
                json!({ "type": "error", "error": err }),
            ));
            out.push(Output::Error(parsed.clone()));
            return out;
        }

        self.ensure_started(&mut out);

        if let Some(u) = parsed.get("usage").filter(|u| !u.is_null()) {
            let merged = chat_usage_to_messages(u);
            self.usage = Some(merged.clone());
            out.push(Output::Usage(merged));
        }

        let choice = parsed.pointer("/choices/0");
        let delta = choice.and_then(|c| c.get("delta"));

        if let Some(tool_calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for tc in tool_calls {
                let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                if self.open_block != Some(OpenBlock::Tool(index)) {
                    self.used_tool = true;
                    let start = json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": tc
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        "input": {}
                    });
                    self.ensure_block(OpenBlock::Tool(index), start, &mut out);
                }
                if let Some(args) = tc
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                {
                    out.push(self.delta(json!({
                        "type": "input_json_delta",
                        "partial_json": args,
                    })));
                }
            }
        }

        if let Some(text) = delta
            .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.ensure_block(
                OpenBlock::Thinking,
                json!({ "type": "thinking", "thinking": "", "signature": "" }),
                &mut out,
            );
            out.push(self.delta(json!({ "type": "thinking_delta", "thinking": text })));
        }

        if let Some(text) = delta
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.ensure_block(
                OpenBlock::Text,
                json!({ "type": "text", "text": "" }),
                &mut out,
            );
            out.push(self.delta(json!({ "type": "text_delta", "text": text })));
        }

        if let Some(finish) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
        {
            self.stop_reason = Some(finish_to_stop_reason(Some(finish)));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_system_tools_and_tool_round_trip() {
        let body = json!({
            "model": "m1",
            "max_tokens": 256,
            "system": [{"type": "text", "text": "be nice"}],
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "SIG"},
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "c1", "name": "get_weather", "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "sunny"},
                    {"type": "text", "text": "and tomorrow?"}
                ]}
            ],
            "tools": [{"name": "get_weather", "description": "d", "input_schema": {}}],
            "tool_choice": {"type": "any"}
        });
        let out = request_to_chat(&body, &ChatRequestOptions::default()).unwrap();
        assert_eq!(out["model"], "m1");
        assert_eq!(out["max_tokens"], 256);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be nice");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "weather?");
        // Thinking dropped; text + tool_calls preserved.
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "checking");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"SF\"}"
        );
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["content"], "sunny");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "and tomorrow?");
        // Tools + tool_choice mapped; empty schema normalized.
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(out["tool_choice"], "required");
        // No reasoning_effort unless asked; stream off by default.
        assert!(out.get("reasoning_effort").is_none());
        assert_eq!(out["stream"], false);
        assert!(out.get("stream_options").is_none());
    }

    #[test]
    fn request_maps_images_and_stream_options() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abcd"}}
            ]}],
            "stream": true
        });
        let out = request_to_chat(&body, &ChatRequestOptions::default()).unwrap();
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,abcd");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn request_emits_reasoning_effort_only_when_asked() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let opts = ChatRequestOptions {
            reasoning_effort: Some("high".into()),
            force_stream: true,
        };
        let out = request_to_chat(&body, &opts).unwrap();
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn nonstream_response_builds_message() {
        let completion = json!({
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hi",
                    "reasoning_content": "think",
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": {"name": "f", "arguments": "{\"a\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 30,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 20}
            }
        });
        let msg = response_from_completion(&completion, "display-x");
        assert_eq!(msg["id"], "msg_abc");
        assert_eq!(msg["model"], "display-x");
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][0]["thinking"], "think");
        assert_eq!(msg["content"][1]["text"], "Hi");
        assert_eq!(msg["content"][2]["type"], "tool_use");
        assert_eq!(msg["content"][2]["input"]["a"], 1);
        assert_eq!(msg["stop_reason"], "tool_use");
        // chat prompt_tokens is a total → messages input is exclusive.
        assert_eq!(msg["usage"]["input_tokens"], 10);
        assert_eq!(msg["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(msg["usage"]["output_tokens"], 5);
    }

    #[test]
    fn nonstream_response_handles_unparseable_arguments() {
        let completion = json!({
            "id": "chatcmpl-1",
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": "not json"}}
            ]}}]
        });
        let msg = response_from_completion(&completion, "m");
        assert_eq!(msg["content"][0]["input"]["_raw"], "not json");
        // No finish_reason but a tool call → tool_use.
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    fn chunk(delta: Value, finish: Value) -> String {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
        .to_string()
    }

    #[test]
    fn stream_converts_full_turn() {
        let mut conv = ChatToMessagesStream::new("display-x");
        let mut wire: Vec<(String, Value)> = Vec::new();
        let mut usage = None;
        let mut done = false;
        let events = [
            chunk(json!({"role": "assistant", "content": ""}), Value::Null),
            chunk(json!({"reasoning_content": "think"}), Value::Null),
            chunk(json!({"content": "Hi"}), Value::Null),
            chunk(
                json!({"tool_calls": [{
                    "index": 0, "id": "c1", "type": "function",
                    "function": {"name": "f", "arguments": ""}
                }]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":"}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
            json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "choices": [],
                "usage": {"prompt_tokens": 12, "completion_tokens": 15,
                          "prompt_tokens_details": {"cached_tokens": 2}}
            })
            .to_string(),
            "[DONE]".to_string(),
        ];
        for data in &events {
            for out in conv.push_event(None, data) {
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
                "content_block_start", // thinking (index 0)
                "content_block_delta", // thinking_delta
                "content_block_stop",  // index 0
                "content_block_start", // text (index 1)
                "content_block_delta", // text_delta
                "content_block_stop",  // index 1
                "content_block_start", // tool_use (index 2)
                "content_block_delta", // input_json_delta
                "content_block_delta", // input_json_delta
                "content_block_stop",  // index 2
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(wire[0].1["message"]["model"], "display-x");
        assert_eq!(wire[1].1["content_block"]["type"], "thinking");
        assert_eq!(wire[4].1["content_block"]["type"], "text");
        assert_eq!(wire[4].1["index"], 1);
        assert_eq!(wire[7].1["content_block"]["id"], "c1");
        assert_eq!(wire[7].1["content_block"]["name"], "f");
        let args: String = [&wire[8].1, &wire[9].1]
            .iter()
            .map(|v| v["delta"]["partial_json"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(args, "{\"a\":1}");
        assert_eq!(wire[11].1["delta"]["stop_reason"], "tool_use");
        // usage: prompt 12 total - 2 cached = 10 exclusive input.
        assert_eq!(wire[11].1["usage"]["input_tokens"], 10);
        let usage = usage.unwrap();
        assert_eq!(usage["cache_read_input_tokens"], 2);
        assert_eq!(usage["output_tokens"], 15);
    }

    #[test]
    fn stream_without_finish_reason_defaults_from_tool_use() {
        let mut conv = ChatToMessagesStream::new("m");
        let events = [
            chunk(
                json!({"tool_calls": [{
                    "index": 0, "id": "c1", "type": "function",
                    "function": {"name": "f", "arguments": "{}"}
                }]}),
                Value::Null,
            ),
            "[DONE]".to_string(),
        ];
        let mut stop = None;
        for data in &events {
            for out in conv.push_event(None, data) {
                if let Output::Event(e) = &out {
                    if e.event.as_deref() == Some("message_delta") {
                        let v: Value = serde_json::from_str(&e.data).unwrap();
                        stop = v["delta"]["stop_reason"].as_str().map(str::to_string);
                    }
                }
            }
        }
        assert_eq!(stop.as_deref(), Some("tool_use"));
    }

    #[test]
    fn stream_maps_upstream_error() {
        let mut conv = ChatToMessagesStream::new("m");
        let outs = conv.push_event(
            None,
            &json!({"error": {"type": "server_error", "message": "boom"}}).to_string(),
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

    #[test]
    fn stream_handles_bare_done_without_chunks() {
        let mut conv = ChatToMessagesStream::new("m");
        let outs = conv.push_event(None, "[DONE]");
        // Still yields a well-formed message_start … message_stop envelope.
        let names: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Event(e) => e.event.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec!["message_start", "message_delta", "message_stop"]
        );
        assert!(matches!(outs.last(), Some(Output::Done)));
    }
}
