//! Pair: **chat client ⇄ messages upstream**.
//!
//! - [`request_to_messages`] — chat request body → messages request body.
//! - [`response_from_message`] — complete messages message → `chat.completion`.
//! - [`MessagesToChatStream`] — messages SSE events → `chat.completion.chunk`
//!   events (push-based; feed it from SSE bytes or WebSocket frames).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::common::{
    MessagesRequestOptions, assemble, chat_content_to_blocks, content_to_plain_text,
    content_to_system_blocks, convert_chat_tools, convert_tool_choice, push_message,
    resolve_thinking_budget,
};
use crate::usage::{merge_messages_usage, messages_usage_to_chat};
use crate::util::{now_unix, short_id};
use crate::{ConvertError, Output, SignatureCapture, WireEvent};

/// Map a messages `stop_reason` to a chat `finish_reason`.
pub fn stop_reason_to_finish(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") | Some("pause_turn") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

// ---------------------------------------------------------------------------
// Request: chat → messages
// ---------------------------------------------------------------------------

/// Convert a chat request body to a messages request body.
pub fn request_to_messages(
    body: &Value,
    opts: &MessagesRequestOptions,
) -> Result<Value, ConvertError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ConvertError::InvalidRequest("request body must be a JSON object".into()))?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ConvertError::InvalidRequest("missing model field".into()))?;

    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(arr) = obj.get("messages").and_then(Value::as_array) {
        for m in arr {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    system.extend(content_to_system_blocks(m.get("content")));
                }
                "tool" => {
                    let tool_use_id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let text = content_to_plain_text(m.get("content"));
                    push_message(
                        &mut messages,
                        "user",
                        vec![json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": text,
                        })],
                    );
                }
                "assistant" => {
                    let mut blocks = chat_content_to_blocks(m.get("content"));
                    if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                        for tc in tcs {
                            let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = tc
                                .pointer("/function/name")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let args = tc
                                .pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}");
                            let input =
                                serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": input,
                            }));
                        }
                    }
                    // Skip a truly-empty assistant turn (no text, no tool_calls):
                    // the messages dialect rejects empty content arrays.
                    if blocks.is_empty() {
                        continue;
                    }
                    push_message(&mut messages, "assistant", blocks);
                }
                _ => {
                    let blocks = chat_content_to_blocks(m.get("content"));
                    if blocks.is_empty() {
                        continue;
                    }
                    push_message(&mut messages, "user", blocks);
                }
            }
        }
    }

    let max_tokens = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(opts.default_max_tokens);

    let tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| convert_chat_tools(t))
        .unwrap_or_default();
    let tool_choice = obj.get("tool_choice").and_then(convert_tool_choice);
    let thinking_budget = resolve_thinking_budget(
        obj,
        obj.get("reasoning_effort").and_then(Value::as_str),
        opts,
    );

    Ok(assemble(
        model,
        system,
        messages,
        max_tokens,
        obj.get("temperature").and_then(Value::as_f64),
        obj.get("stream").and_then(Value::as_bool),
        tools,
        tool_choice,
        thinking_budget,
        opts,
    ))
}

// ---------------------------------------------------------------------------
// Response (non-stream): messages message → chat.completion
// ---------------------------------------------------------------------------

/// Build the full `chat.completion` body from a complete messages message.
/// `display_model` is echoed back as the response `model` (the model the client
/// originally requested).
pub fn response_from_message(msg: &Value, display_model: &str) -> Value {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .map(|s| format!("chatcmpl-{s}"))
        .unwrap_or_else(|| format!("chatcmpl-{}", short_id()));

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        content.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    // Non-stream tool_calls have no `index` field (that belongs
                    // only to streaming deltas).
                    tool_calls.push(json!({
                        "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                            "arguments": input.to_string(),
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let finish = stop_reason_to_finish(msg.get("stop_reason").and_then(Value::as_str));
    let usage = msg.get("usage").map(messages_usage_to_chat).unwrap_or_else(
        || json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }),
    );

    json!({
        "id": id,
        "object": "chat.completion",
        "created": now_unix(),
        "model": display_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish,
        }],
        "usage": usage,
    })
}

// ---------------------------------------------------------------------------
// Stream: messages SSE → chat.completion.chunk
// ---------------------------------------------------------------------------

/// Push-based converter from messages stream events to chat chunks.
///
/// Feed every upstream event via [`push`](Self::push) (or
/// [`push_event`](Self::push_event)); each call returns the outputs produced by
/// that event, in order. After the upstream `message_stop` you will receive a
/// [`Output::Capture`] (when the turn produced signed thinking + tool calls) and
/// a final [`Output::Done`].
pub struct MessagesToChatStream {
    display_model: String,
    /// Emit the terminal `{choices: [], usage}` chunk (the client opted in via
    /// `stream_options.include_usage`). [`Output::Usage`] accounting items are
    /// emitted regardless of this flag.
    include_usage: bool,
    chat_id: String,
    created: u64,
    /// messages content-block index → chat tool_call index (tool_use blocks only).
    tool_index_by_block: HashMap<u64, u32>,
    tool_counter: u32,
    finish_reason: &'static str,
    last_usage: Option<Value>,
    /// Per-block (thinking text, signature) accumulation while streaming.
    thinking_acc: HashMap<u64, (String, String)>,
    thinking_blocks: Vec<Value>,
    tool_ids: Vec<String>,
}

impl MessagesToChatStream {
    pub fn new(display_model: impl Into<String>, include_usage: bool) -> Self {
        Self {
            display_model: display_model.into(),
            include_usage,
            chat_id: format!("chatcmpl-{}", short_id()),
            created: now_unix(),
            tool_index_by_block: HashMap::new(),
            tool_counter: 0,
            finish_reason: "stop",
            last_usage: None,
            thinking_acc: HashMap::new(),
            thinking_blocks: Vec::new(),
            tool_ids: Vec::new(),
        }
    }

    fn chunk(&self, choices: Value, usage: Option<Value>) -> Output {
        let mut obj = json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.display_model,
            "choices": choices,
        });
        if let Some(u) = usage {
            obj["usage"] = u;
        }
        Output::Event(WireEvent::data_only(obj.to_string()))
    }

    fn delta_chunk(&self, delta: Value) -> Output {
        self.chunk(
            json!([{ "index": 0, "delta": delta, "finish_reason": Value::Null }]),
            None,
        )
    }

    /// Feed one parsed wire event.
    pub fn push(&mut self, ev: &WireEvent) -> Vec<Output> {
        self.push_event(ev.event.as_deref(), &ev.data)
    }

    /// Feed one upstream event by (optional) name + data payload. When the name
    /// is absent (e.g. a WebSocket frame carrying only JSON) it is taken from
    /// the payload's `type` field.
    pub fn push_event(&mut self, name: Option<&str>, data: &str) -> Vec<Output> {
        let parsed: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let name = name
            .filter(|n| !n.is_empty())
            .or_else(|| parsed.get("type").and_then(Value::as_str))
            .unwrap_or("");
        let block_index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0);
        let mut out: Vec<Output> = Vec::new();

        match name {
            "message_start" => {
                if let Some(id) = parsed.pointer("/message/id").and_then(Value::as_str) {
                    self.chat_id = format!("chatcmpl-{id}");
                }
                self.created = now_unix();
                // Seed usage from message_start (full prompt/cache accounting)
                // and surface it for accounting, independent of include_usage.
                if let Some(u) = parsed.pointer("/message/usage") {
                    self.last_usage = Some(u.clone());
                    out.push(Output::Usage(u.clone()));
                }
                out.push(self.delta_chunk(json!({ "role": "assistant", "content": "" })));
            }
            "content_block_start" => {
                let block_type = parsed
                    .pointer("/content_block/type")
                    .and_then(Value::as_str);
                if block_type == Some("thinking") {
                    self.thinking_acc
                        .insert(block_index, (String::new(), String::new()));
                }
                if block_type == Some("tool_use") {
                    let tidx = self.tool_counter;
                    self.tool_counter += 1;
                    self.tool_index_by_block.insert(block_index, tidx);
                    let id = parsed
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !id.is_empty() {
                        self.tool_ids.push(id.to_string());
                    }
                    let tool_name = parsed
                        .pointer("/content_block/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    out.push(self.delta_chunk(json!({ "tool_calls": [{
                        "index": tidx,
                        "id": id,
                        "type": "function",
                        "function": { "name": tool_name, "arguments": "" }
                    }]})));
                }
            }
            "content_block_delta" => {
                let delta = parsed.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        out.push(self.delta_chunk(json!({ "content": text })));
                    }
                    Some("thinking_delta") => {
                        let text = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some((acc, _)) = self.thinking_acc.get_mut(&block_index) {
                            acc.push_str(text);
                        }
                        out.push(self.delta_chunk(json!({ "reasoning_content": text })));
                    }
                    Some("signature_delta") => {
                        // Captured for the signature round-trip; never forwarded.
                        if let Some(sig) = delta
                            .and_then(|d| d.get("signature"))
                            .and_then(Value::as_str)
                            && let Some((_, s)) = self.thinking_acc.get_mut(&block_index)
                        {
                            s.push_str(sig);
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let tidx = self
                            .tool_index_by_block
                            .get(&block_index)
                            .copied()
                            .unwrap_or(0);
                        out.push(self.delta_chunk(json!({ "tool_calls": [{
                            "index": tidx,
                            "function": { "arguments": partial }
                        }]})));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                // Finalize a thinking block (text + signature) for the round-trip.
                if let Some((text, sig)) = self.thinking_acc.remove(&block_index)
                    && !sig.is_empty()
                {
                    self.thinking_blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": sig,
                    }));
                }
            }
            "message_delta" => {
                if let Some(sr) = parsed.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish_reason = stop_reason_to_finish(Some(sr));
                }
                // Merge (don't overwrite): the delta usage carries the cumulative
                // output_tokens but omits the prompt/cache fields.
                if let Some(u) = parsed.get("usage") {
                    merge_messages_usage(&mut self.last_usage, u);
                    if let Some(merged) = &self.last_usage {
                        out.push(Output::Usage(merged.clone()));
                    }
                }
            }
            "message_stop" => {
                // Terminal chunk carrying finish_reason.
                out.push(self.chunk(
                    json!([{ "index": 0, "delta": {}, "finish_reason": self.finish_reason }]),
                    None,
                ));
                // Final usage-only chunk — only when the client opted in.
                if self.include_usage
                    && let Some(u) = &self.last_usage
                {
                    let usage = messages_usage_to_chat(u);
                    out.push(self.chunk(json!([]), Some(usage)));
                }
                if !self.thinking_blocks.is_empty() && !self.tool_ids.is_empty() {
                    out.push(Output::Capture(SignatureCapture {
                        thinking_blocks: std::mem::take(&mut self.thinking_blocks),
                        tool_use_ids: std::mem::take(&mut self.tool_ids),
                    }));
                }
                out.push(Output::Done);
            }
            "error" => {
                out.push(Output::Error(parsed));
            }
            _ => {} // ping, unknown → no client-visible chunk
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;
    use crate::test_fixtures::MESSAGES_SSE;

    fn opts() -> MessagesRequestOptions {
        MessagesRequestOptions::default()
    }

    #[test]
    fn request_maps_roles_tools_and_tool_results() {
        let body = json!({
            "model": "m1",
            "max_tokens": 100,
            "messages": [
                {"role": "system", "content": "be nice"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}}
                ]},
                {"role": "tool", "content": "sunny", "tool_call_id": "call_1"}
            ],
            "tools": [
                {"type": "function", "function": {"name": "get_weather", "description": "d",
                 "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}
            ],
            "tool_choice": "auto"
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["model"], "m1");
        assert_eq!(out["max_tokens"], 100);
        assert_eq!(out["system"][0]["text"], "be nice");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "SF");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(out["tools"][0]["name"], "get_weather");
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(out["tool_choice"]["type"], "auto");
    }

    #[test]
    fn request_prepends_user_turn_when_first_is_assistant() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "primed"}]
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][1]["role"], "assistant");
    }

    #[test]
    fn request_resolves_thinking_from_effort_and_clamps_max_tokens() {
        let body = json!({
            "model": "m",
            "max_tokens": 1000,
            "reasoning_effort": "high",
            "temperature": 0.2,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["thinking"]["budget_tokens"], 16000);
        // max_tokens raised above the budget; temperature dropped with thinking on.
        assert!(out["max_tokens"].as_i64().unwrap() > 16000);
        assert!(out.get("temperature").is_none());
    }

    #[test]
    fn request_injects_cache_breakpoints() {
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "a"},
                {"role": "user", "content": "two"},
                {"role": "assistant", "content": "b"},
                {"role": "user", "content": "three"}
            ]
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["system"][0]["cache_control"]["type"], "ephemeral");
        let msgs = out["messages"].as_array().unwrap();
        // first user + last two user turns carry breakpoints
        assert!(msgs[0]["content"][0].get("cache_control").is_some());
        assert!(msgs[2]["content"][0].get("cache_control").is_some());
        assert!(msgs[4]["content"][0].get("cache_control").is_some());
        assert!(msgs[1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn request_drops_forced_tool_choice_for_missing_tool() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "real"}}],
            "tool_choice": {"type": "function", "function": {"name": "ghost"}}
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["tool_choice"]["type"], "auto");
    }

    #[test]
    fn nonstream_response_carries_text_reasoning_and_tools() {
        let msg = json!({
            "id": "m01", "type": "message", "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "let me see", "signature": "s"},
                {"type": "text", "text": "Hi"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 3}
        });
        let out = response_from_message(&msg, "display-x");
        assert_eq!(out["id"], "chatcmpl-m01");
        assert_eq!(out["model"], "display-x");
        let m = &out["choices"][0]["message"];
        assert_eq!(m["content"], "Hi");
        assert_eq!(m["reasoning_content"], "let me see");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn stream_converts_canonical_fixture() {
        let mut parser = SseParser::new();
        let events = parser.feed(MESSAGES_SSE.as_bytes());
        let mut conv = MessagesToChatStream::new("display-x", true);
        let mut chunks: Vec<Value> = Vec::new();
        let mut usages = 0;
        let mut captures = 0;
        let mut done = false;
        for ev in &events {
            for out in conv.push(ev) {
                match out {
                    Output::Event(e) => {
                        assert_eq!(e.event, None);
                        chunks.push(serde_json::from_str(&e.data).unwrap());
                    }
                    Output::Usage(_) => usages += 1,
                    Output::Capture(_) => captures += 1,
                    Output::Done => done = true,
                    Output::Error(_) => panic!("unexpected error"),
                }
            }
        }
        assert!(done);
        assert_eq!(usages, 2); // message_start seed + message_delta merge
        assert_eq!(captures, 0); // fixture has no signed thinking
        // role chunk, text delta, tool start, 2 arg deltas, finish, usage chunk
        assert_eq!(chunks.len(), 7);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hello");
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        let args: String = chunks[3..5]
            .iter()
            .map(|c| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(args, "{\"city\":\"SF\"}");
        assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
        // Terminal usage chunk: prompt = 10 input + 2 cached, completion = 15.
        assert_eq!(chunks[6]["usage"]["prompt_tokens"], 12);
        assert_eq!(chunks[6]["usage"]["completion_tokens"], 15);
        assert!(chunks[6]["choices"].as_array().unwrap().is_empty());
        // All chunks echo the display model.
        for c in &chunks {
            assert_eq!(c["model"], "display-x");
        }
    }

    #[test]
    fn stream_without_include_usage_omits_usage_chunk() {
        let mut parser = SseParser::new();
        let events = parser.feed(MESSAGES_SSE.as_bytes());
        let mut conv = MessagesToChatStream::new("m", false);
        let mut chunk_count = 0;
        let mut usage_outputs = 0;
        for ev in &events {
            for out in conv.push(ev) {
                match out {
                    Output::Event(_) => chunk_count += 1,
                    Output::Usage(_) => usage_outputs += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(chunk_count, 6); // no terminal usage chunk
        assert_eq!(usage_outputs, 2); // accounting unaffected by the client flag
    }

    #[test]
    fn stream_captures_signed_thinking() {
        let mut conv = MessagesToChatStream::new("m", false);
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t9","name":"f","input":{}}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let mut capture = None;
        for (name, data) in events {
            for out in conv.push_event(Some(name), &data.to_string()) {
                if let Output::Capture(c) = out {
                    capture = Some(c);
                }
            }
        }
        let capture = capture.expect("capture emitted");
        assert_eq!(capture.tool_use_ids, vec!["t9".to_string()]);
        assert_eq!(capture.thinking_blocks[0]["thinking"], "hmm");
        assert_eq!(capture.thinking_blocks[0]["signature"], "SIG");
    }
}
