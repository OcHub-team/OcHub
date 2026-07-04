//! Reactive Claude thinking-signature rectifier.

use crate::db::proxy_types::RectifierConfig;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct RectifyResult {
    pub applied: bool,
    pub removed_thinking_blocks: usize,
    pub removed_redacted_thinking_blocks: usize,
    pub removed_signature_fields: usize,
}

pub fn should_rectify_thinking_signature(
    error_message: Option<&str>,
    config: &RectifierConfig,
) -> bool {
    if !config.enabled || !config.request_thinking_signature {
        return false;
    }

    let Some(message) = error_message else {
        return false;
    };
    let lower = message.to_ascii_lowercase();

    (lower.contains("invalid")
        && lower.contains("signature")
        && lower.contains("thinking")
        && lower.contains("block"))
        || (lower.contains("thought signature")
            && (lower.contains("not valid") || lower.contains("invalid")))
        || lower.contains("must start with a thinking block")
        || (lower.contains("expected")
            && (lower.contains("thinking") || lower.contains("redacted_thinking"))
            && lower.contains("found")
            && lower.contains("tool_use"))
        || (lower.contains("signature") && lower.contains("field required"))
        || (lower.contains("signature") && lower.contains("extra inputs are not permitted"))
        || ((lower.contains("thinking") || lower.contains("redacted_thinking"))
            && lower.contains("cannot be modified"))
        || lower.contains("非法请求")
        || lower.contains("illegal request")
        || lower.contains("invalid request")
}

pub fn rectify_anthropic_request(body: &mut Value) -> RectifyResult {
    let mut result = RectifyResult::default();
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return result;
    };

    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };

        let mut next = Vec::with_capacity(content.len());
        let mut changed = false;
        for block in content.iter() {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    result.removed_thinking_blocks += 1;
                    changed = true;
                    continue;
                }
                Some("redacted_thinking") => {
                    result.removed_redacted_thinking_blocks += 1;
                    changed = true;
                    continue;
                }
                _ => {}
            }

            if block.get("signature").is_some() {
                let mut clone = block.clone();
                if let Some(object) = clone.as_object_mut() {
                    object.remove("signature");
                    result.removed_signature_fields += 1;
                    changed = true;
                }
                next.push(clone);
            } else {
                next.push(block.clone());
            }
        }

        if changed {
            result.applied = true;
            *content = next;
        }
    }

    let snapshot = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if should_remove_top_level_thinking(body, &snapshot) {
        if let Some(object) = body.as_object_mut() {
            object.remove("thinking");
            result.applied = true;
        }
    }

    result
}

pub fn normalize_thinking_type(body: Value) -> Value {
    body
}

fn should_remove_top_level_thinking(body: &Value, messages: &[Value]) -> bool {
    let thinking_enabled =
        body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled");
    if !thinking_enabled {
        return false;
    }

    let Some(content) = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if content.is_empty() {
        return false;
    }

    let first_type = content
        .first()
        .and_then(|block| block.get("type").and_then(Value::as_str));
    let missing_prefix = !matches!(first_type, Some("thinking" | "redacted_thinking"));
    missing_prefix
        && content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
}
