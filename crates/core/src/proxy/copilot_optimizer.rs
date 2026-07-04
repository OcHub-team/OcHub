//! GitHub Copilot request optimizer.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CopilotClassification {
    pub initiator: &'static str,
    pub is_warmup: bool,
    pub is_compact: bool,
    pub is_subagent: bool,
}

pub fn classify_request(
    body: &Value,
    has_anthropic_beta: bool,
    compact_detection: bool,
    subagent_detection: bool,
) -> CopilotClassification {
    let is_compact = compact_detection && is_compact_request(body);
    let is_subagent = subagent_detection && detect_subagent(body);

    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return CopilotClassification {
            initiator: "user",
            is_warmup: is_warmup_request(body, has_anthropic_beta, false),
            is_compact: false,
            is_subagent,
        };
    };
    let Some(last) = messages.last() else {
        return CopilotClassification {
            initiator: "user",
            is_warmup: is_warmup_request(body, has_anthropic_beta, false),
            is_compact: false,
            is_subagent,
        };
    };

    if last.get("role").and_then(Value::as_str) != Some("user") {
        return CopilotClassification {
            initiator: if is_subagent { "agent" } else { "user" },
            is_warmup: false,
            is_compact,
            is_subagent,
        };
    }

    let is_user_initiated = match last.get("content") {
        Some(Value::Array(blocks)) => !blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")),
        Some(Value::String(_)) => true,
        _ => false,
    };
    let initiator = if is_subagent || !is_user_initiated || is_compact {
        "agent"
    } else {
        "user"
    };

    CopilotClassification {
        initiator,
        is_warmup: initiator == "user" && is_warmup_request(body, has_anthropic_beta, is_compact),
        is_compact,
        is_subagent,
    }
}

pub fn merge_tool_results(mut body: Value) -> Value {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return body;
    };

    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };

        let mut tool_results = Vec::new();
        let mut text_blocks = Vec::new();
        let mut valid = true;
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_result") => tool_results.push(block.clone()),
                Some("text") => text_blocks.push(block.clone()),
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if valid && !tool_results.is_empty() && !text_blocks.is_empty() {
            message["content"] =
                Value::Array(merge_blocks_into_tool_results(tool_results, text_blocks));
        }
    }

    let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() else {
        return body;
    };
    if messages.len() <= 1 {
        return body;
    }

    let mut merged = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        if is_tool_result_only_message(&messages[i]) {
            let mut combined = Vec::new();
            while i < messages.len() && is_tool_result_only_message(&messages[i]) {
                if let Some(content) = messages[i].get("content").and_then(Value::as_array) {
                    combined.extend(content.iter().cloned());
                }
                i += 1;
            }
            if !combined.is_empty() {
                merged.push(serde_json::json!({
                    "role": "user",
                    "content": combined
                }));
            }
        } else {
            merged.push(messages[i].clone());
            i += 1;
        }
    }
    body["messages"] = Value::Array(merged);
    body
}

pub fn sanitize_orphan_tool_results(mut body: Value) -> Value {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return body;
    };
    if messages.len() < 2 {
        return body;
    }

    for i in 1..messages.len() {
        if messages[i].get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let prev_tool_use_ids: HashSet<String> =
            if messages[i - 1].get("role").and_then(Value::as_str) == Some("assistant") {
                messages[i - 1]
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(Value::as_str) == Some("tool_use")
                            })
                            .filter_map(|block| {
                                block.get("id").and_then(Value::as_str).map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                HashSet::new()
            };

        let Some(content) = messages[i].get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool_use_id.is_empty() || !prev_tool_use_ids.contains(tool_use_id) {
                let content = match block.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                *block = serde_json::json!({
                    "type": "text",
                    "text": format!("[Tool result for {tool_use_id}]: {content}")
                });
            }
        }
    }
    body
}

pub fn strip_thinking_blocks(mut body: Value) -> Value {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return body;
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        content.retain(|block| {
            !matches!(
                block.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            )
        });
    }
    body
}

pub fn deterministic_request_id(body: &Value, session_id: &str) -> String {
    if let Some(content) = find_last_user_content(body) {
        let mut hasher = Sha256::new();
        hasher.update(session_id.as_bytes());
        hasher.update(content.as_bytes());
        let result = hasher.finalize();

        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&result[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes).to_string()
    } else {
        Uuid::new_v4().to_string()
    }
}

pub fn deterministic_interaction_id(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"interaction:");
    hasher.update(session_id.as_bytes());
    let result = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&result[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(Uuid::from_bytes(bytes).to_string())
}

fn is_warmup_request(body: &Value, has_anthropic_beta: bool, is_compact: bool) -> bool {
    has_anthropic_beta
        && !is_compact
        && body
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(|tools| tools.is_empty())
}

fn is_compact_request(body: &Value) -> bool {
    if extract_system_text(body)
        .starts_with("You are a helpful AI assistant tasked with summarizing conversations")
    {
        return true;
    }
    let Some(last) = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
    else {
        return false;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let text = extract_text_from_message(last);
    text.contains("CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.")
        || (text.contains("Pending Tasks:") && text.contains("Current Work:"))
}

fn detect_subagent(body: &Value) -> bool {
    if extract_system_text(body).contains("__SUBAGENT_MARKER__") {
        return true;
    }
    if body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .is_some_and(|value| value.contains("_agent_"))
    {
        return true;
    }
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && extract_text_from_message(message).contains("__SUBAGENT_MARKER__")
            })
        })
}

fn merge_blocks_into_tool_results(
    mut tool_results: Vec<Value>,
    text_blocks: Vec<Value>,
) -> Vec<Value> {
    if tool_results.len() == text_blocks.len() {
        for (tool_result, text) in tool_results.iter_mut().zip(text_blocks.iter()) {
            append_text_to_tool_result(tool_result, text);
        }
    } else if let Some(last) = tool_results.last_mut() {
        for text in &text_blocks {
            append_text_to_tool_result(last, text);
        }
    }
    tool_results
}

fn append_text_to_tool_result(tool_result: &mut Value, text_block: &Value) {
    let text = text_block
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        return;
    }
    match tool_result.get_mut("content") {
        Some(Value::String(existing)) => {
            existing.push('\n');
            existing.push_str(text);
        }
        Some(Value::Array(items)) => items.push(serde_json::json!({"type": "text", "text": text})),
        _ => tool_result["content"] = Value::String(text.to_string()),
    }
}

fn extract_system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn extract_text_from_message(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn is_tool_result_only_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                !blocks.is_empty()
                    && blocks.iter().all(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
            })
}

fn find_last_user_content(body: &Value) -> Option<String> {
    for message in body.get("messages")?.as_array()?.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match message.get("content")? {
            Value::String(text) => return Some(text.clone()),
            Value::Array(blocks) => {
                let filtered = blocks
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(Value::as_str) != Some("tool_result")
                    })
                    .map(|block| {
                        let mut block = block.clone();
                        if let Some(object) = block.as_object_mut() {
                            object.remove("cache_control");
                        }
                        block
                    })
                    .collect::<Vec<_>>();
                if !filtered.is_empty() {
                    return Some(serde_json::to_string(&filtered).unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_tool_result_as_agent() {
        let body = json!({
            "messages": [{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}]
        });
        let result = classify_request(&body, true, true, false);
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_warmup);
    }

    #[test]
    fn merges_tool_result_and_text() {
        let result = merge_tool_results(json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "a"},
                {"type": "text", "text": "b"}
            ]}]
        }));
        let content = result["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_result");
        assert!(content[0]["content"].as_str().unwrap().contains("b"));
    }

    #[test]
    fn deterministic_ids_are_stable() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(
            deterministic_request_id(&body, "s"),
            deterministic_request_id(&body, "s")
        );
        assert_eq!(
            deterministic_interaction_id("s"),
            deterministic_interaction_id("s")
        );
    }
}
