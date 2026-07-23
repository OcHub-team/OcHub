//! Round-trip persistence for extended-thinking signatures.
//!
//! The messages dialect returns `thinking` content blocks carrying an opaque
//! `signature`. On a follow-up turn the *complete, unmodified* thinking block
//! (text + signature) must be replayed before the `tool_use` block it preceded,
//! or the upstream rejects the request ("a final assistant message must start
//! with a thinking block"). The chat/responses dialects have no field for the
//! signature and clients do not echo it back.
//!
//! Rather than smuggle the signature through the client (fragile — depends on
//! the client preserving reasoning text byte-for-byte), we key it on the
//! **tool_use id**, which clients reliably echo back in `tool_calls[].id` /
//! `tool_call_id` / `call_id`. When an upstream turn produces thinking +
//! tool_use we persist the turn's thinking blocks under each tool_use id; when
//! the client replays those tool calls we reconstruct the thinking blocks
//! verbatim from the store. The client never sees the signature.
//!
//! Storage is pluggable via [`SignatureStore`]; [`MemorySignatureStore`] is a
//! TTL-bounded in-process default suitable for the local gateway.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::SignatureCapture;

/// Keyed blob store for thinking-block payloads (JSON array of blocks).
pub trait SignatureStore: Send + Sync {
    fn put(&self, tool_use_id: &str, thinking_blocks_json: &str);
    fn get(&self, tool_use_id: &str) -> Option<String>;
}

/// In-process TTL store. Thinking blocks are small; the TTL just needs to
/// outlive an agentic tool loop.
pub struct MemorySignatureStore {
    inner: Mutex<HashMap<String, (Instant, String)>>,
    ttl: Duration,
    capacity: usize,
}

impl Default for MemorySignatureStore {
    fn default() -> Self {
        Self::new(Duration::from_secs(24 * 3600), 4096)
    }
}

impl MemorySignatureStore {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            capacity,
        }
    }
}

impl SignatureStore for MemorySignatureStore {
    fn put(&self, tool_use_id: &str, thinking_blocks_json: &str) {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (t, _)| now.duration_since(*t) < self.ttl);
        // Hard bound: drop oldest entries when full (rare; entries expire first).
        while map.len() >= self.capacity {
            let oldest = map
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
        map.insert(
            tool_use_id.to_string(),
            (now, thinking_blocks_json.to_string()),
        );
    }

    fn get(&self, tool_use_id: &str) -> Option<String> {
        let map = self.inner.lock().unwrap();
        let (t, payload) = map.get(tool_use_id)?;
        if Instant::now().duration_since(*t) >= self.ttl {
            return None;
        }
        Some(payload.clone())
    }
}

/// Pull the signed `thinking` blocks and the `tool_use` ids out of a finalized
/// assistant content array. Only thinking blocks that actually carry a non-empty
/// signature are returned (an unsigned block is useless for replay).
pub fn collect_thinking_and_tool_ids(content: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut thinking = Vec::new();
    let mut tool_ids = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                let signed = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if signed {
                    thinking.push(block.clone());
                }
            }
            Some("tool_use") => {
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    tool_ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    (thinking, tool_ids)
}

/// Build a [`SignatureCapture`] from a finalized assistant content array
/// (non-stream path; stream converters emit their own capture).
pub fn capture_from_content(content: &[Value]) -> Option<SignatureCapture> {
    let (thinking_blocks, tool_use_ids) = collect_thinking_and_tool_ids(content);
    if thinking_blocks.is_empty() || tool_use_ids.is_empty() {
        return None;
    }
    Some(SignatureCapture {
        thinking_blocks,
        tool_use_ids,
    })
}

/// Persist a turn's thinking blocks under each of its tool_use ids.
pub fn store_capture(store: &dyn SignatureStore, capture: &SignatureCapture) {
    if capture.thinking_blocks.is_empty() || capture.tool_use_ids.is_empty() {
        return;
    }
    let payload = match serde_json::to_string(&capture.thinking_blocks) {
        Ok(p) => p,
        Err(_) => return,
    };
    for id in &capture.tool_use_ids {
        if !id.is_empty() {
            store.put(id, &payload);
        }
    }
}

/// Reconstruct thinking blocks for assistant tool-use turns in an outbound
/// messages-dialect request body.
///
/// Only runs when `thinking` is enabled on the request (otherwise the blocks are
/// not required). For each assistant message that has `tool_use` blocks but no
/// leading `thinking` block, look the blocks up by the first tool_use id and
/// prepend them. If any such turn cannot be restored, `thinking` is removed from
/// the request as a safety net, so the upstream does not reject a tool-use turn
/// that is missing its required thinking block.
pub fn restore_thinking_blocks(body: &mut Value, store: &dyn SignatureStore) {
    if body.get("thinking").is_none() {
        return; // thinking disabled → no blocks required
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return;
    };

    // Gather (message index, lookup id) for assistant tool-use turns lacking a
    // thinking block.
    let mut lookups: Vec<(usize, String)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        let has_thinking = content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("thinking"));
        if has_thinking {
            continue;
        }
        let first_tool_id = content.iter().find_map(|b| {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                b.get("id").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        });
        if let Some(id) = first_tool_id {
            lookups.push((i, id));
        }
    }

    if lookups.is_empty() {
        return;
    }

    let mut unrestored = false;
    for (idx, id) in lookups {
        let mut restored = false;
        if let Some(payload) = store.get(&id) {
            if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(&payload) {
                if let Some(content) = body
                    .pointer_mut(&format!("/messages/{idx}/content"))
                    .and_then(Value::as_array_mut)
                {
                    for (j, block) in blocks.into_iter().enumerate() {
                        content.insert(j, block);
                    }
                    restored = true;
                }
            }
        }
        if !restored {
            unrestored = true;
        }
    }

    // Safety net: a tool-use turn missing its thinking block would be rejected
    // with thinking enabled.
    if unrestored {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("thinking");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_signed_thinking_and_tool_ids() {
        let content = vec![
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig1"}),
            json!({"type": "thinking", "thinking": "unsigned", "signature": ""}),
            json!({"type": "text", "text": "hi"}),
            json!({"type": "tool_use", "id": "tool_1", "name": "f", "input": {}}),
        ];
        let (thinking, ids) = collect_thinking_and_tool_ids(&content);
        assert_eq!(thinking.len(), 1); // only the signed block
        assert_eq!(thinking[0]["signature"], "sig1");
        assert_eq!(ids, vec!["tool_1".to_string()]);
    }

    #[test]
    fn restore_prepends_blocks_and_keeps_thinking() {
        let store = MemorySignatureStore::default();
        store.put(
            "tool_1",
            r#"[{"type":"thinking","thinking":"t","signature":"s"}]"#,
        );
        let mut body = json!({
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tool_1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": "ok"}]}
            ]
        });
        restore_thinking_blocks(&mut body, &store);
        assert!(body.get("thinking").is_some());
        assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
    }

    #[test]
    fn restore_disables_thinking_when_unrestorable() {
        let store = MemorySignatureStore::default();
        let mut body = json!({
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "unknown", "name": "f", "input": {}}
                ]}
            ]
        });
        restore_thinking_blocks(&mut body, &store);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn memory_store_expires() {
        let store = MemorySignatureStore::new(Duration::from_millis(0), 16);
        store.put("k", "v");
        assert_eq!(store.get("k"), None);
    }
}
