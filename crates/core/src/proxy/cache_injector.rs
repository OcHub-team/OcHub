//! Bedrock prompt-cache breakpoint injector.

use crate::db::proxy_types::OptimizerConfig;
use serde_json::{json, Value};

pub fn inject(body: &mut Value, config: &OptimizerConfig) {
    if !config.cache_injection {
        return;
    }

    let existing = count_existing(body);
    upgrade_existing_ttl(body, &config.cache_ttl);

    let mut budget = 4usize.saturating_sub(existing);
    if budget == 0 {
        return;
    }

    if budget > 0 {
        if let Some(last) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
        {
            if last.get("cache_control").is_none() {
                if let Some(object) = last.as_object_mut() {
                    object.insert(
                        "cache_control".to_string(),
                        make_cache_control(&config.cache_ttl),
                    );
                    budget -= 1;
                }
            }
        }
    }

    if budget > 0 {
        if let Some(text) = body
            .get("system")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            body["system"] = json!([{ "type": "text", "text": text }]);
        }
        if let Some(last) = body
            .get_mut("system")
            .and_then(Value::as_array_mut)
            .and_then(|system| system.last_mut())
        {
            if last.get("cache_control").is_none() {
                if let Some(object) = last.as_object_mut() {
                    object.insert(
                        "cache_control".to_string(),
                        make_cache_control(&config.cache_ttl),
                    );
                    budget -= 1;
                }
            }
        }
    }

    if budget > 0 {
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            if let Some(assistant) = messages
                .iter_mut()
                .rev()
                .find(|msg| msg.get("role").and_then(Value::as_str) == Some("assistant"))
            {
                if let Some(blocks) = assistant.get_mut("content").and_then(Value::as_array_mut) {
                    if let Some(block) = blocks.iter_mut().rev().find(|block| {
                        !matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("thinking" | "redacted_thinking")
                        )
                    }) {
                        if block.get("cache_control").is_none() {
                            if let Some(object) = block.as_object_mut() {
                                object.insert(
                                    "cache_control".to_string(),
                                    make_cache_control(&config.cache_ttl),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn make_cache_control(ttl: &str) -> Value {
    if ttl == "5m" {
        json!({"type": "ephemeral"})
    } else {
        json!({"type": "ephemeral", "ttl": ttl})
    }
}

fn count_existing(body: &Value) -> usize {
    let mut count = 0;
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        count += tools
            .iter()
            .filter(|item| item.get("cache_control").is_some())
            .count();
    }
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        count += system
            .iter()
            .filter(|item| item.get("cache_control").is_some())
            .count();
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                count += blocks
                    .iter()
                    .filter(|item| item.get("cache_control").is_some())
                    .count();
            }
        }
    }
    count
}

fn upgrade_existing_ttl(body: &mut Value, ttl: &str) {
    fn upgrade(value: &mut Value, ttl: &str) {
        if let Some(cache) = value
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        {
            if ttl == "5m" {
                cache.remove("ttl");
            } else {
                cache.insert("ttl".to_string(), json!(ttl));
            }
        }
    }

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for item in tools {
            upgrade(item, ttl);
        }
    }
    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
        for item in system {
            upgrade(item, ttl);
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    upgrade(block, ttl);
                }
            }
        }
    }
}
