//! Bedrock-oriented Claude thinking optimizer.

use crate::db::proxy_types::OptimizerConfig;
use serde_json::{json, Value};

pub fn optimize(body: &mut Value, config: &OptimizerConfig) {
    if !config.thinking_optimizer {
        return;
    }

    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return;
    };
    let model = model.to_ascii_lowercase();
    if model.contains("haiku") {
        return;
    }

    if uses_adaptive_thinking(&model) {
        body["thinking"] = json!({"type": "adaptive"});
        body["output_config"] = json!({"effort": "max"});
        append_beta(body, "context-1m-2025-08-07");
        return;
    }

    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(16_384);
    let target_budget = max_tokens.saturating_sub(1);

    match body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
    {
        None | Some("disabled") => {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": target_budget
            });
        }
        Some("enabled") => {
            let current = body["thinking"]
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if current < target_budget {
                body["thinking"]["budget_tokens"] = json!(target_budget);
            }
        }
        _ => {}
    }

    append_beta(body, "interleaved-thinking-2025-05-14");
}

fn uses_adaptive_thinking(model: &str) -> bool {
    let normalized = model.replace('.', "-");
    ["opus-4-8", "opus-4-7", "opus-4-6", "sonnet-4-6"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn append_beta(body: &mut Value, beta: &str) {
    match body.get_mut("anthropic_beta") {
        Some(Value::Array(values)) => {
            if !values.iter().any(|value| value.as_str() == Some(beta)) {
                values.push(json!(beta));
            }
        }
        _ => body["anthropic_beta"] = json!([beta]),
    }
}
