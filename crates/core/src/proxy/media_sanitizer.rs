//! Media fallback for text-only upstream models.

use crate::model::Provider;
use crate::proxy::error::ProxyError;
use serde_json::{json, Value};

pub const UNSUPPORTED_IMAGE_MARKER: &str = "[Unsupported Image]";

pub fn replace_images_for_text_only_model(
    body: &mut Value,
    provider: &Provider,
    allow_heuristic: bool,
) -> usize {
    if !contains_image_blocks(body) {
        return 0;
    }

    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    match explicit_model_image_support(provider, model) {
        Some(true) => return 0,
        Some(false) => return replace_images_in_body(body),
        None => {}
    }

    if !allow_heuristic || !known_text_only_model(model) {
        return 0;
    }

    replace_images_in_body(body)
}

pub fn contains_image_blocks(body: &Value) -> bool {
    messages_have_image_blocks(body) || responses_input_has_image_blocks(body.get("input"))
}

pub fn replace_image_blocks_with_marker(body: &mut Value) -> usize {
    replace_images_in_body(body)
}

pub fn is_unsupported_image_error(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError { status, body } = error else {
        return false;
    };
    if !matches!(*status, 400 | 415 | 422 | 501) {
        return false;
    }

    let Some(body) = body.as_deref() else {
        return false;
    };
    let message = extract_error_text(body).to_ascii_lowercase();
    let mentions_image = [
        "image",
        "vision",
        "multimodal",
        "multi-modal",
        "modality",
        "modalities",
        "media",
        "attachment",
    ]
    .iter()
    .any(|needle| message.contains(needle));
    if !mentions_image {
        return false;
    }

    [
        "unsupported",
        "not supported",
        "does not support",
        "doesn't support",
        "do not support",
        "don't support",
        "only supports text",
        "text only",
        "text-only",
        "invalid content type",
        "invalid message content",
        "unknown variant",
        "unknown content type",
        "unrecognized content type",
        "cannot process",
        "cannot handle",
        "can't process",
        "can't handle",
        "unable to process",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn messages_have_image_blocks(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content"))
                .any(content_has_image_blocks)
        })
}

fn content_has_image_blocks(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            is_image_block_type(block.get("type").and_then(Value::as_str))
                || block.get("content").is_some_and(content_has_image_blocks)
        })
    })
}

fn responses_input_has_image_blocks(input: Option<&Value>) -> bool {
    match input {
        Some(Value::Array(items)) => items.iter().any(responses_input_item_has_image_blocks),
        Some(item @ Value::Object(_)) => responses_input_item_has_image_blocks(item),
        _ => false,
    }
}

fn responses_input_item_has_image_blocks(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("input_image")
        || item.get("content").is_some_and(content_has_image_blocks)
}

fn replace_images_in_body(body: &mut Value) -> usize {
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .map(|messages| {
            messages
                .iter_mut()
                .filter_map(|message| message.get_mut("content"))
                .map(|content| replace_images_in_content_with_text_type(content, "text"))
                .sum()
        })
        .unwrap_or(0);

    messages
        + body
            .get_mut("input")
            .map(replace_images_in_responses_input)
            .unwrap_or(0)
}

fn replace_images_in_responses_input(input: &mut Value) -> usize {
    match input {
        Value::Array(items) => items
            .iter_mut()
            .map(replace_images_in_responses_input_item)
            .sum(),
        Value::Object(_) => replace_images_in_responses_input_item(input),
        _ => 0,
    }
}

fn replace_images_in_responses_input_item(item: &mut Value) -> usize {
    let mut replaced = 0;
    if item.get("type").and_then(Value::as_str) == Some("input_image") {
        replace_image_block_with_text_marker(item, "input_text");
        replaced += 1;
    }
    if let Some(content) = item.get_mut("content") {
        replaced += replace_images_in_content_with_text_type(content, "input_text");
    }
    replaced
}

fn replace_images_in_content_with_text_type(content: &mut Value, text_type: &str) -> usize {
    let Some(blocks) = content.as_array_mut() else {
        return 0;
    };
    let mut replaced = 0;
    for block in blocks {
        if is_image_block_type(block.get("type").and_then(Value::as_str)) {
            replace_image_block_with_text_marker(block, text_type);
            replaced += 1;
        } else if let Some(nested) = block.get_mut("content") {
            replaced += replace_images_in_content_with_text_type(nested, text_type);
        }
    }
    replaced
}

fn is_image_block_type(block_type: Option<&str>) -> bool {
    matches!(block_type, Some("image" | "image_url" | "input_image"))
}

fn replace_image_block_with_text_marker(block: &mut Value, text_type: &str) {
    let cache_control = block.get("cache_control").cloned();
    *block = json!({
        "type": text_type,
        "text": UNSUPPORTED_IMAGE_MARKER,
    });
    if let (Some(cache_control), Some(object)) = (cache_control, block.as_object_mut()) {
        object.insert("cache_control".to_string(), cache_control);
    }
}

fn explicit_model_image_support(provider: &Provider, model: &str) -> Option<bool> {
    let settings = &provider.settings_config;
    [
        settings
            .get("modelCatalog")
            .and_then(|catalog| catalog.get("models")),
        settings.get("modelCatalog"),
        settings.get("models"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| explicit_model_image_support_in_value(value, model))
}

fn explicit_model_image_support_in_value(value: &Value, model: &str) -> Option<bool> {
    if let Some(models) = value.as_array() {
        return models.iter().find_map(|entry| {
            model_entry_matches(entry, None, model).then(|| explicit_image_support(entry))?
        });
    }

    value.as_object()?.iter().find_map(|(key, entry)| {
        model_entry_matches(entry, Some(key), model).then(|| explicit_image_support(entry))?
    })
}

fn explicit_image_support(entry: &Value) -> Option<bool> {
    if let Some(value) = entry
        .get("supportsImage")
        .or_else(|| entry.get("supports_image"))
        .or_else(|| entry.get("vision"))
        .and_then(Value::as_bool)
    {
        return Some(value);
    }

    [
        entry.get("input"),
        entry.pointer("/modalities/input"),
        entry.get("input_modalities"),
        entry.get("inputModalities"),
    ]
    .into_iter()
    .flatten()
    .find_map(input_modalities_support_image)
}

fn input_modalities_support_image(value: &Value) -> Option<bool> {
    Some(value.as_array()?.iter().any(|item| {
        item.as_str()
            .map(str::trim)
            .is_some_and(|item| item.eq_ignore_ascii_case("image"))
    }))
}

fn model_entry_matches(entry: &Value, key: Option<&str>, model: &str) -> bool {
    key.is_some_and(|key| model_ids_match(key, model))
        || ["model", "id", "name"]
            .into_iter()
            .filter_map(|field| entry.get(field).and_then(Value::as_str))
            .any(|candidate| model_ids_match(candidate, model))
}

fn model_ids_match(candidate: &str, model: &str) -> bool {
    let candidate = normalize_model_id(candidate);
    let model = normalize_model_id(model);
    if candidate.is_empty() || model.is_empty() {
        return false;
    }
    if candidate == model {
        return true;
    }

    let candidate_tail = candidate.rsplit('/').next().unwrap_or(candidate.as_str());
    let model_tail = model.rsplit('/').next().unwrap_or(model.as_str());
    candidate_tail == model_tail || candidate == model_tail || candidate_tail == model
}

fn known_text_only_model(model: &str) -> bool {
    let normalized = normalize_model_id(model);
    let tail = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    const EXACT_TAILS: &[&str] = &[
        "ark-code-latest",
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "glm-5.1",
        "kat-coder",
        "kat-coder-pro",
        "kat-coder-pro v1",
        "kat-coder-pro v2",
        "kat-coder-pro-v1",
        "kat-coder-pro-v2",
        "ling-2.5-1t",
        "longcat-flash-chat",
        "mimo-v2.5-pro",
        "us.deepseek.r1-v1",
    ];
    const TAIL_PREFIXES: &[&str] = &["minimax-m2.7", "qwen3-coder", "step-3.5-flash"];
    EXACT_TAILS.contains(&tail) || TAIL_PREFIXES.iter().any(|prefix| tail.starts_with(prefix))
}

fn normalize_model_id(value: &str) -> String {
    crate::proxy::model_mapper::strip_one_m_suffix_for_upstream(value)
        .trim()
        .trim_start_matches("models/")
        .trim()
        .to_ascii_lowercase()
}

fn extract_error_text(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        for pointer in ["/error/message", "/message", "/detail", "/error"] {
            if let Some(message) = value.pointer(pointer).and_then(Value::as_str) {
                return message.to_string();
            }
        }
        if let Ok(compact) = serde_json::to_string(&value) {
            return compact;
        }
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;
    use serde_json::json;

    fn provider(settings_config: Value) -> Provider {
        Provider::with_id("p".to_string(), "P".to_string(), settings_config, None)
    }

    #[test]
    fn known_text_model_replaces_images() {
        let mut body = json!({
            "model": "deepseek/deepseek-v4-pro",
            "messages": [{"role": "user", "content": [{"type": "image", "source": {}}]}]
        });

        assert_eq!(
            replace_images_for_text_only_model(&mut body, &provider(json!({})), true),
            1
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn explicit_image_model_preserves_images() {
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{"role": "user", "content": [{"type": "image", "source": {}}]}]
        });
        let provider = provider(json!({
            "models": [{"id": "deepseek-v4-pro", "input": ["text", "image"]}]
        }));

        assert_eq!(
            replace_images_for_text_only_model(&mut body, &provider, true),
            0
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
    }

    #[test]
    fn recognizes_unsupported_image_errors() {
        let error = ProxyError::UpstreamError {
            status: 400,
            body: Some(r#"{"error":{"message":"This model does not support image input"}}"#.into()),
        };
        assert!(is_unsupported_image_error(&error));
    }
}
