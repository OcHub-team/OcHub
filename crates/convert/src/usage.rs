//! Usage accounting conversion between the three dialects.
//!
//! Token semantics differ across dialects and getting them wrong corrupts both
//! the client display and local accounting:
//! - *messages* `input_tokens` **excludes** cached tokens (cache reads/writes are
//!   reported separately).
//! - *chat* `prompt_tokens` and *responses* `input_tokens` are **totals**
//!   (cached tokens included, detailed in `*_tokens_details`).

use serde_json::{json, Value};

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// messages usage → chat `usage` object.
pub fn messages_usage_to_chat(usage: &Value) -> Value {
    let input = u64_field(usage, "input_tokens");
    let cached = u64_field(usage, "cache_read_input_tokens");
    let cache_creation = u64_field(usage, "cache_creation_input_tokens");
    let output = u64_field(usage, "output_tokens");
    let prompt = input + cached + cache_creation;
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": output,
        "total_tokens": prompt + output,
        // `cached_tokens` = cache reads (standard field). `cache_creation_input_tokens`
        // is a passthrough so callers can also observe cache writes.
        "prompt_tokens_details": {
            "cached_tokens": cached,
            "cache_creation_input_tokens": cache_creation,
        },
        "completion_tokens_details": { "reasoning_tokens": 0 }
    })
}

/// messages usage → responses `usage` object.
pub fn messages_usage_to_responses(usage: &Value) -> Value {
    let input = u64_field(usage, "input_tokens");
    let cached = u64_field(usage, "cache_read_input_tokens");
    let cache_creation = u64_field(usage, "cache_creation_input_tokens");
    let output = u64_field(usage, "output_tokens");
    let total_input = input + cached + cache_creation;
    json!({
        "input_tokens": total_input,
        "input_tokens_details": {
            "cached_tokens": cached,
            "cache_creation_input_tokens": cache_creation,
        },
        "output_tokens": output,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": total_input + output
    })
}

/// chat usage → messages `usage` object.
///
/// chat `prompt_tokens` is a total (cached tokens included), so the
/// messages-side (exclusive) input is `prompt - cached - cache_writes`,
/// saturating at 0 against inconsistent upstream accounting.
pub fn chat_usage_to_messages(usage: &Value) -> Value {
    let prompt = u64_field(usage, "prompt_tokens");
    let details = usage.get("prompt_tokens_details");
    let cached = details.map(|d| u64_field(d, "cached_tokens")).unwrap_or(0);
    let cache_creation = details
        .map(|d| u64_field(d, "cache_creation_input_tokens"))
        .unwrap_or(0);
    json!({
        "input_tokens": prompt.saturating_sub(cached + cache_creation),
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cached,
        "output_tokens": u64_field(usage, "completion_tokens"),
        "service_tier": "standard"
    })
}

/// responses usage → messages `usage` object.
///
/// `input_tokens` here is a total, so the messages-side (exclusive) input is
/// `input - cached - cache_writes`, saturating at 0 against inconsistent
/// upstream accounting.
pub fn responses_usage_to_messages(usage: &Value) -> Value {
    let input = u64_field(usage, "input_tokens");
    let details = usage.get("input_tokens_details");
    let cached = details.map(|d| u64_field(d, "cached_tokens")).unwrap_or(0);
    let cache_creation = details
        .map(|d| {
            let w = u64_field(d, "cache_write_tokens");
            if w != 0 {
                w
            } else {
                u64_field(d, "cache_creation_input_tokens")
            }
        })
        .unwrap_or(0);
    let output = u64_field(usage, "output_tokens");
    json!({
        "input_tokens": input.saturating_sub(cached + cache_creation),
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cached,
        "output_tokens": output,
        "service_tier": "standard"
    })
}

/// Merge a `message_delta` usage object onto the running usage seeded from
/// `message_start`. On the wire the delta usage carries the cumulative
/// `output_tokens` but frequently omits (or nulls) the prompt/cache fields, so a
/// plain overwrite would zero out the prompt accounting. Overlay only the
/// non-null fields the delta actually provides.
pub fn merge_messages_usage(base: &mut Option<Value>, delta: &Value) {
    match base {
        Some(Value::Object(base_obj)) => {
            if let Some(delta_obj) = delta.as_object() {
                for (k, v) in delta_obj {
                    if !v.is_null() {
                        base_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        // No seed yet (delta arrived before a usable message_start usage).
        _ => *base = Some(delta.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_usage_totals_include_cache() {
        let u = json!({
            "input_tokens": 10, "output_tokens": 5,
            "cache_read_input_tokens": 20, "cache_creation_input_tokens": 30
        });
        let chat = messages_usage_to_chat(&u);
        assert_eq!(chat["prompt_tokens"], 60);
        assert_eq!(chat["completion_tokens"], 5);
        assert_eq!(chat["total_tokens"], 65);
        assert_eq!(chat["prompt_tokens_details"]["cached_tokens"], 20);
    }

    #[test]
    fn chat_usage_round_trips_to_messages() {
        let chat = json!({
            "prompt_tokens": 60,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 20, "cache_creation_input_tokens": 30 }
        });
        let messages = chat_usage_to_messages(&chat);
        assert_eq!(messages["input_tokens"], 10);
        assert_eq!(messages["cache_read_input_tokens"], 20);
        assert_eq!(messages["cache_creation_input_tokens"], 30);
        assert_eq!(messages["output_tokens"], 5);
        // And back: totals reconstruct.
        let back = messages_usage_to_chat(&messages);
        assert_eq!(back["prompt_tokens"], 60);
        assert_eq!(back["total_tokens"], 65);
    }

    #[test]
    fn responses_usage_round_trips_to_messages() {
        let responses = json!({
            "input_tokens": 60,
            "input_tokens_details": { "cached_tokens": 20, "cache_creation_input_tokens": 30 },
            "output_tokens": 5,
            "total_tokens": 65
        });
        let messages = responses_usage_to_messages(&responses);
        assert_eq!(messages["input_tokens"], 10);
        assert_eq!(messages["cache_read_input_tokens"], 20);
        assert_eq!(messages["cache_creation_input_tokens"], 30);
        // And back: totals reconstruct.
        let back = messages_usage_to_responses(&messages);
        assert_eq!(back["input_tokens"], 60);
        assert_eq!(back["total_tokens"], 65);
    }

    #[test]
    fn merge_keeps_prompt_fields_from_seed() {
        let mut base =
            Some(json!({ "input_tokens": 10, "cache_read_input_tokens": 2, "output_tokens": 1 }));
        merge_messages_usage(
            &mut base,
            &json!({ "output_tokens": 15, "input_tokens": null }),
        );
        let merged = base.unwrap();
        assert_eq!(merged["input_tokens"], 10);
        assert_eq!(merged["cache_read_input_tokens"], 2);
        assert_eq!(merged["output_tokens"], 15);
    }

    #[test]
    fn merge_adopts_delta_without_seed() {
        let mut base = None;
        merge_messages_usage(&mut base, &json!({ "output_tokens": 3 }));
        assert_eq!(base.unwrap()["output_tokens"], 3);
    }
}
