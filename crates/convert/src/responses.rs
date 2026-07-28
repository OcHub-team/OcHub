//! Pair: **responses client ⇄ messages upstream**.
//!
//! - [`request_to_messages`] — responses request body → messages request body.
//! - [`response_from_message`] — complete messages message → `response` object.
//! - [`MessagesToResponsesStream`] — messages SSE events → typed `response.*`
//!   event sequence (push-based; usable over SSE or WebSocket).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::common::{
    MessagesRequestOptions, assemble, content_to_plain_text, convert_responses_tools,
    convert_tool_choice, image_block_from_url, push_message, resolve_thinking_budget,
};
use crate::usage::{merge_messages_usage, messages_usage_to_responses};
use crate::util::{now_unix, short_id};
use crate::{ConvertError, Output, SignatureCapture, WireEvent};

// ---------------------------------------------------------------------------
// Request: responses → messages
// ---------------------------------------------------------------------------

/// Convert a responses-dialect content array (`input_text` / `output_text` /
/// `input_image` parts) to messages content blocks.
fn responses_content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({ "type": "text", "text": s })],
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("input_image") | Some("image") => {
                        let url = match p.get("image_url") {
                            Some(Value::Object(m)) => m.get("url").and_then(Value::as_str),
                            Some(Value::String(s)) => Some(s.as_str()),
                            _ => None,
                        };
                        if let Some(block) = url.and_then(image_block_from_url) {
                            blocks.push(block);
                        }
                    }
                    _ => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        }
                    }
                }
            }
            blocks
        }
        _ => vec![],
    }
}

/// Convert a responses request body to a messages request body.
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

    if let Some(instr) = obj.get("instructions").and_then(Value::as_str)
        && !instr.is_empty()
    {
        system.push(json!({ "type": "text", "text": instr }));
    }

    match obj.get("input") {
        Some(Value::String(s)) => {
            push_message(
                &mut messages,
                "user",
                vec![json!({ "type": "text", "text": s })],
            );
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                match item_type {
                    "message" => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        if role == "system" || role == "developer" {
                            system.extend(responses_content_to_blocks(item.get("content")));
                        } else {
                            let blocks = responses_content_to_blocks(item.get("content"));
                            if blocks.is_empty() {
                                continue;
                            }
                            let role = if role == "assistant" {
                                "assistant"
                            } else {
                                "user"
                            };
                            push_message(&mut messages, role, blocks);
                        }
                    }
                    "function_call" => {
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input =
                            serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
                        push_message(
                            &mut messages,
                            "assistant",
                            vec![json!({
                                "type": "tool_use",
                                "id": call_id,
                                "name": name,
                                "input": input,
                            })],
                        );
                    }
                    "function_call_output" => {
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let output = content_to_plain_text(item.get("output"));
                        push_message(
                            &mut messages,
                            "user",
                            vec![json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": output,
                            })],
                        );
                    }
                    // Reasoning items have no messages-side input representation
                    // (the upstream signs its own thinking blocks); drop them.
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let max_tokens = obj
        .get("max_output_tokens")
        .or_else(|| obj.get("max_tokens"))
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(opts.default_max_tokens);

    let tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| convert_responses_tools(t))
        .unwrap_or_default();
    let tool_choice = obj.get("tool_choice").and_then(convert_tool_choice);
    // The responses dialect carries reasoning effort under `reasoning.effort`.
    let thinking_budget = resolve_thinking_budget(
        obj,
        obj.get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str),
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
// Response (non-stream): messages message → response object
// ---------------------------------------------------------------------------

/// A minimal but well-formed `response` object.
fn response_obj(
    id: &str,
    model: &str,
    created: u64,
    status: &str,
    output: Value,
    usage: Option<Value>,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "model": model,
        "status": status,
        "output": output,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "tools": [],
        "instructions": Value::Null,
        "metadata": {},
        "temperature": Value::Null,
        "top_p": Value::Null,
        "usage": usage.unwrap_or(Value::Null),
    })
}

/// Build the full `response` body (status `completed`) from a complete messages
/// message.
pub fn response_from_message(msg: &Value, display_model: &str) -> Value {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .map(|s| format!("resp_{s}"))
        .unwrap_or_else(|| format!("resp_{}", short_id()));

    let mut output: Vec<Value> = Vec::new();
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", short_id()),
                        "role": "assistant",
                        "status": "completed",
                        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    }));
                }
                Some("thinking") => {
                    let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("rs_{}", short_id()),
                        "summary": [{ "type": "summary_text", "text": text }],
                    }));
                }
                Some("tool_use") => {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let call_id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    output.push(json!({
                        "type": "function_call",
                        "id": format!("fc_{}", short_id()),
                        "call_id": call_id,
                        "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                        "arguments": input.to_string(),
                        "status": "completed",
                    }));
                }
                _ => {}
            }
        }
    }

    let usage = msg.get("usage").map(messages_usage_to_responses);
    response_obj(
        &id,
        display_model,
        now_unix(),
        "completed",
        Value::Array(output),
        usage,
    )
}

// ---------------------------------------------------------------------------
// Stream: messages SSE → response.* events
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

struct BlockState {
    kind: BlockKind,
    output_index: u32,
    item_id: String,
    /// Accumulated text (Text/Thinking) or tool-call arguments JSON (Tool).
    accum: String,
    /// Accumulated thinking-block signature (Thinking only).
    signature: String,
    name: String,
    call_id: String,
}

/// Push-based converter from messages stream events to the typed `response.*`
/// event sequence, ending in `response.completed`.
pub struct MessagesToResponsesStream {
    display_model: String,
    seq: u64,
    response_id: String,
    created: u64,
    output_counter: u32,
    blocks: HashMap<u64, BlockState>,
    finalized: Vec<Value>,
    last_usage: Option<Value>,
    thinking_blocks: Vec<Value>,
    tool_ids: Vec<String>,
}

impl MessagesToResponsesStream {
    pub fn new(display_model: impl Into<String>) -> Self {
        Self {
            display_model: display_model.into(),
            seq: 0,
            response_id: format!("resp_{}", short_id()),
            created: now_unix(),
            output_counter: 0,
            blocks: HashMap::new(),
            finalized: Vec::new(),
            last_usage: None,
            thinking_blocks: Vec::new(),
            tool_ids: Vec::new(),
        }
    }

    /// Emit one typed event, stamping `type` + an incrementing `sequence_number`.
    fn emit(&mut self, event: &str, mut body: Value) -> Output {
        body["type"] = json!(event);
        body["sequence_number"] = json!(self.seq);
        self.seq += 1;
        Output::Event(WireEvent::new(event, body.to_string()))
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
        let block_index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0);
        let mut out: Vec<Output> = Vec::new();

        match name {
            "message_start" => {
                self.response_id = format!("resp_{}", short_id());
                self.created = now_unix();
                if let Some(u) = parsed.pointer("/message/usage") {
                    self.last_usage = Some(u.clone());
                    out.push(Output::Usage(u.clone()));
                }
                let obj = response_obj(
                    &self.response_id,
                    &self.display_model,
                    self.created,
                    "in_progress",
                    json!([]),
                    None,
                );
                let created = self.emit("response.created", json!({ "response": obj.clone() }));
                out.push(created);
                let in_progress = self.emit("response.in_progress", json!({ "response": obj }));
                out.push(in_progress);
            }
            "content_block_start" => {
                let cb = parsed.get("content_block");
                let cb_type = cb.and_then(|c| c.get("type")).and_then(Value::as_str);
                let output_index = self.output_counter;
                self.output_counter += 1;
                match cb_type {
                    Some("tool_use") => {
                        let call_id = cb
                            .and_then(|c| c.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool_name = cb
                            .and_then(|c| c.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let item_id = format!("fc_{}", short_id());
                        let ev = self.emit(
                            "response.output_item.added",
                            json!({
                                "output_index": output_index,
                                "item": {
                                    "type": "function_call",
                                    "id": item_id,
                                    "call_id": call_id,
                                    "name": tool_name,
                                    "arguments": "",
                                    "status": "in_progress",
                                }
                            }),
                        );
                        out.push(ev);
                        self.blocks.insert(
                            block_index,
                            BlockState {
                                kind: BlockKind::Tool,
                                output_index,
                                item_id,
                                accum: String::new(),
                                signature: String::new(),
                                name: tool_name,
                                call_id,
                            },
                        );
                    }
                    Some("thinking") => {
                        let item_id = format!("rs_{}", short_id());
                        let added = self.emit(
                            "response.output_item.added",
                            json!({
                                "output_index": output_index,
                                "item": { "type": "reasoning", "id": item_id, "summary": [] }
                            }),
                        );
                        out.push(added);
                        let part = self.emit(
                            "response.reasoning_summary_part.added",
                            json!({
                                "item_id": item_id,
                                "output_index": output_index,
                                "summary_index": 0,
                                "part": { "type": "summary_text", "text": "" }
                            }),
                        );
                        out.push(part);
                        self.blocks.insert(
                            block_index,
                            BlockState {
                                kind: BlockKind::Thinking,
                                output_index,
                                item_id,
                                accum: String::new(),
                                signature: String::new(),
                                name: String::new(),
                                call_id: String::new(),
                            },
                        );
                    }
                    _ => {
                        // text (and any unknown block treated as text)
                        let item_id = format!("msg_{}", short_id());
                        let added = self.emit(
                            "response.output_item.added",
                            json!({
                                "output_index": output_index,
                                "item": {
                                    "type": "message",
                                    "id": item_id,
                                    "role": "assistant",
                                    "status": "in_progress",
                                    "content": [],
                                }
                            }),
                        );
                        out.push(added);
                        let part = self.emit(
                            "response.content_part.added",
                            json!({
                                "item_id": item_id,
                                "output_index": output_index,
                                "content_index": 0,
                                "part": { "type": "output_text", "text": "", "annotations": [] }
                            }),
                        );
                        out.push(part);
                        self.blocks.insert(
                            block_index,
                            BlockState {
                                kind: BlockKind::Text,
                                output_index,
                                item_id,
                                accum: String::new(),
                                signature: String::new(),
                                name: String::new(),
                                call_id: String::new(),
                            },
                        );
                    }
                }
            }
            "content_block_delta" => {
                let delta = parsed.get("delta");
                let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
                let Some(state) = self.blocks.get_mut(&block_index) else {
                    return out;
                };
                match delta_type {
                    Some("text_delta") => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        state.accum.push_str(text);
                        let body = json!({
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "content_index": 0,
                            "delta": text,
                            "logprobs": [],
                        });
                        let ev = self.emit("response.output_text.delta", body);
                        out.push(ev);
                    }
                    Some("thinking_delta") => {
                        let text = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        state.accum.push_str(text);
                        let body = json!({
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "summary_index": 0,
                            "delta": text,
                        });
                        let ev = self.emit("response.reasoning_summary_text.delta", body);
                        out.push(ev);
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        state.accum.push_str(partial);
                        let body = json!({
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "delta": partial,
                        });
                        let ev = self.emit("response.function_call_arguments.delta", body);
                        out.push(ev);
                    }
                    Some("signature_delta") => {
                        // Captured for the signature round-trip; never emitted.
                        if let Some(sig) = delta
                            .and_then(|d| d.get("signature"))
                            .and_then(Value::as_str)
                        {
                            state.signature.push_str(sig);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(state) = self.blocks.remove(&block_index) else {
                    return out;
                };
                match state.kind {
                    BlockKind::Text => {
                        let done = self.emit(
                            "response.output_text.done",
                            json!({
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "content_index": 0,
                                "text": state.accum,
                                "logprobs": [],
                            }),
                        );
                        out.push(done);
                        let part_done = self.emit(
                            "response.content_part.done",
                            json!({
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "content_index": 0,
                                "part": { "type": "output_text", "text": state.accum, "annotations": [] }
                            }),
                        );
                        out.push(part_done);
                        let item = json!({
                            "type": "message",
                            "id": state.item_id,
                            "role": "assistant",
                            "status": "completed",
                            "content": [{ "type": "output_text", "text": state.accum, "annotations": [] }],
                        });
                        let item_done = self.emit(
                            "response.output_item.done",
                            json!({ "output_index": state.output_index, "item": item.clone() }),
                        );
                        out.push(item_done);
                        self.finalized.push(item);
                    }
                    BlockKind::Thinking => {
                        let text_done = self.emit(
                            "response.reasoning_summary_text.done",
                            json!({
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "summary_index": 0,
                                "text": state.accum,
                            }),
                        );
                        out.push(text_done);
                        let part_done = self.emit(
                            "response.reasoning_summary_part.done",
                            json!({
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "summary_index": 0,
                                "part": { "type": "summary_text", "text": state.accum }
                            }),
                        );
                        out.push(part_done);
                        let item = json!({
                            "type": "reasoning",
                            "id": state.item_id,
                            "summary": [{ "type": "summary_text", "text": state.accum }],
                        });
                        let item_done = self.emit(
                            "response.output_item.done",
                            json!({ "output_index": state.output_index, "item": item.clone() }),
                        );
                        out.push(item_done);
                        self.finalized.push(item);
                        // Capture the signed thinking block for the round-trip.
                        if !state.signature.is_empty() {
                            self.thinking_blocks.push(json!({
                                "type": "thinking",
                                "thinking": state.accum,
                                "signature": state.signature,
                            }));
                        }
                    }
                    BlockKind::Tool => {
                        if !state.call_id.is_empty() {
                            self.tool_ids.push(state.call_id.clone());
                        }
                        let args_done = self.emit(
                            "response.function_call_arguments.done",
                            json!({
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "name": state.name,
                                "arguments": state.accum,
                            }),
                        );
                        out.push(args_done);
                        let item = json!({
                            "type": "function_call",
                            "id": state.item_id,
                            "call_id": state.call_id,
                            "name": state.name,
                            "arguments": state.accum,
                            "status": "completed",
                        });
                        let item_done = self.emit(
                            "response.output_item.done",
                            json!({ "output_index": state.output_index, "item": item.clone() }),
                        );
                        out.push(item_done);
                        self.finalized.push(item);
                    }
                }
            }
            "message_delta" => {
                // Merge (don't overwrite): keep the message_start prompt/cache
                // accounting; overlay the cumulative output_tokens.
                if let Some(u) = parsed.get("usage") {
                    merge_messages_usage(&mut self.last_usage, u);
                    if let Some(merged) = &self.last_usage {
                        out.push(Output::Usage(merged.clone()));
                    }
                }
            }
            "message_stop" => {
                let usage = self.last_usage.as_ref().map(messages_usage_to_responses);
                let obj = response_obj(
                    &self.response_id,
                    &self.display_model,
                    self.created,
                    "completed",
                    Value::Array(self.finalized.clone()),
                    usage,
                );
                let completed = self.emit("response.completed", json!({ "response": obj }));
                out.push(completed);
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
            _ => {}
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::MessagesRequestOptions;
    use crate::sse::SseParser;
    use crate::test_fixtures::MESSAGES_SSE;

    fn opts() -> MessagesRequestOptions {
        MessagesRequestOptions::default()
    }

    #[test]
    fn request_maps_instructions_input_and_function_items() {
        let body = json!({
            "model": "m1",
            "instructions": "be brief",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "weather?"}
                ]},
                {"type": "function_call", "call_id": "c1", "name": "get_weather",
                 "arguments": "{\"city\":\"SF\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"}
            ],
            "tools": [
                {"type": "function", "name": "get_weather", "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "tool_choice": "required",
            "reasoning": {"effort": "low"}
        });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["system"][0]["text"], "be brief");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "SF");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        // Only the plain function tool survives; server-side tools are dropped.
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["thinking"]["budget_tokens"], 4096);
        // `required` + thinking → relaxed to auto.
        assert_eq!(out["tool_choice"]["type"], "auto");
    }

    #[test]
    fn request_accepts_string_input() {
        let body = json!({ "model": "m", "input": "hello" });
        let out = request_to_messages(&body, &opts()).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn nonstream_response_orders_reasoning_text_and_tool() {
        let msg = json!({
            "id": "m1", "type": "message", "role": "assistant", "model": "up-x",
            "content": [
                {"type": "thinking", "thinking": "let me see"},
                {"type": "text", "text": "Hi"},
                {"type": "tool_use", "id": "tool_1", "name": "f", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 3}
        });
        let out = response_from_message(&msg, "display-x");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["model"], "display-x");
        assert_eq!(out["output"][0]["type"], "reasoning");
        assert_eq!(out["output"][0]["summary"][0]["text"], "let me see");
        assert_eq!(out["output"][1]["type"], "message");
        assert_eq!(out["output"][1]["content"][0]["text"], "Hi");
        assert_eq!(out["output"][2]["type"], "function_call");
        assert_eq!(out["output"][2]["call_id"], "tool_1");
        assert_eq!(out["output"][2]["arguments"], "{\"a\":1}");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(out["usage"]["output_tokens"], 3);
    }

    #[test]
    fn stream_converts_canonical_fixture() {
        let mut parser = SseParser::new();
        let events = parser.feed(MESSAGES_SSE.as_bytes());
        let mut conv = MessagesToResponsesStream::new("display-x");
        let mut typed: Vec<(String, Value)> = Vec::new();
        let mut done = false;
        for ev in &events {
            for out in conv.push(ev) {
                match out {
                    Output::Event(e) => typed.push((
                        e.event.clone().unwrap(),
                        serde_json::from_str(&e.data).unwrap(),
                    )),
                    Output::Done => done = true,
                    Output::Error(_) => panic!("unexpected error"),
                    _ => {}
                }
            }
        }
        assert!(done);
        let names: Vec<&str> = typed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // sequence_number strictly increments
        for (i, (_, body)) in typed.iter().enumerate() {
            assert_eq!(body["sequence_number"], i as u64);
        }
        let completed = &typed.last().unwrap().1["response"];
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["model"], "display-x");
        let output = completed["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["content"][0]["text"], "Hello");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "tool_01");
        assert_eq!(output[1]["arguments"], "{\"city\":\"SF\"}");
        // usage: input total = 10 + 2 cached; output merged to 15
        assert_eq!(completed["usage"]["input_tokens"], 12);
        assert_eq!(completed["usage"]["output_tokens"], 15);
    }
}
