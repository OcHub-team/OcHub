//! Aggregate a streamed response body back into its single non-stream JSON form.
//!
//! Some upstreams force streaming even when the client asked for a non-stream
//! response. The non-stream converters then receive an SSE byte body instead of a
//! single JSON object; these helpers detect that and rebuild the complete
//! message / response object first.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

use crate::usage::merge_messages_usage;

/// Parse a drained *messages*-dialect body into a complete message value,
/// transparently handling either a raw JSON object or an SSE event stream.
pub fn parse_message_body(buf: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(buf);
    let trimmed = text.trim_start();
    if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
        aggregate_messages_sse(&text)
    } else {
        serde_json::from_str::<Value>(trimmed).ok()
    }
}

/// Parse a drained *responses*-dialect body into a complete `response` value,
/// transparently handling either a raw JSON object or an SSE event stream.
pub fn parse_response_body(buf: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(buf);
    let trimmed = text.trim_start();
    if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
        aggregate_responses_sse(&text)
    } else {
        serde_json::from_str::<Value>(trimmed).ok()
    }
}

fn append_string_field(block: &mut Value, field: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(obj) = block.as_object_mut() {
        match obj
            .entry(field.to_string())
            .or_insert(Value::String(String::new()))
        {
            Value::String(s) => s.push_str(text),
            other => *other = Value::String(text.to_string()),
        }
    }
}

/// Reduce a messages SSE stream (`message_start` … `message_stop`) to the final
/// complete message JSON.
pub fn aggregate_messages_sse(text: &str) -> Option<Value> {
    let mut message: Option<Value> = None;
    let mut blocks: BTreeMap<usize, Value> = BTreeMap::new();
    let mut input_json: HashMap<usize, String> = HashMap::new();
    let mut usage: Option<Value> = None;

    for raw_block in text.split("\n\n") {
        let mut data = "";
        for line in raw_block.lines() {
            if let Some(d) = line.strip_prefix("data:") {
                data = d.trim();
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let event = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event {
            "message_start" => {
                if let Some(m) = value.get("message") {
                    usage = m.get("usage").cloned();
                    if let Some(content) = m.get("content").and_then(Value::as_array) {
                        for (i, b) in content.iter().enumerate() {
                            blocks.insert(i, b.clone());
                        }
                    }
                    message = Some(m.clone());
                }
            }
            "content_block_start" => {
                if let (Some(idx), Some(cb)) = (
                    value.get("index").and_then(Value::as_u64),
                    value.get("content_block"),
                ) {
                    blocks.insert(idx as usize, cb.clone());
                }
            }
            "content_block_delta" => {
                let idx = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta");
                let block = blocks
                    .entry(idx)
                    .or_insert_with(|| json!({ "type": "text", "text": "" }));
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => append_string_field(
                        block,
                        "text",
                        delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    Some("thinking_delta") => append_string_field(
                        block,
                        "thinking",
                        delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    Some("signature_delta") => append_string_field(
                        block,
                        "signature",
                        delta
                            .and_then(|d| d.get("signature"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    Some("input_json_delta") => {
                        let partial = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        input_json.entry(idx).or_default().push_str(partial);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(partial) = input_json.remove(&idx) {
                    let input = if partial.trim().is_empty() {
                        Value::Object(Map::new())
                    } else {
                        serde_json::from_str::<Value>(&partial)
                            .unwrap_or_else(|_| Value::Object(Map::new()))
                    };
                    if let Some(obj) = blocks.get_mut(&idx).and_then(Value::as_object_mut) {
                        obj.insert("input".to_string(), input);
                    }
                }
            }
            "message_delta" => {
                // Merge, not overwrite: the delta usage carries the cumulative
                // output_tokens but omits the prompt/cache fields seeded by
                // message_start.
                if let Some(u) = value.get("usage") {
                    merge_messages_usage(&mut usage, u);
                }
                if let Some(delta) = value.get("delta")
                    && let Some(obj) = message.as_mut().and_then(Value::as_object_mut)
                {
                    if let Some(sr) = delta.get("stop_reason") {
                        obj.insert("stop_reason".to_string(), sr.clone());
                    }
                    if let Some(ss) = delta.get("stop_sequence") {
                        obj.insert("stop_sequence".to_string(), ss.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let mut message = message?;
    if let Some(obj) = message.as_object_mut() {
        obj.insert(
            "content".to_string(),
            Value::Array(blocks.into_values().collect()),
        );
        if let Some(u) = usage {
            obj.insert("usage".to_string(), u);
        }
    }
    Some(message)
}

/// Reduce a responses SSE stream to its final `response` object: take the
/// `response.completed` payload, backfilling `output` from the individual
/// `response.output_item.done` events when the terminal payload omits them.
pub fn aggregate_responses_sse(text: &str) -> Option<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut completed: Option<Value> = None;

    for raw_block in text.split("\n\n") {
        let mut data = "";
        for line in raw_block.lines() {
            if let Some(d) = line.strip_prefix("data:") {
                data = d.trim();
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    items.push(item.clone());
                }
            }
            Some("response.completed") => {
                completed = value.get("response").cloned();
            }
            _ => {}
        }
    }

    let mut response = completed?;
    let empty = response
        .get("output")
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if empty
        && !items.is_empty()
        && let Some(obj) = response.as_object_mut()
    {
        obj.insert("output".to_string(), Value::Array(items));
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_messages_sse_into_message() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":1}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let msg = parse_message_body(sse.as_bytes()).unwrap();
        assert_eq!(msg["id"], "m1");
        assert_eq!(msg["stop_reason"], "tool_use");
        let content = msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "SIG");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["input"]["a"], 1);
        assert_eq!(msg["usage"]["output_tokens"], 9);
        // input_tokens from message_start survives the message_delta merge.
        assert_eq!(msg["usage"]["input_tokens"], 5);
    }

    #[test]
    fn passes_through_plain_json() {
        let body = br#"{"type":"message","id":"x","content":[{"type":"text","text":"hi"}]}"#;
        let msg = parse_message_body(body).unwrap();
        assert_eq!(msg["content"][0]["text"], "hi");
    }

    #[test]
    fn aggregates_responses_sse_and_backfills_output() {
        let sse = concat!(
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"output\":[],\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n",
        );
        let resp = parse_response_body(sse.as_bytes()).unwrap();
        assert_eq!(resp["id"], "resp_1");
        assert_eq!(resp["output"][0]["call_id"], "c1");
        assert_eq!(resp["usage"]["input_tokens"], 4);
    }
}
